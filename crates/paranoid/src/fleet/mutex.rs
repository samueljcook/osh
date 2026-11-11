//! Public mutex operations of the native runtime.

use crate::db::Database;
use crate::fleet::lease::PgNativeLease;
use crate::fleet::lease::handle::{
	PgLeaseHolder, PgLeaseRelease, PgLeaseRunCause, PgLeaseRunError, PgLeaseRunOptions,
	PgLeaseTaskCtx,
};
use crate::fleet::lease::owned_driver::NativeOwnedLeaseClient;
use crate::fleet::lease::owned_driver::acquire::{
	NativeLeaseAcquireFields, NativeLeaseAcquisition,
};
use crate::fleet::lease::owned_driver::close::NativeLeaseCloseWait;
use crate::fleet::lease::owned_driver::loss::NativeLeaseAuthorityObservation;
use crate::fleet::lease::owned_driver::options::{
	NativeLeaseAcquisitionChoice, NativeLeaseOwnerOptions,
};
use crate::fleet::{HolderPrefix, MutexKey, PgLease, PgLeaseError};

/// The inputs of a mutex handle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PgMutexFields {
	/// The mutex's key.
	pub key: MutexKey,
	/// The lease options of every run of the mutex.
	pub lease_options: PgLeaseRunOptions,
}

/// One mutex of the runtime's installation: at most one run holds it at a time.
///
/// A run holds the mutex's lease while its task runs and renews the lease automatically. On an
/// installation imported from v10, the mutex uses the lease key that published v10 uses for the
/// same mutex key, so a run of this handle and a v10 holder of that key exclude each other while
/// both releases run.
///
/// A run does not stop its task when its claim ends, for example after failed renewals, so
/// another run can hold the mutex while the task still runs. Make the task safe to overlap with
/// the next holder, and send the fencing token of [`PgLeaseTaskCtx::holder`] to an external
/// resource that must reject an older holder.
pub struct PgMutex {
	key: MutexKey,
	native: PgNativeLease,
	holder_prefix: HolderPrefix,
	lease_options: PgLeaseRunOptions,
	close_wait: NativeLeaseCloseWait,
}

impl PgMutex {
	pub(crate) fn new(
		fields: PgMutexFields,
		native: PgNativeLease,
		holder_prefix: HolderPrefix,
	) -> Self {
		let close_wait = native.close_wait();
		Self {
			key: fields.key,
			native,
			holder_prefix,
			lease_options: fields.lease_options,
			close_wait,
		}
	}

	/// The mutex's key.
	pub fn key(&self) -> &MutexKey {
		&self.key
	}

	/// Reports the current live holder, if any. The report grants nothing.
	pub async fn fetch_live_holder(
		&self,
		database: impl Database,
	) -> Result<Option<PgLeaseHolder>, PgLeaseError> {
		PgLease::new(self.native.clone(), self.holder_prefix.clone())
			.fetch_live_holder(database)
			.await
	}

	/// Waits until this run holds the mutex, runs `task_fn`, and releases the mutex.
	///
	/// An error means that the task did not run. When the task ran, the result holds the task's
	/// own output, whether the run held the mutex until the task returned, and whether the
	/// release was confirmed.
	pub fn run_task<T, E, TaskFn>(
		&self,
		task_fn: TaskFn,
	) -> impl Future<Output = Result<PgMutexTaskResult<T, E>, PgLeaseRunError>>
	where
		TaskFn: AsyncFnOnce(PgLeaseTaskCtx) -> Result<T, E>,
	{
		// The boxed task keeps its captured state out of the futures that run it.
		self.run_task_boxed(Box::new(task_fn))
	}

	async fn run_task_boxed<T, E, TaskFn>(
		&self,
		task_fn: Box<TaskFn>,
	) -> Result<PgMutexTaskResult<T, E>, PgLeaseRunError>
	where
		TaskFn: AsyncFnOnce(PgLeaseTaskCtx) -> Result<T, E>,
	{
		let owner = self.owner_options();
		let acquisition = self
			.native
			.acquire_owned(NativeLeaseAcquireFields {
				options: &owner,
				acquisition: NativeLeaseAcquisitionChoice::Waiting(self.lease_options.waiting),
			})
			.await
			.map_err(PgLeaseRunError::from_acquire)?;
		let NativeLeaseAcquisition::Acquired(client) = acquisition else {
			unreachable!("a waiting acquisition returns only when it acquires the lease")
		};
		self.run_acquired(client, task_fn).await
	}

