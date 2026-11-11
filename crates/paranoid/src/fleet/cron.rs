//! Public cron operations of the native runtime.

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::time::Duration;

use tokio::time::{Instant, sleep, sleep_until};

use crate::db::Database;
use crate::fleet::lease::PgNativeLease;
use crate::fleet::lease::handle::{
	PgLeaseKeyError, PgLeaseRelease, PgLeaseRunError, PgLeaseRunOptions, PgLeaseTaskCtx,
};
use crate::fleet::lease::owned_driver::acquire::{
	NativeLeaseAcquireError, NativeLeaseAcquireFields, NativeLeaseAcquisition,
};
use crate::fleet::lease::owned_driver::close::{
	NativeLeaseCloseWait, NativeLeaseCloseWaitError, NativeLeaseCloseWaitResult,
};
use crate::fleet::lease::owned_driver::loss::NativeLeaseAuthorityObservation;
use crate::fleet::lease::owned_driver::options::NativeLeaseAcquisitionChoice;
use crate::fleet::lease::owned_driver::{NativeLeaseDriverError, NativeOwnedLeaseClient};
use crate::fleet::{
	CronKey, HolderPrefix, PgLease, PgLeaseError, PgLeaseHolder, RandomnessError,
	generate_jittered_acquire_retry_delay,
};

/// The inputs of a cron handle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PgCronFields {
	/// The cron's key.
	pub key: CronKey,
	/// The positive time between the end of one run of the task and the start of the next.
	pub interval: Duration,
	/// The lease options of the cron's leadership.
	pub lease_options: PgLeaseRunOptions,
}

/// One cron of the runtime's installation: a task that runs periodically on whichever process
/// holds the cron's leadership.
///
/// Leadership is a lease that Paranoid renews automatically. On an installation imported from
/// v10, the cron uses the lease key that v10's `Store::cron` uses for the same key, so a leader
/// of this handle and a v10 leader of that key exclude each other while both releases run.
pub struct PgCron {
	key: CronKey,
	interval: Duration,
	native: PgNativeLease,
	holder_prefix: HolderPrefix,
	lease_options: PgLeaseRunOptions,
	close_wait: NativeLeaseCloseWait,
}

impl PgCron {
	pub(crate) fn new(
		fields: PgCronFields,
		native: PgNativeLease,
		holder_prefix: HolderPrefix,
	) -> Result<Self, PgCronError> {
		if fields.interval.is_zero() {
			return Err(PgCronError::IntervalIsZero);
		}
		if primitives_rs::time::Deadline::new_after(fields.interval).is_err() {
			return Err(PgCronError::LocalClockDomain);
		}
		let close_wait = native.close_wait();
		Ok(Self {
			key: fields.key,
			interval: fields.interval,
			native,
			holder_prefix,
			lease_options: fields.lease_options,
			close_wait,
		})
	}

	/// The cron's key.
	pub fn key(&self) -> &CronKey {
		&self.key
	}

	/// Reports the current live leader, if any. The report grants nothing.
	pub async fn fetch_live_leader(
		&self,
		database: impl Database,
	) -> Result<Option<PgLeaseHolder>, PgLeaseError> {
		PgLease::new(self.native.clone(), self.holder_prefix.clone())
			.fetch_live_holder(database)
			.await
	}

