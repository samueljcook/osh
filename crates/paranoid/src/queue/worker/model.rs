use crate::db::WritePool;
use crate::queue::{
	ClaimedJob, GenerateWorkerOwnerIdError, JobTimeout, QUEUE_WORKER_OWNER_ID_SEPARATOR,
	QueueDeadLetterPolicy, TaskName, WorkerName, WorkerOwnerIdError, WorkerRunError,
	check_worker_owner_id,
};
use primitives_rs::id::time_sortable as id;
use std::collections::HashMap;
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

/// Custom retry backoff function. It receives the retry count, which is 1 for the first retry,
/// and the task error.
pub type RetryBackoffFn = Arc<dyn Fn(u32, &TaskError) -> Duration + Send + Sync + 'static>;

/// How long a worker waits before it retries a failed job of one task.
///
/// Each variant has a `jitter_fraction`, which is the symmetric random spread of the wait: `0.2`
/// means plus or minus 20 percent. A wait is never below
/// [`MIN_RETRY_BACKOFF`](crate::queue::MIN_RETRY_BACKOFF), and registration rejects a policy that
/// could wait longer than [`MAX_RETRY_BACKOFF`](crate::queue::MAX_RETRY_BACKOFF).
#[derive(Clone)]
pub enum RetryPolicy {
	/// Wait the same time before every retry.
	Fixed {
		/// The wait before a retry. It must be positive, and `delay` multiplied by
		/// `1 + jitter_fraction` must be at most
		/// [`MAX_RETRY_BACKOFF`](crate::queue::MAX_RETRY_BACKOFF).
		delay: Duration,
		/// Symmetric jitter fraction. It must be finite and not negative.
		jitter_fraction: f64,
	},
	/// Wait `initial` before the first retry and `multiplier` times the previous wait before
	/// each later retry, up to `max`.
	Exponential {
		/// The wait before the first retry. It must be positive and at most `max`.
		initial: Duration,
		/// The factor between consecutive waits. It must be finite and greater than one.
		multiplier: f64,
		/// The longest wait. It must be at least
		/// [`MIN_RETRY_BACKOFF`](crate::queue::MIN_RETRY_BACKOFF) and at most
		/// [`MAX_RETRY_BACKOFF`](crate::queue::MAX_RETRY_BACKOFF).
		max: Duration,
		/// Symmetric jitter fraction. It must be finite and not negative.
		jitter_fraction: f64,
	},
	/// Wait the time that a caller-supplied function returns, up to `max`.
	Custom {
		/// The function that returns the wait.
		backoff_fn: RetryBackoffFn,
		/// The longest wait. It must be at least
		/// [`MIN_RETRY_BACKOFF`](crate::queue::MIN_RETRY_BACKOFF) and at most
		/// [`MAX_RETRY_BACKOFF`](crate::queue::MAX_RETRY_BACKOFF).
		max: Duration,
		/// Symmetric jitter fraction. It must be finite and not negative.
		jitter_fraction: f64,
	},
}

