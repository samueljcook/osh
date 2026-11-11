//! Public Queue operations of the native runtime.

use std::error::Error;
use std::fmt;
use std::num::NonZeroU32;
use std::time::Duration;

use serde::Serialize;

use crate::db::{Database, DatabaseError, TransactionFailure, WriteDatabase, WritePool};
use crate::fleet::cron::{PgCron, PgCronFields};
use crate::fleet::lease::PgNativeLease;
use crate::fleet::lease::handle::PgLeaseRunOptions;
use crate::fleet::{CronKey, HolderPrefix};
use crate::queue::engine::{
	NativeClaimRequest, NativeEnqueueRejection, NativeJobListRequest,
	NativeQueueDeadLetterDeletion, NativeQueueDeadLetterMove, NativeQueueDeadLetterRequeue,
	NativeQueueGroupError, NativeQueueGroupOutcome, NativeQueueJobOperatorAction,
	NativeQueueOwnedReconciliation, NativeQueuePauseChange, OwnedJobMutation, PauseAction,
	PgNativeQueueGroup, QueueRequeueSchedule, RetryConsumption,
};
use crate::queue::state::{QueueJobOperatorAction, QueueJobOperatorOutcome};
use crate::queue::{
	ClaimedJob, DeadLetterJob, DeadLetterReason, EnqueueOptions, EnqueueRequestError,
	EnqueueResult, Job, JobId, JobRunAtOrAfter, JobStatus, MAX_PAYLOAD_JSON_SIZE_LIMIT,
	NativeFinishedDeletion, QueueDeadLetterPolicy, QueuePauseScope, TaskDef, TaskName,
	WorkerClaimToken, WorkerName, WorkerOwnerId, native_enqueue_request,
};

/// The key of the cron under which one worker of an installation at a time cleans up. Paranoid
/// holds its leadership under its own lease key, so an app cron with this key is a different
/// cron.
const CLEANUP_CRON_KEY: &str = "paranoid.queue.cleanup";

/// The inputs of a runtime's Queue handle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PgQueueFields {
	/// The largest serialized payload, in bytes, that an enqueue accepts. It must be positive and
	/// at most [`MAX_PAYLOAD_JSON_SIZE_LIMIT`].
	pub payload_json_size_limit: usize,
}

/// The Queue of the runtime's installation.
///
/// On an installation that imported a v10 Queue, every operation that v10 also has runs on the
/// v10 Queue tables until v10 writers retire, with the effect that published v10 gives the same
/// operation. V10 deployments and this runtime then enqueue, claim, run, and recover each other's
/// jobs, so a job's payload JSON must decode with the task definitions of both. The operations
/// on single jobs, pauses, and dead letters fail with [`PgQueueErrorKind::Admission`] until then,
/// except the reads, as their block below describes.
#[derive(Clone)]
pub struct PgQueue {
	store: PgNativeQueueGroup,
	payload_json_size_limit: usize,
	cleanup_lease: PgNativeLease,
	holder_prefix: HolderPrefix,
}

impl PgQueue {
	pub(crate) fn new(
		store: PgNativeQueueGroup,
		cleanup_lease: PgNativeLease,
		holder_prefix: HolderPrefix,
		fields: PgQueueFields,
	) -> Result<Self, PgQueueError> {
		if fields.payload_json_size_limit == 0
			|| fields.payload_json_size_limit > MAX_PAYLOAD_JSON_SIZE_LIMIT
		{
			return Err(PgQueueError::new(Cause::InvalidPayloadLimit));
		}
		Ok(Self {
			store,
			payload_json_size_limit: fields.payload_json_size_limit,
			cleanup_lease,
			holder_prefix,
		})
	}

	/// Enqueues one job for `def`'s task with `def`'s retry and timeout policy.
	///
	/// When `options` has a dedupe key that an active job of the same task already holds, the
	/// result names that job and reports `deduplicated`. A paused queue or task inserts nothing
	/// and fails with [`PgQueueErrorKind::QueuePaused`] or [`PgQueueErrorKind::TaskPaused`].
	///
	/// The payload's JSON has at most 100 arrays or objects along any path and at most the
	/// handle's `payload_json_size_limit` bytes. A deeper or larger payload fails with
	/// [`PgQueueErrorKind::Request`] before any database work.
	pub async fn enqueue<T: Serialize>(
		&self,
		database: impl WriteDatabase,
		def: &TaskDef<T>,
		payload: &T,
		options: EnqueueOptions,
	) -> Result<EnqueueResult, PgQueueError> {
		let request = native_enqueue_request(def, payload, options, self.payload_json_size_limit)
			.map_err(|error| PgQueueError::new(Cause::Request(error)))?;
		match self.store.enqueue(database, &request).await {
			Ok(NativeQueueGroupOutcome::Enqueued {
				job_id,
				deduplicated,
			}) => Ok(EnqueueResult {
				job_id,
				deduplicated,
			}),
			Ok(NativeQueueGroupOutcome::EnqueueRejected(rejection)) => {
				Err(PgQueueError::new(Cause::Rejected(rejection)))
			}
			Ok(outcome) => Err(PgQueueError::new(Cause::UnexpectedOutcome(format!(
				"{outcome:?}"
			)))),
			Err(error) => Err(PgQueueError::new(Cause::Group(error))),
		}
	}
}

/// The inputs of one claim attempt.
pub struct PgClaimDueFields<'a> {
	/// The tasks that this worker can execute.
	pub task_names: &'a [TaskName],
	/// The worker's name, which prefixes the observable owner label of the claim.
	pub worker_name: &'a WorkerName,
	/// The most due jobs that one attempt considers before it reports that none was claimable.
	pub candidate_limit: NonZeroU32,
}

impl PgQueue {
	/// Claims the earliest due pending job of one of `fields.task_names`, in its own transaction.
	///
	/// The claim is returned only after its transaction is known to have committed. A paused
	/// queue or task is skipped, and `None` means that no considered job was claimable.
	///
	/// A `COMMIT` that was sent runs to its end even when the caller drops the returned future,
	/// so a dropped future can leave a committed claim that no caller holds, and a failed or lost
	/// `COMMIT` reply leaves the same uncertainty
	/// ([`PgQueueErrorKind::CommitOutcomeUnknown`]). Such a claim holds its job until a reclaim
	/// returns the job to pending after the installation's stale threshold.
	pub async fn claim_due(
		&self,
		pool: &WritePool,
		fields: PgClaimDueFields<'_>,
	) -> Result<Option<PgClaimedJob>, PgQueueError> {
		let owner = WorkerOwnerId::generate(fields.worker_name)
			.map_err(|error| PgQueueError::new(Cause::ClaimMaterial(error.to_string())))?;
		let token = WorkerClaimToken::generate()
			.map_err(|error| PgQueueError::new(Cause::ClaimMaterial(error.to_string())))?;
		let task_names: Vec<&str> = fields.task_names.iter().map(TaskName::as_str).collect();
		let claimed = crate::db::run_transaction_write(pool, "db.queue.claim_due", async |tx| {
			let candidates = self
				.store
				.select_due(&mut *tx, &task_names, fields.candidate_limit.get())
				.await
				.map_err(Cause::Group)?;
			for job_id in candidates {
				let request = NativeClaimRequest {
					job_id,
					owner: owner.clone(),
					token: token.clone(),
				};
				match self
					.store
					.claim(&mut *tx, &request)
					.await
					.map_err(Cause::Group)?
				{
					NativeQueueGroupOutcome::Claimed { job } => return Ok(Some(*job)),
					NativeQueueGroupOutcome::ClaimSkipped => {}
					outcome => return Err(Cause::UnexpectedOutcome(format!("{outcome:?}"))),
				}
			}
			Ok(None)
		})
		.await
		.map_err(PgQueueError::new)?;
		Ok(claimed.map(|job| PgClaimedJob {
			store: self.store.clone(),
			claim: ClaimedJob::new(job, owner, token),
		}))
	}
}