	/// Runs `task_fn` on this process whenever it holds leadership, until `stop_signal` resolves or
	/// `on_task_error` returns `Stop`.
	///
	/// The cron waits for leadership with the acquire retry delays of its lease options. While
	/// it holds leadership, the cron runs the task at once and then again each time `interval`
	/// passes after the previous run returned. A stop never interrupts a running task. After it
	/// loses or releases leadership, the cron waits one jittered acquire retry interval and
	/// contends again. An attempt to acquire leadership that fails with a transient database
	/// failure (a connection failure, an unavailable server, a serialization failure, a
	/// deadlock, an unavailable lock, a canceled statement, or exhausted server resources) is
	/// retried after one jittered acquire retry interval; any other acquisition failure stops
	/// the cron with an error.
	pub fn run_until_stopped<E, StopSignal, TaskFn, TaskFuture, OnTaskError>(
		&self,
		stop_signal: StopSignal,
		task_fn: TaskFn,
		on_task_error: OnTaskError,
	) -> impl Future<Output = Result<(), PgCronRunError<E>>>
	where
		StopSignal: Future<Output = ()>,
		TaskFn: FnMut(PgLeaseTaskCtx) -> TaskFuture,
		TaskFuture: Future<Output = Result<(), E>>,
		OnTaskError: FnMut(&E) -> PgCronTaskErrorAction,
	{
		// The boxed task keeps its captured state out of the futures that run it.
		self.run_until_stopped_boxed(stop_signal, Box::new(task_fn), on_task_error)
	}

	async fn run_until_stopped_boxed<E, StopSignal, TaskFn, TaskFuture, OnTaskError>(
		&self,
		stop_signal: StopSignal,
		mut task_fn: Box<TaskFn>,
		mut on_task_error: OnTaskError,
	) -> Result<(), PgCronRunError<E>>
	where
		StopSignal: Future<Output = ()>,
		TaskFn: FnMut(PgLeaseTaskCtx) -> TaskFuture,
		TaskFuture: Future<Output = Result<(), E>>,
		OnTaskError: FnMut(&E) -> PgCronTaskErrorAction,
	{
		tokio::pin!(stop_signal);
		let owner = self.lease_options.owner_options(&self.holder_prefix);
		loop {
			let acquired = tokio::select! {
				biased;
				() = &mut stop_signal => return Ok(()),
				acquired = self.native.acquire_owned(NativeLeaseAcquireFields {
					options: &owner,
					acquisition: NativeLeaseAcquisitionChoice::Waiting(self.lease_options.waiting),
				}) => acquired,
			};
			match acquired {
				Ok(NativeLeaseAcquisition::Acquired(client)) => {
					match self
						.run_tenure(client, &mut stop_signal, &mut task_fn, &mut on_task_error)
						.await
					{
						TenureEnd::Stopped => return Ok(()),
						TenureEnd::TaskStopped { error, release } => {
							return Err(PgCronRunError::Task { error, release });
						}
						TenureEnd::Ended => {}
					}
				}
				Ok(NativeLeaseAcquisition::Denied(_)) => {
					unreachable!("a waiting acquisition returns only when it acquires the lease")
				}
				// An acquisition that met a transient database failure, or that committed after
				// its claim could no longer be used, is attempted again.
				Err(NativeLeaseAcquireError::Start { source, observer })
					if matches!(source.as_ref(), NativeLeaseDriverError::AuthorityLost)
						|| crate::db::is_transient_database_failure(source.as_ref()) =>
				{
					if let Some(observer) = observer {
						drop(observer.wait(&self.close_wait).await);
					}
				}
				Err(source) => {
					return Err(PgCronRunError::Lease(PgLeaseRunError::from_acquire(source)));
				}
			}
			let delay = generate_jittered_acquire_retry_delay(
				self.lease_options.waiting.acquire_retry_interval,
			)
			.map_err(PgCronRunError::Randomness)?;
			tokio::select! {
				() = &mut stop_signal => return Ok(()),
				() = sleep(delay) => {}
			}
		}
	}