/// The validated settings of a long-running worker: the settings of
/// [`PgWorkerConfig`](crate::queue::PgWorkerConfig) and the installation's Queue recovery
/// settings.
#[derive(Clone)]
pub struct WorkerConfig {
	/// How often an idle long-running worker polls for due jobs.
	pub poll_interval: Duration,
	/// Maximum random delay before the first long-running worker claim. `Duration::ZERO`
	/// disables startup jitter.
	pub startup_jitter_max_delay: Duration,
	/// Maximum jobs claimed by one worker pass.
	pub concurrency: u32,
	/// The age of a running job's newest execution heartbeat at which the job counts as
	/// abandoned. A worker that maintains the queue reclaims such jobs of every worker, and
	/// another worker can then run them.
	///
	/// A worker also gives up a job of its own when that time has passed since the worker sent
	/// the job's newest heartbeat that the database stored, or since it sent the job's start.
	/// The worker measures the time on its own clock. It drops the handler's future at the
	/// handler's next await, and the job fails with the retryable error
	/// `queue job heartbeats failed for the stale threshold`, which the worker stores under its
	/// claim, as it stores a timeout, if it still holds the claim. The time between that
	/// moment and a reclaim is therefore short, and it is not zero: the two clocks can
	/// differ, and a handler that blocks its thread, work that the handler moved to another
	/// task, and a paused process are not stopped. A handler must tolerate a second run of
	/// its job, as for every retry.
	///
	/// A worker takes the value from the installation's Queue recovery policy, which
	/// [`PgQueue::reclaim_stale_running_jobs`](crate::queue::PgQueue::reclaim_stale_running_jobs)
	/// also uses.
	pub stale_threshold: Duration,
	/// How often a worker writes the execution heartbeat of each job that it runs. It must be
	/// below `stale_threshold`.
	pub execution_heartbeat_interval: Duration,
	/// What happens to a job that fails for the last time.
	pub dead_letters: QueueDeadLetterPolicy,
	/// Maximum graceful wait after a long-running worker receives a stop request.
	pub shutdown_grace_period: Duration,
	/// Maximum wait for one worker-owned database operation. It is positive and at most
	/// [`MAX_WORKER_DATABASE_OPERATION_TIMEOUT`](crate::queue::MAX_WORKER_DATABASE_OPERATION_TIMEOUT).
	pub database_operation_timeout: Duration,
	/// Wait after the first failed claim attempt of a long-running worker. Each further
	/// consecutive failure doubles the wait up to `claim_error_max_backoff`, and a successful
	/// claim resets it. It must be positive.
	pub claim_error_min_backoff: Duration,
	/// Longest wait after consecutive failed claim attempts of a long-running worker. It must
	/// be at least `claim_error_min_backoff`.
	pub claim_error_max_backoff: Duration,
}

/// Error returned by a queue task handler.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskError {
	pub(in crate::queue) msg: String,
	pub(in crate::queue) permanent: bool,
}

/// Context passed to queue task handlers.
#[derive(Clone)]
pub struct JobExecutionCtx {
	pub(in crate::queue) queue: WorkerQueue,
	pub(in crate::queue) pool: WritePool,
	pub(in crate::queue) claimed_job: ClaimedJob,
	pub(in crate::queue) worker_shutdown_signal: RuntimeCancellationSignal,
	pub(in crate::queue) job_cancellation_signal: RuntimeCancellationSignal,
	pub(in crate::queue) database_operation_timeout: Duration,
	pub(in crate::queue) newest_heartbeat: NewestJobHeartbeat,
}

/// When this worker sent the newest heartbeat of a job that the database stored.
///
/// The database stored that heartbeat after the worker sent it, and a heartbeat never moves
/// the stored time back, so the heartbeat in the database is at least as new as this instant.
/// The clones of one value share the instant.
#[derive(Clone)]
pub(in crate::queue) struct NewestJobHeartbeat {
	sent_at: Arc<std::sync::Mutex<tokio::time::Instant>>,
}

impl NewestJobHeartbeat {
	pub(in crate::queue) fn new(sent_at: tokio::time::Instant) -> Self {
		Self {
			sent_at: Arc::new(std::sync::Mutex::new(sent_at)),
		}
	}

	pub(in crate::queue) fn sent_at(&self) -> tokio::time::Instant {
		*self
			.sent_at
			.lock()
			.expect("no code panics while it holds the heartbeat instant")
	}

	/// Records `sent_at` as the instant at which the worker sent a heartbeat, when it is newer
	/// than the recorded one. The start of a job records the instant of each attempt before it
	/// sends the attempt, and nothing reads the value until an attempt has succeeded.
	pub(in crate::queue) fn record(&self, sent_at: tokio::time::Instant) {
		let mut newest = self
			.sent_at
			.lock()
			.expect("no code panics while it holds the heartbeat instant");
		*newest = (*newest).max(sent_at);
	}
}

pub(in crate::queue) type TaskFuture =
	Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + 'static>>;

pub(in crate::queue) type TaskHandler =
	Arc<dyn Fn(JobExecutionCtx, String) -> TaskFuture + Send + Sync + 'static>;

