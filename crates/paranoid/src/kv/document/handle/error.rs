//! Failures of document operations. The errors never format the app's values.

use std::{error::Error, fmt};

use crate::installation::contract::ContractStateReadError;
use crate::kv::document::schema::DocumentProjectionError;
use crate::kv::document::state::DocumentMutationError;
use crate::records::RecordIdentity;
use crate::records::state_schema::StateSchemaError;
use crate::scope::ScopeIdentity;

use super::NativeDocumentError;
use crate::db::{DatabaseError, TransactionFailure};
use crate::records::postgres::StateStorageError;

pub(super) struct PgDocumentErrorFields<'a, C, P, F> {
	pub(super) scope: &'a ScopeIdentity,
	pub(super) schema: &'static str,
	pub(super) record: &'a RecordIdentity,
	pub(super) source: NativeDocumentError<C, P, F>,
}
struct DocumentDiagnostic<C, P, F> {
	scope: ScopeIdentity,
	schema: &'static str,
	record: RecordIdentity,
	source: NativeDocumentError<C, P, F>,
}

/// A failed document operation. `C` is the error type of the app's codec, `P` of its index
/// projectors, and `F` of the operation's callback.
pub struct PgDocumentError<C, P, F> {
	diagnostic: Box<DocumentDiagnostic<C, P, F>>,
}

/// The class of a failed document operation. A failed rollback or cleanup does not change the
/// class of the original failure. Several kinds can involve a database failure, so check
/// `PgDocumentError::database_error` to detect every database failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgDocumentErrorKind {
	/// The runtime cannot run the operation, for one of these reasons: the installation's
	/// recorded policy does not allow it, for example because the runtime has no active use of
	/// the role that the operation needs; this runtime has stopped accepting operations; or
	/// reading the installation's control state or this runtime's registration failed, and
	/// `PgDocumentError::database_error` returns the database error.
	Admission,
	/// The scope's declared document code does not match the installation's recorded policy,
	/// for example because the code cannot read the stored format that the policy selects.
	ScopeBinding,
	/// The app's codec could not decode a stored document.
	Read,
	/// The app's codec could not encode the new document, or an index projector failed.
	Projection,
	/// The operation's initializer or mutation callback returned an error.
	Callback,
	/// The operation requires a live document, and the document is absent or expired at the
	/// database clock of the operation.
	NotLive,
	/// The expiry time that the TTL gives is too large for a stored timestamp.
	ExpirationOverflow,
	/// Reading or writing a stored record failed, for example because a database statement
	/// failed, other transactions kept changing the record until Paranoid used up its
	/// attempts, or a stored record is invalid. For a failed statement,
	/// `PgDocumentError::database_error` returns the database error.
	Storage,
	/// Beginning, committing, or rolling back the operation's own transaction failed.
	/// `PgDocumentError::database_error` returns the database error.
	Database,
	/// The transaction failed outside a database statement.
	Protocol,
}

/// Why the new document could not be encoded or indexed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PgDocumentProjectionReason {
	/// The app's codec could not encode the document.
	Codec,
	/// The app's codec could not decode the stored document that the operation re-indexes.
	Read,
	/// The document code does not declare an index that the installation maintains, or has no
	/// projector for it.
	Binding,
	/// An index projector returned an error.
	Projector,
	/// An index projector returned a number of fields other than its index declares.
	FieldCount,
	/// An index projector returned a field of a type other than its index declares.
	FieldType,
	/// The encoded document does not have the stored format that the installation's writer
	/// selects. This indicates a defect in Paranoid.
	WrittenRepresentationMismatch,
}

impl<C, P, F> PgDocumentError<C, P, F> {
	pub(super) fn new(fields: PgDocumentErrorFields<'_, C, P, F>) -> Self {
		Self {
			diagnostic: Box::new(DocumentDiagnostic {
				scope: fields.scope.clone(),
				schema: fields.schema,
				record: fields.record.clone(),
				source: fields.source,
			}),
		}
	}

