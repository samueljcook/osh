//! Semaphore failures, with their complete cause chains. Formatting omits keys and scope data.

mod cause;

use crate::db::{DatabaseError, TransactionFailure};
use crate::fleet::RandomnessError;
use crate::fleet::semaphore::engine::NativeSemaphoreError;
use crate::runtime::semaphore::NativeRuntimeSemaphoreHandleError;
use std::{error::Error, fmt, sync::Arc};

enum Cause {
	Construction(NativeRuntimeSemaphoreHandleError),
	Native(Arc<NativeSemaphoreError>),
	Randomness(RandomnessError),
}

/// A failed Semaphore operation, with its complete cause chain.
pub struct PgSemaphoreError(Box<Cause>);

/// The class of a Semaphore failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgSemaphoreErrorKind {
	/// The runtime declares no Semaphore code for the scope, the scope declares another
	/// family, or the installation does not select the scope.
	Scope,
	/// The stored Semaphore has other settings than the handle's.
	SettingsMismatch,
	/// The acquire retry interval is zero or exceeds the local monotonic clock domain.
	InvalidTiming,
	/// A database statement or the transaction's own control failed. The error's source chain
	/// holds the database error.
	Database,
	/// Paranoid's own work on the Semaphore exceeded its time limit.
	Timeout,
	/// The runtime stopped accepting work.
	RuntimeStopped,
	/// The runtime cannot operate on the Semaphore under the installation's recorded policy.
	Admission,
	/// The stored Semaphore could not be interpreted.
	State,
	/// A local failure that the app cannot correct, such as a failed random source.
	Internal,
}

impl PgSemaphoreError {
	pub(super) fn construction(source: NativeRuntimeSemaphoreHandleError) -> Self {
		Self(Box::new(Cause::Construction(source)))
	}
	pub(super) fn operation(source: NativeSemaphoreError) -> Self {
		Self::shared(Arc::new(source))
	}
	pub(super) fn shared(source: Arc<NativeSemaphoreError>) -> Self {
		Self(Box::new(Cause::Native(source)))
	}
	pub(super) fn randomness(source: RandomnessError) -> Self {
		Self(Box::new(Cause::Randomness(source)))
	}

	fn native(&self) -> Option<&NativeSemaphoreError> {
		match self.0.as_ref() {
			Cause::Native(source) => Some(source),
			Cause::Construction(NativeRuntimeSemaphoreHandleError::Semaphore(source)) => {
				Some(source)
			}
			Cause::Construction(_) | Cause::Randomness(_) => None,
		}
	}

	/// The class of the failure. When any database work of the operation failed, including a
	/// cleanup or the rollback after another failure, the class is `Database`, even when the
	/// original failure has another class; for example, a `SettingsMismatch` followed by a failed
	/// rollback reports `Database`.
	pub fn kind(&self) -> PgSemaphoreErrorKind {
		match self.0.as_ref() {
			Cause::Construction(
				NativeRuntimeSemaphoreHandleError::ScopeCode(_)
				| NativeRuntimeSemaphoreHandleError::WrongFamily { .. },
			) => PgSemaphoreErrorKind::Scope,
			Cause::Randomness(_) => PgSemaphoreErrorKind::Internal,
			Cause::Native(source) => cause::kind(source),
			Cause::Construction(NativeRuntimeSemaphoreHandleError::Semaphore(source)) => {
				cause::kind(source)
			}
		}
	}

	/// The database error, when a database statement or the transaction's own control failed.
	pub fn database_error(&self) -> Option<&DatabaseError> {
		self.native().and_then(cause::database_error)
	}

	/// Whether a transaction's own `COMMIT` failed, which means that its change may or may not
	/// have committed.
	pub fn commit_outcome_is_unknown(&self) -> bool {
		self.native()
			.is_some_and(TransactionFailure::commit_outcome_is_unknown)
	}
}

impl fmt::Debug for PgSemaphoreError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "PgSemaphoreError({:?}): {self}", self.kind())?;
		let mut source = self.source();
		while let Some(error) = source {
			write!(f, ": {error}")?;
			source = error.source();
		}
		Ok(())
	}
}
impl fmt::Display for PgSemaphoreError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("Semaphore operation failed")
	}
}
impl Error for PgSemaphoreError {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		Some(match self.0.as_ref() {
			Cause::Construction(source) => source,
			Cause::Native(source) => source.as_ref(),
			Cause::Randomness(source) => source,
		})
	}
}