/// What a registry stores for one task.
#[derive(Clone)]
pub(in crate::queue) struct RegisteredTask {
	pub(in crate::queue) handler: TaskHandler,
	pub(in crate::queue) retry_policy: RetryPolicy,
	pub(in crate::queue) job_timeout: JobTimeout,
}

/// Registered worker task handlers.
///
/// A handler can run more than once for one job. A run that fails is retried while retries
/// remain, a worker that stops or loses its claim before it records the outcome leaves the job
/// to be claimed again, and a stale running job is returned to pending. The application must
/// make every effect of a handler, inside and outside the database, safe to repeat.
#[derive(Clone, Default)]
pub struct TaskRegistry {
	pub(in crate::queue) tasks: Arc<HashMap<TaskName, RegisteredTask>>,
}

/// The observable label of the worker that claimed a job: the worker's name and a fresh suffix
/// for each batch of claims.
///
/// This is not a claim capability and must not be treated as authorization. The
/// [`PgClaimedJob`](crate::queue::PgClaimedJob) that
/// [`PgQueue::claim_due`](crate::queue::PgQueue::claim_due) returns carries the unforgeable
/// authority for heartbeat, completion, retry, and failure.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct WorkerOwnerId {
	value: String,
}

impl WorkerOwnerId {
	/// Creates a fresh observable worker owner ID from a worker name and a new sortable ID.
	pub fn generate(worker_name: &WorkerName) -> Result<Self, GenerateWorkerOwnerIdError> {
		let suffix = id::SortableId::generate()
			.map_err(GenerateWorkerOwnerIdError::IdGeneration)?
			.to_text();
		let value = format!(
			"{}{QUEUE_WORKER_OWNER_ID_SEPARATOR}{suffix}",
			worker_name.as_str()
		);
		Ok(Self::from_validated_text(value)
			.expect("a worker name and a generated suffix form a valid worker owner id"))
	}

	/// Validates and copies owner ID text that Queue did not generate, which means that tests
	/// can exercise claims that share a label.
	#[cfg(feature = "__internal")]
	#[doc(hidden)]
	pub fn from_text_for_tests(text: impl Into<String>) -> Result<Self, WorkerOwnerIdError> {
		Self::from_validated_text(text.into())
	}

	/// Returns the worker owner label stored in running queue rows.
	pub fn as_str(&self) -> &str {
		&self.value
	}

	pub(in crate::queue) fn from_validated_text(value: String) -> Result<Self, WorkerOwnerIdError> {
		check_worker_owner_id(&value)?;
		Ok(Self { value })
	}
}

impl AsRef<str> for WorkerOwnerId {
	fn as_ref(&self) -> &str {
		self.as_str()
	}
}

impl fmt::Display for WorkerOwnerId {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(self.as_str())
	}
}

/// How a worker ended its running job tasks after a stop request.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WorkerShutdownOutcome {
	/// Every job task returned without forced cancellation. Jobs may have failed,
	/// retried, or lost ownership; the summary's counters report those outcomes.
	#[default]
	Drained,
	/// The grace period expired, and the worker requested cancellation of its remaining
	/// job tasks and joined them. Tasks that finished before cancellation took effect
	/// retain their ordinary outcomes in the summary.
	GracePeriodExpired {
		/// Job tasks for which a join result confirmed cancellation. This can be zero when
		/// tasks finish while cancellation is being requested. Cancelling a job task
		/// does not establish that its database or external effects were rolled back.
		cancelled_job_count: u32,
	},
}

/// Summary returned after a long-running worker stops and its claim cleanup succeeds.
/// This accounts for this worker's job tasks, not detached work or other workers.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct WorkerRunLoopSummary {
	/// Whether job tasks drained or exceeded the shutdown grace period.
	pub shutdown: WorkerShutdownOutcome,
	/// Claimed jobs count.
	pub claimed_count: u32,
	/// Jobs completed successfully.
	pub succeeded_count: u32,
	/// Jobs scheduled for retry.
	pub retried_count: u32,
	/// Jobs marked failed without dead-lettering.
	pub failed_count: u32,
	/// Jobs moved to dead-letter storage.
	pub dead_lettered_count: u32,
	/// Jobs that lost ownership before a terminal worker update.
	pub lost_ownership_count: u32,
}

