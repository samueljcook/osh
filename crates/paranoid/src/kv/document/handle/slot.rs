//! One admitted native slot consumes an actual physical guard and selected Producer result.

use std::convert::Infallible;

use super::error::PgDocumentErrorFields;
use super::many::error::{NativeDocumentManyError as Cause, PgDocumentManyErrorFields};
use super::{NativeDocumentError, PgDocument, PgDocumentError, PgDocumentManyError};
use crate::db::{WriteDatabase, run_transaction_write};
use crate::kv::document::postgres::slot::{PgRecordSlotSelection, RecordSlotAcquisitionFields};
use crate::kv::document::state::slot::{
	DocumentSlotPreparationError, DocumentSlotPreparationFields, prepare_document_slot,
};
use crate::kv::document::state::{DocumentMutationError, DocumentSlotRequest, DocumentSlotResult};
use crate::runtime::admission::NativeOperationError;

impl<T, C, P> PgDocument<'_, T, C, P> {
	/// Writes the request's document under one candidate key that holds no live document and that
	/// no other transaction has locked. Paranoid tries the keys in byte order, not request order.
	///
	/// Requires the `producer` role. The operation skips candidates that another transaction has
	/// locked, and it can wait for another transaction's reservation of a candidate. `Unavailable`
	/// means that this attempt found no free candidate; a later attempt can succeed. The operation
	/// does not decode stored documents, and it leaves the other candidates unchanged. When
	/// `database` is a caller's transaction, the change is durable only after the caller commits.
	pub async fn acquire_slot(
		&self,
		database: impl WriteDatabase,
		request: DocumentSlotRequest<'_, T>,
	) -> Result<DocumentSlotResult, PgDocumentManyError<C, P>> {
		if request.is_empty() {
			return Ok(DocumentSlotResult::Empty);
		}
		let keys = request.keys_in_input_order();
		let result =
			run_transaction_write(database, "db.native_document.acquire_slot", async |tx| {
				self.admission
					.execute_write_with_contracts(tx, async |contracts, tx| {
						let producer = contracts
							.bind_document_producer(&self.declaration)
							.map_err(NativeOperationError::Admission)?;
						let result = async {
							let selected =
								PgRecordSlotSelection::acquire(RecordSlotAcquisitionFields {
									storage: &self.storage,
									tx,
									namespace: self.declaration.identity().namespace(),
									candidates: request.candidates(),
								})
								.await
								.map_err(|error| Cause::SlotStorage(Box::new(error)))?;
							let slot = match selected {
								PgRecordSlotSelection::Unavailable { observed_at } => {
									return Ok(DocumentSlotResult::Unavailable { observed_at });
								}
								PgRecordSlotSelection::Selected(slot) => slot,
							};
							let identity = slot.identity().clone();
							let input_index = slot.input_index();
							let prepared = prepare_document_slot(DocumentSlotPreparationFields {
								request: &request,
								producer: &producer,
								observed_at: slot.observed_at(),
							});
							let encoded = match prepared {
								Ok(encoded) => encoded,
								Err(source) => {
									let mutation = match source {
								DocumentSlotPreparationError::ExpirationOverflow => {
									DocumentMutationError::<C, P, Infallible, _>::ExpirationOverflow
								}
								DocumentSlotPreparationError::Projection(source) => {
									DocumentMutationError::Projection(*source)
								}
							};
									let operation = Cause::Member {
										input_index,
										source: Box::new(PgDocumentError::new(
											PgDocumentErrorFields {
												scope: self.declaration.identity(),
												schema: self
													.declaration
													.document()
													.schema()
													.identifier(),
												record: &identity,
												source: NativeDocumentError::Mutation(mutation),
											},
										)),
									};
									return Err(match slot.keep().await {
										Ok(()) => operation,
										Err(cleanup) => Cause::SlotCleanup {
											operation_error: Box::new(operation),
											cleanup_error: Box::new(cleanup),
										},
									});
								}
							};
							let metadata = encoded.record().metadata();
							slot.apply(encoded)
								.await
								.map_err(|error| Cause::SlotStorage(Box::new(error)))?;
							Ok(DocumentSlotResult::Written {
								key: identity.key().clone(),
								metadata,
							})
						}
						.await;
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