	/// Runs `task_fn` when no other run holds the mutex, and releases the mutex.
	///
	/// The acquisitions of one mutex in this runtime run one at a time, in the order in which
	/// their tasks reach the mutex's local queue. Task scheduling can differ from call order.
	/// This wait has no limit of its own; dropping the returned future ends it without running
	/// the task.
	///
	/// An error means that the task did not run. `MutexHeld` means that another run held the
	/// mutex or contended for its v10 acquisition locks, so the task did not run.
	pub fn try_run_task<T, E, TaskFn>(
		&self,
		task_fn: TaskFn,
	) -> impl Future<Output = Result<PgMutexTryRunTaskResult<T, E>, PgLeaseRunError>>
	where
		TaskFn: AsyncFnOnce(PgLeaseTaskCtx) -> Result<T, E>,
	{
		// The boxed task keeps its captured state out of the futures that run it.
		self.try_run_task_boxed(Box::new(task_fn))
	}

	async fn try_run_task_boxed<T, E, TaskFn>(
		&self,
		task_fn: Box<TaskFn>,
	) -> Result<PgMutexTryRunTaskResult<T, E>, PgLeaseRunError>
	where
		TaskFn: AsyncFnOnce(PgLeaseTaskCtx) -> Result<T, E>,
	{
		let owner = self.owner_options();
		let acquisition = self
			.native
			.acquire_owned(NativeLeaseAcquireFields {
				options: &owner,
				acquisition: NativeLeaseAcquisitionChoice::TryOnce,
			})
			.await
			.map_err(PgLeaseRunError::from_acquire)?;
		match acquisition {
			NativeLeaseAcquisition::Acquired(client) => self
				.run_acquired(client, task_fn)
				.await
				.map(PgMutexTryRunTaskResult::Ran),
			NativeLeaseAcquisition::Denied(observer) => {
				// The denied attempt's driver settles within the release wait; its outcome
				// changes nothing for the caller.
				drop(observer.wait(&self.close_wait).await);
				Ok(PgMutexTryRunTaskResult::MutexHeld)
			}
		}
	}

	fn owner_options(&self) -> NativeLeaseOwnerOptions {
		self.lease_options.owner_options(&self.holder_prefix)
	}

	async fn run_acquired<T, E, TaskFn>(
		&self,
		mut client: NativeOwnedLeaseClient,
		task_fn: TaskFn,
	) -> Result<PgMutexTaskResult<T, E>, PgLeaseRunError>
	where
		TaskFn: AsyncFnOnce(PgLeaseTaskCtx) -> Result<T, E>,
	{
		let outcome = client
			.run_effect(async |context| Box::pin(task_fn(PgLeaseTaskCtx::from(context))).await)
			.await;
		let release = client.close().wait(&self.close_wait).await;
		let outcome =
			outcome.map_err(|error| PgLeaseRunError::new(PgLeaseRunCause::Caller(error)))?;
		Ok(PgMutexTaskResult {
			task: outcome.output,
			leadership: leadership_at_completion(&outcome.authority_at_completion),
			release: PgLeaseRelease::from_close_wait(&release),
		})
	}
}

/// Whether a run held its lease until its task returned.
fn leadership_at_completion(observation: &NativeLeaseAuthorityObservation) -> MutexLeadership {
	match observation {
		NativeLeaseAuthorityObservation::ConfirmedLocally { .. } => MutexLeadership::HeldThroughout,
		NativeLeaseAuthorityObservation::Lost { .. } => MutexLeadership::Lost,
	}
}

/// The result of a mutex run that does not wait.
#[derive(Debug)]
pub enum PgMutexTryRunTaskResult<T, E> {
	/// This run held the mutex and ran the task.
	Ran(PgMutexTaskResult<T, E>),
	/// Another run held the mutex or contended for its v10 acquisition locks, so the task did
	/// not run.
	MutexHeld,
}

/// What a mutex run reports about a task that it ran.
#[derive(Debug)]
#[non_exhaustive]
pub struct PgMutexTaskResult<T, E> {
	/// The task's own output.
	pub task: Result<T, E>,
	/// Whether the run held the mutex until the task returned. `Lost` means that the run lost
	/// the mutex before the task returned, as [`MutexLeadership::Lost`] describes.
	pub leadership: MutexLeadership,
	/// Whether the run confirmed that the mutex was released.
	pub release: PgLeaseRelease,
}

/// Whether a run held its lease from the start of its task until the task returned.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutexLeadership {
	/// The run's claim stayed live until the task returned.
	HeldThroughout,
	/// The run lost the lease before the task returned because its claim could not be renewed
	/// or passed its bound, or because its runtime stopped. Another holder may have run
	/// concurrently with the task once the task ran past the `usable_until` of its last
	/// [`PgLeadership::Held`](crate::fleet::PgLeadership::Held) report.
	Lost,
}
