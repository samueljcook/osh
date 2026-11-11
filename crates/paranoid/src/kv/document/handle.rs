//! Admitted typed document point operations in explicitly owned application scopes.
//!
//! Each operation authenticates its exact deployment roles under current control pins.
//! Application namespaces use configured native placement, including during source coexistence;
//! these handles do not infer assignments for published raw values or supply query readiness.

mod many;
pub use many::{PgDocumentManyError, PgDocumentManyErrorKind};
mod error;
mod namespace;
mod slot;
pub use namespace::{PgDocumentNamespaceError, PgDocumentNamespaceErrorKind};
pub(crate) mod selection;
pub use error::{PgDocumentError, PgDocumentErrorKind, PgDocumentProjectionReason};
pub use selection::{
	DocumentSelectFields, DocumentSelectionEntry, DocumentSelectionPage, PgDocumentSelectionError,
	PgDocumentSelectionErrorKind, PgDocumentSelectionProjectionReason,
};

use std::{convert::Infallible, fmt, num::NonZeroU32, sync::Arc};

use crate::kv::Ttl;
pub use crate::kv::document::state::DocumentConditionalWriteResult;
use crate::kv::document::state::DocumentMetadataChange;
use crate::kv::document::state::{
	self, DocumentLiveMutationCurrent, DocumentLiveOrInitMutationResult, DocumentMutation,
	DocumentMutationCurrent, DocumentMutationError, DocumentMutationOutcome,
	DocumentMutationResult, DocumentRead,
};
use crate::records::{
	RecordHeaderObservation, RecordIdentity, RecordKey, RecordMetadata, StateTimestampMicros,
};
use crate::scope::{DocumentScopeDeclaration, ScopeReadError};

use crate::db::{
	Database, DatabaseError, TransactionFailure, WriteDatabase, run_transaction_read,
	run_transaction_write,
};
use crate::installation::ProtocolError;
use crate::records::postgres::{PgPointStorage, StateStorageError};
use crate::runtime::admission::{NativeAdmissionError, NativeOperationError, PgNativeAdmission};

pub(crate) struct PgDocumentFields<'a, T, C, P> {
	pub(crate) declaration: DocumentScopeDeclaration<'a, 'a, T, C, P>,
	pub(crate) admission: Arc<PgNativeAdmission>,
	pub(crate) conflict_attempts: NonZeroU32,
}

/// The handle of one declared document scope, with documents of type `T`.
///
/// Each operation checks the installation's recorded policy for the scope and requires an
/// active use of the role that it needs: `point_reader` to read, `producer` to write, and
/// `maintenance` to change a document's metadata. An operation on a pool runs in its own
/// transaction; an operation on a caller's transaction leaves the commit to the caller.
pub struct PgDocument<'a, T, C, P> {
	declaration: DocumentScopeDeclaration<'a, 'a, T, C, P>,
	storage: PgPointStorage,
	admission: Arc<PgNativeAdmission>,
	conflict_attempts: NonZeroU32,
}

impl<'a, T, C, P> PgDocument<'a, T, C, P> {
	pub(crate) fn new(fields: PgDocumentFields<'a, T, C, P>) -> Result<Self, NativeAdmissionError> {
		let storage = fields
			.admission
			.app_storage(fields.declaration.identity())?;
		Ok(Self {
			declaration: fields.declaration,
			storage,
			admission: fields.admission,
			conflict_attempts: fields.conflict_attempts,
		})
	}

	fn identity(&self, key: &RecordKey) -> RecordIdentity {
		RecordIdentity::new(self.declaration.identity().namespace().clone(), key.clone())
	}

	/// Sets a live document's modification time to the database clock and keeps its stored document
	/// and expiry.
	///
	/// Requires the `maintenance` role. The operation decodes the stored document and updates its
	/// index entries; it does not encode the document again. An absent or expired document fails
	/// with the kind `NotLive`. When `database` is a caller's transaction, the change is durable
	/// only after the caller commits.
	pub async fn touch(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
	) -> Result<RecordMetadata, PgDocumentError<C, P, Infallible>> {
		self.update_metadata(database, key, DocumentMetadataChange::Touch)
			.await
	}

