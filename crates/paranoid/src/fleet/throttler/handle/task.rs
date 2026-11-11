//! Runs a task while one Throttler acquisition holds its permit.

use std::time::Duration;

use super::PgThrottler;
use super::error::PgThrottlerError;
use crate::fleet::throttler::engine::{
	NativeThrottlerAccountingObservation, NativeThrottlerClient, NativeThrottlerDenial,
	NativeThrottlerLocalObservation, NativeThrottlerObserver, NativeThrottlerStartOptionsFields,
	NativeThrottlerStartResult,
};

/// What a task receives while its acquisition holds a Throttler permit.
pub struct PgThrottlerTaskCtx {
	observer: NativeThrottlerObserver,
}

impl PgThrottlerTaskCtx {
	/// Reports whether the acquisition still holds its permit, by the local record of its
	/// confirmed acquisition and reservation window. The report performs no database work.
	pub fn hold(&self) -> PgThrottlerHold {
		PgThrottlerHold::from_observation(self.observer.snapshot())
	}
}

/// Whether an acquisition holds its Throttler permit. A loss is permanent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PgThrottlerHold {
	/// The acquisition holds its permit.
	Held,
	/// The acquisition lost its permit, for example because its concurrency slot or probe
	/// reservation expired or the runtime stopped.
	Lost,
}

impl PgThrottlerHold {
	fn from_observation(observation: NativeThrottlerLocalObservation) -> Self {
		match observation {
			NativeThrottlerLocalObservation::ConfirmedLocally => PgThrottlerHold::Held,
			NativeThrottlerLocalObservation::Pending
			| NativeThrottlerLocalObservation::Lost(_)
			| NativeThrottlerLocalObservation::Closing
			| NativeThrottlerLocalObservation::Finished => PgThrottlerHold::Lost,
		}
	}
}

/// Whether a run confirmed that its task outcome was recorded and its permit released.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PgThrottlerRelease {
	/// The run recorded the task's outcome and released its permit.
	Released,
	/// The run did not confirm the release. A concurrency slot or probe reservation is free no
	/// later than when its window ends.
	Unconfirmed,
}

/// What a Throttler run reports about a task that it ran.
#[derive(Debug)]
#[non_exhaustive]
pub struct PgThrottlerTaskResult<T, E> {
	/// The task's own output. An error counts as a failure for the circuit.
	pub task: Result<T, E>,
	/// Whether the acquisition held its permit until the task returned.
	pub hold: PgThrottlerHold,
	/// Whether the run confirmed the outcome and the release.
	pub release: PgThrottlerRelease,
}

/// The result of a Throttler run.
#[derive(Debug)]
pub enum PgThrottlerTryRunTaskResult<T, E> {
	/// This run held a permit and ran the task.
	Ran(PgThrottlerTaskResult<T, E>),
	/// The rate or concurrency limit denied the run, so it did not run the task.
	Throttled {
		/// The suggested wait before another attempt, when the limit reports one.
		retry_after: Option<Duration>,
	},
	/// The circuit is open, so the run did not run the task.
	CircuitOpen,
}

/// The outcome of one acquisition attempt.
enum Attempt {
	Acquired(NativeThrottlerClient),
	Throttled { retry_after: Option<Duration> },
	CircuitOpen,
}

