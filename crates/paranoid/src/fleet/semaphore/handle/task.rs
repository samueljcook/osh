//! Runs a task while one Semaphore acquisition holds a slot.

use super::PgSemaphore;
use super::error::PgSemaphoreError;
use crate::fleet::SemaphoreClaimSnapshot;
use crate::fleet::semaphore::engine::{
	NativeSemaphoreAccountingObservation, NativeSemaphoreAcquisitionChoice,
	NativeSemaphoreCallerCtx, NativeSemaphoreCleanupObservation, NativeSemaphoreClient,
	NativeSemaphoreLocalObservation, NativeSemaphoreObserver, NativeSemaphoreReleaseObservation,
	NativeSemaphoreStartOptions, NativeSemaphoreStartOptionsFields, NativeSemaphoreStartResult,
};

/// What a task receives while its acquisition holds a Semaphore slot.
pub struct PgSemaphoreTaskCtx {
	claim: SemaphoreClaimSnapshot,
	observer: NativeSemaphoreObserver,
}

impl PgSemaphoreTaskCtx {
	/// The acquisition's holder and slot labels, which grant nothing.
	pub fn claim_snapshot(&self) -> &SemaphoreClaimSnapshot {
		&self.claim
	}

	/// Reports whether the acquisition still holds its slot, by the local record of its
	/// confirmed acquisition and hold window. The report performs no database work.
	pub fn hold(&self) -> PgSemaphoreHold {
		PgSemaphoreHold::from_observation(self.observer.snapshot())
	}
}

/// Whether an acquisition holds its Semaphore slot. A loss is permanent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PgSemaphoreHold {
	/// The acquisition holds its slot.
	Held,
	/// The acquisition lost its slot, for example because its hold window elapsed or the
	/// runtime stopped. Another acquisition may hold the slot now.
	Lost,
}

impl PgSemaphoreHold {
	fn from_observation(observation: NativeSemaphoreLocalObservation) -> Self {
		match observation {
			NativeSemaphoreLocalObservation::ConfirmedLocally => PgSemaphoreHold::Held,
			NativeSemaphoreLocalObservation::Pending
			| NativeSemaphoreLocalObservation::Lost(_)
			| NativeSemaphoreLocalObservation::Closing
			| NativeSemaphoreLocalObservation::Finished => PgSemaphoreHold::Lost,
		}
	}
}

/// Whether a run confirmed the release of its Semaphore slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PgSemaphoreRelease {
	/// The run released its slot.
	Released,
	/// The run's acquisition no longer held its slot when the run released it.
	NotHeld,
	/// The run did not confirm the release. The slot is free no later than when the
	/// acquisition's hold window ends.
	Unconfirmed,
}

impl PgSemaphoreRelease {
	fn from_accounting(accounting: &NativeSemaphoreAccountingObservation) -> Self {
		match accounting {
			NativeSemaphoreAccountingObservation::Complete(
				NativeSemaphoreCleanupObservation::Released(
					NativeSemaphoreReleaseObservation::Released,
				),
			) => PgSemaphoreRelease::Released,
			NativeSemaphoreAccountingObservation::Complete(
				NativeSemaphoreCleanupObservation::Released(
					NativeSemaphoreReleaseObservation::NoMatch,
				)
				| NativeSemaphoreCleanupObservation::NotAcquired,
			) => PgSemaphoreRelease::NotHeld,
			NativeSemaphoreAccountingObservation::Pending
			| NativeSemaphoreAccountingObservation::Unresolved
			| NativeSemaphoreAccountingObservation::Abandoned => PgSemaphoreRelease::Unconfirmed,
		}
	}
}

/// What a Semaphore run reports about a task that it ran.
#[derive(Debug)]
#[non_exhaustive]
pub struct PgSemaphoreTaskResult<T, E> {
	/// The task's own output.
	pub task: Result<T, E>,
	/// Whether the acquisition held its slot until the task returned. `Lost` means that more
	/// tasks than the capacity may have run at the same time.
	pub hold: PgSemaphoreHold,
	/// Whether the run confirmed that the slot was released.
	pub release: PgSemaphoreRelease,
}

/// The result of a Semaphore run that does not wait.
#[derive(Debug)]
pub enum PgSemaphoreTryRunTaskResult<T, E> {
	/// This run held a slot and ran the task.
	Ran(PgSemaphoreTaskResult<T, E>),
	/// Every slot was held, so this run did not run the task.
	NoSlotAvailable,
}