	fn primary(&self) -> &NativeDocumentError<C, P, F> {
		let mut source = &self.diagnostic.source;
		while let NativeDocumentError::RollbackFailed {
			operation_error, ..
		} = source
		{
			source = operation_error;
		}
		source
	}
	fn mutation(&self) -> Option<&DocumentMutationError<C, P, F, StateStorageError>> {
		match self.primary() {
			NativeDocumentError::Mutation(source) => Some(source),
			_ => None,
		}
	}
	fn projection(&self) -> Option<&DocumentProjectionError<C, P>> {
		self.mutation()
			.and_then(DocumentMutationError::projection_error)
	}
	fn mutation_read(&self) -> Option<&ContractStateReadError<C>> {
		self.mutation().and_then(DocumentMutationError::read_error)
	}
	/// The operation's scope.
	pub fn scope(&self) -> &ScopeIdentity {
		&self.diagnostic.scope
	}
	/// The identifier of the scope's document schema.
	pub fn schema_identifier(&self) -> &'static str {
		self.diagnostic.schema
	}
	/// The record that the operation addressed. The record need not exist.
	pub fn record_identity(&self) -> &RecordIdentity {
		&self.diagnostic.record
	}
	/// The class of the failure.
	pub fn kind(&self) -> PgDocumentErrorKind {
		Self::operation_kind(&self.diagnostic.source)
	}
	fn operation_kind(source: &NativeDocumentError<C, P, F>) -> PgDocumentErrorKind {
		use PgDocumentErrorKind as Kind;
		match source {
			NativeDocumentError::Admission(_) => Kind::Admission,
			NativeDocumentError::Read(_) => Kind::Read,
			NativeDocumentError::Mutation(source) => Self::mutation_kind(source),
			NativeDocumentError::Storage(_) => Kind::Storage,
			NativeDocumentError::Database(_) => Kind::Database,
			NativeDocumentError::Protocol(_) => Kind::Protocol,
			NativeDocumentError::RollbackFailed {
				operation_error, ..
			} => Self::operation_kind(operation_error),
		}
	}
	fn mutation_kind<S>(source: &DocumentMutationError<C, P, F, S>) -> PgDocumentErrorKind {
		use PgDocumentErrorKind as Kind;
		match source {
			DocumentMutationError::Scope(_) => Kind::ScopeBinding,
			DocumentMutationError::Read(_) => Kind::Read,
			DocumentMutationError::Projection(_) => Kind::Projection,
			DocumentMutationError::Callback(_) => Kind::Callback,
			DocumentMutationError::NotLive => Kind::NotLive,
			DocumentMutationError::ExpirationOverflow => Kind::ExpirationOverflow,
			DocumentMutationError::Storage(_) => Kind::Storage,
			DocumentMutationError::CleanupFailed {
				operation_error, ..
			} => Self::mutation_kind(operation_error),
		}
	}

	/// Why the new document could not be encoded or indexed, when the kind is `Projection`.
	pub fn projection_reason(&self) -> Option<PgDocumentProjectionReason> {
		use PgDocumentProjectionReason as Reason;
		self.projection().map(|error| match error {
			DocumentProjectionError::Codec(_) => Reason::Codec,
			DocumentProjectionError::Read(_) => Reason::Read,
			DocumentProjectionError::Binding(_) => Reason::Binding,
			DocumentProjectionError::Projector { .. } => Reason::Projector,
			DocumentProjectionError::FieldCountMismatch { .. } => Reason::FieldCount,
			DocumentProjectionError::ScalarTypeMismatch { .. } => Reason::FieldType,
			DocumentProjectionError::WrittenRepresentationMismatch => {
				Reason::WrittenRepresentationMismatch
			}
		})
	}

	/// The codec failure, when the app's codec could not decode or encode a document.
	pub fn schema_error(&self) -> Option<&StateSchemaError<C>> {
		match self.primary() {
			NativeDocumentError::Read(source) => source.representation_error(),
			_ => match self.mutation_read() {
				Some(ContractStateReadError::Representation(source)) => Some(source),
				_ => self
					.projection()
					.and_then(DocumentProjectionError::codec_error),
			},
		}
	}
	/// The app codec's own error.
	pub fn codec_error(&self) -> Option<&C> {
		match self.schema_error() {
			Some(StateSchemaError::Codec(source)) => Some(source),
			_ => None,
		}
	}
	/// The index projector's own error.
	pub fn projector_error(&self) -> Option<&P> {
		self.projection()
			.and_then(DocumentProjectionError::projector_error)
	}
	/// The callback's own error.
	pub fn callback_error(&self) -> Option<&F> {
		self.mutation()
			.and_then(DocumentMutationError::callback_error)
	}
	/// The position of the failed index among the indexes that the installation maintains,
	/// when the failure concerns one index.
	pub fn index_position(&self) -> Option<usize> {
		self.projection()
			.and_then(DocumentProjectionError::index_position)
	}
	/// The position of the failed field, when an index projector returned a field of the wrong
	/// type.
	pub fn field_position(&self) -> Option<usize> {
		self.projection()
			.and_then(DocumentProjectionError::field_position)
	}

	fn rollback_errors(&self) -> impl Iterator<Item = &DatabaseError> {
		std::iter::successors(Some(&self.diagnostic.source), |error| match error {
			NativeDocumentError::RollbackFailed {
				operation_error, ..
			} => Some(operation_error.as_ref()),
			_ => None,
		})
		.filter_map(|error| match error {
			NativeDocumentError::RollbackFailed { rollback_error, .. } => Some(rollback_error),
			_ => None,
		})
	}
	pub(crate) fn involves_database_failure(&self) -> bool {
		self.diagnostic.source.involves_database_failure()
	}
	/// The database error of the original failure, or else of a failed cleanup, or else of the
	/// first failed rollback.
	pub fn database_error(&self) -> Option<&DatabaseError> {
		let primary = match self.primary() {
			NativeDocumentError::Admission(source) => source.database_error(),
			NativeDocumentError::Mutation(source) => source
				.primary_storage_error()
				.and_then(StateStorageError::database_error)
				.or_else(|| {
					source
						.cleanup_errors()
						.find_map(StateStorageError::database_error)
				}),
			NativeDocumentError::Storage(source) => source.database_error(),
			NativeDocumentError::Database(source) => Some(source),
			_ => None,
		};
		primary.or_else(|| self.rollback_errors().next())
	}
	/// Whether the transaction's own `COMMIT` failed, which means that the change may or may not
	/// have been stored.
	pub fn commit_outcome_is_unknown(&self) -> bool {
		self.diagnostic.source.commit_outcome_is_unknown()
	}
}

impl<C, P, F> fmt::Debug for PgDocumentError<C, P, F> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("PgDocumentError")
			.field("kind", &self.kind())
			.field("scope", self.scope())
			.finish_non_exhaustive()
	}
}
impl<C, P, F> fmt::Display for PgDocumentError<C, P, F> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("PostgreSQL document operation failed")
	}
}
impl<C: Error + 'static, P: Error + 'static, F: Error + 'static> Error
	for PgDocumentError<C, P, F>
{
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		Some(&self.diagnostic.source)
	}
}