	async fn run_tenure<E, StopSignal, TaskFn, TaskFuture, OnTaskError>(
		&self,
		mut client: NativeOwnedLeaseClient,
		stop_signal: &mut std::pin::Pin<&mut StopSignal>,
		task_fn: &mut TaskFn,
		on_task_error: &mut OnTaskError,
	) -> TenureEnd<E>
	where
		StopSignal: Future<Output = ()>,
		TaskFn: FnMut(PgLeaseTaskCtx) -> TaskFuture,
		TaskFuture: Future<Output = Result<(), E>>,
		OnTaskError: FnMut(&E) -> PgCronTaskErrorAction,
	{
		let mut loss = client.loss_observer();
		let end = loop {
			// An error means that the task did not run because leadership was lost or the lease
			// driver closed; the cron contends again.
			let Ok(outcome) = client
				.run_effect(async |context| Box::pin(task_fn(PgLeaseTaskCtx::from(context))).await)
				.await
			else {
				break TenureEnd::Ended;
			};
			if let Err(error) = outcome.output {
				match on_task_error(&error) {
					PgCronTaskErrorAction::Continue => {}
					PgCronTaskErrorAction::ReleaseLeadership => break TenureEnd::Ended,
					PgCronTaskErrorAction::Stop => {
						let release = close(client, &self.close_wait).await;
						return TenureEnd::TaskStopped { error, release };
					}
				}
			}
			if matches!(
				outcome.authority_at_completion,
				NativeLeaseAuthorityObservation::Lost { .. }
			) {
				break TenureEnd::Ended;
			}
			// An interval that reaches past the monotonic clock's range never elapses.
			let next_run = Instant::now().checked_add(self.interval);
			tokio::select! {
				() = stop_signal.as_mut() => break TenureEnd::Stopped,
				_ = loss.wait_until_lost() => break TenureEnd::Ended,
				() = async {
					match next_run {
						Some(next_run) => sleep_until(next_run).await,
						None => std::future::pending().await,
					}
				} => {}
			}
		};
		close(client, &self.close_wait).await;
		end
	}
}

enum TenureEnd<E> {
	Stopped,
	TaskStopped { error: E, release: PgLeaseRelease },
	Ended,
}

async fn close(client: NativeOwnedLeaseClient, wait: &NativeLeaseCloseWait) -> PgLeaseRelease {
	let outcome: Result<NativeLeaseCloseWaitResult, NativeLeaseCloseWaitError> =
		client.close().wait(wait).await;
	PgLeaseRelease::from_close_wait(&outcome)
}

/// What a cron does after its task returns an error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PgCronTaskErrorAction {
	/// Keep leadership and run the task again after the interval.
	Continue,
	/// Release leadership and contend for it again.
	ReleaseLeadership,
	/// Release leadership and return the task's error.
	Stop,
}

/// A cron handle that the runtime could not build.
#[derive(Debug, thiserror::Error)]
pub enum PgCronError {
	/// The interval is zero.
	#[error("the cron interval must be positive")]
	IntervalIsZero,
	/// The interval exceeds the local monotonic clock domain.
	#[error("the cron interval exceeds the local monotonic clock domain")]
	LocalClockDomain,
	/// The cron key cannot become a lease key.
	#[error("the cron key cannot become a lease key")]
	LeaseKey(#[source] PgLeaseKeyError),
}

/// A cron stopped with an error.
#[derive(Debug)]
pub enum PgCronRunError<E> {
	/// The task returned an error, and the task-error policy chose `Stop`.
	Task {
		/// The task's error.
		error: E,
		/// Whether the cron confirmed the release of leadership.
		release: PgLeaseRelease,
	},
	/// Acquiring leadership failed with a failure that the cron does not retry.
	Lease(PgLeaseRunError),
	/// The local random source failed while the cron computed a retry delay.
	Randomness(RandomnessError),
}

impl<E> fmt::Display for PgCronRunError<E> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Task { .. } => formatter.write_str("the cron task failed"),
			Self::Lease(_) => formatter.write_str("the cron could not acquire leadership"),
			Self::Randomness(_) => formatter.write_str("the cron retry delay failed"),
		}
	}
}

impl<E: Error + 'static> Error for PgCronRunError<E> {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		match self {
			Self::Task { error, .. } => Some(error),
			Self::Lease(source) => Some(source),
			Self::Randomness(source) => Some(source),
		}
	}
}
