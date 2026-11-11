use super::model::{
	JobExecutionCtx, NewestJobHeartbeat, ProcessedJobOutcome, RetryPolicy,
	RuntimeCancellationSignal, TaskError, TaskFuture, TaskHandler, WorkerOwnerId, WorkerQueue,
	WorkerRunLoopSummary, WorkerRuntime,
};
use crate::db::WritePool;
use crate::queue::{
	ClaimedJob, DeadLetterReason, HeartbeatError, OwnedJobUpdate,
	OwnedRunningJobDeadLetterPersistence, OwnedRunningJobRetryPersistence, QueueDeadLetterPolicy,
	WorkerClaimToken, WorkerJobError, WorkerPersistenceError, WorkerRequeueError, WorkerRunError,
	WorkerStepError, compute_queue_retry_backoff,
	mark_owned_running_job_completed_with_database_operation_timeout,
	mark_owned_running_job_failed_with_database_operation_timeout,
	mark_owned_running_job_started_with_database_operation_timeout,
	move_owned_running_job_to_dead_letter_with_database_operation_timeout,
	resolve_queue_job_timeout,
	return_available_owned_started_running_jobs_to_pending_with_database_operation_timeout,
	return_available_owned_unstarted_running_jobs_to_pending_with_database_operation_timeout,
	return_jobs_claimed_with_token_with_database_operation_timeout,
	return_owned_started_running_job_to_pending_with_database_operation_timeout,
	run_owned_job_worker_database_operation,
	schedule_owned_running_job_retry_with_database_operation_timeout,
};
use std::collections::HashMap;
use std::time::Duration;
use tokio::runtime::Handle as RuntimeHandle;

/// The joined result of one job task.
pub(super) type JoinedJob =
	Result<Result<ProcessedJobOutcome, WorkerJobError>, tokio::task::JoinError>;

/// The jobs that a worker claimed and has not settled.
///
/// The value of each claim moves between two states:
///
/// - Running: the job's task runs the job. `running` holds the claim under the task's ID, and
///   `tasks` holds the task.
/// - Unsettled: the job's task failed or was cancelled before it settled the job, so the claim
///   can still hold the job. `unsettled` holds the claim, and the worker returns the job to
///   pending when it stops.
///
/// A task that ends with an outcome settled its job, and the worker drops the claim when it joins
/// the task.
pub(super) struct WorkerJobs {
	tasks: tokio::task::JoinSet<Result<ProcessedJobOutcome, WorkerJobError>>,
	running: HashMap<tokio::task::Id, ClaimedJob>,
	unsettled: Vec<ClaimedJob>,
}

impl WorkerJobs {
	pub(super) fn new() -> Self {
		Self {
			tasks: tokio::task::JoinSet::new(),
			running: HashMap::new(),
			unsettled: Vec::new(),
		}
	}

	/// The number of running jobs.
	pub(super) fn running_count(&self) -> usize {
		self.tasks.len()
	}

	/// Starts the task that runs `claimed_job`.
	pub(super) fn spawn(&mut self, runtime: &WorkerRuntime, claimed_job: ClaimedJob) {
		let task = self.tasks.spawn(process_claimed_queue_job(
			runtime.clone(),
			claimed_job.clone(),
		));
		self.running.insert(task.id(), claimed_job);
	}

	/// Waits for the next job task to end, and moves the task's claim out of the running state:
	/// a task that ended with an outcome settled its job, and any other task leaves its claim
	/// unsettled. `None` means that no job runs.
	pub(super) async fn join_next(&mut self) -> Option<JoinedJob> {
		let (task, joined) = match self.tasks.join_next_with_id().await? {
			Ok((task, result)) => (task, Ok(result)),
			Err(error) => (error.id(), Err(error)),
		};
		let claimed_job = self
			.running
			.remove(&task)
			.expect("each running job task has its claim");
		if !matches!(joined, Ok(Ok(_))) {
			self.unsettled.push(claimed_job);
		}
		Some(joined)
	}

	/// Requests the cancellation of every running job task.
	pub(super) fn abort_all(&mut self) {
		self.tasks.abort_all();
	}

	/// The claims of the jobs that the worker has not settled. The worker calls this after it
	/// joined every job task, when no job runs.
	pub(super) fn unsettled(&self) -> &[ClaimedJob] {
		&self.unsettled
	}
}