/// The options of one reclaim pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgReclaimOptions {
	/// The maximum number of jobs that the pass reclaims. It must be positive.
	pub batch_size: NonZeroU32,
}

impl PgQueue {
	/// Reclaims running jobs when their newest heartbeat is older than the installation's stale
	/// threshold, oldest heartbeat first, at most `options.batch_size` per call. Each reclaim is
	/// its own transaction. A started job consumes a retry and becomes pending again, or fails
	/// when its retries are exhausted, as the installation's recovery settings direct. Returns
	/// how many jobs the pass reclaimed.
	pub async fn reclaim_stale_running_jobs(
		&self,
		pool: &WritePool,
		options: PgReclaimOptions,
	) -> Result<u32, PgQueueError> {
		self.store
			.recover_stale_claims(pool, options.batch_size.get())
			.await
			.map_err(|error| PgQueueError::new(Cause::Recovery(error)))
	}
}

/// Which tasks' jobs a listing returns.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PgJobTaskFilter {
	/// The jobs of every task.
	AllTasks,
	/// The jobs of this task.
	Task(TaskName),
}

/// The filters and page bounds of a job listing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PgListJobsOptions {
	/// The statuses to list. It must not be empty.
	pub statuses: Vec<JobStatus>,
	/// Which tasks' jobs to list.
	pub task: PgJobTaskFilter,
	/// The maximum number of jobs in the page. It must be positive.
	pub limit: NonZeroU32,
	/// The listing starts after this job, in job-identifier order; `None` starts at the first
	/// job.
	pub cursor_id: Option<JobId>,
}

/// One page of listed jobs.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct PgListJobsResult {
	/// The listed jobs in job-identifier order, which is creation order.
	pub jobs: Vec<Job>,
	/// The value of `cursor_id` that lists the next page, present when this page is full.
	pub next_cursor_id: Option<JobId>,
}

/// The ages and the batch size of one cleanup pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgCleanupOptions {
	/// A completed job is deleted once it finished longer ago than this.
	pub completed_jobs_older_than: Duration,
	/// A failed job is deleted once it finished longer ago than this.
	pub failed_jobs_older_than: Duration,
	/// A dead letter is deleted once its job moved to dead-letter storage longer ago than this.
	pub dead_letter_jobs_older_than: Duration,
	/// The maximum number of records of each kind that the pass deletes: completed jobs,
	/// failed jobs, and dead letters. It must be positive.
	pub batch_size: NonZeroU32,
}

/// How many records one cleanup pass deleted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct PgCleanupResult {
	/// Completed jobs that the pass deleted.
	pub completed_count: u32,
	/// Failed jobs that the pass deleted.
	pub failed_count: u32,
	/// Dead letters that the pass deleted.
	pub dead_letter_count: u32,
}

impl PgQueue {
	/// Deletes completed jobs, failed jobs, and dead letters that are older than their ages in
	/// `options` by the database clock, the oldest first, at most `options.batch_size` of each
	/// kind. Each deletion is its own transaction, which checks the age again under the
	/// record's lock. A job's active deduplication record goes with it.
	///
	/// Every process can call this; a cron runs it on one process at a time.
	pub async fn cleanup(
		&self,
		pool: &WritePool,
		options: PgCleanupOptions,
	) -> Result<PgCleanupResult, PgQueueError> {
		let counts = self
			.store
			.delete_finished(pool, finished_deletion(options)?)
			.await
			.map_err(|error| PgQueueError::new(Cause::Cleanup(error)))?;
		Ok(PgCleanupResult {
			completed_count: counts.completed,
			failed_count: counts.failed,
			dead_letter_count: counts.dead_letters,
		})
	}
}

/// The result of an operation that names one job by its identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgJobChangeResult {
	/// The operation changed the job.
	Changed,
	/// No job has this identifier.
	NotFound,
	/// The job's status is `actual`, which is not the status that the operation needs, so the
	/// operation changed nothing.
	StatusMismatch {
		/// The job's status.
		actual: JobStatus,
	},
}

/// The result of a requeue of one dead letter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgRequeueDeadLetterResult {
	/// The dead letter's job is pending again as the job with `job_id`, and the dead letter is
	/// deleted.
	Requeued {
		/// The identifier of the new pending job.
		job_id: JobId,
	},
	/// No dead letter has this identifier.
	NotFound,
}

/// The result of a deletion of one dead letter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgDeleteDeadLetterResult {
	/// The dead letter is deleted.
	Deleted,
	/// No dead letter has this identifier.
	NotFound,
}

/// When a job that a retry or a requeue makes pending becomes due.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgRequeueOptions {
	/// Earliest claim time. `None` makes the job claimable immediately.
	pub run_at_or_after: Option<JobRunAtOrAfter>,
}

/// Which jobs a pause or a resume applies to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PgQueuePauseTarget {
	/// The jobs of every task.
	Queue,
	/// The jobs of one task.
	Task(TaskName),
}

/// The inputs of a pause or a resume.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PgQueuePauseFields {
	/// The jobs that the pause or the resume applies to.
	pub target: PgQueuePauseTarget,
}

/// The page bounds of a dead-letter listing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgListDeadLettersOptions {
	/// The maximum number of dead letters in the page. It must be positive.
	pub limit: NonZeroU32,
	/// The listing starts after this dead letter, in identifier order; `None` starts at the
	/// first dead letter.
	pub cursor_id: Option<JobId>,
}

/// One page of dead letters.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct PgListDeadLettersResult {
	/// The dead letters in identifier order, which is the order in which their jobs moved to
	/// dead-letter storage.
	pub dead_letters: Vec<DeadLetterJob>,
	/// The value of `cursor_id` that lists the next page, present when this page is full.
	pub next_cursor_id: Option<JobId>,
}

