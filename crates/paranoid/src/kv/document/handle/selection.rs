//! Admitted logical query selection and decoding of the same returned opaque bodies.
//!
//! Configured expressions and current logical Ready do not certify physical registration,
//! catalog validity, coverage or public availability. No readiness is created by this method.

use std::{fmt, time::Duration};

mod error;
pub use error::{
	PgDocumentSelectionError, PgDocumentSelectionErrorKind, PgDocumentSelectionProjectionReason,
};

use crate::kv::document::query::DocumentQueryCursor;
use crate::kv::document::state::DocumentRead;
use crate::records::{IndexScalar, RecordIdentity, StateTimestampMicros};
use crate::scope::ScopeReadError;

use super::PgDocument;
use crate::db::{Database, DatabaseError, TransactionFailure, run_transaction_read};
use crate::installation::ProtocolError;
use crate::kv::document::postgres::index::{PgDocumentQueryBinding, PgDocumentQueryBindingFields};
use crate::kv::document::postgres::selection::{
	PgDocumentSelectionFields, PgDocumentSelectionRequest, PgPhysicalDocumentSelectionError,
	select_opaque,
};
use crate::kv::document::query::ScopedDocumentIndexQuery;
use crate::runtime::admission::{NativeAdmissionError, NativeOperationError};

/// The inputs of an index query.
pub struct DocumentSelectFields<'query, 'a, 's, 'd, T, C, P> {
	/// The query, built with `PgDocument::query` on a handle of the same scope.
	pub query: &'query ScopedDocumentIndexQuery<'a, 's, 'd, T, C, P>,
	/// The maximum time that Paranoid waits for the query statement. It must be positive and does
	/// not include the policy check, decoding, or the transaction's end. When the wait elapses,
	/// the query fails with `StatementWaitElapsed`, and the statement can keep running in the
	/// database.
	pub statement_wait_timeout: Duration,
}

/// One document that a query returned, with its index values.
pub struct DocumentSelectionEntry<T> {
	identity: RecordIdentity,
	document: DocumentRead<T>,
	values: Vec<IndexScalar>,
}

/// One page of query results. Each page reads the database at its own time.
pub struct DocumentSelectionPage<T> {
	entries: Vec<DocumentSelectionEntry<T>>,
	observed_at: StateTimestampMicros,
	continuation: Option<DocumentQueryCursor>,
}

impl<T> DocumentSelectionEntry<T> {
	/// The document's record.
	pub fn identity(&self) -> &RecordIdentity {
		&self.identity
	}
	/// The decoded document and its metadata.
	pub fn document(&self) -> &DocumentRead<T> {
		&self.document
	}
	/// The document's values of the queried index's fields.
	pub fn values(&self) -> &[IndexScalar] {
		&self.values
	}
	/// Returns the record, the document, and the index values.
	pub fn into_parts(self) -> (RecordIdentity, DocumentRead<T>, Vec<IndexScalar>) {
		(self.identity, self.document, self.values)
	}
}
impl<T> DocumentSelectionPage<T> {
	/// The returned documents, in the query's order of index values and then keys.
	pub fn entries(&self) -> &[DocumentSelectionEntry<T>] {
		&self.entries
	}
	/// The database clock of the query statement, including for an empty page.
	pub fn observed_at(&self) -> StateTimestampMicros {
		self.observed_at
	}
	/// The cursor after the last returned document. A cursor does not mean that another
	/// document exists.
	pub fn continuation(&self) -> Option<&DocumentQueryCursor> {
		self.continuation.as_ref()
	}
	/// Returns the documents, the database clock, and the cursor.
	pub fn into_parts(
		self,
	) -> (
		Vec<DocumentSelectionEntry<T>>,
		StateTimestampMicros,
		Option<DocumentQueryCursor>,
	) {
		(self.entries, self.observed_at, self.continuation)
	}
}

impl<'a, T, C, P> PgDocument<'a, T, C, P> {
	/// Builds a query of one of the scope's indexes without database work.
	pub fn query(
		&self,
		fields: crate::kv::document::query::DocumentQueryFields,
	) -> Result<
		ScopedDocumentIndexQuery<'_, 'a, 'a, T, C, P>,
		crate::kv::document::query::DocumentQueryError,
	> {
		ScopedDocumentIndexQuery::new_for_index(&self.declaration, fields)
	}

