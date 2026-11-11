use super::model::{
	CancellationState, JobExecutionCtx, RegisteredTask, RetryPolicy, RuntimeCancellationSignal,
	TaskError, TaskRegistry, WorkerOwnerId,
};
use crate::db::WritePool;
use crate::queue::{
	HeartbeatError, JobId, RegisterTaskError, TaskDef, TaskName, resolve_queue_retry_policy,
	touch_owned_running_job_execution_heartbeat_with_database_operation_timeout,
};
use serde::de::DeserializeOwned;
use std::fmt;
use std::sync::Arc;

impl TaskError {
	/// Creates a retryable task error.
	pub fn retryable(msg: impl Into<String>) -> Self {
		Self {
			msg: msg.into(),
			permanent: false,
		}
	}

	/// Creates a permanent task error.
	pub fn permanent(msg: impl Into<String>) -> Self {
		Self {
			msg: msg.into(),
			permanent: true,
		}
	}

	/// Returns the task error message.
	pub fn msg(&self) -> &str {
		&self.msg
	}

	/// Returns whether this error should skip retries.
	pub fn is_permanent(&self) -> bool {
		self.permanent
	}
}

impl fmt::Display for TaskError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(&self.msg)
	}
}

impl std::error::Error for TaskError {}

impl JobExecutionCtx {
	/// Returns the current job ID.
	pub fn job_id(&self) -> JobId {
		self.claimed_job.job_id()
	}

	/// Returns the current task name.
	pub fn task_name(&self) -> &TaskName {
		&self.claimed_job.job().task_name
	}

	/// Returns the owner label of the claim that holds the job. The worker creates a fresh label
	/// for each batch of claims.
	pub fn worker_owner_id(&self) -> &WorkerOwnerId {
		self.claimed_job.worker_owner_id()
	}

	/// Returns the current retry count.
	pub fn retry_count(&self) -> u32 {
		self.claimed_job.job().retry_count
	}

	/// Returns the configured max retry count.
	pub fn max_retries(&self) -> u32 {
		self.claimed_job.job().max_retries
	}

	/// Returns the write pool of this job's queue. Writes through it commit in their own
	/// transactions, not with the job's completion: the worker marks the job completed in a
	/// separate transaction after the task returns, so a task can run again after its writes
	/// have committed, for example when the worker stops before it records the completion.
	pub fn pool(&self) -> &WritePool {
		&self.pool
	}

	/// Returns true when the owning long-running worker has been asked to stop.
	pub fn is_worker_shutdown_requested(&self) -> bool {
		self.worker_shutdown_signal.is_cancellation_requested()
	}

	/// Waits until the owning long-running worker has been asked to stop.
	pub async fn wait_until_worker_shutdown_requested(&self) {
		self.worker_shutdown_signal
			.wait_until_cancellation_requested()
			.await;
	}

	/// Returns true when this job should stop because ownership was lost.
	pub fn is_job_cancellation_requested(&self) -> bool {
		self.job_cancellation_signal.is_cancellation_requested()
	}

	/// Waits until this job should stop because ownership was lost.
	pub async fn wait_until_job_cancellation_requested(&self) {
		self.job_cancellation_signal
			.wait_until_cancellation_requested()
			.await;
	}

	/// Records an execution heartbeat for the current job if this worker still owns it.
	///
	/// A heartbeat that finds the job no longer owned, or that meets an incompatible protocol,
	/// requests the job's cancellation, and a heartbeat after that cancellation fails with
	/// [`HeartbeatError::JobNotRunning`] without a database operation.
	pub async fn touch_execution_heartbeat(&self) -> Result<(), HeartbeatError> {
		if self.job_cancellation_signal.is_cancellation_requested() {
			return Err(HeartbeatError::JobNotRunning);
		}
		let sent_at = tokio::time::Instant::now();
		let result = touch_owned_running_job_execution_heartbeat_with_database_operation_timeout(
			&self.queue,
			&self.pool,
			&self.claimed_job,
			self.database_operation_timeout,
		)
		.await;
		match &result {
			Ok(()) => self.newest_heartbeat.record(sent_at),
			Err(error)
				if matches!(error, HeartbeatError::JobNotRunning)
					|| error.is_protocol_failure() =>
			{
				self.job_cancellation_signal.request_cancellation();
			}
			Err(_) => {}
		}
		result
	}
}

