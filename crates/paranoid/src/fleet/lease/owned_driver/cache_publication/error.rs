//! Identified publication causes preserve arbitrary application encoding errors.

use super::super::*;
use crate::db::{DatabaseError, TransactionFailure};
use crate::fleet::cache::engine::publication_identity::NativeCachePublicationIdentityError;
use crate::fleet::lease::state::{LeaseBindingError, LeaseRepresentationError};
use crate::fleet::payload::scope::{PayloadScopeError, PayloadScopeValueError};
use crate::installation::ProtocolError;
use crate::records::postgres::record_set::{RecordSetError, RecordSetRejectionError};
use crate::runtime::admission::NativeAdmissionError;
use crate::runtime::admission::lease::LeaseRegistrationError;
use std::fmt;

pub(crate) enum NativeCachePublicationCause<C> {
	Identity(NativeCachePublicationIdentityError),
	Authority(Arc<NativeLeaseDriverError>),
	Registration(LeaseRegistrationError),
	Admission(NativeAdmissionError),
	LeaseBinding(LeaseBindingError),
	PayloadDeclaration(crate::scope::ScopeError),
	LeaseRecord(LeaseRepresentationError),
	Payload(PayloadScopeValueError<C>),
	Envelope(PayloadScopeError),
	ExpirationOverflow,
	Records(RecordSetError),
	Mutation(RecordSetRejectionError),
	PreparationCleanup {
		operation_error: Box<NativeCachePublicationError<C>>,
		cleanup_error: RecordSetError,
	},
	Database(DatabaseError),
	Protocol(ProtocolError),
	RollbackFailed {
		operation_error: Box<NativeCachePublicationError<C>>,
		rollback_error: DatabaseError,
	},
}
pub(crate) struct NativeCachePublicationError<C> {
	source: Box<NativeCachePublicationCause<C>>,
}
impl<C> NativeCachePublicationError<C> {
	pub(super) fn new(source: NativeCachePublicationCause<C>) -> Self {
		Self {
			source: Box::new(source),
		}
	}
}
impl<C> fmt::Debug for NativeCachePublicationError<C> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("NativeCachePublicationError { typed_cause_retained: true }")
	}
}
impl<C> fmt::Display for NativeCachePublicationError<C> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("native Cache publication failed")
	}
}
impl<C: std::error::Error + 'static> std::error::Error for NativeCachePublicationError<C> {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self.source.as_ref() {
			NativeCachePublicationCause::Identity(source) => Some(source),
			NativeCachePublicationCause::Authority(source) => Some(source.as_ref()),
			NativeCachePublicationCause::Registration(source) => Some(source),
			NativeCachePublicationCause::Admission(source) => Some(source),
			NativeCachePublicationCause::LeaseBinding(source) => Some(source),
			NativeCachePublicationCause::PayloadDeclaration(source) => Some(source),
			NativeCachePublicationCause::LeaseRecord(source) => Some(source),
			NativeCachePublicationCause::Payload(source) => Some(source),
			NativeCachePublicationCause::Envelope(source) => Some(source),
			NativeCachePublicationCause::Records(source) => Some(source),
			NativeCachePublicationCause::Mutation(source) => Some(source),
			NativeCachePublicationCause::Database(source) => Some(source),
			NativeCachePublicationCause::Protocol(source) => Some(source),
			NativeCachePublicationCause::RollbackFailed {
				operation_error, ..
			} => Some(operation_error.as_ref()),
			NativeCachePublicationCause::PreparationCleanup {
				operation_error, ..
			} => Some(operation_error.as_ref()),
			NativeCachePublicationCause::ExpirationOverflow => None,
		}
	}
}
impl<C> TransactionFailure for NativeCachePublicationError<C> {
	fn from_protocol_error(source: ProtocolError) -> Self {
		Self::new(NativeCachePublicationCause::Protocol(source))
	}
	fn from_database_error(source: DatabaseError) -> Self {
		Self::new(NativeCachePublicationCause::Database(source))
	}
	fn from_rollback_failure(operation_error: Self, rollback_error: DatabaseError) -> Self {
		Self::new(NativeCachePublicationCause::RollbackFailed {
			operation_error: Box::new(operation_error),
			rollback_error,
		})
	}
	fn involves_database_failure(&self) -> bool {
		match self.source.as_ref() {
			NativeCachePublicationCause::Registration(source) => source.involves_database_failure(),
			NativeCachePublicationCause::Admission(source) => source.involves_database_failure(),
			NativeCachePublicationCause::Records(source) => source.involves_database_failure(),
			NativeCachePublicationCause::PreparationCleanup {
				operation_error,
				cleanup_error,
			} => {
				operation_error.involves_database_failure()
					|| cleanup_error.involves_database_failure()
			}
			NativeCachePublicationCause::Database(_)
			| NativeCachePublicationCause::RollbackFailed { .. } => true,
			_ => false,
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self.source.as_ref() {
			NativeCachePublicationCause::Registration(source) => source.commit_outcome_is_unknown(),
			NativeCachePublicationCause::Admission(source) => source.commit_outcome_is_unknown(),
			NativeCachePublicationCause::Records(source) => source.commit_outcome_is_unknown(),
			NativeCachePublicationCause::Database(source) => source.commit_outcome_is_unknown(),
			NativeCachePublicationCause::PreparationCleanup {
				operation_error,
				cleanup_error,
			} => {
				operation_error.commit_outcome_is_unknown()
					|| cleanup_error.commit_outcome_is_unknown()
			}
			NativeCachePublicationCause::RollbackFailed {
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
pub(crate) struct NativeCachePublicationErrorWithObserver<C> {
	pub(crate) error: NativeCachePublicationError<C>,
	pub(crate) observer: NativeLeaseCloseObserver,
}
impl<C> fmt::Debug for NativeCachePublicationErrorWithObserver<C> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("NativeCachePublicationErrorWithObserver { typed_cause_retained: true }")
	}
}
impl<C> fmt::Display for NativeCachePublicationErrorWithObserver<C> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("native Cache publication failed; Lease cleanup remains observable")
	}
}
impl<C: std::error::Error + 'static> std::error::Error
	for NativeCachePublicationErrorWithObserver<C>
{
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(&self.error)
	}
}