/// Operations on single jobs, pauses, and dead letters.
///
/// On an installation that imported a v10 Queue, the operations of this block fail with
/// [`PgQueueErrorKind::Admission`] while v10 deployments still write, and v10's own operations
/// perform them then. While `PgRuntime::retire_v10` moves the v10 data, the operations of this
/// block act on a job or a dead letter wherever the move has left it.
impl PgQueue {
	/// Deletes the pending job with `job_id`, with its active deduplication record, which means
	/// that no worker runs the job.
	pub async fn cancel_pending_job(
		&self,
		database: impl WriteDatabase,
		job_id: JobId,
	) -> Result<PgJobChangeResult, PgQueueError> {
		self.apply_operator_action(database, job_id, QueueJobOperatorAction::CancelPending)
			.await
	}

	/// Makes the failed job with `job_id` pending again, with its retry count reset, due as
	/// `options` says. It fails with [`PgQueueErrorKind::DedupeKeyHeld`] when another active job
	/// holds the job's dedupe key.
	pub async fn retry_failed_job(
		&self,
		database: impl WriteDatabase,
		job_id: JobId,
		options: PgRequeueOptions,
	) -> Result<PgJobChangeResult, PgQueueError> {
		self.apply_operator_action(
			database,
			job_id,
			QueueJobOperatorAction::RetryFailed {
				schedule: requeue_schedule(options),
			},
		)
		.await
	}

	async fn apply_operator_action(
		&self,
		database: impl WriteDatabase,
		job_id: JobId,
		action: QueueJobOperatorAction,
	) -> Result<PgJobChangeResult, PgQueueError> {
		match self
			.store
			.apply_job_operator_action(database, NativeQueueJobOperatorAction { job_id, action })
			.await
		{
			Ok(NativeQueueGroupOutcome::JobOperator(QueueJobOperatorOutcome::Applied)) => {
				Ok(PgJobChangeResult::Changed)
			}
			Ok(NativeQueueGroupOutcome::JobOperator(QueueJobOperatorOutcome::NotFound)) => {
				Ok(PgJobChangeResult::NotFound)
			}
			Ok(NativeQueueGroupOutcome::JobOperator(QueueJobOperatorOutcome::StateMismatch {
				actual,
				..
			})) => Ok(PgJobChangeResult::StatusMismatch { actual }),
			Ok(outcome) => Err(PgQueueError::new(Cause::UnexpectedOutcome(format!(
				"{outcome:?}"
			)))),
			Err(error) => Err(PgQueueError::new(Cause::Group(error))),
		}
	}

	/// Pauses `fields.target`: until a resume, an enqueue of its jobs fails with
	/// [`PgQueueErrorKind::QueuePaused`] or [`PgQueueErrorKind::TaskPaused`], and no worker
	/// claims its due jobs. Jobs that are running when the pause commits keep running. Pausing
	/// a paused target changes nothing.
	pub async fn pause(
		&self,
		database: impl WriteDatabase,
		fields: PgQueuePauseFields,
	) -> Result<(), PgQueueError> {
		self.change_pause(database, fields, PauseAction::Pause)
			.await
	}

	/// Resumes `fields.target` so that its jobs are enqueued and claimed again. A queue pause
	/// and a task pause are separate: resuming the queue leaves a paused task paused. Resuming
	/// a target that is not paused changes nothing.
	pub async fn resume(
		&self,
		database: impl WriteDatabase,
		fields: PgQueuePauseFields,
	) -> Result<(), PgQueueError> {
		self.change_pause(database, fields, PauseAction::Unpause)
			.await
	}

	async fn change_pause(
		&self,
		database: impl WriteDatabase,
		fields: PgQueuePauseFields,
		action: PauseAction,
	) -> Result<(), PgQueueError> {
		let scope = match fields.target {
			PgQueuePauseTarget::Queue => QueuePauseScope::Global,
			PgQueuePauseTarget::Task(task) => QueuePauseScope::Task(task),
		};
		match self
			.store
			.change_pause(database, NativeQueuePauseChange { scope, action })
			.await
		{
			Ok(NativeQueueGroupOutcome::PauseChanged | NativeQueueGroupOutcome::Unchanged) => {
				Ok(())
			}
			Ok(outcome) => Err(PgQueueError::new(Cause::UnexpectedOutcome(format!(
				"{outcome:?}"
			)))),
			Err(error) => Err(PgQueueError::new(Cause::Group(error))),
		}
	}

	/// Enqueues the job of the dead letter with `dead_letter_id` again as a new pending job with
	/// the same task, payload, retry and timeout policy, and dedupe key, due as `options` says,
	/// and deletes the dead letter. It fails with [`PgQueueErrorKind::DedupeKeyHeld`] when
	/// another active job holds the dedupe key.
	pub async fn requeue_dead_letter(
		&self,
		database: impl WriteDatabase,
		dead_letter_id: JobId,
		options: PgRequeueOptions,
	) -> Result<PgRequeueDeadLetterResult, PgQueueError> {
		let new_job_id = JobId::generate()
			.map_err(|error| PgQueueError::new(Cause::JobIdGeneration(error.to_string())))?;
		match self
			.store
			.requeue_dead_letter(
				database,
				NativeQueueDeadLetterRequeue {
					dead_letter_id,
					new_job_id,
					schedule: requeue_schedule(options),
				},
			)
			.await
		{
			Ok(NativeQueueGroupOutcome::DeadLetterRequeued { job_id }) => {
				Ok(PgRequeueDeadLetterResult::Requeued { job_id })
			}
			Ok(NativeQueueGroupOutcome::Unchanged) => Ok(PgRequeueDeadLetterResult::NotFound),
			Ok(outcome) => Err(PgQueueError::new(Cause::UnexpectedOutcome(format!(
				"{outcome:?}"
			)))),
			Err(error) => Err(PgQueueError::new(Cause::Group(error))),
		}
	}

	/// Deletes the dead letter with `dead_letter_id`.
	pub async fn delete_dead_letter(
		&self,
		database: impl WriteDatabase,
		dead_letter_id: JobId,
	) -> Result<PgDeleteDeadLetterResult, PgQueueError> {
		match self
			.store
			.delete_dead_letter(database, NativeQueueDeadLetterDeletion { dead_letter_id })
			.await
		{
			Ok(NativeQueueGroupOutcome::DeadLetterDeleted) => Ok(PgDeleteDeadLetterResult::Deleted),
			Ok(NativeQueueGroupOutcome::Unchanged) => Ok(PgDeleteDeadLetterResult::NotFound),
			Ok(outcome) => Err(PgQueueError::new(Cause::UnexpectedOutcome(format!(
				"{outcome:?}"
			)))),
			Err(error) => Err(PgQueueError::new(Cause::Group(error))),
		}
	}
}

