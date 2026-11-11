//! Namespace operation failures retain real scope, full requests and independent ending causes.

use std::{error::Error, fmt};

use super::RecordSelectionDeleteRequest;
use crate::db::{DatabaseError, TransactionFailure};
use crate::installation::ProtocolError;
use crate::kv::raw::model::delete::RecordDeleteBatchRequest;
use crate::kv::raw::model::page::{RecordKeySelection, RecordPageRequestError};
use crate::kv::raw::postgres::aggregate::RawAggregateStorageError;
use crate::kv::raw::postgres::delete::{
	RawDeleteStorageError, namespace::RawNamespaceStorageError,
};
use crate::kv::raw::postgres::key_headers::KeyHeaderPageError;
use crate::records::RecordIdentity;
use crate::records::postgres::group_attempt::GroupAttemptError;
use crate::runtime::admission::{NativeAdmissionError, NativeOperationError};
use crate::scope::ScopeIdentity;

#[derive(Debug, thiserror::Error)]
pub(in crate::kv::document::handle) enum NativeDocumentNamespaceError {
	#[error("document namespace admission failed")]
	Admission(#[source] Box<NativeAdmissionError>),
	#[error("document namespace request does not match its context")]
	Request(#[source] RecordPageRequestError),
	#[error("document key header page failed")]
	Page(#[source] Box<KeyHeaderPageError>),
	#[error("document live count failed")]
	Count(#[source] Box<RawAggregateStorageError>),
	#[error("document bounded physical deletion failed")]
	Delete(#[source] Box<RawDeleteStorageError>),
	#[error("document atomic physical deletion failed")]
	DeleteNamespace(#[source] Box<RawNamespaceStorageError>),
	#[error("document namespace database work failed")]
	Database(#[source] DatabaseError),
	#[error("document namespace transaction protocol failed")]
	Protocol(#[source] ProtocolError),
	#[error("document namespace work and rollback failed")]
	RollbackFailed {
		#[source]
		operation_error: Box<Self>,
		rollback_error: DatabaseError,
	},
}
impl NativeDocumentNamespaceError {
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
			Self::RollbackFailed {
				operation_error, ..
			} => operation_error.primary(),
			_ => self,
		}
	}
	fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Admission(source) => source.database_error(),
			Self::Page(source) => source.database_error(),
			Self::Count(source) => source.database_error(),
			Self::Delete(source) => source.database_error(),
			Self::DeleteNamespace(source) => source.database_error(),
			Self::Database(source) => Some(source),
			Self::RollbackFailed {
				operation_error,
				rollback_error,
			} => operation_error.database_error().or(Some(rollback_error)),
			Self::Request(_) | Self::Protocol(_) => None,
		}
	}
}
impl TransactionFailure for NativeDocumentNamespaceError {
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
			Self::RollbackFailed { .. } => true,
			_ => self.database_error().is_some(),
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Admission(source) => source.commit_outcome_is_unknown(),
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

pub(in crate::kv::document::handle) enum NamespaceRequest {
	Selection(RecordKeySelection),
	DeleteBatch(RecordDeleteBatchRequest),
	DeleteSelection(RecordSelectionDeleteRequest),
}
impl NamespaceRequest {
	fn selection(&self) -> &RecordKeySelection {
		match self {
			Self::Selection(selection) => selection,
			Self::DeleteBatch(request) => request.selection(),
			Self::DeleteSelection(request) => request.selection(),
		}
	}
}

pub(in crate::kv::document::handle) struct PgDocumentNamespaceErrorFields<'a> {
	pub(in crate::kv::document::handle) scope: &'a ScopeIdentity,
	pub(in crate::kv::document::handle) schema: &'static str,
	pub(in crate::kv::document::handle) request: NamespaceRequest,
	pub(in crate::kv::document::handle) source: NativeDocumentNamespaceError,
}
struct NamespaceDiagnostic {
	scope: ScopeIdentity,
	schema: &'static str,
	request: NamespaceRequest,
	source: NativeDocumentNamespaceError,
}
/// A failed operation on the keys of a document scope: a key page, a live count, or a deletion.
pub struct PgDocumentNamespaceError {
	diagnostic: Box<NamespaceDiagnostic>,
}
/// The class of a failed key operation. A failed rollback does not change the class of the
/// original failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgDocumentNamespaceErrorKind {
	/// The runtime cannot run the operation, for one of these reasons: the installation's
	/// recorded policy does not allow it, for example because the runtime has no active use of
	/// the role that the operation needs; this runtime has stopped accepting operations; or
	/// reading the installation's control state or this runtime's registration failed, and
	/// `PgDocumentNamespaceError::database_error` returns the database error.
	Admission,
	/// The request is invalid, for example because its cursor belongs to another request.
	Request,
	/// A stored record could not be read or deleted, for example because its stored form is
	/// invalid.
	Storage,
	/// The database returned a key outside the request, or keys out of order. This indicates a
	/// defect in Paranoid.
	PageBounds,
	/// The returned rows report different database clocks. This indicates a defect in Paranoid.
	InconsistentClock,
	/// The deletion did not account for each candidate record exactly once. This indicates a
	/// defect in Paranoid.
	DeletionCoverage,
	/// A deletion count does not fit in its integer type.
	CountUnrepresentable,
	/// A deletion of the whole selection was rejected, for example because the set of records
	/// changed while it ran.
	Contention,
	/// A database statement or the transaction's own control failed.
	Database,
	/// The transaction failed outside a database statement.
	Protocol,
}
impl PgDocumentNamespaceError {
	pub(in crate::kv::document::handle) fn new(fields: PgDocumentNamespaceErrorFields<'_>) -> Self {
		Self {
			diagnostic: Box::new(NamespaceDiagnostic {
				scope: fields.scope.clone(),
				schema: fields.schema,
				request: fields.request,
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
	/// The requested keys.
	pub fn selection(&self) -> &RecordKeySelection {
		self.diagnostic.request.selection()
	}
	/// The deletion request, when the operation deleted a batch of records.
	pub fn delete_batch_request(&self) -> Option<&RecordDeleteBatchRequest> {
		match &self.diagnostic.request {
			NamespaceRequest::DeleteBatch(request) => Some(request),
			_ => None,
		}
	}
	/// The deletion request, when the operation deleted a whole selection of the scope at once.
	pub fn namespace_delete_request(&self) -> Option<&RecordSelectionDeleteRequest> {
		match &self.diagnostic.request {
			NamespaceRequest::DeleteSelection(request) => Some(request),
			_ => None,
		}
	}

	/// The record that failed, when the failure concerns one record.
	pub fn record_identity(&self) -> Option<&RecordIdentity> {
		match self.diagnostic.source.primary() {
			NativeDocumentNamespaceError::Page(source) => source.identity(),
			NativeDocumentNamespaceError::Count(source) => source.identity(),
			NativeDocumentNamespaceError::Delete(source) => source.identity(),
			NativeDocumentNamespaceError::DeleteNamespace(source) => source.identity(),
			_ => None,
		}
	}
	/// The class of the failure.
	pub fn kind(&self) -> PgDocumentNamespaceErrorKind {
		use NativeDocumentNamespaceError as Cause;
		use PgDocumentNamespaceErrorKind as Kind;
		match self.diagnostic.source.primary() {
			Cause::Admission(_) => Kind::Admission,
			Cause::Request(_) => Kind::Request,
			Cause::Page(source) if source.database_error().is_some() => Kind::Database,
			Cause::Page(source) => match source.primary() {
				KeyHeaderPageError::Bounds => Kind::PageBounds,
				KeyHeaderPageError::InconsistentClock => Kind::InconsistentClock,
				_ => Kind::Storage,
			},
			Cause::Count(source) if source.database_error().is_some() => Kind::Database,
			Cause::Count(_) => Kind::Storage,
			Cause::Delete(source) => deletion_kind(source),
			Cause::DeleteNamespace(source) => namespace_deletion_kind(source),
			Cause::Database(_) => Kind::Database,
			Cause::Protocol(_) => Kind::Protocol,
			Cause::RollbackFailed { .. } => unreachable!("primary strips rollback wrappers"),
		}
	}
	/// The page request failure, when the kind is `Request`.
	pub fn request_error(&self) -> Option<&RecordPageRequestError> {
		match self.diagnostic.source.primary() {
			NativeDocumentNamespaceError::Request(source) => Some(source),
			_ => None,
		}
	}
	/// The database error of the original failure, or else of the first failed rollback.
	pub fn database_error(&self) -> Option<&DatabaseError> {
		self.diagnostic.source.database_error()
	}
	/// Whether the transaction's own `COMMIT` failed, which means that a deletion may or may not
	/// have been stored.
	pub fn commit_outcome_is_unknown(&self) -> bool {
		self.diagnostic.source.commit_outcome_is_unknown()
	}
}
impl fmt::Debug for PgDocumentNamespaceError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("PgDocumentNamespaceError")
			.field("kind", &self.kind())
			.field("scope", self.scope())
			.finish_non_exhaustive()
	}
}
impl fmt::Display for PgDocumentNamespaceError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("PostgreSQL document namespace operation failed")
	}
}
impl Error for PgDocumentNamespaceError {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		Some(&self.diagnostic.source)
	}
}

fn deletion_kind(source: &RawDeleteStorageError) -> PgDocumentNamespaceErrorKind {
	use PgDocumentNamespaceErrorKind as Kind;
	match source {
		RawDeleteStorageError::Record { source, .. } => deletion_kind(source),
		RawDeleteStorageError::Database(_) => Kind::Database,
		RawDeleteStorageError::Native(source) if source.database_error().is_some() => {
			Kind::Database
		}
		RawDeleteStorageError::Coverage => Kind::DeletionCoverage,
		_ => Kind::Storage,
	}
}
fn namespace_deletion_kind(source: &RawNamespaceStorageError) -> PgDocumentNamespaceErrorKind {
	use PgDocumentNamespaceErrorKind as Kind;
	match source {
		RawNamespaceStorageError::Storage(source) => deletion_kind(source),
		RawNamespaceStorageError::Boundary(source) => match source {
			GroupAttemptError::Storage(source) if source.database_error().is_none() => {
				Kind::Storage
			}
			GroupAttemptError::Storage(_) | GroupAttemptError::Boundary { .. } => Kind::Database,
			GroupAttemptError::RejectionCleanup { .. }
			| GroupAttemptError::ReacquisitionCleanup { .. } => Kind::Contention,
		},
		RawNamespaceStorageError::Rejected(_)
		| RawNamespaceStorageError::RejectionCleanup { .. } => Kind::Contention,
		RawNamespaceStorageError::CountUnrepresentable => Kind::CountUnrepresentable,
		RawNamespaceStorageError::CoveredIdentityReappeared { .. } => Kind::DeletionCoverage,
	}
}