impl PgSemaphore {
	/// Waits until this run holds a slot, runs `task_fn`, and releases the slot. The run retries
	/// after each settled denial with the handle's acquire retry interval.
	///
	/// An error means that the task did not run.
	pub fn run_task<T, E, TaskFn>(
		&self,
		task_fn: TaskFn,
	) -> impl Future<Output = Result<PgSemaphoreTaskResult<T, E>, PgSemaphoreError>>
	where
		TaskFn: AsyncFnOnce(PgSemaphoreTaskCtx) -> Result<T, E>,
	{
		// The boxed task keeps its captured state out of the futures that run it.
		self.run_task_boxed(Box::new(task_fn))
	}

	async fn run_task_boxed<T, E, TaskFn>(
		&self,
		task_fn: Box<TaskFn>,
	) -> Result<PgSemaphoreTaskResult<T, E>, PgSemaphoreError>
	where
		TaskFn: AsyncFnOnce(PgSemaphoreTaskCtx) -> Result<T, E>,
	{
		match self
			.start(NativeSemaphoreAcquisitionChoice::Waiting(self.waiting))
			.await?
		{
			NativeSemaphoreStartResult::Acquired(client) => run(client, task_fn).await,
			NativeSemaphoreStartResult::Unavailable => {
				unreachable!("a waiting acquisition returns only when it acquires")
			}
		}
	}

	/// Runs `task_fn` when a slot is free, and releases the slot. A run that finds every slot
	/// held returns without waiting for a slot.
	///
	/// The acquisitions of one key in this runtime run one at a time, in the order in which
	/// their tasks reach the key's local queue. Task scheduling can differ from call order.
	/// This wait has no limit of its own; dropping the returned future ends it without running
	/// the task.
	///
	/// An error means that the task did not run.
	pub fn try_run_task<T, E, TaskFn>(
		&self,
		task_fn: TaskFn,
	) -> impl Future<Output = Result<PgSemaphoreTryRunTaskResult<T, E>, PgSemaphoreError>>
	where
		TaskFn: AsyncFnOnce(PgSemaphoreTaskCtx) -> Result<T, E>,
	{
		// The boxed task keeps its captured state out of the futures that run it.
		self.try_run_task_boxed(Box::new(task_fn))
	}

	async fn try_run_task_boxed<T, E, TaskFn>(
		&self,
		task_fn: Box<TaskFn>,
	) -> Result<PgSemaphoreTryRunTaskResult<T, E>, PgSemaphoreError>
	where
		TaskFn: AsyncFnOnce(PgSemaphoreTaskCtx) -> Result<T, E>,
	{
		match self
			.start(NativeSemaphoreAcquisitionChoice::TryOnce)
			.await?
		{
			NativeSemaphoreStartResult::Acquired(client) => run(client, task_fn)
				.await
				.map(PgSemaphoreTryRunTaskResult::Ran),
			NativeSemaphoreStartResult::Unavailable => {
				Ok(PgSemaphoreTryRunTaskResult::NoSlotAvailable)
			}
		}
	}

	async fn start(
		&self,
		acquisition: NativeSemaphoreAcquisitionChoice,
	) -> Result<NativeSemaphoreStartResult, PgSemaphoreError> {
		let holder = self
			.holder_prefix
			.generate_holder_id()
			.map_err(PgSemaphoreError::randomness)?;
		let pending = self
			.execution
			.start_owned(NativeSemaphoreStartOptions::new(
				NativeSemaphoreStartOptionsFields {
					holder,
					acquisition,
				},
			))
			.map_err(PgSemaphoreError::operation)?;
		pending.receive().await.map_err(PgSemaphoreError::shared)
	}
}

async fn run<T, E, TaskFn>(
	client: NativeSemaphoreClient,
	task_fn: TaskFn,
) -> Result<PgSemaphoreTaskResult<T, E>, PgSemaphoreError>
where
	TaskFn: AsyncFnOnce(PgSemaphoreTaskCtx) -> Result<T, E>,
{
	let result = client
		.run_task(move |NativeSemaphoreCallerCtx { claim, observer }| {
			Box::pin(task_fn(PgSemaphoreTaskCtx { claim, observer }))
		})
		.await
		.map_err(PgSemaphoreError::shared)?;
	Ok(PgSemaphoreTaskResult {
		task: result.task,
		hold: PgSemaphoreHold::from_observation(result.local),
		release: PgSemaphoreRelease::from_accounting(&result.accounting),
	})
}
