//! Failures of operations on several documents.

use std::{convert::Infallible, error::Error, fmt};

use crate::db::{DatabaseError, TransactionFailure};
use crate::installation::ProtocolError;
use crate::kv::document::postgres::slot::RecordSlotError;
use crate::records::postgres::record_many::RecordManyStorageError;
use crate::records::postgres::record_many::write::RecordManyWriteError;
use crate::records::{RecordIdentity, RecordKey};
use crate::runtime::admission::{NativeAdmissionError, NativeOperationError};
use crate::scope::ScopeIdentity;

use super::super::{PgDocumentError, PgDocumentErrorKind};

pub(in crate::kv::document::handle) enum NativeDocumentManyError<C, P> {
	Admission(Box<NativeAdmissionError>),
	ReadStorage(Box<RecordManyStorageError>),
	SlotStorage(Box<RecordSlotError>),
	Member {
		input_index: usize,
		source: Box<PgDocumentError<C, P, Infallible>>,
	},
	ExpirationOverflow,
	WriteStorage(Box<RecordManyWriteError>),
	Cleanup {
		operation_error: Box<Self>,
		cleanup_error: Box<RecordManyWriteError>,
	},
	SlotCleanup {
		operation_error: Box<Self>,
		cleanup_error: Box<RecordSlotError>,
	},
	Database(DatabaseError),
	Protocol(ProtocolError),
	RollbackFailed {
		operation_error: Box<Self>,
		rollback_error: DatabaseError,
	},
}
impl<C, P> NativeDocumentManyError<C, P> {
	pub(in crate::kv::document::handle) fn from_operation(
		source: NativeOperationError<Self>,
	) -> Self {
		match source {
			NativeOperationError::Admission(source) => Self::Admission(Box::new(source)),
			NativeOperationError::Operation(source) => source,
		}
	}
	fn primary(&self) -> &Self {
		match self {
			Self::Cleanup {
				operation_error: operation,
				..
			}
			| Self::SlotCleanup {
				operation_error: operation,
				..
			}
			| Self::RollbackFailed {
				operation_error: operation,
				..
			} => operation.primary(),
			_ => self,
		}
	}
	fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Admission(source) => source.database_error(),
			Self::ReadStorage(source) => source.database_error(),
			Self::SlotStorage(source) => source.database_error(),
			Self::Member { source, .. } => source.database_error(),
			Self::WriteStorage(source) => source.database_error(),
			Self::Cleanup {
				operation_error,
				cleanup_error,
			} => operation_error
				.database_error()
				.or_else(|| cleanup_error.database_error()),
			Self::SlotCleanup {
				operation_error,
				cleanup_error,
			} => operation_error
				.database_error()
				.or_else(|| cleanup_error.database_error()),
			Self::Database(source) => Some(source),
			Self::RollbackFailed {
				operation_error,
				rollback_error,
			} => operation_error.database_error().or(Some(rollback_error)),
			Self::ExpirationOverflow | Self::Protocol(_) => None,
		}
	}
}
impl<C, P> TransactionFailure for NativeDocumentManyError<C, P> {
	fn from_protocol_error(source: ProtocolError) -> Self {
		Self::Protocol(source)
	}
	fn from_database_error(source: DatabaseError) -> Self {
		Self::Database(source)
	}
	fn from_rollback_failure(operation_error: Self, rollback_error: DatabaseError) -> Self {
		Self::RollbackFailed {
			operation_error: Box::new(operation_error),
			rollback_error,
		}
	}
	fn involves_database_failure(&self) -> bool {
		match self {
			Self::Admission(source) => source.involves_database_failure(),
			Self::Member { source, .. } => source.involves_database_failure(),
			Self::Cleanup {
				operation_error,
				cleanup_error,
			} => {
				operation_error.involves_database_failure()
					|| cleanup_error.database_error().is_some()
			}
			Self::SlotCleanup {
				operation_error,
				cleanup_error,
			} => {
				operation_error.involves_database_failure()
					|| cleanup_error.database_error().is_some()
			}
			Self::RollbackFailed { .. } => true,
			_ => self.database_error().is_some(),
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Admission(source) => source.commit_outcome_is_unknown(),
			Self::Member { source, .. } => source.commit_outcome_is_unknown(),
			Self::Cleanup {
				operation_error,
				cleanup_error,
			} => {
				operation_error.commit_outcome_is_unknown()
					|| cleanup_error
						.database_error()
						.is_some_and(DatabaseError::commit_outcome_is_unknown)
			}
			Self::SlotCleanup {
				operation_error,
				cleanup_error,
			} => {
				operation_error.commit_outcome_is_unknown()
					|| cleanup_error.commit_outcome_is_unknown()
			}
			Self::SlotStorage(source) => source.commit_outcome_is_unknown(),
			Self::WriteStorage(source) => source.commit_outcome_is_unknown(),
			Self::RollbackFailed {
				operation_error,
				rollback_error,
			} => {
				operation_error.commit_outcome_is_unknown()
					|| rollback_error.commit_outcome_is_unknown()
			}
			_ => self
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
		}
	}
}
impl<C, P> fmt::Debug for NativeDocumentManyError<C, P> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("NativeDocumentManyError")
			.finish_non_exhaustive()
	}
}
impl<C, P> fmt::Display for NativeDocumentManyError<C, P> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("document batch operation failed")
	}
}
impl<C: Error + 'static, P: Error + 'static> Error for NativeDocumentManyError<C, P> {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		match self {
			Self::Admission(source) => Some(source.as_ref()),
			Self::ReadStorage(source) => Some(source.as_ref()),
			Self::SlotStorage(source) => Some(source.as_ref()),
			Self::Member { source, .. } => Some(source.as_ref()),
			Self::WriteStorage(source) => Some(source.as_ref()),
			Self::Cleanup {
				operation_error: operation,
				..
			}
			| Self::SlotCleanup {
				operation_error: operation,
				..
			}
			| Self::RollbackFailed {
				operation_error: operation,
				..
			} => Some(operation.as_ref()),
			Self::Database(source) => Some(source),
			Self::Protocol(source) => Some(source),
			Self::ExpirationOverflow => None,
		}
	}
}