pub(super) async fn process_claimed_queue_job(
	runtime: WorkerRuntime,
	claimed_job: ClaimedJob,
) -> Result<ProcessedJobOutcome, WorkerJobError> {
	let job = claimed_job.job();
	let registered_task = runtime
		.task_registry
		.registered_task(&job.task_name)
		.cloned()
		.expect("a worker claims only the jobs of the tasks that its registry holds");

	if job.retry_count > job.max_retries {
		return move_running_job_to_dead_letter_or_fail(
			&runtime,
			&claimed_job,
			"max retries exceeded",
			false,
			DeadLetterReason::MaxRetriesExceeded,
		)
		.await;
	}

	// The start writes the job's first heartbeat, and each attempt of the start is sent after
	// the instant that it records here.
	let newest_heartbeat = NewestJobHeartbeat::new(tokio::time::Instant::now());
	match run_owned_job_worker_database_operation(
		runtime.config.database_operation_timeout,
		|operation_timeout| {
			newest_heartbeat.record(tokio::time::Instant::now());
			mark_owned_running_job_started_with_database_operation_timeout(
				&runtime.queue,
				&runtime.pool,
				&claimed_job,
				operation_timeout,
			)
		},
	)
	.await
	{
		Ok(Some(())) => {}
		Ok(None) => return Ok(ProcessedJobOutcome::LostOwnership),
		Err(error) => return Err(WorkerJobError::Started(error)),
	}

	let job_cancellation_signal = RuntimeCancellationSignal::new();
	let ctx = JobExecutionCtx {
		queue: runtime.queue.clone(),
		pool: runtime.pool.clone(),
		claimed_job: claimed_job.clone(),
		worker_shutdown_signal: runtime.worker_shutdown_signal.clone(),
		job_cancellation_signal: job_cancellation_signal.clone(),
		database_operation_timeout: runtime.config.database_operation_timeout,
		newest_heartbeat,
	};
	let heartbeat_handle = start_worker_heartbeat_loop(
		ctx.clone(),
		job_cancellation_signal.clone(),
		runtime.config.execution_heartbeat_interval,
	);
	let handler_result = run_queue_task_handler(
		registered_task.handler.clone(),
		ctx,
		job.payload_json.clone(),
		HandlerDeadlines {
			timeout: resolve_queue_job_timeout(job.timeout, registered_task.job_timeout),
			stale_threshold: runtime.config.stale_threshold,
		},
	)
	.await;
	let heartbeat_result = stop_worker_heartbeat_loop(heartbeat_handle).await;

	// A job with requested cancellation lost its ownership (or met an incompatible
	// protocol), so no outcome can be persisted under this claim.
	let finalization_result = if job_cancellation_signal.is_cancellation_requested() {
		Ok(ProcessedJobOutcome::LostOwnership)
	} else {
		match handler_result {
			Ok(()) => complete_processed_queue_job(&runtime, &claimed_job).await,
			Err(error) => {
				handle_processed_queue_job_error(
					&runtime,
					&claimed_job,
					&registered_task.retry_policy,
					error,
				)
				.await
			}
		}
	};
	combine_worker_heartbeat_and_job_finalization_results(heartbeat_result, finalization_result)
}

/// The two times at which a worker stops a handler that has not ended.
pub(super) struct HandlerDeadlines {
	/// The job's timeout, counted from the start of the handler.
	pub(super) timeout: Option<Duration>,
	/// The worker's stale threshold, counted from the time at which the worker sent the newest
	/// heartbeat of the job that the database stored.
	pub(super) stale_threshold: Duration,
}

/// The message of the failure that a worker stores for a job when no heartbeat of the job
/// succeeded for the stale threshold.
pub(super) const HEARTBEATS_FAILED_FOR_STALE_THRESHOLD_MESSAGE: &str =
	"queue job heartbeats failed for the stale threshold";