	/// Replaces a live document's expiry and keeps its stored document.
	///
	/// Requires the `maintenance` role. The TTL counts from the database clock of the operation.
	/// The operation decodes the stored document and updates its index entries. An absent or
	/// expired document fails with the kind `NotLive`. The document can expire before the
	/// transaction commits. When `database` is a caller's transaction, the change is durable only
	/// after the caller commits.
	pub async fn set_ttl(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		ttl: Ttl,
	) -> Result<RecordMetadata, PgDocumentError<C, P, Infallible>> {
		self.update_metadata(database, key, DocumentMetadataChange::SetTtl(ttl))
			.await
	}

	/// Expires a live document at the database clock of the operation and keeps its stored
	/// document.
	///
	/// Requires the `maintenance` role. The operation decodes the stored document and updates its
	/// index entries. An absent or expired document fails with the kind `NotLive`. When `database`
	/// is a caller's transaction, the change is durable only after the caller commits.
	pub async fn expire(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
	) -> Result<RecordMetadata, PgDocumentError<C, P, Infallible>> {
		self.update_metadata(database, key, DocumentMetadataChange::Expire)
			.await
	}

	async fn update_metadata(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		change: DocumentMetadataChange,
	) -> Result<RecordMetadata, PgDocumentError<C, P, Infallible>> {
		let identity = self.identity(key);
		let operation = match change {
			DocumentMetadataChange::Touch => "db.native_document.touch",
			DocumentMetadataChange::SetTtl(_) => "db.native_document.set_ttl",
			DocumentMetadataChange::Expire => "db.native_document.expire",
		};
		run_transaction_write(database, operation, async |tx| {
			self.admission
				.execute_write_with_contracts(tx, async |contracts, tx| {
					let maintenance = contracts
						.bind_document_maintenance(&self.declaration)
						.map_err(NativeOperationError::Admission)?;
					let result = async {
						let lock = self
							.storage
							.lock(tx, &identity, self.conflict_attempts)
							.await
							.map_err(NativeDocumentError::Storage)?;
						state::update_scoped_document_metadata(lock, &maintenance, change)
							.await
							.map_err(NativeDocumentError::Mutation)
					}
					.await;
					Ok(result)
				})
				.await
				.map_err(NativeDocumentError::from_operation)?
		})
		.await
		.map_err(|source| {
			PgDocumentError::new(error::PgDocumentErrorFields {
				scope: self.declaration.identity(),
				schema: self.declaration.document().schema().identifier(),
				record: &identity,
				source,
			})
		})
	}

	/// Reads a record's metadata and the database clock without reading the stored document.
	///
	/// Requires the `point_reader` role. The operation does not decode the document, so a
	/// result says nothing about whether the codec can read it. An expired record still
	/// reports its metadata. An absent key and a slot reservation report only the clock.
	pub async fn get_header_observation(
		&self,
		database: impl Database,
		key: &RecordKey,
	) -> Result<RecordHeaderObservation, PgDocumentError<C, P, Infallible>> {
		let identity = self.identity(key);
		run_transaction_read(
			database,
			"db.native_document.get_header_observation",
			async |tx| {
				self.admission
					.execute_read_with_contracts(tx, async |contracts, tx| {
						let _reader = contracts
							.bind_document_reader(&self.declaration)
							.map_err(NativeOperationError::Admission)?;
						self.storage
							.read_header(tx, &identity)
							.await
							.map_err(|error| {
								NativeOperationError::Operation(NativeDocumentError::Storage(error))
							})
					})
					.await
					.map_err(NativeDocumentError::from_operation)
			},
		)
		.await
		.map_err(|source| {
			PgDocumentError::new(error::PgDocumentErrorFields {
				scope: self.declaration.identity(),
				schema: self.declaration.document().schema().identifier(),
				record: &identity,
				source,
			})
		})
	}

