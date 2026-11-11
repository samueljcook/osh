//! The worker's database steps on native Queue storage, with the published steps' outcomes.
//!
//! Each step runs inside the transaction that the worker step owns. A step waits for a job that
//! another transaction holds instead of skipping the job.

use super::model::{
	RuntimeCancellationSignal, TaskRegistry, WorkerClaimOwner, WorkerConfig, WorkerHandle,
	WorkerOwnerId, WorkerRunLoopSummary, WorkerRuntime, WorkerTaskState,
};
use crate::db::{WritePool, WriteTx};
use crate::queue::engine::{
	NativeClaimCheckRequest, NativeClaimRequest, NativeQueueDeadLetterMove, NativeQueueGroupError,
	NativeQueueGroupOutcome, NativeQueueOwnedReconciliation, NativeQueueStaleClaimRecovery,
	OwnedJobMutation, PgNativeQueueGroup, RequiredExecutionState, RetryConsumption,
};
use crate::queue::{
	ClaimedJob, DeadLetterReason, DeleteFinishedJobsError, FromNativeQueueError, JobId, JobStatus,
	OwnedJobUpdate, RecoverStaleClaimsError, TaskName, WorkerClaimError, WorkerClaimToken,
	WorkerName, WorkerPersistenceError, WorkerRunError, WorkerStepError, run_worker_step,
};
use std::time::Duration;

/// The wait for the statement that selects stale-claim candidates.
const CANDIDATE_SELECTION_TIMEOUT: Duration = Duration::from_secs(30);

impl PgNativeQueueGroup {
	/// Claims at most `claim_limit` due jobs of `task_names` under one claim token.
	pub(in crate::queue) async fn worker_claim(
		&self,
		tx: &mut WriteTx<'_>,
		task_names: &[TaskName],
		claim_limit: u32,
		worker_owner_id: &WorkerOwnerId,
		worker_claim_token: WorkerClaimToken,
	) -> Result<Vec<ClaimedJob>, WorkerClaimError> {
		let names: Vec<&str> = task_names.iter().map(TaskName::as_str).collect();
		let candidates = self
			.select_due(tx, &names, claim_limit)
			.await
			.map_err(WorkerClaimError::from_native_queue_error)?;
		let mut claims = Vec::new();
		for job_id in candidates {
			let request = NativeClaimRequest {
				job_id,
				owner: worker_owner_id.clone(),
				token: worker_claim_token.clone(),
			};
			match self
				.claim(&mut *tx, &request)
				.await
				.map_err(WorkerClaimError::from_native_queue_error)?
			{
				NativeQueueGroupOutcome::Claimed { job } => claims.push(ClaimedJob::new(
					*job,
					worker_owner_id.clone(),
					worker_claim_token.clone(),
				)),
				NativeQueueGroupOutcome::ClaimSkipped => {}
				outcome => {
					return Err(WorkerClaimError::from_native_queue_error(
						NativeQueueGroupError::UnexpectedOutcome(format!("{outcome:?}")),
					));
				}
			}
		}
		Ok(claims)
	}

	/// Applies one mutation of a running job that `claimed_job` holds.
	pub(in crate::queue) async fn worker_owned<F: FromNativeQueueError>(
		&self,
		tx: &mut WriteTx<'_>,
		claimed_job: &ClaimedJob,
		mutation: OwnedJobMutation,
	) -> Result<OwnedJobUpdate<()>, F> {
		match self
			.reconcile_owned(
				&mut *tx,
				NativeQueueOwnedReconciliation {
					authority: claimed_job,
					mutation,
				},
			)
			.await
			.map_err(F::from_native_queue_error)?
		{
			NativeQueueGroupOutcome::Reconciled => Ok(OwnedJobUpdate::Applied(())),
			NativeQueueGroupOutcome::Unchanged => Ok(OwnedJobUpdate::NotRunning),
			outcome => Err(F::from_native_queue_error(
				NativeQueueGroupError::UnexpectedOutcome(format!("{outcome:?}")),
			)),
		}
	}