impl PgThrottler {
	/// Runs `task_fn` when the Throttler permits it, records the task's outcome, and releases
	/// the permit. A denied run returns without waiting for a permit.
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
	) -> impl Future<Output = Result<PgThrottlerTryRunTaskResult<T, E>, PgThrottlerError>>
	where
		TaskFn: AsyncFnOnce(PgThrottlerTaskCtx) -> Result<T, E>,
	{
		// The boxed task keeps its captured state out of the futures that run it.
		self.try_run_task_boxed(Box::new(task_fn))
	}

	async fn try_run_task_boxed<T, E, TaskFn>(
		&self,
		task_fn: Box<TaskFn>,
	) -> Result<PgThrottlerTryRunTaskResult<T, E>, PgThrottlerError>
	where
		TaskFn: AsyncFnOnce(PgThrottlerTaskCtx) -> Result<T, E>,
	{
		match self.attempt().await? {
			Attempt::Acquired(client) => Ok(PgThrottlerTryRunTaskResult::Ran(
				self.run_acquired(client, task_fn).await?,
			)),
			Attempt::Throttled { retry_after } => {
				Ok(PgThrottlerTryRunTaskResult::Throttled { retry_after })
			}
			Attempt::CircuitOpen => Ok(PgThrottlerTryRunTaskResult::CircuitOpen),
		}
	}

	/// Waits until the Throttler permits a run, then runs `task_fn`, records the task's outcome,
	/// and releases the permit. After a denial by the rate limit, the run tries again when the
	/// denial reports that the next token is available; after a denial by the concurrency
	/// limit or an open circuit, it tries again after the handle's acquire retry interval.
	///
	/// The wait has no limit of its own; dropping the returned future while it waits ends the
	/// wait without running the task. An error means that the task did not run.
	pub fn run_task<T, E, TaskFn>(
		&self,
		task_fn: TaskFn,
	) -> impl Future<Output = Result<PgThrottlerTaskResult<T, E>, PgThrottlerError>>
	where
		TaskFn: AsyncFnOnce(PgThrottlerTaskCtx) -> Result<T, E>,
	{
		// The boxed task keeps its captured state out of the futures that run it.
		self.run_task_boxed(Box::new(task_fn))
	}

	async fn run_task_boxed<T, E, TaskFn>(
		&self,
		task_fn: Box<TaskFn>,
	) -> Result<PgThrottlerTaskResult<T, E>, PgThrottlerError>
	where
		TaskFn: AsyncFnOnce(PgThrottlerTaskCtx) -> Result<T, E>,
	{
		loop {
			let wait = match self.attempt().await? {
				Attempt::Acquired(client) => return self.run_acquired(client, task_fn).await,
				Attempt::Throttled { retry_after } => {
					retry_after.unwrap_or(self.acquire_retry_interval)
				}
				Attempt::CircuitOpen => self.acquire_retry_interval,
			};
			tokio::time::sleep(wait).await;
		}
	}

	async fn attempt(&self) -> Result<Attempt, PgThrottlerError> {
		let settings = self.handle.settings();
		let holder = if settings.concurrency().is_some() || settings.circuit().is_some() {
			Some(
				self.holder_prefix
					.generate_holder_id()
					.map_err(PgThrottlerError::randomness)?,
			)
		} else {
			None
		};
		let options = self
			.handle
			.start_options(NativeThrottlerStartOptionsFields {
				rate_limit_cost: self.rate_limit_cost,
				holder,
				probe_heartbeat: self.probe_heartbeat,
			})
			.map_err(PgThrottlerError::operation)?;
		let pending = self
			.handle
			.start_owned(options)
			.map_err(PgThrottlerError::operation)?;
		Ok(
			match pending.receive().await.map_err(PgThrottlerError::shared)? {
				NativeThrottlerStartResult::Acquired(client) => Attempt::Acquired(client),
				NativeThrottlerStartResult::Denied(NativeThrottlerDenial::Throttled {
					retry_after,
				}) => Attempt::Throttled { retry_after },
				NativeThrottlerStartResult::Denied(NativeThrottlerDenial::CircuitOpen) => {
					Attempt::CircuitOpen
				}
			},
		)
	}

	async fn run_acquired<T, E, TaskFn>(
		&self,
		client: NativeThrottlerClient,
		task_fn: TaskFn,
	) -> Result<PgThrottlerTaskResult<T, E>, PgThrottlerError>
	where
		TaskFn: AsyncFnOnce(PgThrottlerTaskCtx) -> Result<T, E>,
	{
		let result = client
			.run_task(move |observer| Box::pin(task_fn(PgThrottlerTaskCtx { observer })))
			.await
			.map_err(PgThrottlerError::shared)?;
		let accounting = result.observer.wait_accounting().await;
		Ok(PgThrottlerTaskResult {
			task: result.task,
			hold: PgThrottlerHold::from_observation(result.local),
			release: match accounting {
				NativeThrottlerAccountingObservation::Complete => PgThrottlerRelease::Released,
				NativeThrottlerAccountingObservation::Pending
				| NativeThrottlerAccountingObservation::Unresolved
				| NativeThrottlerAccountingObservation::Abandoned => PgThrottlerRelease::Unconfirmed,
			},
		})
	}
}