fn finished_deletion(options: PgCleanupOptions) -> Result<NativeFinishedDeletion, PgQueueError> {
	let micros = |retention: Duration| {
		i64::try_from(retention.as_micros())
			.map_err(|_| PgQueueError::new(Cause::RetentionTooLarge))
	};
	Ok(NativeFinishedDeletion {
		completed_retention_micros: micros(options.completed_jobs_older_than)?,
		failed_retention_micros: micros(options.failed_jobs_older_than)?,
		dead_letter_retention_micros: micros(options.dead_letter_jobs_older_than)?,
		limit: options.batch_size.get(),
	})
}

fn requeue_schedule(options: PgRequeueOptions) -> QueueRequeueSchedule {
	match options.run_at_or_after {
		None => QueueRequeueSchedule::ObservedNow,
		Some(at) => QueueRequeueSchedule::At(at),
	}
}

/// The polling, concurrency, heartbeat, and shutdown settings of a runtime worker. The stale
/// threshold and the dead-letter choice come from the installation's Queue recovery settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgWorkerConfig {
	/// How often an idle worker polls for due jobs. It must be positive.
	pub poll_interval: Duration,
	/// The largest random delay before the worker's first claim. `Duration::ZERO` starts at
	/// once.
	pub startup_jitter_max_delay: Duration,
	/// The most jobs that one worker pass claims, from 1 to 1,000.
	pub concurrency: u32,
	/// How often the worker writes the heartbeat of each job that it runs. It must be positive
	/// and shorter than the installation's stale threshold.
	pub execution_heartbeat_interval: Duration,
	/// The grace period before the worker requests cancellation of remaining job tasks.
	/// Joining cancelled tasks and cleaning up their claims follow this period.
	/// [`WorkerRunLoopSummary::shutdown`](crate::queue::WorkerRunLoopSummary::shutdown)
	/// reports whether the grace period expired.
	pub shutdown_grace_period: Duration,
	/// The longest wait for one database operation of the worker. It must be positive and at most
	/// [`MAX_WORKER_DATABASE_OPERATION_TIMEOUT`](crate::queue::MAX_WORKER_DATABASE_OPERATION_TIMEOUT),
	/// because the worker sets PostgreSQL's `statement_timeout` from it.
	pub database_operation_timeout: Duration,
	/// The wait after the first failed claim. It must be positive. Each further consecutive
	/// failure doubles the wait up to `claim_error_max_backoff`, and a successful claim resets
	/// it.
	pub claim_error_min_backoff: Duration,
	/// The longest wait after consecutive failed claims. It must be at least
	/// `claim_error_min_backoff`.
	pub claim_error_max_backoff: Duration,
}

/// The inputs of a long-running runtime worker.
pub struct PgWorkerFields<'a> {
	/// The pool for the worker's own transactions.
	pub pool: WritePool,
	/// The handlers of the tasks that this worker runs.
	pub task_registry: crate::queue::TaskRegistry,
	/// The worker's name, which prefixes the owner label of its claims.
	pub worker_name: &'a WorkerName,
	/// The worker's polling, concurrency, heartbeat, and shutdown settings.
	pub config: PgWorkerConfig,
	/// The interval between automatic stale-claim recovery attempts. It must be positive.
	/// Workers of the same runtime share an in-progress pass. A completed pass with an equal
	/// or larger batch also satisfies this worker's interval.
	pub reclaim_interval: Duration,
	/// The most jobs that one reclaim pass reclaims.
	pub reclaim_batch_size: NonZeroU32,
	/// Whether the worker deletes finished jobs and dead letters.
	pub cleanup: PgWorkerCleanup,
}

/// Whether a worker deletes finished jobs and dead letters.
pub enum PgWorkerCleanup {
	/// The worker deletes nothing. Completed jobs, failed jobs, and dead letters stay until
	/// [`PgQueue::cleanup`] or another worker's cleanup deletes them.
	Disabled,
	/// The worker contends for the installation's Queue cleanup leadership, a lease that one
	/// process at a time holds, and while it holds the leadership, it cleans up at once and then
	/// each time `interval` passes after the previous cleanup ended.
	Periodic(PgWorkerCleanupFields),
}

/// The schedule, ages, and leadership options of a worker's cleanup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PgWorkerCleanupFields {
	/// The positive time between the end of one cleanup and the start of the next.
	pub interval: Duration,
	/// The ages and batch size of each cleanup pass. One cleanup runs passes until a pass
	/// deletes fewer completed jobs, fewer failed jobs, and fewer dead letters than the batch
	/// size.
	pub options: PgCleanupOptions,
	/// The wait between two passes of one cleanup.
	pub delay_between_passes: Duration,
	/// The lease options of the cleanup leadership.
	pub lease_options: PgLeaseRunOptions,
}

impl PgQueue {
	/// Starts a long-running worker on this Queue. The worker reads the installation's Queue
	/// recovery settings first. Beside the worker loop, the worker reclaims jobs when their newest
	/// heartbeat is older than the installation's stale threshold, and it cleans up as
	/// `fields.cleanup` directs. A reclaim or cleanup pass that times out or meets a transient
	/// database failure runs again at its next interval; any other reclaim or cleanup failure
	/// stops the worker and is its result.
	pub async fn start_worker(
		&self,
		fields: PgWorkerFields<'_>,
	) -> Result<crate::queue::WorkerHandle, PgQueueError> {
		if fields.reclaim_interval.is_zero() {
			return Err(PgQueueError::new(Cause::ReclaimIntervalIsZero));
		}
		let cleanup = match fields.cleanup {
			PgWorkerCleanup::Disabled => None,
			PgWorkerCleanup::Periodic(cleanup) => {
				let deletion = finished_deletion(cleanup.options)?;
				let cron = PgCron::new(
					PgCronFields {
						key: CronKey::new(CLEANUP_CRON_KEY)
							.expect("the Queue cleanup cron key is a valid cron key"),
						interval: cleanup.interval,
						lease_options: cleanup.lease_options,
					},
					self.cleanup_lease.clone(),
					self.holder_prefix.clone(),
				)
				.map_err(|error| PgQueueError::new(Cause::CleanupSchedule(error)))?;
				Some(crate::queue::NativeWorkerCleanup {
					cron,
					deletion,
					delay_between_passes: cleanup.delay_between_passes,
				})
			}
		};
		let recovery = self
			.store
			.installed_recovery_policy(&fields.pool)
			.await
			.map_err(|error| PgQueueError::new(Cause::Group(error)))?;
		let config =
			crate::queue::WorkerConfig::new(fields.config, &recovery).map_err(|error| {
				PgQueueError::new(Cause::StartWorker(crate::queue::StartWorkerError::Config(
					error,
				)))
			})?;
		Ok(crate::queue::start_native_worker(
			self.store.clone(),
			fields.pool,
			fields.task_registry,
			fields.worker_name,
			config,
			crate::queue::NativeWorkerRecovery {
				interval: fields.reclaim_interval,
				batch_size: fields.reclaim_batch_size.get(),
			},
			cleanup,
		))
	}
}

