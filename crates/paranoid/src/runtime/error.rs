//! Failures to start a runtime.

use std::{error::Error, fmt, sync::Arc};

use super::lifecycle::{
	PgRuntimeRemovalResult, PgRuntimeShutdown, PgRuntimeShutdownResult,
	PgRuntimeShutdownWaitErrorKind, PgRuntimeSnapshot, removal_error, removal_outcome,
	removal_outcome_is_unknown, shutdown_commit_outcome_is_unknown,
};
use crate::db::{DatabaseError, TransactionFailure};
use crate::runtime::NativeRuntimeStartupError;
use crate::runtime::participation::{
	NativeParticipationShutdownWaitError, NativeParticipationStartupError,
	commit_outcome_is_unknown,
};

/// A runtime that could not start.
///
/// When registration as a participant timed out, the registration may or may not have
/// committed, and there is no shutdown handle; an uncommitted or orphaned registration expires
/// on its own. When the registration's lifetime ended before startup finished, the runtime
/// removed its registration, and `registration_removal` reports the result. When the runtime's
/// check of the installation through the app pool failed after registration, the runtime shut
/// itself down, and `shutdown` returns the handle of that shutdown.
pub struct PgRuntimeStartError {
	source: Box<NativeRuntimeStartupError>,
}

impl PgRuntimeStartError {
	/// The class of the failure.
	pub fn kind(&self) -> PgRuntimeStartErrorKind {
		match self.source.as_ref() {
			NativeRuntimeStartupError::SharedConnectionCapacity => {
				PgRuntimeStartErrorKind::SharedConnectionCapacity
			}
			NativeRuntimeStartupError::Administration(_) => PgRuntimeStartErrorKind::Administration,
			NativeRuntimeStartupError::SourcePreparation(_) => PgRuntimeStartErrorKind::V10Access,
			NativeRuntimeStartupError::ScopeAddition(_) => PgRuntimeStartErrorKind::ScopeAddition,
			NativeRuntimeStartupError::Participation(_) => PgRuntimeStartErrorKind::Participation,
			NativeRuntimeStartupError::AppAdmission { .. } => PgRuntimeStartErrorKind::AppAdmission,
		}
	}

	pub(super) fn new(source: NativeRuntimeStartupError) -> Self {
		Self {
			source: Box::new(source),
		}
	}

	/// The shutdown that the runtime started after a failure that followed its registration,
	/// when the kind is `AppAdmission`.
	pub fn shutdown(&self) -> Option<PgRuntimeShutdown> {
		match self.source.as_ref() {
			NativeRuntimeStartupError::AppAdmission { shutdown, .. } => Some(PgRuntimeShutdown {
				shutdown: shutdown.clone(),
			}),
			_ => None,
		}
	}

	/// The outcome of that shutdown, when it finished within Paranoid's startup cleanup wait.
	pub fn cleanup_result(&self) -> Option<PgRuntimeShutdownResult> {
		match self.source.as_ref() {
			NativeRuntimeStartupError::AppAdmission {
				cleanup: Ok(outcome),
				..
			} => Some(PgRuntimeShutdownResult(Arc::clone(outcome))),
			_ => None,
		}
	}

	/// Why the startup did not report the shutdown's outcome, when the shutdown did not finish
	/// within Paranoid's startup cleanup wait.
	pub fn cleanup_wait_kind(&self) -> Option<PgRuntimeShutdownWaitErrorKind> {
		let NativeRuntimeStartupError::AppAdmission {
			cleanup: Err(source),
			..
		} = self.source.as_ref()
		else {
			return None;
		};
		Some(match source.as_ref() {
			NativeParticipationShutdownWaitError::InvalidWait => {
				PgRuntimeShutdownWaitErrorKind::InvalidWait
			}
			NativeParticipationShutdownWaitError::Pending { .. } => {
				PgRuntimeShutdownWaitErrorKind::Pending
			}
		})
	}

	/// The runtime's state when the startup cleanup wait elapsed. `shutdown` observes the
	/// shutdown afterwards.
	pub fn cleanup_pending_snapshot(&self) -> Option<PgRuntimeSnapshot> {
		match self.source.as_ref() {
			NativeRuntimeStartupError::AppAdmission {
				cleanup: Err(source),
				..
			} => match source.as_ref() {
				NativeParticipationShutdownWaitError::Pending { snapshot } => {
					Some(PgRuntimeSnapshot(snapshot.as_ref().clone()))
				}
				_ => None,
			},
			_ => None,
		}
	}

