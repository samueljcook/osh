//! Public Once operations of the native runtime.

use crate::db::{Database, TransactionFailure, WritePool};
use crate::fleet::lease::handle::{PgLeaseRelease, PgLeaseRunOptions, PgLeaseTaskCtx};
use crate::fleet::lease::owned_driver::NativeLeaseDriverError;
use crate::fleet::lease::owned_driver::close::NativeLeaseCloseWait;
use crate::fleet::lease::owned_driver::loss::NativeLeaseCallerCtx;
use crate::fleet::lease::owned_driver::once_completion::NativeOncePreparedCompletion;
use crate::fleet::lease::owned_driver::once_completion::error::NativeOnceCompletionError;
use crate::fleet::lease::owned_driver::once_pair::NativeOnceCompletionSnapshot;
use crate::fleet::lease::owned_driver::options::NativeLeaseAcquisitionChoice;
use crate::fleet::once::engine::managed::NativeOnceManagedFields;
use crate::fleet::once::engine::managed::error::NativeOnceManagedError;
use crate::fleet::once::engine::managed::options::{
	NativeOnceManagedOptions, NativeOnceManagedOptionsFields,
};
use crate::fleet::once::engine::managed::result::{
	NativeOnceCompletionNotRequested, NativeOnceManagedCompletion, NativeOnceManagedResult,
};
use crate::fleet::once::engine::{NativeOnceConstructionError, NativeOnceError, PgNativeOnce};
use crate::fleet::once::state::NativeOnceCompletion;
use crate::fleet::{FencingToken, HolderId, HolderPrefix, OnceKey};
use std::error::Error;
use std::fmt;
use std::sync::Arc;

/// The inputs of a Once handle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PgOnceFields {
	/// The Once's key.
	pub key: OnceKey,
	/// The lease options of every run of the Once.
	pub lease_options: PgLeaseRunOptions,
}

/// One task of the runtime's installation that runs to a recorded completion at most once.
///
/// A run holds the Once's lease while the task runs and renews the lease automatically. After
/// the task succeeds, the run records the completion and releases the lease in one
/// transaction, but only while the run's claim still holds the lease. A recorded completion is
/// permanent: every later run reports it and does not run the task.
///
/// A task can run more than once: when a run's claim ends before the run records the
/// completion, or when recording it fails, a later run runs the task again. Make the task's
/// effects safe to repeat. On an installation that imported v10, every operation fails with
/// [`PgOnceErrorKind::Admission`] until
/// [`PgRuntime::retire_v10`](crate::runtime::PgRuntime::retire_v10) has finished.
pub struct PgOnce {
	native: Arc<PgNativeOnce>,
	pool: WritePool,
	holder_prefix: HolderPrefix,
	lease_options: PgLeaseRunOptions,
	close_wait: NativeLeaseCloseWait,
}

pub(crate) struct PgOnceParts {
	pub(crate) native: PgNativeOnce,
	pub(crate) pool: WritePool,
	pub(crate) holder_prefix: HolderPrefix,
	pub(crate) lease_options: PgLeaseRunOptions,
}

impl PgOnce {
	pub(crate) fn new(parts: PgOnceParts) -> Self {
		let close_wait = parts.native.lease().close_wait();
		Self {
			native: Arc::new(parts.native),
			pool: parts.pool,
			holder_prefix: parts.holder_prefix,
			lease_options: parts.lease_options,
			close_wait,
		}
	}

	/// The Once's key.
	pub fn key(&self) -> &OnceKey {
		self.native.key()
	}

	/// Reports the recorded completion, if any. The report grants nothing.
	pub async fn fetch_completion(
		&self,
		database: impl Database,
	) -> Result<Option<PgOnceCompletion>, PgOnceError> {
		self.native
			.fetch_completion(database)
			.await
			.map(|completion| completion.as_ref().map(PgOnceCompletion::from_record))
			.map_err(|error| PgOnceError::new(PgOnceErrorCause::Read(error)))
	}

