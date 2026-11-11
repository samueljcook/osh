//! Identified Once association and read failures with redacted default formatting.

use std::fmt;

use crate::db::{DatabaseError, TransactionFailure};
use crate::fleet::OnceKey;
use crate::fleet::lease::CoordinationKeyError;
use crate::fleet::scalar_codec::ScalarStateError;
use crate::installation::ProtocolError;
use crate::records::RecordIdentity;
use crate::records::postgres::StateStorageError;
use crate::runtime::admission::{NativeAdmissionError, NativeOperationError};

pub(crate) enum NativeOnceConstructionCause {
	AdmissionAssociation,
	LeasePlacementMismatch,
	OverlappingPlacement,
	MutexKey(CoordinationKeyError),
}

pub(crate) struct NativeOnceConstructionError {
	source: NativeOnceConstructionCause,
}
impl NativeOnceConstructionError {
	pub(super) fn new(source: NativeOnceConstructionCause) -> Self {
		Self { source }
	}
}
impl fmt::Debug for NativeOnceConstructionError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("NativeOnceConstructionError { .. }")
	}
}
impl fmt::Display for NativeOnceConstructionError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("native Once code, admission or placement cannot be associated")
	}
}
impl std::error::Error for NativeOnceConstructionError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match &self.source {
			NativeOnceConstructionCause::MutexKey(source) => Some(source),
			_ => None,
		}
	}
}

pub(crate) enum NativeOnceCause {
	Admission(NativeAdmissionError),
	State(ScalarStateError),
	UnexpectedIndexes,
	Storage(StateStorageError),
	Database(DatabaseError),
	Protocol(ProtocolError),
	RollbackFailed {
		operation_error: Box<NativeOnceError>,
		rollback_error: DatabaseError,
	},
}

struct OnceErrorContext {
	key: OnceKey,
	identity: RecordIdentity,
}

pub(crate) struct NativeOnceError {
	context: Option<Box<OnceErrorContext>>,
	source: Box<NativeOnceCause>,
}
impl NativeOnceError {
	pub(super) fn new(source: NativeOnceCause) -> Self {
		Self {
			context: None,
			source: Box::new(source),
		}
	}
	pub(super) fn with_context_if_absent(
		mut self,
		key: &OnceKey,
		identity: &RecordIdentity,
	) -> Self {
		if self.context.is_none() {
			self.context = Some(Box::new(OnceErrorContext {
				key: key.clone(),
				identity: identity.clone(),
			}));
		}
		self
	}
	pub(super) fn from_operation(source: NativeOperationError<Self>) -> Self {
		match source {
			NativeOperationError::Admission(source) => {
				Self::new(NativeOnceCause::Admission(source))
			}
			NativeOperationError::Operation(source) => source,
		}
	}
}
impl fmt::Debug for NativeOnceError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("NativeOnceError { .. }")
	}
}
impl fmt::Display for NativeOnceError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("native Once completion observation failed")
	}
}
impl std::error::Error for NativeOnceError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self.source.as_ref() {
			NativeOnceCause::Admission(source) => Some(source),
			NativeOnceCause::State(source) => Some(source),
			NativeOnceCause::Storage(source) => Some(source),
			NativeOnceCause::Database(source) => Some(source),
			NativeOnceCause::Protocol(source) => Some(source),
			NativeOnceCause::RollbackFailed {
				operation_error, ..
			} => Some(operation_error.as_ref()),
			NativeOnceCause::UnexpectedIndexes => None,
		}
	}
}
impl TransactionFailure for NativeOnceError {
	fn from_protocol_error(source: ProtocolError) -> Self {
		Self::new(NativeOnceCause::Protocol(source))
	}
	fn from_database_error(source: DatabaseError) -> Self {
		Self::new(NativeOnceCause::Database(source))
	}
	fn from_rollback_failure(operation_error: Self, rollback_error: DatabaseError) -> Self {
		let context = operation_error.context.as_ref().map(|context| {
			Box::new(OnceErrorContext {
				key: context.key.clone(),
				identity: context.identity.clone(),
			})
		});
		Self {
			context,
			source: Box::new(NativeOnceCause::RollbackFailed {
				operation_error: Box::new(operation_error),
				rollback_error,
			}),
		}
	}
	fn involves_database_failure(&self) -> bool {
		match self.source.as_ref() {
			NativeOnceCause::Admission(source) => source.involves_database_failure(),
			NativeOnceCause::Storage(source) => source.database_error().is_some(),
			NativeOnceCause::Database(_) | NativeOnceCause::RollbackFailed { .. } => true,
			NativeOnceCause::State(_)
			| NativeOnceCause::UnexpectedIndexes
			| NativeOnceCause::Protocol(_) => false,
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self.source.as_ref() {
			NativeOnceCause::Admission(source) => source.commit_outcome_is_unknown(),
			NativeOnceCause::Storage(source) => source
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
			NativeOnceCause::Database(source) => source.commit_outcome_is_unknown(),
			NativeOnceCause::RollbackFailed {
				operation_error,
				rollback_error,
			} => {
				operation_error.commit_outcome_is_unknown()
					|| rollback_error.commit_outcome_is_unknown()
			}
			NativeOnceCause::State(_)
			| NativeOnceCause::UnexpectedIndexes
			| NativeOnceCause::Protocol(_) => false,
		}
	}
}