/// One claimed running job. Every operation is checked against the claim, so an operation after
/// another worker has taken over the job fails with [`PgQueueErrorKind::ClaimLost`] and changes
/// nothing.
///
/// On an installation that imported a v10 Queue, the jobs stay in the v10 Queue tables until
/// v10 writers retire, and v10 deployments identify a claim by its owner label alone. Until then
/// the check compares the owner label; every claim that Paranoid makes has an owner label of its
/// own, so a later claim of the same job never passes the check of an earlier one. After v10
/// writers retire, the check compares the claim's private token, and a claim made before the
/// retirement no longer holds its job (see
/// [`PgRuntime::retire_v10`](crate::runtime::PgRuntime::retire_v10)).
pub struct PgClaimedJob {
	store: PgNativeQueueGroup,
	claim: ClaimedJob,
}

impl PgClaimedJob {
	/// The job as it was claimed.
	pub fn job(&self) -> &Job {
		self.claim.job()
	}

	/// Records that the handler started running the job.
	pub async fn start_execution(&self, database: impl WriteDatabase) -> Result<(), PgQueueError> {
		self.apply(database, OwnedJobMutation::StartExecution).await
	}

	/// Records that the handler is still running the job.
	pub async fn heartbeat(&self, database: impl WriteDatabase) -> Result<(), PgQueueError> {
		self.apply(database, OwnedJobMutation::Heartbeat).await
	}

	/// Marks the job completed and ends the claim.
	pub async fn complete(&self, database: impl WriteDatabase) -> Result<(), PgQueueError> {
		self.apply(database, OwnedJobMutation::Complete).await
	}

	/// Returns the job to pending after `retry_after`, consumes one retry, and ends the claim.
	pub async fn schedule_retry(
		&self,
		database: impl WriteDatabase,
		retry_after: Duration,
		error_msg: String,
	) -> Result<(), PgQueueError> {
		let new_retry_count = self
			.claim
			.job()
			.retry_count
			.checked_add(1)
			.filter(|&count| count <= super::MAX_RETRIES)
			.ok_or_else(|| PgQueueError::new(Cause::RetryCountOverflow))?;
		self.apply(
			database,
			OwnedJobMutation::ScheduleRetry {
				new_retry_count,
				retry_after,
				error_msg,
			},
		)
		.await
	}

	/// Marks the job failed, consumes the attempt, and ends the claim. When the installation's
	/// Queue recovery settings move failed jobs to dead letters, the same transaction moves the
	/// job to dead-letter storage with the reason [`DeadLetterReason::PermanentError`].
	pub async fn fail(
		&self,
		database: impl WriteDatabase,
		error_msg: String,
	) -> Result<(), PgQueueError> {
		crate::db::run_transaction_write(database, "db.queue.fail", async |tx| {
			match self
				.store
				.reconcile_owned(
					&mut *tx,
					NativeQueueOwnedReconciliation {
						authority: &self.claim,
						mutation: OwnedJobMutation::Fail {
							error_msg,
							retry_consumption: RetryConsumption::ConsumeAttempt,
						},
					},
				)
				.await
				.map_err(Cause::Group)?
			{
				NativeQueueGroupOutcome::Reconciled => {}
				NativeQueueGroupOutcome::Unchanged => return Err(Cause::ClaimLost),
				outcome => return Err(Cause::UnexpectedOutcome(format!("{outcome:?}"))),
			}
			let recovery = self
				.store
				.installed_recovery_policy(&mut *tx)
				.await
				.map_err(Cause::Group)?;
			if recovery.dead_letters() != QueueDeadLetterPolicy::MoveToDeadLetters {
				return Ok(());
			}
			let dead_letter_id =
				JobId::generate().map_err(|error| Cause::ClaimMaterial(error.to_string()))?;
			match self
				.store
				.move_to_dead_letter(
					&mut *tx,
					NativeQueueDeadLetterMove {
						job_id: self.claim.job_id(),
						dead_letter_id,
						reason: DeadLetterReason::PermanentError,
					},
				)
				.await
				.map_err(Cause::Group)?
			{
				NativeQueueGroupOutcome::DeadLetterMoved { .. } => Ok(()),
				outcome => Err(Cause::UnexpectedOutcome(format!("{outcome:?}"))),
			}
		})
		.await
		.map_err(PgQueueError::new)
	}

	async fn apply(
		&self,
		database: impl WriteDatabase,
		mutation: OwnedJobMutation,
	) -> Result<(), PgQueueError> {
		match self
			.store
			.reconcile_owned(
				database,
				NativeQueueOwnedReconciliation {
					authority: &self.claim,
					mutation,
				},
			)
			.await
		{
			Ok(NativeQueueGroupOutcome::Reconciled) => Ok(()),
			Ok(NativeQueueGroupOutcome::Unchanged) => Err(PgQueueError::new(Cause::ClaimLost)),
			Ok(outcome) => Err(PgQueueError::new(Cause::UnexpectedOutcome(format!(
				"{outcome:?}"
			)))),
			Err(error) => Err(PgQueueError::new(Cause::Group(error))),
		}
	}
}

/// The category of a Queue failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgQueueErrorKind {
	/// The payload size limit is zero or larger than [`MAX_PAYLOAD_JSON_SIZE_LIMIT`].
	InvalidPayloadLimit,
	/// A job listing named no status.
	EmptyStatusFilter,
	/// A retention is longer than the database clock can represent.
	RetentionTooLarge,
	/// The task policy or payload cannot be stored.
	Request,
	/// The whole queue is paused.
	QueuePaused,
	/// The task is paused.
	TaskPaused,
	/// The runtime cannot operate on the Queue under the installation's recorded policy.
	Admission,
	/// A database statement or the transaction's own control failed.
	Database,
	/// Stored Queue state could not be interpreted or is inconsistent.
	State,
	/// Fresh claim material could not be generated.
	ClaimMaterial,
	/// The claim no longer holds the job, so the operation changed nothing.
	ClaimLost,
	/// The claim's retry count cannot grow further.
	RetryCountOverflow,
	/// The operation's own `COMMIT` failed, which means that the change may or may not have
	/// committed. A claim of [`PgQueue::claim_due`] that ends this way is not delivered.
	CommitOutcomeUnknown,
	/// Concurrent operations on the same jobs or dedupe keys used up Paranoid's attempts.
	/// Repeating the operation can succeed.
	Contention,
	/// Another active job holds the dedupe key of the job that the operation would make
	/// pending, so the operation changed nothing.
	DedupeKeyHeld,
	/// A fresh job identifier could not be generated.
	JobIdGeneration,
	/// The worker configuration is invalid, or the worker could not start.
	StartWorker,
}