	/// Fails the running job and moves it to dead-letter storage in the same transaction.
	pub(in crate::queue) async fn worker_move_to_dead_letter(
		&self,
		tx: &mut WriteTx<'_>,
		claimed_job: &ClaimedJob,
		error_msg: &str,
		increment_retry_count: bool,
		reason: DeadLetterReason,
	) -> Result<OwnedJobUpdate<JobId>, WorkerPersistenceError> {
		let failed = self
			.worker_owned::<WorkerPersistenceError>(
				tx,
				claimed_job,
				OwnedJobMutation::Fail {
					error_msg: error_msg.to_owned(),
					retry_consumption: retry_consumption(increment_retry_count),
				},
			)
			.await?;
		if !matches!(failed, OwnedJobUpdate::Applied(())) {
			return Ok(OwnedJobUpdate::NotRunning);
		}
		let dead_letter_id = JobId::generate()?;
		match self
			.move_to_dead_letter(
				&mut *tx,
				NativeQueueDeadLetterMove {
					job_id: claimed_job.job_id(),
					dead_letter_id,
					reason,
				},
			)
			.await
			.map_err(WorkerPersistenceError::from_native_queue_error)?
		{
			NativeQueueGroupOutcome::DeadLetterMoved { dead_letter_id } => {
				Ok(OwnedJobUpdate::Applied(dead_letter_id))
			}
			outcome => Err(WorkerPersistenceError::from_native_queue_error(
				NativeQueueGroupError::UnexpectedOutcome(format!("{outcome:?}")),
			)),
		}
	}

	/// Applies the same return-to-pending to every supplied claim and counts the jobs it moved.
	pub(in crate::queue) async fn worker_return_claims(
		&self,
		tx: &mut WriteTx<'_>,
		claimed_jobs: &[ClaimedJob],
		required: RequiredExecutionState,
	) -> Result<u64, WorkerStepError> {
		let mut returned = 0;
		for claimed_job in claimed_jobs {
			if let OwnedJobUpdate::Applied(()) = self
				.worker_owned::<WorkerStepError>(
					tx,
					claimed_job,
					OwnedJobMutation::ReturnPending(required),
				)
				.await?
			{
				returned += 1;
			}
		}
		Ok(returned)
	}

	/// Returns the unstarted running jobs that one claim token holds to pending.
	pub(in crate::queue) async fn worker_return_jobs_of_token(
		&self,
		tx: &mut WriteTx<'_>,
		worker_owner_id: &WorkerOwnerId,
		worker_claim_token: &WorkerClaimToken,
	) -> Result<(), WorkerStepError> {
		let candidates = self
			.select_running_by_owner(tx, worker_owner_id.as_str())
			.await
			.map_err(WorkerStepError::from_native_queue_error)?;
		for job_id in candidates {
			let request = NativeClaimCheckRequest {
				job_id,
				owner: worker_owner_id.clone(),
				token: worker_claim_token.clone(),
			};
			match self
				.check_claim(&mut *tx, &request)
				.await
				.map_err(WorkerStepError::from_native_queue_error)?
			{
				NativeQueueGroupOutcome::ClaimReturned
				| NativeQueueGroupOutcome::ClaimNotReturned => {}
				outcome => {
					return Err(WorkerStepError::from_native_queue_error(
						NativeQueueGroupError::UnexpectedOutcome(format!("{outcome:?}")),
					));
				}
			}
		}
		Ok(())
	}

	/// Recovers running jobs after their claim heartbeats have stopped for longer than the installation's
	/// stale threshold, oldest heartbeat first, at most `limit` per call. Each recovery is its own
	/// transaction. Returns how many jobs were recovered.
	pub(in crate::queue) async fn recover_stale_claims(
		&self,
		pool: &WritePool,
		limit: u32,
	) -> Result<u32, RecoverStaleClaimsError> {
		let candidates = run_worker_step::<_, RecoverStaleClaimsError>(
			pool,
			"select running jobs by oldest heartbeat",
			CANDIDATE_SELECTION_TIMEOUT,
			async |tx| {
				self.select_stale_running(tx, limit)
					.await
					.map_err(RecoverStaleClaimsError::from_native_queue_error)
			},
		)
		.await?;
		let mut recovered = 0;
		'groups: for group in candidates {
			for job_id in group {
				if recovered == limit {
					break 'groups;
				}
				let dead_letter_id = JobId::generate()?;
				match self
					.recover_stale_claim(
						pool,
						NativeQueueStaleClaimRecovery {
							job_id,
							dead_letter_id,
						},
					)
					.await
					.map_err(RecoverStaleClaimsError::from_native_queue_error)?
				{
					NativeQueueGroupOutcome::Reconciled
					| NativeQueueGroupOutcome::DeadLetterMoved { .. } => {
						recovered += 1;
					}
					// A candidate can complete or renew its heartbeat after selection.
					// Its current state establishes nothing about later candidates.
					NativeQueueGroupOutcome::Unchanged => {}
					outcome => {
						return Err(RecoverStaleClaimsError::from_native_queue_error(
							NativeQueueGroupError::UnexpectedOutcome(format!("{outcome:?}")),
						));
					}
				}
			}
		}
		Ok(recovered)
	}

	/// Deletes at most `request.limit` records from each of completed jobs, failed jobs, and dead
	/// letters, oldest first, when their database age exceeds their retention. Each deletion is
	/// its own transaction, which checks the age again under the record's lock.
	pub(in crate::queue) async fn delete_finished(
		&self,
		pool: &WritePool,
		request: NativeFinishedDeletion,
	) -> Result<NativeFinishedDeletionCounts, DeleteFinishedJobsError> {
		let mut counts = NativeFinishedDeletionCounts {
			completed: 0,
			failed: 0,
			dead_letters: 0,
		};
		for (status, retention_micros) in [
			(JobStatus::Completed, request.completed_retention_micros),
			(JobStatus::Failed, request.failed_retention_micros),
		] {
			let candidates = run_worker_step::<_, DeleteFinishedJobsError>(
				pool,
				"select finished jobs",
				CANDIDATE_SELECTION_TIMEOUT,
				async |tx| {
					self.select_finished(tx, status, retention_micros, request.limit)
						.await
						.map_err(DeleteFinishedJobsError::from_native_queue_error)
				},
			)
			.await?;
			for job_id in candidates {
				if let NativeQueueGroupOutcome::JobDeleted = self
					.delete_finished_job(pool, job_id, retention_micros)
					.await
					.map_err(DeleteFinishedJobsError::from_native_queue_error)?
				{
					match status {
						JobStatus::Completed => counts.completed += 1,
						_ => counts.failed += 1,
					}
				}
			}
		}
		let dead_letters = run_worker_step::<_, DeleteFinishedJobsError>(
			pool,
			"select expired dead letters",
			CANDIDATE_SELECTION_TIMEOUT,
			async |tx| {
				self.select_expired_dead_letters(
					tx,
					request.dead_letter_retention_micros,
					request.limit,
				)
				.await
				.map_err(DeleteFinishedJobsError::from_native_queue_error)
			},
		)
		.await?;
		for dead_letter_id in dead_letters {
			if let NativeQueueGroupOutcome::DeadLetterDeleted = self
				.delete_expired_dead_letter(
					pool,
					dead_letter_id,
					request.dead_letter_retention_micros,
				)
				.await
				.map_err(DeleteFinishedJobsError::from_native_queue_error)?
			{
				counts.dead_letters += 1;
			}
		}
		Ok(counts)
	}
}

