//! Identified ordinary read errors retain typed codec causes without generic formatting bounds.

use super::{PayloadPointError, PayloadPointFamily};
use crate::db::{DatabaseError, TransactionFailure};
use crate::installation::ProtocolError;
use crate::records::RecordIdentity;
use crate::records::postgres::StateStorageError;
use crate::runtime::admission::{NativeAdmissionError, NativeOperationError};
use crate::scope::{ScopeError, ScopeIdentity};
use std::fmt;

pub(crate) enum NativePayloadReadCause<C> {
	Admission(NativeAdmissionError),
	Scope(ScopeError),
	Epoch(Box<crate::fleet::cache::epoch::NativeCacheEpochError>),
	DifferentCache,
	Read(PayloadPointError<C>),
	EnvelopeRead(PayloadPointError<std::convert::Infallible>),
	Storage(StateStorageError),
	Database(DatabaseError),
	Protocol(ProtocolError),
	RollbackFailed {
		operation_error: Box<NativePayloadReadError<C>>,
		rollback_error: DatabaseError,
	},
}
struct NativePayloadReadContext {
	scope: ScopeIdentity,
	identity: RecordIdentity,
	family: PayloadPointFamily,
}
pub(crate) struct NativePayloadReadError<C> {
	context: Option<Box<NativePayloadReadContext>>,
	source: Box<NativePayloadReadCause<C>>,
}
impl<C> NativePayloadReadError<C> {
	pub(super) fn new(fields: NativePayloadReadErrorFields<'_, C>) -> Self {
		Self {
			context: Some(Box::new(NativePayloadReadContext {
				scope: fields.scope.clone(),
				identity: fields.identity.clone(),
				family: fields.family,
			})),
			source: Box::new(fields.source),
		}
	}
	pub(super) fn from_operation(fields: NativePayloadOperationFailureFields<'_, C>) -> Self {
		match fields.error {
			NativeOperationError::Admission(source) => Self::new(NativePayloadReadErrorFields {
				scope: fields.scope,
				identity: fields.identity,
				family: fields.family,
				source: NativePayloadReadCause::Admission(source),
			}),
			NativeOperationError::Operation(source) => source,
		}
	}
	pub(super) fn with_context_if_absent(
		mut self,
		fields: NativePayloadReadContextFields<'_>,
	) -> Self {
		if self.context.is_none() {
			self.context = Some(Box::new(NativePayloadReadContext {
				scope: fields.scope.clone(),
				identity: fields.identity.clone(),
				family: fields.family,
			}));
		}
		self
	}
}
pub(super) struct NativePayloadReadContextFields<'a> {
	pub(super) scope: &'a ScopeIdentity,
	pub(super) identity: &'a RecordIdentity,
	pub(super) family: PayloadPointFamily,
}
pub(super) struct NativePayloadReadErrorFields<'a, C> {
	pub(super) scope: &'a ScopeIdentity,
	pub(super) identity: &'a RecordIdentity,
	pub(super) family: PayloadPointFamily,
	pub(super) source: NativePayloadReadCause<C>,
}
pub(super) struct NativePayloadOperationFailureFields<'a, C> {
	pub(super) scope: &'a ScopeIdentity,
	pub(super) identity: &'a RecordIdentity,
	pub(super) family: PayloadPointFamily,
	pub(super) error: NativeOperationError<NativePayloadReadError<C>>,
}
impl<C> fmt::Debug for NativePayloadReadError<C> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("NativePayloadReadError")
			.field(
				"family",
				&self.context.as_ref().map(|context| context.family),
			)
			.finish_non_exhaustive()
	}
}
impl<C> fmt::Display for NativePayloadReadError<C> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("ordinary native payload point read failed")
	}
}
impl<C: std::error::Error + 'static> std::error::Error for NativePayloadReadError<C> {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self.source.as_ref() {
			NativePayloadReadCause::Admission(source) => Some(source),
			NativePayloadReadCause::Scope(source) => Some(source),
			NativePayloadReadCause::Epoch(source) => Some(source.as_ref()),
			NativePayloadReadCause::DifferentCache => None,
			NativePayloadReadCause::Read(source) => Some(source),
			NativePayloadReadCause::EnvelopeRead(source) => Some(source),
			NativePayloadReadCause::Storage(source) => Some(source),
			NativePayloadReadCause::Database(source) => Some(source),
			NativePayloadReadCause::Protocol(source) => Some(source),
			NativePayloadReadCause::RollbackFailed {
				operation_error, ..
			} => Some(operation_error.as_ref()),
		}
	}
}

impl<C> TransactionFailure for NativePayloadReadError<C> {
	fn from_protocol_error(source: ProtocolError) -> Self {
		Self {
			context: None,
			source: Box::new(NativePayloadReadCause::Protocol(source)),
		}
	}
	fn from_database_error(source: DatabaseError) -> Self {
		Self {
			context: None,
			source: Box::new(NativePayloadReadCause::Database(source)),
		}
	}
	fn from_rollback_failure(operation_error: Self, rollback_error: DatabaseError) -> Self {
		let context = operation_error.context.as_ref().map(|context| {
			Box::new(NativePayloadReadContext {
				scope: context.scope.clone(),
				identity: context.identity.clone(),
				family: context.family,
			})
		});
		Self {
			context,
			source: Box::new(NativePayloadReadCause::RollbackFailed {
				operation_error: Box::new(operation_error),
				rollback_error,
			}),
		}
	}
	fn involves_database_failure(&self) -> bool {
		match self.source.as_ref() {
			NativePayloadReadCause::Admission(source) => source.involves_database_failure(),
			NativePayloadReadCause::Storage(source) => source.database_error().is_some(),
			NativePayloadReadCause::Epoch(source) => source.involves_database_failure(),
			NativePayloadReadCause::Database(_) | NativePayloadReadCause::RollbackFailed { .. } => {
				true
			}
			NativePayloadReadCause::DifferentCache
			| NativePayloadReadCause::Scope(_)
			| NativePayloadReadCause::Read(_)
			| NativePayloadReadCause::EnvelopeRead(_)
			| NativePayloadReadCause::Protocol(_) => false,
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self.source.as_ref() {
			NativePayloadReadCause::Admission(source) => source.commit_outcome_is_unknown(),
			NativePayloadReadCause::Epoch(source) => source.commit_outcome_is_unknown(),
			NativePayloadReadCause::Storage(source) => source
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
			NativePayloadReadCause::Database(source) => source.commit_outcome_is_unknown(),
			NativePayloadReadCause::RollbackFailed {
				operation_error,
				rollback_error,
			} => {
				operation_error.commit_outcome_is_unknown()
					|| rollback_error.commit_outcome_is_unknown()
			}
			NativePayloadReadCause::DifferentCache
			| NativePayloadReadCause::Scope(_)
			| NativePayloadReadCause::Read(_)
			| NativePayloadReadCause::EnvelopeRead(_)
			| NativePayloadReadCause::Protocol(_) => false,
		}
	}
}
