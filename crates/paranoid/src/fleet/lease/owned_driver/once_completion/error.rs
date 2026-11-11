//! Exact ordinary Once finish causes with complete placed diagnostic identities.

use super::{Arc, NativeLeaseCloseObserver, NativeLeaseDriverError};
use crate::db::{DatabaseError, TransactionFailure};
use crate::fleet::lease::state::LeaseBindingError;
use crate::fleet::scalar_codec::ScalarStateError;
use crate::installation::ProtocolError;
use crate::records::postgres::record_set::{RecordSetError, RecordSetRejectionError};
use crate::runtime::admission::{NativeAdmissionError, lease::LeaseRegistrationError};

pub(crate) enum NativeOnceCompletionCause {
	Registration(LeaseRegistrationError),
	Authority(Arc<NativeLeaseDriverError>),
	Admission(NativeAdmissionError),
	LeaseBinding(LeaseBindingError),
	Pair(super::super::once_pair::OncePairPreparationError),
	Completion(ScalarStateError),
	AliasedRecords,
	Records(RecordSetError),
	Mutation(RecordSetRejectionError),
	PreparationCleanup {
		operation_error: Box<NativeOnceCompletionError>,
		cleanup_error: RecordSetError,
	},
	Database(DatabaseError),
	Protocol(ProtocolError),
	RollbackFailed {
		operation_error: Box<NativeOnceCompletionError>,
		rollback_error: DatabaseError,
	},
}
pub(crate) struct NativeOnceCompletionError {
	source: Box<NativeOnceCompletionCause>,
}
impl NativeOnceCompletionError {
	pub(in crate::fleet::lease::owned_driver) fn new(source: NativeOnceCompletionCause) -> Self {
		Self {
			source: Box::new(source),
		}
	}
}
impl std::fmt::Debug for NativeOnceCompletionError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("NativeOnceFinishError { typed_cause_retained: true }")
	}
}
impl std::fmt::Display for NativeOnceCompletionError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("native Once completion and release failed")
	}
}
impl std::error::Error for NativeOnceCompletionError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self.source.as_ref() {
			NativeOnceCompletionCause::Registration(source) => Some(source),
			NativeOnceCompletionCause::Authority(source) => Some(source.as_ref()),
			NativeOnceCompletionCause::Admission(source) => Some(source),
			NativeOnceCompletionCause::LeaseBinding(source) => Some(source),
			NativeOnceCompletionCause::Pair(source) => Some(source),
			NativeOnceCompletionCause::Completion(source) => Some(source),
			NativeOnceCompletionCause::Records(source) => Some(source),

			NativeOnceCompletionCause::Mutation(source) => Some(source),
			NativeOnceCompletionCause::PreparationCleanup {
				operation_error: operation,
				..
			}
			| NativeOnceCompletionCause::RollbackFailed {
				operation_error: operation,
				..
			} => Some(operation.as_ref()),
			NativeOnceCompletionCause::Database(source) => Some(source),
			NativeOnceCompletionCause::Protocol(source) => Some(source),
			NativeOnceCompletionCause::AliasedRecords => None,
		}
	}
}
impl TransactionFailure for NativeOnceCompletionError {
	fn from_protocol_error(source: ProtocolError) -> Self {
		Self::new(NativeOnceCompletionCause::Protocol(source))
	}
	fn from_database_error(source: DatabaseError) -> Self {
		Self::new(NativeOnceCompletionCause::Database(source))
	}
	fn from_rollback_failure(operation_error: Self, rollback_error: DatabaseError) -> Self {
		Self::new(NativeOnceCompletionCause::RollbackFailed {
			operation_error: Box::new(operation_error),
			rollback_error,
		})
	}
	fn involves_database_failure(&self) -> bool {
		match self.source.as_ref() {
			NativeOnceCompletionCause::Registration(source) => source.involves_database_failure(),
			NativeOnceCompletionCause::Authority(source) => source.involves_database_failure(),
			NativeOnceCompletionCause::Admission(source) => source.involves_database_failure(),
			NativeOnceCompletionCause::Records(source) => source.involves_database_failure(),

			NativeOnceCompletionCause::PreparationCleanup {
				operation_error,
				cleanup_error,
			} => {
				operation_error.involves_database_failure()
					|| cleanup_error.involves_database_failure()
			}
			NativeOnceCompletionCause::Database(_)
			| NativeOnceCompletionCause::RollbackFailed { .. } => true,
			_ => false,
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self.source.as_ref() {
			NativeOnceCompletionCause::Registration(source) => source.commit_outcome_is_unknown(),
			NativeOnceCompletionCause::Authority(source) => source.commit_outcome_is_unknown(),
			NativeOnceCompletionCause::Admission(source) => source.commit_outcome_is_unknown(),
			NativeOnceCompletionCause::Records(source) => source.commit_outcome_is_unknown(),

			NativeOnceCompletionCause::PreparationCleanup {
				operation_error,
				cleanup_error,
			} => {
				operation_error.commit_outcome_is_unknown()
					|| cleanup_error.commit_outcome_is_unknown()
			}
			NativeOnceCompletionCause::Database(source) => source.commit_outcome_is_unknown(),
			NativeOnceCompletionCause::RollbackFailed {
				operation_error,
				rollback_error,
			} => {
				operation_error.commit_outcome_is_unknown()
					|| rollback_error.commit_outcome_is_unknown()
			}
			_ => false,
		}
	}
}
pub(crate) struct NativeOnceCompletionErrorWithObserver {
	pub(crate) error: NativeOnceCompletionError,
	pub(crate) observer: NativeLeaseCloseObserver,
}
impl std::fmt::Debug for NativeOnceCompletionErrorWithObserver {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("NativeOnceCompletionErrorWithObserver { typed_cause_retained: true }")
	}
}
impl std::fmt::Display for NativeOnceCompletionErrorWithObserver {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("native Once completion failed; release remains observable")
	}
}
impl std::error::Error for NativeOnceCompletionErrorWithObserver {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(&self.error)
	}
}