/// Retention ages in microseconds and the per-category limit of one cleanup pass.
#[derive(Clone, Copy)]
pub(crate) struct NativeFinishedDeletion {
	pub(in crate::queue) completed_retention_micros: i64,
	pub(in crate::queue) failed_retention_micros: i64,
	pub(in crate::queue) dead_letter_retention_micros: i64,
	pub(in crate::queue) limit: u32,
}

/// How many records one cleanup pass deleted.
pub(in crate::queue) struct NativeFinishedDeletionCounts {
	pub(in crate::queue) completed: u32,
	pub(in crate::queue) failed: u32,
	pub(in crate::queue) dead_letters: u32,
}

pub(in crate::queue) fn retry_consumption(increment_retry_count: bool) -> RetryConsumption {
	if increment_retry_count {
		RetryConsumption::ConsumeAttempt
	} else {
		RetryConsumption::KeepCount
	}
}

/// Explicit stale-claim recovery that a native worker runs beside its worker loop.
pub(crate) struct NativeWorkerRecovery {
	pub(in crate::queue) interval: Duration,
	pub(in crate::queue) batch_size: u32,
}

/// Cleanup passes that a native worker runs beside its worker loop while it holds the
/// installation's Queue cleanup leadership.
pub(crate) struct NativeWorkerCleanup {
	pub(in crate::queue) cron: crate::fleet::PgCron,
	pub(in crate::queue) deletion: NativeFinishedDeletion,
	pub(in crate::queue) delay_between_passes: Duration,
}

/// Starts the worker loop on native Queue storage, a task that recovers stale claims every
/// `recovery.interval`, and, when `cleanup` is present, a cron that deletes finished jobs and
/// dead letters. A recovery or cleanup pass that times out or meets a transient database
/// failure runs again at its next interval; any other recovery or cleanup failure stops the
/// worker and is its result.
pub(crate) fn start_native_worker(
	queue: PgNativeQueueGroup,
	pool: WritePool,
	task_registry: TaskRegistry,
	worker_name: &WorkerName,
	config: WorkerConfig,
	recovery: NativeWorkerRecovery,
	cleanup: Option<NativeWorkerCleanup>,
) -> WorkerHandle {
	let worker_shutdown_signal = RuntimeCancellationSignal::new();
	let runtime = WorkerRuntime {
		queue: queue.clone(),
		pool: pool.clone(),
		task_registry,
		claim_owner: WorkerClaimOwner::PerBatch(worker_name.clone()),
		config,
		worker_shutdown_signal: worker_shutdown_signal.clone(),
	};
	let join_handle = tokio::spawn(run_native_worker(NativeWorkerRun {
		runtime,
		queue,
		pool,
		recovery,
		cleanup,
	}));
	WorkerHandle {
		worker_shutdown_signal,
		task: WorkerTaskState::Running(join_handle),
	}
}