/// Runs the handler until it ends, the job's timeout passes, the job's cancellation is
/// requested, or no heartbeat of the job has succeeded for the stale threshold.
///
/// A job's claim is a lease that the heartbeat renews. A reclaim returns a job to the queue
/// when the job's heartbeat in the database is older than the stale threshold, and another
/// worker can then run the job. The database stores a heartbeat after the worker sends it, so
/// the heartbeat in the database is not older than the time since the worker sent the newest
/// stored heartbeat, as long as the worker's clock and the database's clock advance at the
/// same rate. When that time reaches the stale threshold, the handler's future is dropped and
/// the handler fails with a retryable error, which the worker stores under its claim if it
/// still holds the claim. The check runs in the job's own task, before each poll of the
/// handler, so it does not wait for the task that writes the heartbeats. A handler that blocks
/// its thread and a paused process are not stopped.
pub(super) async fn run_queue_task_handler(
	handler: TaskHandler,
	ctx: JobExecutionCtx,
	payload_json: String,
	deadlines: HandlerDeadlines,
) -> Result<(), TaskError> {
	let job_cancellation_signal = ctx.job_cancellation_signal.clone();
	let newest_heartbeat = ctx.newest_heartbeat.clone();
	let heartbeats_fail_for_stale_threshold = async {
		loop {
			let Some(stale_at) = newest_heartbeat
				.sent_at()
				.checked_add(deadlines.stale_threshold)
			else {
				// The threshold is beyond what the clock can represent.
				return std::future::pending::<()>().await;
			};
			tokio::time::sleep_until(stale_at).await;
			// A heartbeat that succeeded during the wait moved the instant.
			if newest_heartbeat.sent_at().elapsed() >= deadlines.stale_threshold {
				return;
			}
		}
	};
	let timeout_passes = async {
		match deadlines.timeout {
			Some(timeout) => tokio::time::sleep(timeout).await,
			None => std::future::pending().await,
		}
	};
	let stale_failure = || TaskError::retryable(HEARTBEATS_FAILED_FOR_STALE_THRESHOLD_MESSAGE);
	// The start of the job can take as long as the stale threshold, so the handler is not
	// called then.
	if newest_heartbeat.sent_at().elapsed() >= deadlines.stale_threshold {
		return Err(stale_failure());
	}
	// The handler runs inside the job's own task so that dropping the job's future drops the
	// handler's future and the job's outcome is stored only after the handler is closed.
	let handler_future = queue_task_result_from_panic(handler(ctx, payload_json));
	tokio::pin!(handler_future);
	tokio::select! {
		biased;
		() = heartbeats_fail_for_stale_threshold => Err(stale_failure()),
		result = &mut handler_future => result,
		() = timeout_passes => Err(TaskError::retryable("queue job timed out")),
		_ = job_cancellation_signal.wait_until_cancellation_requested() => {
			Err(TaskError::retryable("queue job ownership lost"))
		}
	}
}

/// Runs a handler's future and turns a panic into a permanent task error, without a message
/// that could carry the panic payload.
pub async fn queue_task_result_from_panic(handler_future: TaskFuture) -> Result<(), TaskError> {
	match primitives_rs::future::CatchUnwind::new(handler_future).await {
		Ok(result) => result,
		Err(_payload) => Err(TaskError::permanent("queue task handler panicked")),
	}
}

/// Starts the task that writes the job's heartbeat every `execution_heartbeat_interval`, which
/// the worker configuration keeps positive.
fn start_worker_heartbeat_loop(
	ctx: JobExecutionCtx,
	job_cancellation_signal: RuntimeCancellationSignal,
	execution_heartbeat_interval: Duration,
) -> WorkerHeartbeatLoopHandle {
	let (stop_sender, mut stop_receiver) = tokio::sync::oneshot::channel();
	WorkerHeartbeatLoopHandle::new(
		tokio::spawn(async move {
			let mut interval = tokio::time::interval(execution_heartbeat_interval);
			interval.tick().await;
			loop {
				tokio::select! {
					_ = &mut stop_receiver => return Ok(()),
					_ = interval.tick() => {
						// A stop request abandons a heartbeat that waits, for example, for a lock,
						// so that a finished job's outcome does not wait behind it.
						let heartbeat = tokio::select! {
							_ = &mut stop_receiver => return Ok(()),
							heartbeat = ctx.touch_execution_heartbeat() => heartbeat,
						};
						match heartbeat {
							Ok(()) => {}
							Err(HeartbeatError::JobNotRunning) => {
								job_cancellation_signal.request_cancellation();
								return Ok(());
							}
							Err(error) if error.is_protocol_failure() => {
								job_cancellation_signal.request_cancellation();
								return Err(error);
							}
							// The next heartbeat tries again. The job's own task stops the
							// handler when no heartbeat has succeeded for the stale threshold.
							Err(_) => {}
						}
					}
				}
			}
		}),
		stop_sender,
	)
}