	/// Runs `task_fn` unless the Once already has a recorded completion, waiting while another
	/// run holds the Once's lease, and records the completion when the task succeeds.
	///
	/// An error means that the task did not run. When the task ran, the result holds the task's
	/// own output, whether the completion was recorded, and whether the lease release was
	/// confirmed.
	pub fn run_task<T, E, TaskFn>(
		&self,
		task_fn: TaskFn,
	) -> impl Future<Output = Result<PgOnceRunTaskResult<T, E>, PgOnceError>>
	where
		TaskFn: AsyncFnOnce(PgLeaseTaskCtx) -> Result<T, E>,
	{
		// The boxed task keeps its captured state out of the futures that run it.
		self.run_task_boxed(Box::new(task_fn))
	}

	async fn run_task_boxed<T, E, TaskFn>(
		&self,
		task_fn: Box<TaskFn>,
	) -> Result<PgOnceRunTaskResult<T, E>, PgOnceError>
	where
		TaskFn: AsyncFnOnce(PgLeaseTaskCtx) -> Result<T, E>,
	{
		let choice = NativeLeaseAcquisitionChoice::Waiting(self.lease_options.waiting);
		Ok(match self.run(choice, task_fn).await? {
			PgOnceTryRunTaskResult::Ran(ran) => PgOnceRunTaskResult::Ran(ran),
			PgOnceTryRunTaskResult::AlreadyDone(completion) => {
				PgOnceRunTaskResult::AlreadyDone(completion)
			}
			PgOnceTryRunTaskResult::AlreadyRunning => {
				unreachable!("a waiting run returns only after it acquires the lease")
			}
		})
	}

	/// Runs `task_fn` unless the Once already has a recorded completion or another run holds
	/// the Once's lease or contends for its v10 acquisition locks. The run records the
	/// completion when the task succeeds.
	///
	/// An error means that the task did not run. When the task ran, the result holds the task's
	/// own output, whether the completion was recorded, and whether the lease release was
	/// confirmed.
	pub fn try_run_task<T, E, TaskFn>(
		&self,
		task_fn: TaskFn,
	) -> impl Future<Output = Result<PgOnceTryRunTaskResult<T, E>, PgOnceError>>
	where
		TaskFn: AsyncFnOnce(PgLeaseTaskCtx) -> Result<T, E>,
	{
		// The boxed task keeps its captured state out of the futures that run it.
		self.try_run_task_boxed(Box::new(task_fn))
	}

	async fn try_run_task_boxed<T, E, TaskFn>(
		&self,
		task_fn: Box<TaskFn>,
	) -> Result<PgOnceTryRunTaskResult<T, E>, PgOnceError>
	where
		TaskFn: AsyncFnOnce(PgLeaseTaskCtx) -> Result<T, E>,
	{
		self.run(NativeLeaseAcquisitionChoice::TryOnce, task_fn)
			.await
	}

	async fn run<T, E, TaskFn>(
		&self,
		acquisition: NativeLeaseAcquisitionChoice,
		task_fn: TaskFn,
	) -> Result<PgOnceTryRunTaskResult<T, E>, PgOnceError>
	where
		TaskFn: AsyncFnOnce(PgLeaseTaskCtx) -> Result<T, E>,
	{
		let options = NativeOnceManagedOptions::new(NativeOnceManagedOptionsFields {
			lease: self.lease_options.owner_options(&self.holder_prefix),
			acquisition,
			close_wait: self.close_wait.clone(),
		});
		let result = self
			.native
			.run_task(NativeOnceManagedFields {
				pool: &self.pool,
				options: &options,
				task_fn: async move |ctx: NativeLeaseCallerCtx| {
					Box::pin(task_fn(PgLeaseTaskCtx::from(ctx))).await
				},
			})
			.await
			.map_err(|error| PgOnceError::new(PgOnceErrorCause::Start(error)))?;
		Ok(match result {
			NativeOnceManagedResult::AlreadyDone { completion, .. } => {
				PgOnceTryRunTaskResult::AlreadyDone(PgOnceCompletion::from_record(&completion))
			}
			NativeOnceManagedResult::AlreadyRunning => PgOnceTryRunTaskResult::AlreadyRunning,
			NativeOnceManagedResult::Ran(ran) => PgOnceTryRunTaskResult::Ran(PgOnceTaskResult {
				task: ran.task,
				completion: PgOnceCompletionResult::from_native(ran.completion),
				release: PgLeaseRelease::from_close_wait(&ran.release.wait),
			}),
		})
	}
}

