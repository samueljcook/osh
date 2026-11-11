//! Failures of document index queries.

use super::NativeDocumentSelectionError;
use crate::db::{DatabaseError, TransactionFailure};
use crate::kv::document::postgres::index::{PgDocumentIndexError, PgDocumentIndexProjectionError};
use crate::kv::document::postgres::selection::PgPhysicalDocumentSelectionError;
use crate::kv::document::query::DocumentQueryInnerError;
use crate::{
	kv::document::schema::IndexContractIdentity, records::RecordIdentity,
	records::state_schema::StateSchemaError, scope::ScopeIdentity,
};
use std::{error::Error, fmt};

pub(super) struct SelectionErrorFields<'a, C> {
	pub(super) scope: &'a ScopeIdentity,
	pub(super) schema: &'static str,
	pub(super) index: &'a IndexContractIdentity,
	pub(super) source: NativeDocumentSelectionError<C>,
}
struct Diagnostic<C> {
	scope: ScopeIdentity,
	schema: &'static str,
	index: IndexContractIdentity,
	source: NativeDocumentSelectionError<C>,
}
/// A failed document index query. `C` is the error type of the app's codec.
pub struct PgDocumentSelectionError<C> {
	diagnostic: Box<Diagnostic<C>>,
}
/// The class of a failed document index query. A failed rollback does not change the class
/// of the original failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgDocumentSelectionErrorKind {
	/// The query was built from a handle of another scope.
	DifferentDocumentBinding,
	/// The runtime cannot run the query, for one of these reasons: the installation's recorded
	/// policy does not allow it because the runtime has no active use of the query or the
	/// index is not ready for queries; this runtime has stopped accepting operations; or
	/// reading the installation's control state or this runtime's registration failed, and
	/// `PgDocumentSelectionError::database_error` returns the database error.
	Admission,
	/// The app's codec could not decode a returned document.
	Read,
	/// The statement wait is zero or too large for the local clock.
	InvalidStatementWaitTimeout,
	/// The query statement did not complete within the statement wait.
	StatementWaitElapsed,
	/// The index that the query names does not match the installed index.
	Binding,
	/// A stored index entry of a returned document is missing a field or has a field of
	/// another type.
	Projection,
	/// A database statement or the transaction's own control failed.
	Database,
	/// A returned row could not be read as a stored document.
	Record,
	/// A returned index value does not match the queried index fields, or the cursor does not
	/// belong to this query.
	Query,
	/// The database returned a row of an unexpected shape. This indicates a defect in Paranoid.
	InvalidObservationShape,
	/// The returned rows report different database clocks. This indicates a defect in Paranoid.
	InconsistentClock,
	/// The database returned more rows than the query's limit. This indicates a defect in
	/// Paranoid.
	RowLimitExceeded,
	/// A returned row belongs to another scope. This indicates a defect in Paranoid.
	NamespaceMismatch,
	/// A returned document had expired at the database clock of the query. This indicates a
	/// defect in Paranoid.
	ExpiredRecord,
	/// A returned row is outside the query's predicate or cursor. This indicates a defect in
	/// Paranoid.
	PositionOutsideSelection,
	/// The returned rows are not in the query's order. This indicates a defect in Paranoid.
	PositionOrderViolation,
	/// The transaction failed outside a database statement.
	Protocol,
}
/// Why a returned document's stored index entry was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PgDocumentSelectionProjectionReason {
	/// The entry does not record whether the document is in the index.
	CoverageMissing,
	/// The entry records that the document is not in the index.
	CoverageFalse,
	/// The entry's index membership is not a Boolean.
	CoverageType,
	/// The entry is missing one of the index's fields.
	FieldMissing,
	/// One of the entry's fields has a type other than the index declares.
	FieldType,
}