	/// Stores a document when no live document has the key at the database clock of the
	/// operation.
	///
	/// Requires the `producer` role. A live document stays unchanged, and the operation does
	/// not decode it. An insertion writes the document and its index entries together.
	/// When `database` is a caller's transaction, the change is durable only after the
	/// caller commits.
	pub async fn set_if_absent(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		value: T,
		ttl: Ttl,
	) -> Result<DocumentConditionalWriteResult, PgDocumentError<C, P, Infallible>> {
		let identity = self.identity(key);
		run_transaction_write(database, "db.native_document.set_if_absent", async |tx| {
			self.admission
				.execute_write_with_contracts(tx, async |contracts, tx| {
					let producer = contracts
						.bind_document_producer(&self.declaration)
						.map_err(NativeOperationError::Admission)?;
					let result = async {
						let lock = self
							.storage
							.lock_header(tx, &identity, self.conflict_attempts)
							.await
							.map_err(NativeDocumentError::Storage)?;
						state::set_scoped_document_if_absent(lock, &producer, value, ttl)
							.await
							.map_err(NativeDocumentError::Mutation)
					}
					.await;
					Ok(result)
				})
				.await
				.map_err(NativeDocumentError::from_operation)?
		})
		.await
		.map_err(|source| {
			PgDocumentError::new(error::PgDocumentErrorFields {
				scope: self.declaration.identity(),
				schema: self.declaration.document().schema().identifier(),
				record: &identity,
				source,
			})
		})
	}

	/// Deletes the stored document of a key, including an expired document and a document that
	/// the codec cannot decode, and returns whether a document was stored.
	///
	/// Requires the `producer` role. The operation does not decode the document. An absent key
	/// and a slot reservation return `false`. A record with an invalid stored form fails.
	/// When `database` is a caller's transaction, the change is durable only after the
	/// caller commits.
	pub async fn delete(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
	) -> Result<bool, PgDocumentError<C, P, Infallible>> {
		let identity = self.identity(key);
		run_transaction_write(database, "db.native_document.delete", async |tx| {
			self.admission
				.execute_write_with_contracts(tx, async |contracts, tx| {
					let _producer = contracts
						.bind_document_producer(&self.declaration)
						.map_err(NativeOperationError::Admission)?;
					let result = async {
						let lock = self
							.storage
							.lock_header(tx, &identity, self.conflict_attempts)
							.await
							.map_err(NativeDocumentError::Storage)?;
						lock.delete().await.map_err(NativeDocumentError::Storage)
					}
					.await;
					Ok(result)
				})
				.await
				.map_err(NativeDocumentError::from_operation)?
		})
		.await
		.map_err(|source| {
			PgDocumentError::new(error::PgDocumentErrorFields {
				scope: self.declaration.identity(),
				schema: self.declaration.document().schema().identifier(),
				record: &identity,
				source,
			})
		})
	}

	/// Reads one live document and its metadata.
	///
	/// Requires the `point_reader` role. An absent or expired document returns `None`.
	pub async fn get(
		&self,
		database: impl Database,
		key: &RecordKey,
	) -> Result<Option<DocumentRead<T>>, PgDocumentError<C, P, Infallible>> {
		let identity = self.identity(key);
		run_transaction_read(database, "db.native_document.get", async |tx| {
			self.admission
				.execute_read_with_contracts(tx, async |contracts, tx| {
					let reader = contracts
						.bind_document_reader(&self.declaration)
						.map_err(NativeOperationError::Admission)?;
					let result = async {
						let snapshot = self
							.storage
							.read(tx, &identity)
							.await
							.map_err(NativeDocumentError::Storage)?;
						state::decode_scoped_document(&snapshot, &reader)
							.map_err(NativeDocumentError::Read)
					}
					.await;
					result.map_err(NativeOperationError::Operation)
				})
				.await
				.map_err(NativeDocumentError::from_operation)
		})
		.await
		.map_err(|source| {
			PgDocumentError::new(error::PgDocumentErrorFields {
				scope: self.declaration.identity(),
				schema: self.declaration.document().schema().identifier(),
				record: &identity,
				source,
			})
		})
	}

	/// Stores a document under a key and replaces any stored document.
	///
	/// Requires the `producer` role. The operation does not decode the replaced document. It
	/// encodes the new document in the stored format that the installation's writer selects
	/// and writes its index entries. The TTL counts from the database clock of the operation.
	/// When `database` is a caller's transaction, the change is durable only after the
	/// caller commits.
	pub async fn set(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		value: T,
		ttl: Ttl,
	) -> Result<DocumentMutationOutcome, PgDocumentError<C, P, Infallible>> {
		let identity = self.identity(key);
		run_transaction_write(database, "db.native_document.set", async |tx| {
			self.admission
				.execute_write_with_contracts(tx, async |contracts, tx| {
					let producer = contracts
						.bind_document_producer(&self.declaration)
						.map_err(NativeOperationError::Admission)?;
					let result = async {
						let lock = self
							.storage
							.lock_header(tx, &identity, self.conflict_attempts)
							.await
							.map_err(NativeDocumentError::Storage)?;
						state::set_scoped_document(lock, &producer, value, ttl)
							.await
							.map_err(NativeDocumentError::Mutation)
					}
					.await;
					Ok(result)
				})
				.await
				.map_err(NativeDocumentError::from_operation)?
		})
		.await
		.map_err(|source| {
			PgDocumentError::new(error::PgDocumentErrorFields {
				scope: self.declaration.identity(),
				schema: self.declaration.document().schema().identifier(),
				record: &identity,
				source,
			})
		})
	}