/// The result of a Once run that waits.
#[derive(Debug)]
pub enum PgOnceRunTaskResult<T, E> {
	/// This run ran the task.
	Ran(PgOnceTaskResult<T, E>),
	/// The Once already had a recorded completion, so the run did not run the task.
	AlreadyDone(PgOnceCompletion),
}

/// The result of a Once run that does not wait.
#[derive(Debug)]
pub enum PgOnceTryRunTaskResult<T, E> {
	/// This run ran the task.
	Ran(PgOnceTaskResult<T, E>),
	/// The Once already had a recorded completion, so the run did not run the task.
	AlreadyDone(PgOnceCompletion),
	/// Another run held the Once's lease or contended for its v10 acquisition locks, so the
	/// task did not run.
	AlreadyRunning,
}

/// What a Once run reports about a task that it ran.
#[derive(Debug)]
#[non_exhaustive]
pub struct PgOnceTaskResult<T, E> {
	/// The task's own output.
	pub task: Result<T, E>,
	/// Whether the run recorded the completion.
	pub completion: PgOnceCompletionResult,
	/// Whether the run confirmed that the lease was released.
	pub release: PgLeaseRelease,
}

/// Whether a run recorded the Once's completion after its task returned.
#[derive(Debug)]
pub enum PgOnceCompletionResult {
	/// The run recorded this completion.
	Recorded(PgOnceCompletion),
	/// A completion was already recorded when the run tried to record its own.
	AlreadyRecorded(PgOnceCompletion),
	/// The task failed, so the run did not record a completion, and the task can run again.
	TaskFailed,
	/// The run no longer held the lease, so it did not record a completion, and the task can
	/// run again.
	LeadershipLost,
	/// Recording the completion failed. When the error's `commit_outcome_is_unknown` is true,
	/// the completion may have been recorded; otherwise the task can run again.
	Failed(PgOnceError),
}

impl PgOnceCompletionResult {
	fn from_native(completion: NativeOnceManagedCompletion) -> Self {
		match completion {
			NativeOnceManagedCompletion::NotRequested(
				NativeOnceCompletionNotRequested::AppError,
			) => Self::TaskFailed,
			NativeOnceManagedCompletion::NotRequested(
				NativeOnceCompletionNotRequested::AuthorityLost,
			) => Self::LeadershipLost,
			NativeOnceManagedCompletion::Committed(prepared) => match prepared {
				NativeOncePreparedCompletion::Recorded(completion) => {
					Self::Recorded(PgOnceCompletion::from_snapshot(&completion))
				}
				NativeOncePreparedCompletion::AlreadyRecorded(completion) => {
					Self::AlreadyRecorded(PgOnceCompletion::from_snapshot(&completion))
				}
				NativeOncePreparedCompletion::Refused => Self::LeadershipLost,
			},
			NativeOnceManagedCompletion::HandoffFailed(source) => Self::Failed(PgOnceError::new(
				PgOnceErrorCause::CompletionHandoff(source),
			)),
			NativeOnceManagedCompletion::Failed(source) => {
				Self::Failed(PgOnceError::new(PgOnceErrorCause::Completion(source)))
			}
		}
	}
}

/// A recorded completion of a Once.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PgOnceCompletion {
	finished_at_unix_micros: i64,
	holder_id: HolderId,
	fencing_token: FencingToken,
}

impl PgOnceCompletion {
	fn from_record(completion: &NativeOnceCompletion) -> Self {
		Self {
			finished_at_unix_micros: completion.finished_at().as_i64(),
			holder_id: completion.holder().clone(),
			fencing_token: completion_fencing_token(completion.fencing_token()),
		}
	}

	fn from_snapshot(snapshot: &NativeOnceCompletionSnapshot) -> Self {
		Self {
			finished_at_unix_micros: snapshot.finished_at.as_i64(),
			holder_id: snapshot.holder.clone(),
			fencing_token: completion_fencing_token(snapshot.fencing_token),
		}
	}