#[cfg(feature = "__internal")]
impl WorkerRunLoopSummary {
	/// Builds a value so that tests can state an expected result.
	#[doc(hidden)]
	pub fn for_tests(
		claimed_count: u32,
		succeeded_count: u32,
		retried_count: u32,
		failed_count: u32,
		dead_lettered_count: u32,
		lost_ownership_count: u32,
	) -> Self {
		Self {
			shutdown: WorkerShutdownOutcome::Drained,
			claimed_count,
			succeeded_count,
			retried_count,
			failed_count,
			dead_lettered_count,
			lost_ownership_count,
		}
	}
}

/// Handle for a long-running queue worker.
///
/// Dropping the handle requests a graceful stop but does not wait for the worker to finish. The
/// worker's result, which holds its summary or the failure that ended it, is then discarded, so
/// the application loses a failure that it did not retrieve. The application must call
/// [`stop_and_wait`](WorkerHandle::stop_and_wait) or [`wait`](WorkerHandle::wait) to observe
/// the result. [`wait_until_finished`](WorkerHandle::wait_until_finished) waits for the worker
/// to finish without consuming the handle.
#[must_use = "call stop_and_wait or request_stop followed by wait so the long-running queue worker lifecycle is observed"]
pub struct WorkerHandle {
	pub(in crate::queue) worker_shutdown_signal: RuntimeCancellationSignal,
	pub(in crate::queue) task: WorkerTaskState,
}

pub(in crate::queue) enum WorkerTaskState {
	Running(tokio::task::JoinHandle<Result<WorkerRunLoopSummary, WorkerRunError>>),
	Finished(Result<WorkerRunLoopSummary, WorkerRunError>),
	Consumed,
}

#[cfg(feature = "__internal")]
#[doc(hidden)]
impl WorkerHandle {
	/// Builds a handle around an already-spawned worker task and the signal
	/// that stops it.
	pub fn from_signal_and_join_handle(
		worker_shutdown_signal: RuntimeCancellationSignal,
		join_handle: tokio::task::JoinHandle<Result<WorkerRunLoopSummary, WorkerRunError>>,
	) -> Self {
		Self {
			worker_shutdown_signal,
			task: WorkerTaskState::Running(join_handle),
		}
	}
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessedJobOutcome {
	Succeeded,
	Retried,
	Failed,
	DeadLettered,
	LostOwnership,
}

/// The Queue storage that a worker uses.
pub(in crate::queue) type WorkerQueue = crate::queue::engine::PgNativeQueueGroup;

/// The owner label that a worker's claims carry.
#[derive(Clone)]
pub(in crate::queue) enum WorkerClaimOwner {
	/// A fresh label for each batch of claims. While v10 writers remain, a native worker
	/// claims from v10's Queue tables, which identify a claim by its label alone, so a label
	/// must never name two claims.
	PerBatch(WorkerName),
}

impl WorkerClaimOwner {
	pub(in crate::queue) fn owner_for_next_batch(
		&self,
	) -> Result<WorkerOwnerId, GenerateWorkerOwnerIdError> {
		match self {
			Self::PerBatch(name) => WorkerOwnerId::generate(name),
		}
	}
}

#[derive(Clone)]
pub(in crate::queue) struct WorkerRuntime {
	pub(in crate::queue) queue: WorkerQueue,
	pub(in crate::queue) pool: WritePool,
	pub(in crate::queue) task_registry: TaskRegistry,
	pub(in crate::queue) claim_owner: WorkerClaimOwner,
	pub(in crate::queue) config: WorkerConfig,
	pub(in crate::queue) worker_shutdown_signal: RuntimeCancellationSignal,
}

#[doc(hidden)]
#[derive(Clone)]
pub struct RuntimeCancellationSignal {
	pub(in crate::queue) state: tokio::sync::watch::Sender<CancellationState>,
}

#[derive(Clone, Copy)]
pub(in crate::queue) enum CancellationState {
	Available,
	Requested,
}