	/// Runs an index query and returns one page of documents.
	///
	/// Requires an active use of the query, and the installation must record the index as
	/// ready for queries. The operation decodes each returned document once.
	pub async fn select(
		&self,
		database: impl Database,
		fields: DocumentSelectFields<'_, '_, 'a, 'a, T, C, P>,
	) -> Result<DocumentSelectionPage<T>, PgDocumentSelectionError<C>> {
		let query = fields.query;
		if self.declaration.identity() != query.declaration().identity() {
			return Err(self.selection_error(
				query,
				NativeDocumentSelectionError::DifferentDocumentBinding,
			));
		}
		let binding = PgDocumentQueryBinding::new(PgDocumentQueryBindingFields {
			declaration: &self.declaration,
			index: query.index(),
			storage: &self.storage,
		})
		.map_err(|error| {
			self.selection_error(
				query,
				NativeDocumentSelectionError::Selection(PgPhysicalDocumentSelectionError::Binding(
					error,
				)),
			)
		})?;
		let request = PgDocumentSelectionRequest::new(PgDocumentSelectionFields {
			binding: &binding,
			query,
			statement_wait_timeout: fields.statement_wait_timeout,
		})
		.map_err(|error| {
			self.selection_error(query, NativeDocumentSelectionError::Selection(error))
		})?;
		run_transaction_read(database, "db.native_document.select", async |tx| {
			self.admission
				.execute_read_with_contracts(tx, async |contracts, tx| {
					let reader = contracts
						.bind_document_query_reader(request.query())
						.map_err(NativeOperationError::Admission)?;
					reader
						.require_query(request.query())
						.map_err(|error| NativeOperationError::Admission(error.into()))?;
					let page = select_opaque(tx, &request).await.map_err(|error| {
						NativeOperationError::Operation(NativeDocumentSelectionError::Selection(
							error,
						))
					})?;
					let mut entries = Vec::with_capacity(page.entries.len());
					for entry in page.entries {
						let (_, value) =
							reader
								.decode_document(entry.record.body())
								.map_err(|error| {
									NativeOperationError::Operation(
										NativeDocumentSelectionError::Read {
											identity: entry.identity.clone(),
											error,
										},
									)
								})?;
						entries.push(DocumentSelectionEntry {
							identity: entry.identity,
							document: DocumentRead {
								value,
								metadata: entry.record.metadata(),
							},
							values: entry.values,
						});
					}
					Ok(DocumentSelectionPage {
						entries,
						observed_at: page.observed_at,
						continuation: page.continuation,
					})
				})
				.await
				.map_err(NativeDocumentSelectionError::from_operation)
		})
		.await
		.map_err(|error| self.selection_error(query, error))
	}
	fn selection_error(
		&self,
		query: &ScopedDocumentIndexQuery<'_, 'a, 'a, T, C, P>,
		source: NativeDocumentSelectionError<C>,
	) -> PgDocumentSelectionError<C> {
		PgDocumentSelectionError::new(error::SelectionErrorFields {
			scope: self.declaration.identity(),
			schema: self.declaration.document().schema().identifier(),
			index: query.index().identity(),
			source,
		})
	}
}

/// Arbitrary codec errors remain typed without bounds on values, codecs or projectors.
pub(in crate::kv) enum NativeDocumentSelectionError<C> {
	DifferentDocumentBinding,
	Admission(NativeAdmissionError),
	Selection(PgPhysicalDocumentSelectionError),
	Read {
		identity: RecordIdentity,
		error: ScopeReadError<C>,
	},
	Database(DatabaseError),
	Protocol(ProtocolError),
	RollbackFailed {
		operation_error: Box<Self>,
		rollback_error: DatabaseError,
	},
}

impl<C> NativeDocumentSelectionError<C> {
	fn from_operation(error: NativeOperationError<Self>) -> Self {
		match error {
			NativeOperationError::Admission(error) => Self::Admission(error),
			NativeOperationError::Operation(error) => error,
		}
	}
	fn description(&self) -> &'static str {
		match self {
			Self::DifferentDocumentBinding => {
				"document selection uses another actual declaration or container"
			}
			Self::Admission(_) => "document selection query admission failed",
			Self::Selection(_) => "document selection physical observation failed",
			Self::Read { .. } => "document selection retained decoder failed",
			Self::Database(_) => "document selection transaction database work failed",
			Self::Protocol(_) => "document selection transaction protocol failed",
			Self::RollbackFailed { .. } => "document selection operation and rollback both failed",
		}
	}
}

impl<C> fmt::Debug for NativeDocumentSelectionError<C> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(self.description())
	}
}
impl<C> fmt::Display for NativeDocumentSelectionError<C> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(self.description())
	}
}
impl<C: std::error::Error + 'static> std::error::Error for NativeDocumentSelectionError<C> {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::DifferentDocumentBinding => None,
			Self::Admission(error) => Some(error),
			Self::Selection(error) => Some(error),
			Self::Read { error, .. } => Some(error),
			Self::Database(error) => Some(error),
			Self::Protocol(error) => Some(error),
			Self::RollbackFailed {
				operation_error, ..
			} => Some(operation_error.as_ref()),
		}
	}
}

impl<C> TransactionFailure for NativeDocumentSelectionError<C> {
	fn from_protocol_error(error: ProtocolError) -> Self {
		Self::Protocol(error)
	}
	fn from_database_error(error: DatabaseError) -> Self {
		Self::Database(error)
	}
	fn from_rollback_failure(operation_error: Self, rollback_error: DatabaseError) -> Self {
		Self::RollbackFailed {
			operation_error: Box::new(operation_error),
			rollback_error,
		}
	}
	fn involves_database_failure(&self) -> bool {
		match self {
			Self::Admission(error) => error.involves_database_failure(),
			Self::Selection(error) => error.involves_database_failure(),
			Self::Database(_) | Self::RollbackFailed { .. } => true,
			Self::DifferentDocumentBinding | Self::Read { .. } | Self::Protocol(_) => false,
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Admission(error) => error.commit_outcome_is_unknown(),
			Self::Selection(error) => error.commit_outcome_is_unknown(),
			Self::Database(error) => error.commit_outcome_is_unknown(),
			Self::RollbackFailed {
				operation_error,
				rollback_error,
			} => {
				operation_error.commit_outcome_is_unknown()
					|| rollback_error.commit_outcome_is_unknown()
			}
			Self::DifferentDocumentBinding | Self::Read { .. } | Self::Protocol(_) => false,
		}
	}
}
