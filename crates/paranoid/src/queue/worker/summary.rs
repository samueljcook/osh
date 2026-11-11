use super::job::queue_worker_run_loop_result_from_join_result;
use super::model::{ProcessedJobOutcome, WorkerHandle, WorkerRunLoopSummary, WorkerTaskState};
use crate::queue::WorkerRunError;

impl WorkerRunLoopSummary {
	/// Adds one claim batch's size to this summary's claimed total.
	#[cfg(not(feature = "__internal"))]
	pub(in crate::queue) fn record_claimed_count(
		&mut self,
		claimed_count: usize,
	) -> Result<(), WorkerRunError> {
		self.record_claimed_count_inner(claimed_count)
	}

	/// Adds one claim batch's size to this summary's claimed total.
	#[cfg(feature = "__internal")]
	#[doc(hidden)]
	pub fn record_claimed_count(&mut self, claimed_count: usize) -> Result<(), WorkerRunError> {
		self.record_claimed_count_inner(claimed_count)
	}

	fn record_claimed_count_inner(&mut self, claimed_count: usize) -> Result<(), WorkerRunError> {
		let claimed_count: u32 = claimed_count
			.try_into()
			.expect("a worker claims at most its concurrency, which is below 2^32");
		self.claimed_count = self
			.claimed_count
			.checked_add(claimed_count)
			.ok_or(WorkerRunError::SummaryCounterOverflow)?;
		Ok(())
	}

	/// Folds one processed job's outcome into this summary's counters.
	#[cfg(not(feature = "__internal"))]
	pub(in crate::queue) fn record_processed_job_outcome(
		&mut self,
		outcome: ProcessedJobOutcome,
	) -> Result<(), WorkerRunError> {
		self.record_processed_job_outcome_inner(outcome)
	}

	/// Folds one processed job's outcome into this summary's counters.
	#[cfg(feature = "__internal")]
	#[doc(hidden)]
	pub fn record_processed_job_outcome(
		&mut self,
		outcome: ProcessedJobOutcome,
	) -> Result<(), WorkerRunError> {
		self.record_processed_job_outcome_inner(outcome)
	}

	fn record_processed_job_outcome_inner(
		&mut self,
		outcome: ProcessedJobOutcome,
	) -> Result<(), WorkerRunError> {
		let counter = match outcome {
			ProcessedJobOutcome::Succeeded => &mut self.succeeded_count,
			ProcessedJobOutcome::Retried => &mut self.retried_count,
			ProcessedJobOutcome::Failed => &mut self.failed_count,
			ProcessedJobOutcome::DeadLettered => &mut self.dead_lettered_count,
			ProcessedJobOutcome::LostOwnership => &mut self.lost_ownership_count,
		};
		*counter = counter
			.checked_add(1)
			.ok_or(WorkerRunError::SummaryCounterOverflow)?;
		Ok(())
	}
}

impl WorkerHandle {
	/// Requests graceful worker shutdown. Returns true if shutdown had not already been requested.
	pub fn request_stop(&self) -> bool {
		self.worker_shutdown_signal.request_cancellation()
	}

	/// Returns whether the worker task has finished.
	pub fn is_finished(&self) -> bool {
		match &self.task {
			WorkerTaskState::Running(join_handle) => join_handle.is_finished(),
			WorkerTaskState::Finished(_) | WorkerTaskState::Consumed => true,
		}
	}

	/// Waits until the worker task has finished, and keeps the worker's result for
	/// [`wait`](Self::wait) or [`stop_and_wait`](Self::stop_and_wait).
	///
	/// The handle stays usable, so an application can wait for the first of several workers to
	/// finish and then stop the others. Dropping the returned future loses nothing.
	pub async fn wait_until_finished(&mut self) {
		if let WorkerTaskState::Running(join_handle) = &mut self.task {
			let join_result = join_handle.await;
			self.task = WorkerTaskState::Finished(queue_worker_run_loop_result_from_join_result(
				join_result,
			));
		}
	}

	/// Waits for the worker task to finish, and returns the worker's result.
	/// A successful result can include forced job cancellation; inspect
	/// [`WorkerRunLoopSummary::shutdown`] to determine whether any tasks required cancellation.
	pub async fn wait(mut self) -> Result<WorkerRunLoopSummary, WorkerRunError> {
		self.wait_until_finished().await;
		match std::mem::replace(&mut self.task, WorkerTaskState::Consumed) {
			WorkerTaskState::Finished(result) => result,
			WorkerTaskState::Running(_) | WorkerTaskState::Consumed => {
				unreachable!("the worker task has finished and retains its result")
			}
		}
	}

	/// Requests graceful worker shutdown, then waits for the worker task to finish.
	/// After the grace period expires, the worker cancels and joins remaining job
	/// tasks and returns their claims to pending where it still holds them. A successful
	/// result reports that path through [`WorkerRunLoopSummary::shutdown`].
	pub async fn stop_and_wait(self) -> Result<WorkerRunLoopSummary, WorkerRunError> {
		self.request_stop();
		self.wait().await
	}
}

impl Drop for WorkerHandle {
	fn drop(&mut self) {
		let _stop_requested = self.worker_shutdown_signal.request_cancellation();
	}
}