impl<C> PgDocumentSelectionError<C> {
	pub(super) fn new(fields: SelectionErrorFields<'_, C>) -> Self {
		Self {
			diagnostic: Box::new(Diagnostic {
				scope: fields.scope.clone(),
				schema: fields.schema,
				index: fields.index.clone(),
				source: fields.source,
			}),
		}
	}
	fn primary(&self) -> &NativeDocumentSelectionError<C> {
		let mut source = &self.diagnostic.source;
		while let NativeDocumentSelectionError::RollbackFailed {
			operation_error, ..
		} = source
		{
			source = operation_error;
		}
		source
	}
	fn physical(&self) -> Option<&PgPhysicalDocumentSelectionError> {
		match self.primary() {
			NativeDocumentSelectionError::Selection(source) => Some(source),
			_ => None,
		}
	}
	fn physical_leaf(&self) -> Option<&PgPhysicalDocumentSelectionError> {
		let mut source = self.physical()?;
		while let PgPhysicalDocumentSelectionError::SelectedRecord { source: nested, .. } = source {
			source = nested;
		}
		Some(source)
	}
	/// The queried scope.
	pub fn scope(&self) -> &ScopeIdentity {
		&self.diagnostic.scope
	}
	/// The identifier of the scope's document schema.
	pub fn schema_identifier(&self) -> &'static str {
		self.diagnostic.schema
	}
	/// The queried index.
	pub fn index_identity(&self) -> &IndexContractIdentity {
		&self.diagnostic.index
	}
	/// The returned record that failed, or `None` when the failure does not concern one
	/// record.
	pub fn record_identity(&self) -> Option<&RecordIdentity> {
		match self.primary() {
			NativeDocumentSelectionError::Read { identity, .. } => Some(identity),
			NativeDocumentSelectionError::Selection(
				PgPhysicalDocumentSelectionError::SelectedRecord { identity, .. },
			) => Some(identity),
			_ => None,
		}
	}
	/// The class of the failure.
	pub fn kind(&self) -> PgDocumentSelectionErrorKind {
		Self::operation_kind(&self.diagnostic.source)
	}
	fn operation_kind(source: &NativeDocumentSelectionError<C>) -> PgDocumentSelectionErrorKind {
		use PgDocumentSelectionErrorKind as Kind;
		match source {
			NativeDocumentSelectionError::DifferentDocumentBinding => {
				Kind::DifferentDocumentBinding
			}
			NativeDocumentSelectionError::Admission(_) => Kind::Admission,
			NativeDocumentSelectionError::Read { .. } => Kind::Read,
			NativeDocumentSelectionError::Selection(source) => Self::physical_kind(source),
			NativeDocumentSelectionError::Database(_) => Kind::Database,
			NativeDocumentSelectionError::Protocol(_) => Kind::Protocol,
			NativeDocumentSelectionError::RollbackFailed {
				operation_error, ..
			} => Self::operation_kind(operation_error),
		}
	}
	fn physical_kind(source: &PgPhysicalDocumentSelectionError) -> PgDocumentSelectionErrorKind {
		use PgDocumentSelectionErrorKind as Kind;
		match source {
			PgPhysicalDocumentSelectionError::SelectedRecord { source, .. } => {
				Self::physical_kind(source)
			}
			PgPhysicalDocumentSelectionError::InvalidStatementWaitTimeout => {
				Kind::InvalidStatementWaitTimeout
			}
			PgPhysicalDocumentSelectionError::StatementWaitElapsed => Kind::StatementWaitElapsed,
			PgPhysicalDocumentSelectionError::Binding(_) => Kind::Binding,
			PgPhysicalDocumentSelectionError::Projection(_) => Kind::Projection,
			PgPhysicalDocumentSelectionError::Database(_) => Kind::Database,
			PgPhysicalDocumentSelectionError::Record(_) => Kind::Record,
			PgPhysicalDocumentSelectionError::Query(_) => Kind::Query,
			PgPhysicalDocumentSelectionError::InvalidObservationShape => {
				Kind::InvalidObservationShape
			}
			PgPhysicalDocumentSelectionError::InconsistentClock => Kind::InconsistentClock,
			PgPhysicalDocumentSelectionError::RowLimitExceeded => Kind::RowLimitExceeded,
			PgPhysicalDocumentSelectionError::NamespaceMismatch => Kind::NamespaceMismatch,
			PgPhysicalDocumentSelectionError::ExpiredRecord => Kind::ExpiredRecord,
			PgPhysicalDocumentSelectionError::PositionOutsideSelection => {
				Kind::PositionOutsideSelection
			}
			PgPhysicalDocumentSelectionError::PositionOrderViolation => {
				Kind::PositionOrderViolation
			}
		}
	}
	/// Why a returned document's stored index entry was rejected, when the kind is
	/// `Projection`.
	pub fn projection_reason(&self) -> Option<PgDocumentSelectionProjectionReason> {
		use PgDocumentSelectionProjectionReason as Reason;
		match self.physical_leaf() {
			Some(PgPhysicalDocumentSelectionError::Projection(source)) => Some(match source {
				PgDocumentIndexProjectionError::CoverageMissing => Reason::CoverageMissing,
				PgDocumentIndexProjectionError::CoverageFalse => Reason::CoverageFalse,
				PgDocumentIndexProjectionError::CoverageType => Reason::CoverageType,
				PgDocumentIndexProjectionError::FieldMissing { .. } => Reason::FieldMissing,
				PgDocumentIndexProjectionError::FieldType { .. } => Reason::FieldType,
			}),
			_ => None,
		}
	}

	/// The decoding failure of a returned document, when the kind is `Read`.
	pub fn schema_error(&self) -> Option<&StateSchemaError<C>> {
		match self.primary() {
			NativeDocumentSelectionError::Read { error, .. } => error.representation_error(),
			_ => None,
		}
	}
	/// The app codec's own error, when the codec rejected a returned document.
	pub fn codec_error(&self) -> Option<&C> {
		match self.schema_error() {
			Some(StateSchemaError::Codec(source)) => Some(source),
			_ => None,
		}
	}
	/// The position of the index field that failed, when the failure concerns one field.
	pub fn field_position(&self) -> Option<usize> {
		match self.physical_leaf() {
			Some(PgPhysicalDocumentSelectionError::Binding(
				PgDocumentIndexError::FieldAbsent { field_index }
				| PgDocumentIndexError::FieldScalarType { field_index, .. },
			))
			| Some(PgPhysicalDocumentSelectionError::Projection(
				PgDocumentIndexProjectionError::FieldMissing { field_index }
				| PgDocumentIndexProjectionError::FieldType { field_index },
			))
			| Some(PgPhysicalDocumentSelectionError::Query(
				DocumentQueryInnerError::ScalarTypeMismatch { field_index },
			)) => Some(*field_index),
			_ => None,
		}
	}
	fn rollback_errors(&self) -> impl Iterator<Item = &DatabaseError> {
		std::iter::successors(Some(&self.diagnostic.source), |error| match error {
			NativeDocumentSelectionError::RollbackFailed {
				operation_error, ..
			} => Some(operation_error.as_ref()),
			_ => None,
		})
		.filter_map(|error| match error {
			NativeDocumentSelectionError::RollbackFailed { rollback_error, .. } => {
				Some(rollback_error)
			}
			_ => None,
		})
	}
	/// The database error of the original failure, or else of the first failed rollback.
	pub fn database_error(&self) -> Option<&DatabaseError> {
		let source = match self.primary() {
			NativeDocumentSelectionError::Admission(source) => source.database_error(),
			NativeDocumentSelectionError::Selection(source) => source.database_error(),
			NativeDocumentSelectionError::Database(source) => Some(source),
			_ => None,
		};
		source.or_else(|| self.rollback_errors().next())
	}
	/// Whether the transaction's own `COMMIT` failed, which means that its outcome is unknown.
	pub fn commit_outcome_is_unknown(&self) -> bool {
		self.diagnostic.source.commit_outcome_is_unknown()
	}
}
impl<C> fmt::Debug for PgDocumentSelectionError<C> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("PgDocumentSelectionError")
			.field("kind", &self.kind())
			.field("scope", self.scope())
			.finish_non_exhaustive()
	}
}
impl<C> fmt::Display for PgDocumentSelectionError<C> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("document selection failed")
	}
}
impl<C: Error + 'static> Error for PgDocumentSelectionError<C> {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		Some(&self.diagnostic.source)
	}
}