#[doc(hidden)]
pub async fn stop_worker_heartbeat_loop(
	mut handle: WorkerHeartbeatLoopHandle,
) -> Result<(), WorkerJobError> {
	handle.request_stop();
	handle.await_stop().await
}

#[doc(hidden)]
pub struct WorkerHeartbeatLoopHandle {
	state: WorkerHeartbeatState,
}

enum WorkerHeartbeatState {
	Running {
		join: tokio::task::JoinHandle<Result<(), HeartbeatError>>,
		stop: tokio::sync::oneshot::Sender<()>,
	},
	StopRequested(tokio::task::JoinHandle<Result<(), HeartbeatError>>),
	Stopped,
}

impl WorkerHeartbeatLoopHandle {
	#[doc(hidden)]
	pub fn new(
		join_handle: tokio::task::JoinHandle<Result<(), HeartbeatError>>,
		stop_sender: tokio::sync::oneshot::Sender<()>,
	) -> Self {
		Self {
			state: WorkerHeartbeatState::Running {
				join: join_handle,
				stop: stop_sender,
			},
		}
	}

	fn request_stop(&mut self) {
		self.state = match std::mem::replace(&mut self.state, WorkerHeartbeatState::Stopped) {
			WorkerHeartbeatState::Running { join, stop } => {
				let _ = stop.send(());
				WorkerHeartbeatState::StopRequested(join)
			}
			state => state,
		};
	}

	async fn await_stop(&mut self) -> Result<(), WorkerJobError> {
		self.request_stop();
		let WorkerHeartbeatState::StopRequested(join) = &mut self.state else {
			return Ok(());
		};
		let result = join.await;
		self.state = WorkerHeartbeatState::Stopped;
		result
			.map_err(WorkerJobError::HeartbeatTaskJoinFailed)?
			.map_err(WorkerJobError::Heartbeat)
	}
}

impl Drop for WorkerHeartbeatLoopHandle {
	fn drop(&mut self) {
		self.request_stop();
		if let WorkerHeartbeatState::StopRequested(join) = &self.state {
			join.abort();
		}
	}
}

pub(super) async fn complete_processed_queue_job(
	runtime: &WorkerRuntime,
	claimed_job: &ClaimedJob,
) -> Result<ProcessedJobOutcome, WorkerJobError> {
	let persisted = run_owned_job_worker_database_operation(
		runtime.config.database_operation_timeout,
		|operation_timeout| {
			mark_owned_running_job_completed_with_database_operation_timeout(
				&runtime.queue,
				&runtime.pool,
				claimed_job,
				operation_timeout,
			)
		},
	)
	.await;
	settle_processed_queue_job(
		runtime,
		claimed_job,
		persisted,
		ProcessedJobOutcome::Succeeded,
	)
	.await
}

pub(super) async fn handle_processed_queue_job_error(
	runtime: &WorkerRuntime,
	claimed_job: &ClaimedJob,
	retry_policy: &RetryPolicy,
	error: TaskError,
) -> Result<ProcessedJobOutcome, WorkerJobError> {
	let job = claimed_job.job();
	let Some(next_retry_count) = job.retry_count.checked_add(1) else {
		use crate::queue::error::FromNativeQueueError;
		let overflow_error = WorkerPersistenceError::from_native_queue_error(
			crate::queue::engine::NativeQueueGroupError::Rejected(
				crate::queue::engine::NativeQueueGroupRejectionError::Preparation(
					crate::queue::QueueGroupError::JobTransition(
						crate::queue::state::BoundJobTransitionError::Transition(
							crate::queue::record_transitions::JobTransitionError::RetryCountOverflow,
						),
					),
				),
			),
		);
		return settle_processed_queue_job::<()>(
			runtime,
			claimed_job,
			Err(overflow_error),
			ProcessedJobOutcome::Failed,
		)
		.await;
	};
	let retries_exhausted = next_retry_count > job.max_retries;
	if error.is_permanent() || retries_exhausted {
		let reason = if error.is_permanent() {
			DeadLetterReason::PermanentError
		} else {
			DeadLetterReason::MaxRetriesExceeded
		};
		return move_running_job_to_dead_letter_or_fail(
			runtime,
			claimed_job,
			error.msg(),
			!error.is_permanent(),
			reason,
		)
		.await;
	}

	let retry_after = compute_queue_retry_backoff(retry_policy, next_retry_count, &error)
		.map_err(WorkerJobError::RetryJitterRandom)?;
	let persisted = run_owned_job_worker_database_operation(
		runtime.config.database_operation_timeout,
		|operation_timeout| {
			schedule_owned_running_job_retry_with_database_operation_timeout(
				&runtime.queue,
				&runtime.pool,
				OwnedRunningJobRetryPersistence {
					claimed_job,
					new_retry_count: next_retry_count,
					retry_after,
					error_msg: error.msg(),
				},
				operation_timeout,
			)
		},
	)
	.await;
	settle_processed_queue_job(
		runtime,
		claimed_job,
		persisted,
		ProcessedJobOutcome::Retried,
	)
	.await
}