enum Cause {
	InvalidPayloadLimit,
	EmptyStatusFilter,
	RetentionTooLarge,
	Cleanup(crate::queue::DeleteFinishedJobsError),
	Request(EnqueueRequestError),
	Rejected(NativeEnqueueRejection),
	Group(NativeQueueGroupError),
	UnexpectedOutcome(String),
	ClaimMaterial(String),
	JobIdGeneration(String),
	Recovery(crate::queue::RecoverStaleClaimsError),
	StartWorker(crate::queue::StartWorkerError),
	ReclaimIntervalIsZero,
	CleanupSchedule(crate::fleet::PgCronError),
	ClaimLost,
	RetryCountOverflow,
	Protocol(crate::installation::ProtocolError),
	Database(DatabaseError),
	RollbackFailed {
		operation_error: Box<Cause>,
		rollback_error: DatabaseError,
	},
}

/// A failed Queue operation, with its complete cause chain.
pub struct PgQueueError {
	source: Box<Cause>,
}

impl PgQueueError {
	fn new(source: Cause) -> Self {
		Self {
			source: Box::new(source),
		}
	}

	/// The failure's category. A failure of the operation's own `COMMIT` is
	/// [`PgQueueErrorKind::CommitOutcomeUnknown`] in every operation.
	pub fn kind(&self) -> PgQueueErrorKind {
		if self.commit_outcome_is_unknown() {
			return PgQueueErrorKind::CommitOutcomeUnknown;
		}
		match self.source.as_ref() {
			Cause::InvalidPayloadLimit => PgQueueErrorKind::InvalidPayloadLimit,
			Cause::EmptyStatusFilter => PgQueueErrorKind::EmptyStatusFilter,
			Cause::RetentionTooLarge => PgQueueErrorKind::RetentionTooLarge,
			Cause::Cleanup(error) => cleanup_kind(error),
			Cause::Request(_) => PgQueueErrorKind::Request,
			Cause::Rejected(NativeEnqueueRejection::QueuePaused) => PgQueueErrorKind::QueuePaused,
			Cause::Rejected(NativeEnqueueRejection::TaskPaused) => PgQueueErrorKind::TaskPaused,
			Cause::Group(error) => native_kind(error),
			Cause::UnexpectedOutcome(_) => PgQueueErrorKind::State,
			Cause::ClaimMaterial(_) => PgQueueErrorKind::ClaimMaterial,
			Cause::JobIdGeneration(_) => PgQueueErrorKind::JobIdGeneration,
			Cause::ClaimLost => PgQueueErrorKind::ClaimLost,
			Cause::Recovery(error) => recovery_kind(error),
			Cause::StartWorker(_) | Cause::ReclaimIntervalIsZero | Cause::CleanupSchedule(_) => {
				PgQueueErrorKind::StartWorker
			}
			Cause::RetryCountOverflow => PgQueueErrorKind::RetryCountOverflow,
			Cause::Protocol(_) => PgQueueErrorKind::Admission,
			Cause::Database(_) | Cause::RollbackFailed { .. } => PgQueueErrorKind::Database,
		}
	}

	/// Whether the operation's own `COMMIT` failed, which means that the change may or may not
	/// have committed.
	pub fn commit_outcome_is_unknown(&self) -> bool {
		self.source.commit_outcome_is_unknown()
	}
}

/// The category of a cleanup failure, by its cause.
fn cleanup_kind(error: &crate::queue::DeleteFinishedJobsError) -> PgQueueErrorKind {
	use crate::queue::DeleteFinishedJobsError as Error;
	match error {
		Error::Runtime(error) => runtime_kind(error),
		Error::Protocol(_) => PgQueueErrorKind::Admission,
		Error::CommitTimedOut { .. } => PgQueueErrorKind::CommitOutcomeUnknown,
		Error::TimedOut { .. } | Error::Database(_) | Error::RollbackFailed { .. } => {
			PgQueueErrorKind::Database
		}
	}
}

/// The category of a reclaim failure, by its cause.
fn recovery_kind(error: &crate::queue::RecoverStaleClaimsError) -> PgQueueErrorKind {
	use crate::queue::RecoverStaleClaimsError as Error;
	match error {
		Error::JobIdGeneration(_) => PgQueueErrorKind::JobIdGeneration,
		Error::Runtime(error) => runtime_kind(error),
		Error::Protocol(_) => PgQueueErrorKind::Admission,
		Error::CommitTimedOut { .. } => PgQueueErrorKind::CommitOutcomeUnknown,
		Error::TimedOut { .. } | Error::Database(_) | Error::RollbackFailed { .. } => {
			PgQueueErrorKind::Database
		}
	}
}

/// The category of a runtime failure of cleanup or reclaim: the category of the Queue failure
/// that it holds, as in every other operation.
fn runtime_kind(error: &crate::queue::QueueRuntimeError) -> PgQueueErrorKind {
	native_kind(error.native())
}

/// The category of a Queue failure, the same in every operation. The first rule that applies
/// decides:
///
/// 1. A failed `COMMIT` with an unknown outcome is
///    [`CommitOutcomeUnknown`](PgQueueErrorKind::CommitOutcomeUnknown).
/// 2. Any other database failure is [`Database`](PgQueueErrorKind::Database).
/// 3. A failure that shows that the code or its layout support does not match what the
///    installation records is [`Admission`](PgQueueErrorKind::Admission).
/// 4. Concurrent work that used up Paranoid's attempts is
///    [`Contention`](PgQueueErrorKind::Contention).
/// 5. A dedupe key that another active job holds is
///    [`DedupeKeyHeld`](PgQueueErrorKind::DedupeKeyHeld).
/// 6. Every other failure, including a failure of the v10 Queue tables, is
///    [`State`](PgQueueErrorKind::State).
fn native_kind(error: &NativeQueueGroupError) -> PgQueueErrorKind {
	use crate::queue::engine::NativeQueueErrorClass as Class;
	match error.classify() {
		Class::CommitOutcomeUnknown => PgQueueErrorKind::CommitOutcomeUnknown,
		Class::Database => PgQueueErrorKind::Database,
		Class::Admission => PgQueueErrorKind::Admission,
		Class::Contention => PgQueueErrorKind::Contention,
		Class::NativeDedupeConflict | Class::V10DedupeConflict => PgQueueErrorKind::DedupeKeyHeld,
		Class::V10 | Class::State => PgQueueErrorKind::State,
	}
}

impl fmt::Display for PgQueueError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		self.source.fmt(formatter)
	}
}