	/// When the completion was recorded, in Unix microseconds by the database clock.
	pub fn finished_at_unix_micros(&self) -> i64 {
		self.finished_at_unix_micros
	}

	/// The holder label of the run that recorded the completion.
	pub fn holder_id(&self) -> &HolderId {
		&self.holder_id
	}

	/// The fencing token of the claim under which the completion was recorded.
	pub fn fencing_token(&self) -> FencingToken {
		self.fencing_token
	}
}

fn completion_fencing_token(value: i64) -> FencingToken {
	FencingToken::from_i64(value).expect("a decoded Once completion has a positive fencing token")
}

enum PgOnceErrorCause {
	Construction(NativeOnceConstructionError),
	Read(NativeOnceError),
	Start(NativeOnceManagedError),
	CompletionHandoff(Arc<NativeLeaseDriverError>),
	Completion(NativeOnceCompletionError),
}

/// The class of a failed Once operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgOnceErrorKind {
	/// A database statement or the transaction's own control failed. The error's source chain
	/// holds the database error.
	Database,
	/// Paranoid's own work on the Once's lease exceeded its time limit.
	Timeout,
	/// The runtime stopped accepting work.
	RuntimeStopped,
	/// The run lost the Once's lease. In an error that `run_task` or `try_run_task` returns,
	/// the lease was lost before the task started; in `PgOnceCompletionResult::Failed`, it was
	/// lost after the task returned.
	LeadershipLost,
	/// The runtime cannot operate on the Once under the installation's recorded policy.
	Admission,
	/// The stored Once could not be interpreted or changed.
	State,
	/// A local failure that the app cannot correct, such as a failed random source.
	Internal,
}

/// A failed Once operation, with its complete cause chain.
pub struct PgOnceError {
	source: Box<PgOnceErrorCause>,
}

impl PgOnceError {
	fn new(source: PgOnceErrorCause) -> Self {
		Self {
			source: Box::new(source),
		}
	}

	pub(crate) fn construction(source: NativeOnceConstructionError) -> Self {
		Self::new(PgOnceErrorCause::Construction(source))
	}

	/// The class of the failure.
	pub fn kind(&self) -> PgOnceErrorKind {
		use crate::fleet::lease::handle::HandleFailureClass as Class;
		if let PgOnceErrorCause::Start(source) = self.source.as_ref()
			&& matches!(
				source.typed_source(),
				crate::fleet::once::engine::managed::error::NativeOnceManagedCause::AuthorityLost
			) {
			return PgOnceErrorKind::LeadershipLost;
		}
		match crate::fleet::lease::handle::classify_handle_failure(self.source_error()) {
			Class::Database => PgOnceErrorKind::Database,
			Class::Timeout => PgOnceErrorKind::Timeout,
			Class::RuntimeStopped => PgOnceErrorKind::RuntimeStopped,
			Class::LeadershipLost => PgOnceErrorKind::LeadershipLost,
			Class::Admission => PgOnceErrorKind::Admission,
			Class::State => PgOnceErrorKind::State,
			Class::Internal => PgOnceErrorKind::Internal,
		}
	}

	/// Whether the completion transaction's own `COMMIT` failed, which means that the
	/// completion may or may not have been recorded.
	pub fn commit_outcome_is_unknown(&self) -> bool {
		match self.source.as_ref() {
			PgOnceErrorCause::Completion(source) => source.commit_outcome_is_unknown(),
			_ => false,
		}
	}

	fn source_error(&self) -> &(dyn Error + 'static) {
		match self.source.as_ref() {
			PgOnceErrorCause::Construction(source) => source,
			PgOnceErrorCause::Read(source) => source,
			PgOnceErrorCause::Start(source) => source,
			PgOnceErrorCause::CompletionHandoff(source) => source.as_ref(),
			PgOnceErrorCause::Completion(source) => source,
		}
	}
}

impl fmt::Display for PgOnceError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("Once operation failed")
	}
}

impl fmt::Debug for PgOnceError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(formatter, "PgOnceError: {self}")?;
		let mut source = self.source();
		while let Some(error) = source {
			write!(formatter, ": {error}")?;
			source = error.source();
		}
		Ok(())
	}
}

impl Error for PgOnceError {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		Some(self.source_error())
	}
}