pub(super) async fn move_running_job_to_dead_letter_or_fail(
	runtime: &WorkerRuntime,
	claimed_job: &ClaimedJob,
	error_msg: &str,
	increment_retry_count: bool,
	reason: DeadLetterReason,
) -> Result<ProcessedJobOutcome, WorkerJobError> {
	if runtime.config.dead_letters == QueueDeadLetterPolicy::MoveToDeadLetters {
		let persisted = run_owned_job_worker_database_operation(
			runtime.config.database_operation_timeout,
			|operation_timeout| {
				move_owned_running_job_to_dead_letter_with_database_operation_timeout(
					&runtime.queue,
					&runtime.pool,
					OwnedRunningJobDeadLetterPersistence {
						claimed_job,
						error_msg,
						increment_retry_count,
						reason,
					},
					operation_timeout,
				)
			},
		)
		.await;
		return settle_processed_queue_job(
			runtime,
			claimed_job,
			persisted,
			ProcessedJobOutcome::DeadLettered,
		)
		.await;
	}

	let persisted = run_owned_job_worker_database_operation(
		runtime.config.database_operation_timeout,
		|operation_timeout| {
			mark_owned_running_job_failed_with_database_operation_timeout(
				&runtime.queue,
				&runtime.pool,
				claimed_job,
				error_msg,
				increment_retry_count,
				operation_timeout,
			)
		},
	)
	.await;
	settle_processed_queue_job(runtime, claimed_job, persisted, ProcessedJobOutcome::Failed).await
}

async fn settle_processed_queue_job<T>(
	runtime: &WorkerRuntime,
	claimed_job: &ClaimedJob,
	persisted: Result<Option<T>, WorkerPersistenceError>,
	outcome: ProcessedJobOutcome,
) -> Result<ProcessedJobOutcome, WorkerJobError> {
	match persisted {
		Ok(Some(_)) => Ok(outcome),
		Ok(None) => Ok(ProcessedJobOutcome::LostOwnership),
		Err(error) => {
			let requeue_result = requeue_started_job_after_worker_persistence_failure(
				&runtime.queue,
				&runtime.pool,
				claimed_job,
				runtime.config.database_operation_timeout,
			)
			.await;
			Err(worker_job_persistence_error_after_requeue(
				error,
				requeue_result,
			))
		}
	}
}

pub(super) async fn requeue_started_job_after_worker_persistence_failure(
	queue: &WorkerQueue,
	pool: &WritePool,
	claimed_job: &ClaimedJob,
	database_operation_timeout: Duration,
) -> Result<(), WorkerRequeueError> {
	match return_owned_started_running_job_to_pending_with_database_operation_timeout(
		queue,
		pool,
		claimed_job,
		database_operation_timeout,
	)
	.await
	{
		Ok(OwnedJobUpdate::Applied(())) | Ok(OwnedJobUpdate::NotRunning) => Ok(()),

		Err(error) => Err(error),
	}
}

#[doc(hidden)]
pub fn worker_job_persistence_error_after_requeue(
	persistence_error: WorkerPersistenceError,
	requeue_result: Result<(), WorkerRequeueError>,
) -> WorkerJobError {
	match requeue_result {
		Ok(()) => WorkerJobError::Persistence(persistence_error),
		Err(requeue_error) => WorkerJobError::PersistenceAndRequeue {
			persistence_error,
			requeue_error,
		},
	}
}

#[doc(hidden)]
pub fn combine_worker_heartbeat_and_job_finalization_results(
	heartbeat_result: Result<(), WorkerJobError>,
	finalization_result: Result<ProcessedJobOutcome, WorkerJobError>,
) -> Result<ProcessedJobOutcome, WorkerJobError> {
	match (heartbeat_result, finalization_result) {
		(Ok(()), result) => result,
		(Err(heartbeat_error), Ok(_)) => Err(heartbeat_error),
		(Err(heartbeat_error), Err(finalization_error)) => {
			Err(WorkerJobError::HeartbeatAndFinalization {
				heartbeat_error: Box::new(heartbeat_error),
				finalization_error: Box::new(finalization_error),
			})
		}
	}
}