impl fmt::Display for Cause {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::InvalidPayloadLimit => formatter.write_str(
				"the Queue payload size limit must be positive and at most the maximum limit",
			),
			Self::EmptyStatusFilter => formatter.write_str("a job listing must name a status"),
			Self::RetentionTooLarge => {
				formatter.write_str("a retention is longer than the database clock can represent")
			}
			Self::Cleanup(_) => formatter.write_str("finished-job deletion failed"),
			Self::Request(_) => formatter.write_str("queue request is invalid"),
			Self::Rejected(NativeEnqueueRejection::QueuePaused) => {
				formatter.write_str("the queue is paused")
			}
			Self::Rejected(NativeEnqueueRejection::TaskPaused) => {
				formatter.write_str("the task is paused")
			}
			Self::Group(_) => formatter.write_str("queue operation failed"),
			Self::UnexpectedOutcome(outcome) => write!(
				formatter,
				"queue operation returned an unexpected outcome: {outcome}"
			),
			Self::ClaimMaterial(error) => {
				write!(
					formatter,
					"queue claim material could not be generated: {error}"
				)
			}
			Self::JobIdGeneration(error) => {
				write!(
					formatter,
					"a job identifier could not be generated: {error}"
				)
			}
			Self::Recovery(_) => formatter.write_str("reclaiming stale running jobs failed"),
			Self::StartWorker(_) => formatter.write_str("the worker could not start"),
			Self::ClaimLost => formatter.write_str("the claim no longer holds the job"),
			Self::RetryCountOverflow => formatter.write_str("the job's retry count cannot grow"),
			Self::ReclaimIntervalIsZero => {
				formatter.write_str("the worker's reclaim interval is zero")
			}
			Self::CleanupSchedule(_) => {
				formatter.write_str("the worker's cleanup schedule is invalid")
			}
			Self::Protocol(_) => {
				formatter.write_str("the installation cannot admit the Queue operation")
			}
			Self::Database(_) => formatter.write_str("the Queue transaction failed"),
			// The source is the rollback's failure, so the message holds the operation's own
			// causes.
			Self::RollbackFailed {
				operation_error, ..
			} => {
				write!(formatter, "{operation_error}")?;
				let mut source = operation_error.source_error();
				while let Some(error) = source {
					write!(formatter, ": {error}")?;
					source = error.source();
				}
				formatter.write_str(", and the rollback also failed")
			}
		}
	}
}

impl TransactionFailure for Cause {
	fn from_protocol_error(error: crate::installation::ProtocolError) -> Self {
		Self::Protocol(error)
	}
	fn from_database_error(error: DatabaseError) -> Self {
		Self::Database(error)
	}
	fn from_rollback_failure(operation_error: Self, rollback_error: DatabaseError) -> Self {
		Self::RollbackFailed {
			operation_error: Box::new(operation_error),
			rollback_error,
		}
	}
	fn involves_database_failure(&self) -> bool {
		match self {
			Self::Database(_) | Self::RollbackFailed { .. } => true,
			Self::Group(error) => error.involves_database_failure(),
			Self::Cleanup(error) => error.involves_database_failure(),
			Self::Recovery(error) => error.involves_database_failure(),
			_ => false,
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Database(error) => error.commit_outcome_is_unknown(),
			Self::Group(error) => error.commit_outcome_is_unknown(),
			Self::Cleanup(error) => error.commit_outcome_is_unknown(),
			Self::Recovery(error) => error.commit_outcome_is_unknown(),
			Self::RollbackFailed {
				operation_error,
				rollback_error,
			} => {
				operation_error.commit_outcome_is_unknown()
					|| rollback_error.commit_outcome_is_unknown()
			}
			_ => false,
		}
	}
}

impl fmt::Debug for PgQueueError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(formatter, "PgQueueError({:?}): {self}", self.kind())?;
		let mut source = self.source();
		while let Some(error) = source {
			write!(formatter, ": {error}")?;
			source = error.source();
		}
		Ok(())
	}
}

impl Error for PgQueueError {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		self.source.source_error()
	}
}

impl Cause {
	fn source_error(&self) -> Option<&(dyn Error + 'static)> {
		match self {
			Cause::Request(error) => Some(error),
			Cause::Group(error) => Some(error),
			Cause::Protocol(error) => Some(error),
			Cause::Database(error) => Some(error),
			Cause::Recovery(error) => Some(error),
			Cause::StartWorker(error) => Some(error),
			Cause::CleanupSchedule(error) => Some(error),
			Cause::Cleanup(error) => Some(error),
			Cause::RollbackFailed { rollback_error, .. } => Some(rollback_error),
			_ => None,
		}
	}
}

/// Read-only Queue operations. A reader grants no worker or mutation authority.
///
/// On an installation that imported a v10 Queue, a reader reads the v10 Queue tables while v10
/// deployments still write. While `PgRuntime::retire_v10` moves the v10 data, a reader reads both
/// stores and finds a job or a dead letter wherever the move has left it.
#[derive(Clone)]
pub struct PgQueueReader {
	store: PgNativeQueueGroup,
}

impl PgQueueReader {
	pub(crate) fn new(store: PgNativeQueueGroup) -> Self {
		Self { store }
	}
	/// Lists jobs in job-identifier order. The listing reads the jobs in key order and filters
	/// by status and task; it holds no lock and grants nothing.
	pub async fn list_jobs(
		&self,
		database: impl Database,
		options: PgListJobsOptions,
	) -> Result<PgListJobsResult, PgQueueError> {
		if options.statuses.is_empty() {
			return Err(PgQueueError::new(Cause::EmptyStatusFilter));
		}
		let task_name = match &options.task {
			PgJobTaskFilter::AllTasks => None,
			PgJobTaskFilter::Task(task) => Some(task),
		};
		self.store
			.list_jobs(
				database,
				NativeJobListRequest {
					statuses: &options.statuses,
					task_name,
					after: options.cursor_id,
					limit: options.limit.get(),
				},
			)
			.await
			.map(|page| PgListJobsResult {
				jobs: page.jobs,
				next_cursor_id: page.next_after,
			})
			.map_err(|error| PgQueueError::new(Cause::Group(error)))
	}
	/// Reads the job with `job_id`. `None` means that no job has this identifier: the job was
	/// cancelled, moved to dead-letter storage, deleted by cleanup, or never existed. The read
	/// holds no lock and grants nothing.
	pub async fn fetch_job(
		&self,
		database: impl Database,
		job_id: JobId,
	) -> Result<Option<Job>, PgQueueError> {
		self.store
			.fetch_job(database, job_id)
			.await
			.map_err(|error| PgQueueError::new(Cause::Group(error)))
	}
	/// Lists dead letters in identifier order. The listing holds no lock and grants nothing.
	pub async fn list_dead_letters(
		&self,
		database: impl Database,
		options: PgListDeadLettersOptions,
	) -> Result<PgListDeadLettersResult, PgQueueError> {
		self.store
			.list_dead_letters(database, options.cursor_id, options.limit)
			.await
			.map(|page| PgListDeadLettersResult {
				dead_letters: page.dead_letters,
				next_cursor_id: page.next_after,
			})
			.map_err(|error| PgQueueError::new(Cause::Group(error)))
	}
}