	/// Locks one key, passes its live document to `decide_mutation_fn`, and applies the decision.
	///
	/// Requires the `point_reader` and `producer` roles. `decide_mutation_fn` receives the live
	/// document, or `None` when the document is absent or expired, and returns whether to keep,
	/// delete, or replace it. The result holds the previous live document. When `database` is a
	/// caller's transaction, the change is durable only after the caller commits.
	pub async fn mutate<F>(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		decide_mutation_fn: impl for<'current> FnOnce(
			DocumentMutationCurrent<'current, T>,
		) -> Result<DocumentMutation<T>, F>,
	) -> Result<DocumentMutationResult<T>, PgDocumentError<C, P, F>> {
		let identity = self.identity(key);
		run_transaction_write(database, "db.native_document.mutate", async |tx| {
			self.admission
				.execute_write_with_contracts(tx, async |contracts, tx| {
					let reader = contracts
						.bind_document_reader(&self.declaration)
						.map_err(NativeOperationError::Admission)?;
					let producer = contracts
						.bind_document_producer(&self.declaration)
						.map_err(NativeOperationError::Admission)?;
					let result = async {
						let lock = self
							.storage
							.lock(tx, &identity, self.conflict_attempts)
							.await
							.map_err(NativeDocumentError::Storage)?;
						state::mutate_scoped_document(lock, &reader, &producer, decide_mutation_fn)
							.await
							.map_err(NativeDocumentError::Mutation)
					}
					.await;
					Ok(result)
				})
				.await
				.map_err(NativeDocumentError::from_operation)?
		})
		.await
		.map_err(|source| {
			PgDocumentError::new(error::PgDocumentErrorFields {
				scope: self.declaration.identity(),
				schema: self.declaration.document().schema().identifier(),
				record: &identity,
				source,
			})
		})
	}

	/// Locks one key, creates its document with `initialize_value_fn` when no live document exists,
	/// passes the live or new document to `decide_mutation_fn`, and applies the decision.
	///
	/// Requires the `point_reader` and `producer` roles. `initialize_value_fn` receives the
	/// database clock of the operation. A live document that the codec cannot decode fails the
	/// operation, and no new document is created. The result holds the document that the decision
	/// received. When `database` is a caller's transaction, the change is durable only after the
	/// caller commits.
	///
	/// After initialization, `KeepExisting` stores the created document with the initializer's
	/// TTL, `SetValuePreservingExpiration` stores the new document with the initializer's TTL,
	/// `SetValue` uses its own TTL, and `Delete` stores nothing.
	pub async fn mutate_live_or_init<F>(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		initialize_value_fn: impl FnOnce(StateTimestampMicros) -> Result<(T, Ttl), F>,
		decide_mutation_fn: impl for<'current> FnOnce(
			DocumentLiveMutationCurrent<'current, T>,
		) -> Result<DocumentMutation<T>, F>,
	) -> Result<DocumentLiveOrInitMutationResult<T>, PgDocumentError<C, P, F>> {
		let identity = self.identity(key);
		run_transaction_write(
			database,
			"db.native_document.mutate_live_or_init",
			async |tx| {
				self.admission
					.execute_write_with_contracts(tx, async |contracts, tx| {
						let reader = contracts
							.bind_document_reader(&self.declaration)
							.map_err(NativeOperationError::Admission)?;
						let producer = contracts
							.bind_document_producer(&self.declaration)
							.map_err(NativeOperationError::Admission)?;
						let result = async {
							let lock = self
								.storage
								.lock(tx, &identity, self.conflict_attempts)
								.await
								.map_err(NativeDocumentError::Storage)?;
							state::mutate_scoped_document_live_or_init(
								lock,
								&reader,
								&producer,
								initialize_value_fn,
								decide_mutation_fn,
							)
							.await
							.map_err(NativeDocumentError::Mutation)
						}
						.await;
						Ok(result)
					})
					.await
					.map_err(NativeDocumentError::from_operation)?
			},
		)
		.await
		.map_err(|source| {
			PgDocumentError::new(error::PgDocumentErrorFields {
				scope: self.declaration.identity(),
				schema: self.declaration.document().schema().identifier(),
				record: &identity,
				source,
			})
		})
	}
}