#[doc(hidden)]
pub fn worker_runtime_error_after_claimed_job_cleanup(
	worker_error: WorkerRunError,
	cleanup_result: Result<(), WorkerStepError>,
) -> WorkerRunError {
	match cleanup_result {
		Ok(()) => worker_error,
		Err(cleanup_error) => WorkerRunError::RuntimeFailureAndClaimedJobCleanupFailed {
			worker_error: Box::new(worker_error),
			cleanup_error,
		},
	}
}

#[doc(hidden)]
pub fn worker_runtime_error_after_in_flight_abort_and_claimed_job_cleanup(
	worker_error: WorkerRunError,
	mut in_flight_errors: Vec<WorkerRunError>,
	cleanup_result: Result<(), WorkerStepError>,
) -> WorkerRunError {
	let primary_error =
		worker_runtime_error_after_claimed_job_cleanup(worker_error, cleanup_result);
	if in_flight_errors.is_empty() {
		return primary_error;
	}

	let mut failures = Vec::with_capacity(in_flight_errors.len() + 1);
	failures.push(primary_error);
	failures.append(&mut in_flight_errors);
	WorkerRunError::MultipleFailures { failures }
}

#[doc(hidden)]
pub fn worker_runtime_shutdown_timeout_result_after_in_flight_abort_and_claimed_job_cleanup(
	summary: WorkerRunLoopSummary,
	mut in_flight_errors: Vec<WorkerRunError>,
	cleanup_result: Result<(), WorkerStepError>,
) -> Result<WorkerRunLoopSummary, WorkerRunError> {
	match (cleanup_result, in_flight_errors.len()) {
		(Ok(()), 0) => Ok(summary),
		(Err(cleanup_error), 0) => Err(WorkerRunError::ClaimedJobCleanup(cleanup_error)),
		(Ok(()), 1) => Err(in_flight_errors.remove(0)),
		(Err(cleanup_error), _) => {
			let mut failures = Vec::with_capacity(in_flight_errors.len() + 1);
			failures.push(WorkerRunError::ClaimedJobCleanup(cleanup_error));
			failures.append(&mut in_flight_errors);
			Err(WorkerRunError::MultipleFailures { failures })
		}
		(Ok(()), _) => Err(WorkerRunError::MultipleFailures {
			failures: in_flight_errors,
		}),
	}
}

pub(super) struct WorkerJobAbortOutcome {
	pub(super) cancelled_job_count: u32,
	pub(super) errors: Vec<WorkerRunError>,
}

pub(super) async fn abort_and_collect_in_flight_jobs(
	in_flight_jobs: &mut WorkerJobs,
	mut summary: Option<&mut WorkerRunLoopSummary>,
) -> WorkerJobAbortOutcome {
	in_flight_jobs.abort_all();
	let mut cancelled_job_count = 0;
	let mut errors = Vec::new();
	while let Some(joined) = in_flight_jobs.join_next().await {
		match joined {
			Ok(Ok(outcome)) => {
				if let Some(summary) = summary.as_deref_mut()
					&& let Err(error) = summary.record_processed_job_outcome(outcome)
				{
					errors.push(error);
				}
			}
			Err(error) if error.is_cancelled() => {
				// The validated worker concurrency bounds this count to at most 1,000.
				cancelled_job_count += 1;
			}
			joined => {
				if let Some(error) =
					worker_runtime_error_from_joined_in_flight_job_after_abort(joined)
				{
					errors.push(error);
				}
			}
		}
	}
	WorkerJobAbortOutcome {
		cancelled_job_count,
		errors,
	}
}

#[doc(hidden)]
pub fn worker_runtime_error_from_joined_in_flight_job_after_abort(
	joined: JoinedJob,
) -> Option<WorkerRunError> {
	match joined {
		Ok(Ok(_)) => None,
		Ok(Err(error)) => Some(WorkerRunError::Job(error)),
		Err(error) if error.is_cancelled() => None,
		Err(error) => Some(WorkerRunError::JobTaskJoinFailed(error)),
	}
}