	/// The result of removing the runtime from the installation's participants, when the
	/// registration expired before startup finished.
	pub fn registration_removal(&self) -> Option<PgRuntimeRemovalResult> {
		match self.source.as_ref() {
			NativeRuntimeStartupError::Participation(
				NativeParticipationStartupError::ConfirmedLifetimeElapsed { removal },
			) => Some(removal_outcome(removal)),
			_ => None,
		}
	}

	/// The failure of that removal, if it failed.
	pub fn registration_removal_error(&self) -> Option<&(dyn Error + 'static)> {
		match self.source.as_ref() {
			NativeRuntimeStartupError::Participation(
				NativeParticipationStartupError::ConfirmedLifetimeElapsed { removal },
			) => removal_error(removal),
			_ => None,
		}
	}

	/// The database error, when a database statement or a transaction's own control failed.
	/// When both a statement and the following rollback failed, this is the statement's
	/// error. Failures of the cleanup are reported by the cleanup accessors.
	pub fn database_error(&self) -> Option<&DatabaseError> {
		match self.source.as_ref() {
			NativeRuntimeStartupError::SourcePreparation(source) => source.database_error(),
			NativeRuntimeStartupError::ScopeAddition(source) => source.database_error(),
			NativeRuntimeStartupError::Participation(
				NativeParticipationStartupError::Registration(source),
			) => source.database_error(),
			NativeRuntimeStartupError::AppAdmission { operation, .. } => operation.database_error(),
			_ => None,
		}
	}

	/// Whether the result of a database change is unknown: a `COMMIT` result was lost, the
	/// registration timed out, or the cleanup did not finish.
	pub fn commit_outcome_is_unknown(&self) -> bool {
		match self.source.as_ref() {
			NativeRuntimeStartupError::SourcePreparation(source) => {
				source.commit_outcome_is_unknown()
			}
			NativeRuntimeStartupError::ScopeAddition(source) => source.commit_outcome_is_unknown(),
			NativeRuntimeStartupError::Participation(source) => match source {
				NativeParticipationStartupError::Registration(source) => {
					commit_outcome_is_unknown(source)
				}
				NativeParticipationStartupError::RegistrationTimedOut => true,
				NativeParticipationStartupError::ConfirmedLifetimeElapsed { removal } => {
					removal_outcome_is_unknown(removal)
				}
				_ => false,
			},
			NativeRuntimeStartupError::AppAdmission {
				operation, cleanup, ..
			} => {
				operation.commit_outcome_is_unknown()
					|| match cleanup {
						Ok(outcome) => shutdown_commit_outcome_is_unknown(outcome),
						Err(source) => matches!(
							source.as_ref(),
							NativeParticipationShutdownWaitError::Pending { .. }
						),
					}
			}
			_ => false,
		}
	}
}

impl fmt::Debug for PgRuntimeStartError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("PgRuntimeStartError")
			.field("kind", &self.kind())
			.finish_non_exhaustive()
	}
}
impl fmt::Display for PgRuntimeStartError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("PostgreSQL runtime startup failed")
	}
}
impl Error for PgRuntimeStartError {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		Some(self.source.as_ref())
	}
}

/// The class of a runtime startup failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgRuntimeStartErrorKind {
	/// The app pool and the control pool share connections. They must be separate pools.
	SharedConnectionCapacity,
	/// The installation's administration records do not match this runtime's code.
	Administration,
	/// Preparing the runtime's access to the v10 installation failed.
	V10Access,
	/// Installing a declared scope that the installation does not have failed, before the
	/// runtime registered. The scope's namespace holds a record, a running participant is not
	/// compatible with the scope, or a database statement failed.
	ScopeAddition,
	/// `start` did not run within a Tokio runtime, registering the runtime as a participant
	/// failed, or the registration expired before startup finished.
	Participation,
	/// After the runtime registered, its check of the installation through the app pool failed:
	/// a table does not match, an installed policy does not bind this runtime's code, the
	/// runtime's registration could not be confirmed, a statement failed, or the runtime stopped
	/// admitting operations. The runtime shut itself down; `shutdown` returns that shutdown.
	AppAdmission,
}
