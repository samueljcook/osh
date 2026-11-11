//! Scope-admitted native metadata observations and physical deletion invoke no business code.

pub(in crate::kv::document::handle) mod error;
pub use error::{PgDocumentNamespaceError, PgDocumentNamespaceErrorKind};

use std::sync::Arc;

use crate::db::{Database, WriteDatabase, run_transaction_read, run_transaction_write};
use crate::kv::raw::model::aggregate::RecordLiveCount;
use crate::kv::raw::model::delete::{RecordDeleteBatchRequest, RecordDeleteBatchResult};
use crate::kv::raw::model::namespace::{RecordSelectionDeleteRequest, RecordSelectionDeleteResult};
use crate::kv::raw::model::page::{
	RawPageContext, RawPageOwner, RecordKeySelection, RecordPageRequest,
};
use crate::kv::raw::postgres::aggregate::RawCountSelection;
use crate::kv::raw::postgres::delete::{
	PgRawDeleteBatch, RawDeleteAcquisitionFields, RawDeleteLayout,
	namespace::{NativeNamespaceDeletionFields, PgRawNamespaceDeletion},
};
use crate::kv::raw::postgres::key_headers::KeyHeaderPageFields;
use crate::runtime::admission::NativeOperationError;

use super::PgDocument;
use error::{
	NamespaceRequest, NativeDocumentNamespaceError as Cause, PgDocumentNamespaceErrorFields,
};

pub(crate) use crate::kv::raw::model::page::RecordKeyPage;
pub(super) use crate::kv::raw::model::page::RecordKeyPageFields as DocumentKeyPageFields;