/// Returns the jobs of `claimed_jobs` to pending after the worker's job tasks ended: first the
/// jobs with an execution that has not started, then the started ones. Each step is its own
/// transaction under the worker's database operation timeout. A step waits for a job that
/// another transaction holds, so no job of `claimed_jobs` stays running under its claim after
/// both steps.
pub(super) async fn return_claimed_jobs_after_worker_task_failure(
	queue: &WorkerQueue,
	pool: &WritePool,
	claimed_jobs: &[ClaimedJob],
	database_operation_timeout: Duration,
) -> Result<(), WorkerStepError> {
	if claimed_jobs.is_empty() {
		return Ok(());
	}
	return_available_owned_unstarted_running_jobs_to_pending_with_database_operation_timeout(
		queue,
		pool,
		claimed_jobs,
		database_operation_timeout,
	)
	.await?;
	return_available_owned_started_running_jobs_to_pending_with_database_operation_timeout(
		queue,
		pool,
		claimed_jobs,
		database_operation_timeout,
	)
	.await?;
	Ok(())
}

/// Returns the jobs of one claim to pending, for a claim with a `COMMIT` that may have succeeded
/// without the claimant learning of it. The claimant never received the claim's jobs, so none of
/// them has started, and the step waits for a job that another transaction holds.
pub(super) async fn return_jobs_of_undelivered_claim(
	queue: &WorkerQueue,
	pool: &WritePool,
	worker_owner_id: &WorkerOwnerId,
	worker_claim_token: &WorkerClaimToken,
	database_operation_timeout: Duration,
) -> Result<(), WorkerStepError> {
	return_jobs_claimed_with_token_with_database_operation_timeout(
		queue,
		pool,
		worker_owner_id,
		worker_claim_token,
		database_operation_timeout,
	)
	.await
}

/// Releases a claim by its token in the background when it is dropped armed. A worker arms it
/// before it claims and disarms it once the claim's result is delivered or the claim cannot
/// have committed.
pub(in crate::queue) struct UndeliveredClaimRelease {
	release: Option<UndeliveredClaim>,
}

struct UndeliveredClaim {
	queue: WorkerQueue,
	pool: WritePool,
	runtime_handle: RuntimeHandle,
	worker_owner_id: WorkerOwnerId,
	worker_claim_token: WorkerClaimToken,
	database_operation_timeout: Duration,
}

impl UndeliveredClaimRelease {
	pub(in crate::queue) fn arm(
		queue: &WorkerQueue,
		pool: &WritePool,
		worker_owner_id: &WorkerOwnerId,
		worker_claim_token: &WorkerClaimToken,
		database_operation_timeout: Duration,
	) -> Self {
		Self {
			release: Some(UndeliveredClaim {
				queue: queue.clone(),
				pool: pool.clone(),
				runtime_handle: RuntimeHandle::current(),
				worker_owner_id: worker_owner_id.clone(),
				worker_claim_token: worker_claim_token.clone(),
				database_operation_timeout,
			}),
		}
	}

	pub(in crate::queue) fn disarm(&mut self) {
		self.release = None;
	}
}

impl Drop for UndeliveredClaimRelease {
	fn drop(&mut self) {
		let Some(claim) = self.release.take() else {
			return;
		};
		// The claim's `COMMIT` may still be running in the background, and a release that runs
		// before it ends finds no claim to release.
		let running_transaction_control = claim.pool.running_transaction_control();
		claim.runtime_handle.clone().spawn(async move {
			let claim = &claim;
			running_transaction_control
				.run_now_and_after_they_end(|| async move {
					// Drop cannot await. Stale-job reclamation remains the durable recovery path
					// if this best-effort release cannot complete.
					let _release_result = return_jobs_of_undelivered_claim(
						&claim.queue,
						&claim.pool,
						&claim.worker_owner_id,
						&claim.worker_claim_token,
						claim.database_operation_timeout,
					)
					.await;
				})
				.await;
		});
	}
}

/// The worker's result from the join result of its task.
pub fn queue_worker_run_loop_result_from_join_result(
	join_result: Result<Result<WorkerRunLoopSummary, WorkerRunError>, tokio::task::JoinError>,
) -> Result<WorkerRunLoopSummary, WorkerRunError> {
	match join_result {
		Ok(result) => result,
		Err(error) => Err(WorkerRunError::WorkerTaskJoinFailed(error)),
	}
}
