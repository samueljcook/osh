//! One admitted fixed-key operation composes actual document code and neutral record storage.

pub(super) mod error;
pub use error::{PgDocumentManyError, PgDocumentManyErrorKind};

use std::convert::Infallible;

use crate::kv::document::state::batch::{
	DocumentBatchPreparationError, DocumentSetPreparationFields, prepare_scoped_document_set,
	read_scoped_documents,
};
use crate::kv::document::state::{
	DocumentManyReadRequest, DocumentManyReadResult, DocumentManySetRequest,
	DocumentManyWriteResult, DocumentMutationError,
};
use crate::records::postgres::record_many::write::{
	PgRecordManyWriteLock, RecordManyWriteAcquisition, RecordManyWriteLayout,
};

use super::error::PgDocumentErrorFields;
use super::{NativeDocumentError, PgDocument, PgDocumentError};
use crate::db::{Database, WriteDatabase, run_transaction_read, run_transaction_write};
use crate::records::postgres::record_many::RecordManyReadFields;
use crate::runtime::admission::NativeOperationError;
use error::{NativeDocumentManyError as Cause, PgDocumentManyErrorFields};

impl<T, C, P> PgDocument<'_, T, C, P> {
	/// Reads every requested key at one database clock.
	///
	/// Requires the `point_reader` role. The results are in the caller's order. An absent key,
	/// a slot reservation, and an expired document return no document. A record with an
	/// invalid stored form or a live document that the codec cannot decode fails the whole
	/// operation. An empty request does no database work and reports no database clock.
	pub async fn get_many(
		&self,
		database: impl Database,
		request: &DocumentManyReadRequest,
	) -> Result<DocumentManyReadResult<T>, PgDocumentManyError<C, P>> {
		if request.is_empty() {
			return Ok(DocumentManyReadResult::Empty);
		}
		let result = run_transaction_read(database, "db.native_document.get_many", async |tx| {
			self.admission
				.execute_read_with_contracts(tx, async |contracts, tx| {
					let reader = contracts
						.bind_document_reader(&self.declaration)
						.map_err(NativeOperationError::Admission)?;
					let observation = self
						.storage
						.read_many(
							tx,
							RecordManyReadFields {
								namespace: self.declaration.identity().namespace(),
								keys: request.keys(),
							},
						)
						.await
						.map_err(|error| {
							NativeOperationError::Operation(Cause::ReadStorage(Box::new(error)))
						})?;
					read_scoped_documents(
						&reader,
						request.keys(),
						observation.records,
						observation.observed_at,
					)
					.map_err(|failure| {
						let identity = self.identity(&request.keys()[failure.input_index]);
						NativeOperationError::Operation(Cause::Member {
							input_index: failure.input_index,
							source: Box::new(PgDocumentError::new(PgDocumentErrorFields {
								scope: self.declaration.identity(),
								schema: self.declaration.document().schema().identifier(),
								record: &identity,
								source: NativeDocumentError::Read(failure.source),
							})),
						})
					})
				})
				.await
				.map_err(Cause::from_operation)
		})
		.await;
		result.map_err(|source| {
			PgDocumentManyError::new(PgDocumentManyErrorFields {
				scope: self.declaration.identity(),
				schema: self.declaration.document().schema().identifier(),
				keys: request.keys().to_vec(),
				source,
			})
		})
	}

	/// Stores a document under each requested key and replaces any stored documents.
	///
	/// Requires the `producer` role. The operation locks every key, reads one database clock, and
	/// encodes every document and its index entries before it writes the first one. It does not
	/// decode the replaced documents. When an encoding fails, no document is written. When a later
	/// step fails, the transaction must be rolled back. When `database` is a caller's transaction,
	/// the change is durable only after the caller commits.
	pub async fn set_many(
		&self,
		database: impl WriteDatabase,
		request: DocumentManySetRequest<T>,
	) -> Result<DocumentManyWriteResult, PgDocumentManyError<C, P>> {
		if request.is_empty() {
			return Ok(DocumentManyWriteResult::Empty);
		}
		let keys = request.keys().to_vec();
		let result = run_transaction_write(database, "db.native_document.set_many", async |tx| {
			self.admission
				.execute_write_with_contracts(tx, async |contracts, tx| {
					let producer = contracts
						.bind_document_producer(&self.declaration)
						.map_err(NativeOperationError::Admission)?;
					let result = async {
						let group = PgRecordManyWriteLock::acquire(RecordManyWriteAcquisition {
							tx,
							layout: RecordManyWriteLayout::Native(&self.storage),
							namespace: self.declaration.identity().namespace(),
							keys: keys.clone(),
							source_texts: None,
						})
						.await
						.map_err(|error| Cause::WriteStorage(Box::new(error)))?;
						let prepared = prepare_scoped_document_set(DocumentSetPreparationFields {
							request,
							producer: &producer,
							namespace: self.declaration.identity().namespace(),
							observed_at: group.observed_at(),
						});
						let prepared = match prepared {
							Ok(prepared) => prepared,
							Err(source) => {
								let operation =
									match source {
										DocumentBatchPreparationError::ExpirationOverflow => {
											Cause::ExpirationOverflow
										}
										DocumentBatchPreparationError::Projection {
											input_index,
											record,
											source,
										} => Cause::Member {
											input_index,
											source: Box::new(PgDocumentError::new(
												PgDocumentErrorFields {
													scope: self.declaration.identity(),
													schema: self
														.declaration
														.document()
														.schema()
														.identifier(),
													record: &record,
													source:
														NativeDocumentError::Mutation(
															DocumentMutationError::<
																C,
																P,
																Infallible,
																_,
															>::Projection(*source),
														),
												},
											)),
										},
									};
								return Err(match group.keep().await {
									Ok(()) => operation,
									Err(cleanup) => Cause::Cleanup {
										operation_error: Box::new(operation),
										cleanup_error: Box::new(cleanup),
									},
								});
							}
						};
						group
							.replace(prepared.records)
							.await
							.map_err(|error| Cause::WriteStorage(Box::new(error)))?;
						Ok(prepared.outcome)
					}
					.await;
					// Completed pure rejection retains the same resolved reservation/admission boundary.
					Ok(result)
				})
				.await
				.map_err(Cause::from_operation)?
		})
		.await;
		result.map_err(|source| {
			PgDocumentManyError::new(PgDocumentManyErrorFields {
				scope: self.declaration.identity(),
				schema: self.declaration.document().schema().identifier(),
				keys,
				source,
			})
		})
	}
}