pub(in crate::kv::document::handle) struct PgDocumentManyErrorFields<'a, C, P> {
	pub(in crate::kv::document::handle) scope: &'a ScopeIdentity,
	pub(in crate::kv::document::handle) schema: &'static str,
	pub(in crate::kv::document::handle) keys: Vec<RecordKey>,
	pub(in crate::kv::document::handle) source: NativeDocumentManyError<C, P>,
}
struct ManyDiagnostic<C, P> {
	scope: ScopeIdentity,
	schema: &'static str,
	keys: Vec<RecordKey>,
	source: NativeDocumentManyError<C, P>,
}
/// A failed operation on several documents. `C` is the error type of the app's codec and `P`
/// of its index projectors.
pub struct PgDocumentManyError<C, P> {
	diagnostic: Box<ManyDiagnostic<C, P>>,
}
/// The class of a failed operation on several documents. A failed rollback or cleanup does
/// not change the class of the original failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgDocumentManyErrorKind {
	/// The runtime cannot run the operation, for one of these reasons: the installation's
	/// recorded policy does not allow it, for example because the runtime has no active use of
	/// the role that the operation needs; this runtime has stopped accepting operations; or
	/// reading the installation's control state or this runtime's registration failed, and
	/// `PgDocumentManyError::database_error` returns the database error.
	Admission,
	/// The operation failed on one document, with this class.
	Member(PgDocumentErrorKind),
	/// The expiry time that the TTL gives is too large for a stored timestamp.
	ExpirationOverflow,
	/// The database did not return exactly one row for each requested key. This indicates a
	/// defect in Paranoid.
	RequestCoverage,
	/// The returned rows report different database clocks. This indicates a defect in Paranoid.
	InconsistentClock,
	/// A stored record could not be read or written, for example because its stored form is
	/// invalid.
	Storage,
	/// The planned changes address a record twice or a record that the operation does not
	/// hold. This indicates a defect in Paranoid.
	GroupPlan,
	/// A database statement or the transaction's own control failed.
	Database,
	/// The transaction failed outside a database statement.
	Protocol,
}
impl<C, P> PgDocumentManyError<C, P> {
	pub(in crate::kv::document::handle) fn new(
		fields: PgDocumentManyErrorFields<'_, C, P>,
	) -> Self {
		Self {
			diagnostic: Box::new(ManyDiagnostic {
				scope: fields.scope.clone(),
				schema: fields.schema,
				keys: fields.keys,
				source: fields.source,
			}),
		}
	}
	/// The operation's scope.
	pub fn scope(&self) -> &ScopeIdentity {
		&self.diagnostic.scope
	}
	/// The identifier of the scope's document schema.
	pub fn schema_identifier(&self) -> &'static str {
		self.diagnostic.schema
	}
	/// Every requested key, in the caller's order.
	pub fn keys(&self) -> &[RecordKey] {
		&self.diagnostic.keys
	}
	/// The failure on one document, when the kind is `Member`.
	pub fn record_error(&self) -> Option<&PgDocumentError<C, P, Infallible>> {
		match self.diagnostic.source.primary() {
			NativeDocumentManyError::Member { source, .. } => Some(source),
			_ => None,
		}
	}
	/// The record that failed, when the failure concerns one record.
	pub fn record_identity(&self) -> Option<&RecordIdentity> {
		if let Some(source) = self.record_error() {
			return Some(source.record_identity());
		}
		match self.diagnostic.source.primary() {
			NativeDocumentManyError::ReadStorage(source) => return source.identity(),
			NativeDocumentManyError::SlotStorage(source) => return source.identity(),
			_ => {}
		}
		match self.diagnostic.source.primary() {
			NativeDocumentManyError::WriteStorage(source) => source.identity(),
			_ => None,
		}
	}
	/// The zero-based position of the failed key in the request, when the failure concerns one
	/// key.
	pub fn input_index(&self) -> Option<usize> {
		match self.diagnostic.source.primary() {
			NativeDocumentManyError::Member { input_index, .. } => Some(*input_index),
			NativeDocumentManyError::ReadStorage(source) => source.input_index(),
			NativeDocumentManyError::SlotStorage(source) => source.input_index(),
			NativeDocumentManyError::WriteStorage(source) => source.input_index(),
			_ => self.record_identity().and_then(|identity| {
				self.diagnostic
					.keys
					.iter()
					.position(|key| key == identity.key())
			}),
		}
	}
	/// The class of the failure.
	pub fn kind(&self) -> PgDocumentManyErrorKind {
		use NativeDocumentManyError as Cause;
		use PgDocumentManyErrorKind as Kind;
		match self.diagnostic.source.primary() {
			Cause::Admission(_) => Kind::Admission,
			Cause::Member { source, .. } => Kind::Member(source.kind()),
			Cause::ExpirationOverflow => Kind::ExpirationOverflow,
			Cause::ReadStorage(source) => {
				let mut leaf = source.as_ref();
				while let RecordManyStorageError::Record { source, .. } = leaf {
					leaf = source;
				}
				match leaf {
					RecordManyStorageError::Coverage => Kind::RequestCoverage,
					RecordManyStorageError::InconsistentClock => Kind::InconsistentClock,
					_ => Kind::Storage,
				}
			}
			Cause::WriteStorage(_) => Kind::Storage,
			Cause::SlotStorage(_) => Kind::Storage,
			Cause::Database(_) => Kind::Database,
			Cause::Protocol(_) => Kind::Protocol,
			Cause::Cleanup { .. } | Cause::SlotCleanup { .. } | Cause::RollbackFailed { .. } => {
				unreachable!("primary strips wrappers")
			}
		}
	}
	/// The database error of the original failure, or else of a failed cleanup, or else of the
	/// first failed rollback.
	pub fn database_error(&self) -> Option<&DatabaseError> {
		self.diagnostic.source.database_error()
	}
	/// Whether the transaction's own `COMMIT` failed, which means that the changes may or may
	/// not have been stored.
	pub fn commit_outcome_is_unknown(&self) -> bool {
		self.diagnostic.source.commit_outcome_is_unknown()
	}
}
impl<C, P> fmt::Debug for PgDocumentManyError<C, P> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("PgDocumentManyError")
			.field("kind", &self.kind())
			.field("scope", self.scope())
			.finish_non_exhaustive()
	}
}
impl<C, P> fmt::Display for PgDocumentManyError<C, P> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("PostgreSQL document batch operation failed")
	}
}
impl<C: Error + 'static, P: Error + 'static> Error for PgDocumentManyError<C, P> {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		Some(&self.diagnostic.source)
	}
}