impl PgQueue {
	/// Returns a reader that shares this handle's runtime admission and storage. The reader lists
	/// and reads the Queue's jobs and dead letters.
	pub fn reader(&self) -> PgQueueReader {
		PgQueueReader::new(self.store.clone())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::installation::ProtocolError;
	use crate::queue::{DeleteFinishedJobsError, QueueRuntimeError, RecoverStaleClaimsError};

	fn protocol_error() -> ProtocolError {
		ProtocolError::RelationAbsent {
			relation: "\"queue_kinds\".\"protocol\"".to_owned(),
		}
	}

	fn runtime(error: NativeQueueGroupError) -> QueueRuntimeError {
		QueueRuntimeError::new(error)
	}

	fn kind(cause: Cause) -> PgQueueErrorKind {
		PgQueueError::new(cause).kind()
	}

	#[test]
	fn cleanup_and_reclaim_failures_have_the_kind_of_their_cause() {
		let timed_out = || DeleteFinishedJobsError::TimedOut {
			operation: "select finished jobs",
			timeout: Duration::from_secs(30),
		};
		assert_eq!(
			kind(Cause::Cleanup(timed_out())),
			PgQueueErrorKind::Database
		);
		assert_eq!(
			kind(Cause::Cleanup(DeleteFinishedJobsError::Protocol(
				protocol_error()
			))),
			PgQueueErrorKind::Admission
		);
		assert_eq!(
			kind(Cause::Cleanup(DeleteFinishedJobsError::Runtime(runtime(
				NativeQueueGroupError::CoexistenceDedupeContention
			)))),
			PgQueueErrorKind::Contention
		);
		assert_eq!(
			kind(Cause::Cleanup(DeleteFinishedJobsError::Runtime(runtime(
				NativeQueueGroupError::SourceMappingAbsent
			)))),
			PgQueueErrorKind::Admission
		);
		assert_eq!(
			kind(Cause::Cleanup(DeleteFinishedJobsError::Runtime(runtime(
				NativeQueueGroupError::InvalidJobKey
			)))),
			PgQueueErrorKind::State
		);
		assert_eq!(
			kind(Cause::Cleanup(DeleteFinishedJobsError::Database(
				DatabaseError::query_for_tests(None)
			))),
			PgQueueErrorKind::Database
		);
		assert_eq!(
			kind(Cause::Recovery(RecoverStaleClaimsError::TimedOut {
				operation: "select running jobs by oldest heartbeat",
				timeout: Duration::from_secs(30),
			})),
			PgQueueErrorKind::Database
		);
		assert_eq!(
			kind(Cause::Recovery(RecoverStaleClaimsError::Protocol(
				protocol_error()
			))),
			PgQueueErrorKind::Admission
		);
		assert_eq!(
			kind(Cause::Recovery(RecoverStaleClaimsError::Runtime(runtime(
				NativeQueueGroupError::CoexistenceDedupeContention
			)))),
			PgQueueErrorKind::Contention
		);
	}

	#[test]
	fn an_unknown_commit_has_its_own_kind_in_every_operation() {
		for cause in [
			Cause::Group(NativeQueueGroupError::Database(
				DatabaseError::transaction_for_tests(),
			)),
			Cause::Cleanup(DeleteFinishedJobsError::Database(
				DatabaseError::transaction_for_tests(),
			)),
			Cause::Recovery(RecoverStaleClaimsError::Database(
				DatabaseError::transaction_for_tests(),
			)),
			Cause::Database(DatabaseError::transaction_for_tests()),
		] {
			let error = PgQueueError::new(cause);
			assert!(error.commit_outcome_is_unknown(), "{error:?}");
			assert_eq!(error.kind(), PgQueueErrorKind::CommitOutcomeUnknown);
		}
		assert_eq!(
			kind(Cause::Group(NativeQueueGroupError::Database(
				DatabaseError::query_for_tests(None)
			))),
			PgQueueErrorKind::Database
		);
	}

	#[test]
	fn dedupe_conflicts_and_maintenance_database_failures_classify_consistently() {
		use crate::queue::QueueRuntimeErrorKind;
		use crate::queue::engine::NativeQueueCleanupError;
		use crate::queue::engine::NativeQueueGroupRejectionError as Rejection;
		use crate::queue::v10::source_group_storage::apply::QueueSourceApplyRejectionError;
		use crate::records::postgres::StateStorageError;
		use crate::records::{RecordIdentity, RecordKey, RecordNamespace};

		let dummy_identity = RecordIdentity::new(
			RecordNamespace::new(vec![1]).expect("valid namespace"),
			RecordKey::new(vec![2]),
		);
		let cleanup_contention = || {
			Box::new(NativeQueueCleanupError::Storage(
				StateStorageError::InvalidRecord {
					identity: dummy_identity.clone(),
					reason: "cleanup state mismatch",
				},
			))
		};

		let native_cleanup_dedupe = NativeQueueGroupError::RejectionCleanup {
			rejection_error: Rejection::Preparation(crate::queue::QueueGroupError::DedupeConflict),
			cleanup_error: cleanup_contention(),
		};
		assert_eq!(
			runtime(native_cleanup_dedupe).kind(),
			QueueRuntimeErrorKind::State
		);
		assert_eq!(
			kind(Cause::Group(NativeQueueGroupError::RejectionCleanup {
				rejection_error: Rejection::Preparation(
					crate::queue::QueueGroupError::DedupeConflict
				),
				cleanup_error: cleanup_contention(),
			})),
			PgQueueErrorKind::DedupeKeyHeld
		);

		let source_dedupe = || {
			Rejection::Source(QueueSourceApplyRejectionError::Preparation {
				identity: dummy_identity.clone(),
				error: crate::queue::QueueGroupError::DedupeConflict,
			})
		};
		assert_eq!(
			runtime(NativeQueueGroupError::Rejected(source_dedupe())).kind(),
			QueueRuntimeErrorKind::V10
		);
		assert_eq!(
			kind(Cause::Group(NativeQueueGroupError::Rejected(
				source_dedupe()
			))),
			PgQueueErrorKind::DedupeKeyHeld
		);
		assert_eq!(
			kind(Cause::Group(NativeQueueGroupError::RejectionCleanup {
				rejection_error: source_dedupe(),
				cleanup_error: cleanup_contention(),
			})),
			PgQueueErrorKind::DedupeKeyHeld
		);

		assert!(
			Cause::Cleanup(DeleteFinishedJobsError::Database(
				DatabaseError::query_for_tests(None)
			))
			.involves_database_failure()
		);
		assert!(
			Cause::Recovery(RecoverStaleClaimsError::Database(
				DatabaseError::query_for_tests(None)
			))
			.involves_database_failure()
		);
		assert!(
			!Cause::Cleanup(DeleteFinishedJobsError::Protocol(protocol_error()))
				.involves_database_failure()
		);
		assert!(
			!Cause::Recovery(RecoverStaleClaimsError::Protocol(protocol_error()))
				.involves_database_failure()
		);
	}
}