impl<T, C, P> PgDocument<'_, T, C, P> {
	/// Reads one page of the keys and metadata of stored documents, in key order.
	///
	/// Requires the `point_reader` role. The request's visibility selects live documents only
	/// or also expired documents. Slot reservations are never included. The operation checks
	/// each record's stored form and index entries but does not read or decode the stored
	/// documents. A full page can carry a cursor even when no later key exists, and each page
	/// reads the database at its own time.
	pub async fn read_keys(
		&self,
		database: impl Database,
		request: &RecordPageRequest,
	) -> Result<RecordKeyPage, PgDocumentNamespaceError> {
		let context = RawPageContext {
			installation: Arc::clone(self.admission.installation_binding()),
			owner: RawPageOwner::Scope(self.declaration.identity().clone()),
			table: self.storage.table().clone(),
			namespace: self.declaration.identity().namespace().clone(),
		};
		let result = async {
			request.require_context(&context).map_err(Cause::Request)?;
			run_transaction_read(database, "db.native_document.read_keys", async |tx| {
				self.admission
					.execute_read_with_contracts(tx, async |contracts, tx| {
						let _reader = contracts
							.bind_document_reader(&self.declaration)
							.map_err(NativeOperationError::Admission)?;
						let page = self
							.storage
							.read_key_headers(
								tx,
								KeyHeaderPageFields {
									namespace: self.declaration.identity().namespace(),
									page: request,
									selection: request.selection(),
								},
							)
							.await
							.map_err(|error| {
								NativeOperationError::Operation(Cause::Page(Box::new(error)))
							})?;
						Ok(RecordKeyPage::from_headers(DocumentKeyPageFields {
							page,
							request,
							context,
						}))
					})
					.await
					.map_err(Cause::from_operation)
			})
			.await
		}
		.await;
		result.map_err(|source| {
			PgDocumentNamespaceError::new(PgDocumentNamespaceErrorFields {
				scope: self.declaration.identity(),
				schema: self.declaration.document().schema().identifier(),
				request: NamespaceRequest::Selection(request.selection().clone()),
				source,
			})
		})
	}

	/// Counts the live documents of the selection at one database clock.
	///
	/// Requires the `point_reader` role. The operation does not decode the documents, so the
	/// count includes documents that the codec cannot decode. On an installation imported
	/// from v10, the count does not include v10 items.
	pub async fn count_live(
		&self,
		database: impl Database,
		selection: &RecordKeySelection,
	) -> Result<RecordLiveCount, PgDocumentNamespaceError> {
		let result = run_transaction_read(database, "db.native_document.count_live", async |tx| {
			self.admission
				.execute_read_with_contracts(tx, async |contracts, tx| {
					let _reader = contracts
						.bind_document_reader(&self.declaration)
						.map_err(NativeOperationError::Admission)?;
					self.storage
						.count_raw_live(
							tx,
							RawCountSelection {
								namespace: self.declaration.identity().namespace(),
								selection,
							},
						)
						.await
						.map_err(|error| {
							NativeOperationError::Operation(Cause::Count(Box::new(error)))
						})
				})
				.await
				.map_err(Cause::from_operation)
		})
		.await;
		result.map_err(|source| {
			PgDocumentNamespaceError::new(PgDocumentNamespaceErrorFields {
				scope: self.declaration.identity(),
				schema: self.declaration.document().schema().identifier(),
				request: NamespaceRequest::Selection(selection.clone()),
				source,
			})
		})
	}
	/// Deletes up to the request's limits of the selection's stored documents.
	///
	/// Requires the `producer` role. The operation does not decode the documents. It checks each
	/// record's stored form and index entries before it deletes the record. When the request
	/// deletes only expired documents, expiry is judged by the database clock after the locks, and
	/// records that another transaction has locked are skipped. A result with fewer deletions than
	/// the limit, or with skipped candidates, does not mean that the selection holds no more
	/// documents. When `database` is a caller's transaction, the change is durable only after the
	/// caller commits.
	pub async fn delete_batch(
		&self,
		database: impl WriteDatabase,
		request: &RecordDeleteBatchRequest,
	) -> Result<RecordDeleteBatchResult, PgDocumentNamespaceError> {
		let result =
			run_transaction_write(database, "db.native_document.delete_batch", async |tx| {
				self.admission
					.execute_write_with_contracts(tx, async |contracts, tx| {
						let _producer = contracts
							.bind_document_producer(&self.declaration)
							.map_err(NativeOperationError::Admission)?;
						let batch = PgRawDeleteBatch::acquire(RawDeleteAcquisitionFields {
							tx,
							layout: RawDeleteLayout::Native(&self.storage),
							namespace: self.declaration.identity().namespace(),
							request,
						})
						.await
						.map_err(|error| {
							NativeOperationError::Operation(Cause::Delete(Box::new(error)))
						})?;
						batch.delete().await.map_err(|error| {
							NativeOperationError::Operation(Cause::Delete(Box::new(error)))
						})
					})
					.await
					.map_err(Cause::from_operation)
			})
			.await;
		result.map_err(|source| {
			PgDocumentNamespaceError::new(PgDocumentNamespaceErrorFields {
				scope: self.declaration.identity(),
				schema: self.declaration.document().schema().identifier(),
				request: NamespaceRequest::DeleteBatch(request.clone()),
				source,
			})
		})
	}

	/// Deletes every stored document of the selection in one transaction, batch by batch, until a
	/// batch finds no document.
	///
	/// Requires the `producer` role. The operation does not decode the documents, and it waits for
	/// records that other transactions have locked. A document that another transaction inserts
	/// while the deletion runs can survive it. When the operation fails, the transaction must be
	/// rolled back. When `database` is a caller's transaction, the change is durable only after the
	/// caller commits.
	pub async fn delete_selection_atomically(
		&self,
		database: impl WriteDatabase,
		request: &RecordSelectionDeleteRequest,
	) -> Result<RecordSelectionDeleteResult, PgDocumentNamespaceError> {
		let result = run_transaction_write(
			database,
			"db.native_document.delete_selection_atomically",
			async |tx| {
				self.admission
					.execute_write_with_contracts(tx, async |contracts, tx| {
						let _producer = contracts
							.bind_document_producer(&self.declaration)
							.map_err(NativeOperationError::Admission)?;
						PgRawNamespaceDeletion::delete_native(NativeNamespaceDeletionFields {
							tx,
							storage: &self.storage,
							namespace: self.declaration.identity().namespace(),
							batch: request.batch(),
						})
						.await
						.map_err(|error| {
							NativeOperationError::Operation(Cause::DeleteNamespace(Box::new(error)))
						})
					})
					.await
					.map_err(Cause::from_operation)
			},
		)
		.await;
		result.map_err(|source| {
			PgDocumentNamespaceError::new(PgDocumentNamespaceErrorFields {
				scope: self.declaration.identity(),
				schema: self.declaration.document().schema().identifier(),
				request: NamespaceRequest::DeleteSelection(request.clone()),
				source,
			})
		})
	}
}