impl Default for RuntimeCancellationSignal {
	fn default() -> Self {
		Self::new()
	}
}

#[doc(hidden)]
impl RuntimeCancellationSignal {
	pub fn new() -> Self {
		let (state, _) = tokio::sync::watch::channel(CancellationState::Available);
		Self { state }
	}

	pub fn request_cancellation(&self) -> bool {
		self.state.send_if_modified(|state| match state {
			CancellationState::Available => {
				*state = CancellationState::Requested;
				true
			}
			CancellationState::Requested => false,
		})
	}

	pub fn is_cancellation_requested(&self) -> bool {
		matches!(*self.state.borrow(), CancellationState::Requested)
	}

	pub async fn wait_until_cancellation_requested(&self) {
		self.state
			.subscribe()
			.wait_for(|state| matches!(state, CancellationState::Requested))
			.await
			.expect("the waiting signal retains a sender");
	}
}

impl TaskRegistry {
	/// Creates an empty task registry.
	#[cfg(feature = "__internal")]
	#[doc(hidden)]
	pub fn new() -> Self {
		Self::default()
	}

	/// Registers the handler of a task, with the retry policy for the task's failed jobs.
	///
	/// The worker decodes each job's payload as `T` and passes it to the handler. It runs the
	/// task's jobs that record no timeout of their own under the definition's timeout.
	///
	/// A job with a payload that is not JSON for `T` fails permanently, and the handler is not
	/// called. The job's stored failure message names the kind and position of the decode failure
	/// and never quotes the payload.
	pub fn register<T, F, Fut>(
		&mut self,
		def: &TaskDef<T>,
		retry_policy: RetryPolicy,
		handler: F,
	) -> Result<(), RegisterTaskError>
	where
		T: DeserializeOwned + Send + 'static,
		F: Fn(JobExecutionCtx, T) -> Fut + Send + Sync + 'static,
		Fut: Future<Output = Result<(), TaskError>> + Send + 'static,
	{
		let policy = def.policy();
		policy.require_storable()?;
		let retry_policy = resolve_queue_retry_policy(retry_policy)?;
		if self.tasks.contains_key(def.name()) {
			return Err(RegisterTaskError::AlreadyRegistered);
		}

		let mut tasks = (*self.tasks).clone();
		let handler = Arc::new(handler);
		tasks.insert(
			def.name().clone(),
			RegisteredTask {
				handler: Arc::new(move |ctx, payload_json| {
					let handler = Arc::clone(&handler);
					Box::pin(async move {
						let payload =
							serde_json::from_str::<T>(&payload_json).map_err(|source| {
								TaskError::permanent(format!(
									"queue payload could not be decoded: {}",
									primitives_rs::serialization::json::JsonDecodeFailureSummary::from(
										&source
									)
								))
							})?;
						handler(ctx, payload).await
					})
				}),
				retry_policy,
				job_timeout: policy.timeout,
			},
		);
		self.tasks = Arc::new(tasks);
		Ok(())
	}

	/// Returns true when the registry has no handlers.
	pub fn is_empty(&self) -> bool {
		self.tasks.is_empty()
	}

	/// Returns the number of registered task handlers.
	pub fn registered_task_count(&self) -> usize {
		self.tasks.len()
	}

	/// Returns registered task names, sorted ascending.
	pub fn registered_task_names(&self) -> Vec<TaskName> {
		let mut task_names = self.tasks.keys().cloned().collect::<Vec<_>>();
		task_names.sort();
		task_names
	}

	pub(in crate::queue) fn registered_task(
		&self,
		task_name: &TaskName,
	) -> Option<&RegisteredTask> {
		self.tasks.get(task_name)
	}
}