struct NativeWorkerRun {
	runtime: WorkerRuntime,
	queue: PgNativeQueueGroup,
	pool: WritePool,
	recovery: NativeWorkerRecovery,
	cleanup: Option<NativeWorkerCleanup>,
}

async fn run_native_worker(run: NativeWorkerRun) -> Result<WorkerRunLoopSummary, WorkerRunError> {
	let shutdown = run.runtime.worker_shutdown_signal.clone();
	let mut worker = tokio::spawn(super::run_loop::run_queue_worker_loop(run.runtime));
	let recovery = async {
		loop {
			tokio::select! {
				() = shutdown.wait_until_cancellation_requested() => return Ok(()),
				() = tokio::time::sleep(run.recovery.interval) => {}
			}
			match run
				.queue
				.recover_stale_claims_automatically(
					&run.pool,
					run.recovery.interval,
					run.recovery.batch_size,
				)
				.await
			{
				Ok(_) => {}
				// A pass that its own timeout or a transient database failure ended runs again
				// at the next interval.
				Err(
					RecoverStaleClaimsError::TimedOut { .. }
					| RecoverStaleClaimsError::CommitTimedOut { .. },
				) => {}
				Err(error) if crate::db::is_transient_database_failure(&error) => {}
				Err(error) => return Err(WorkerRunError::StaleClaimRecovery(error)),
			}
		}
	};
	let cleanup = async {
		let Some(cleanup) = &run.cleanup else {
			return Ok(());
		};
		let pass = CleanupPass {
			queue: run.queue.clone(),
			pool: run.pool.clone(),
			deletion: cleanup.deletion,
			delay_between_passes: cleanup.delay_between_passes,
			shutdown: shutdown.clone(),
		};
		cleanup
			.cron
			.run_until_stopped(
				shutdown.wait_until_cancellation_requested(),
				move |ctx| {
					let pass = pass.clone();
					async move { pass.run(&ctx).await }
				},
				|error| match error {
					DeleteFinishedJobsError::TimedOut { .. }
					| DeleteFinishedJobsError::CommitTimedOut { .. } => {
						crate::fleet::PgCronTaskErrorAction::Continue
					}
					error if crate::db::is_transient_database_failure(error) => {
						crate::fleet::PgCronTaskErrorAction::Continue
					}
					_ => crate::fleet::PgCronTaskErrorAction::Stop,
				},
			)
			.await
			.map_err(|error| WorkerRunError::Cleanup(Box::new(error)))
	};
	let maintained = async { tokio::try_join!(recovery, cleanup) };
	tokio::pin!(maintained);
	tokio::select! {
		joined = &mut worker => {
			shutdown.request_cancellation();
			let cleanup_res = maintained.await;
			let summary = joined.map_err(WorkerRunError::WorkerTaskJoinFailed)??;
			cleanup_res?;
			Ok(summary)
		}
		maintained = &mut maintained => match maintained {
			// Recovery and cleanup end without failure only after a shutdown request.
			Ok(((), ())) => worker.await.map_err(WorkerRunError::WorkerTaskJoinFailed)?,
			Err(error) => {
				shutdown.request_cancellation();
				// The recovery or cleanup failure is the worker's result; the stopped loop's own
				// result is superseded by it.
				drop(worker.await);
				Err(error)
			}
		},
	}
}

/// What one cleanup of a native worker needs.
#[derive(Clone)]
struct CleanupPass {
	queue: PgNativeQueueGroup,
	pool: WritePool,
	deletion: NativeFinishedDeletion,
	delay_between_passes: Duration,
	shutdown: RuntimeCancellationSignal,
}

impl CleanupPass {
	/// Runs cleanup passes until a pass deletes fewer records of every kind than its limit, the
	/// cron's leadership is lost, or the worker's shutdown is requested. Each deletion is its
	/// own transaction, so a pass that a shutdown abandons keeps every deletion that it made.
	async fn run(&self, ctx: &crate::fleet::PgLeaseTaskCtx) -> Result<(), DeleteFinishedJobsError> {
		let limit = self.deletion.limit;
		loop {
			if matches!(ctx.leadership(), crate::fleet::PgLeadership::Lost) {
				return Ok(());
			}
			let counts = tokio::select! {
				() = self.shutdown.wait_until_cancellation_requested() => return Ok(()),
				counts = self.queue.delete_finished(&self.pool, self.deletion) => counts?,
			};
			if counts.completed < limit && counts.failed < limit && counts.dead_letters < limit {
				return Ok(());
			}
			tokio::select! {
				() = self.shutdown.wait_until_cancellation_requested() => return Ok(()),
				() = tokio::time::sleep(self.delay_between_passes) => {}
			}
		}
	}
}