/// Typed operation causes without Debug bounds or diagnostic exposure of application values.
pub(in crate::kv) enum NativeDocumentError<C, P, F> {
	Admission(NativeAdmissionError),
	Read(ScopeReadError<C>),
	Mutation(DocumentMutationError<C, P, F, StateStorageError>),
	Storage(StateStorageError),
	Database(DatabaseError),
	Protocol(ProtocolError),
	RollbackFailed {
		operation_error: Box<Self>,
		rollback_error: DatabaseError,
	},
}

impl<C, P, F> NativeDocumentError<C, P, F> {
	fn from_operation(error: NativeOperationError<Self>) -> Self {
		match error {
			NativeOperationError::Admission(error) => Self::Admission(error),
			NativeOperationError::Operation(error) => error,
		}
	}
	fn description(&self) -> &'static str {
		match self {
			Self::Admission(_) => "document scope admission failed",
			Self::Read(_) => "document scope retained read failed",
			Self::Mutation(_) => "document scope mutation failed",
			Self::Storage(_) => "document scope storage failed",
			Self::Database(_) => "document scope database work failed",
			Self::Protocol(_) => "document scope transaction protocol failed",
			Self::RollbackFailed { .. } => "document scope work and rollback both failed",
		}
	}
}

impl<C, P, F> fmt::Debug for NativeDocumentError<C, P, F> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(self.description())
	}
}
impl<C, P, F> fmt::Display for NativeDocumentError<C, P, F> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(self.description())
	}
}
impl<C, P, F> std::error::Error for NativeDocumentError<C, P, F>
where
	C: std::error::Error + 'static,
	P: std::error::Error + 'static,
	F: std::error::Error + 'static,
{
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Admission(error) => Some(error),
			Self::Read(error) => Some(error),
			Self::Mutation(error) => Some(error),
			Self::Storage(error) => Some(error),
			Self::Database(error) => Some(error),
			Self::Protocol(error) => Some(error),
			Self::RollbackFailed {
				operation_error, ..
			} => Some(operation_error.as_ref()),
		}
	}
}

fn mutation_storage_failure<C, P, F, S>(
	error: &DocumentMutationError<C, P, F, S>,
	check_fn: fn(&S) -> bool,
) -> bool {
	match error {
		DocumentMutationError::Storage(error) => check_fn(error),
		DocumentMutationError::CleanupFailed {
			operation_error,
			cleanup_error,
		} => mutation_storage_failure(operation_error, check_fn) || check_fn(cleanup_error),
		DocumentMutationError::Scope(_)
		| DocumentMutationError::Read(_)
		| DocumentMutationError::Projection(_)
		| DocumentMutationError::Callback(_)
		| DocumentMutationError::NotLive
		| DocumentMutationError::ExpirationOverflow => false,
	}
}

impl<C, P, F> TransactionFailure for NativeDocumentError<C, P, F> {
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
			Self::Mutation(error) => {
				mutation_storage_failure(error, |error| error.database_error().is_some())
			}
			Self::Storage(error) => error.database_error().is_some(),
			Self::Database(_) | Self::RollbackFailed { .. } => true,
			Self::Read(_) | Self::Protocol(_) => false,
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Admission(error) => error.commit_outcome_is_unknown(),
			Self::Mutation(error) => mutation_storage_failure(error, |error| {
				error
					.database_error()
					.is_some_and(DatabaseError::commit_outcome_is_unknown)
			}),
			Self::Storage(error) => error
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
			Self::Database(error) => error.commit_outcome_is_unknown(),
			Self::RollbackFailed {
				operation_error,
				rollback_error,
			} => {
				operation_error.commit_outcome_is_unknown()
					|| rollback_error.commit_outcome_is_unknown()
			}
			Self::Read(_) | Self::Protocol(_) => false,
		}
	}
}
