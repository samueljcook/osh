//! One admitted write interval owns every member and every physical route of a raw batch.

use super::{
	Arc, InstallationStorageRoute, NativeOperationError, NativeRawError, NativeRawKvBindings,
	NativeRawManyError, NativeRawScopeManyFields, RawManyFailure, RawRole, RecordIdentity,
};
use crate::db::{WriteDatabase, run_transaction_write};
use crate::kv::raw::model::many::{RawManySetRequest, RawManyWriteResult};
use crate::records::postgres::record_many::write::{
	PgRecordManyWriteLock, RecordManyWriteAcquisition, RecordManyWriteLayout,
};

pub(in crate::kv) async fn write_core(
	bindings: &Arc<NativeRawKvBindings>,
	database: impl WriteDatabase,
	request: RawManySetRequest,
) -> Result<RawManyWriteResult, NativeRawManyError> {
	if request.is_empty() {
		return Ok(RawManyWriteResult::Empty);
	}
	run_transaction_write(database, "kv.native_store.set_many", async |tx| {
		bindings
			.admission
			.execute_write_with_contracts(tx, async |contracts, tx| {
				let declaration = bindings.component.declaration();
				let _bound = contracts
					.bind(&declaration)
					.map_err(NativeOperationError::Admission)?;
				let result: Result<_, RawManyFailure> = async {
					let (layout, texts) = match contracts.storage_route() {
						InstallationStorageRoute::NativeFresh
						| InstallationStorageRoute::NativeImported => {
							(RecordManyWriteLayout::Native(&bindings.storage), None)
						}
						route @ (InstallationStorageRoute::SourceCoexistence
						| InstallationStorageRoute::SourceRelocation) => {
							let source = bindings.source_storage()?;
							let texts = request.source_texts().map_err(|error| {
								RawManyFailure::SourceKey {
									identity: RecordIdentity::new(
										bindings.namespace.clone(),
										error.key().clone(),
									),
									source: Box::new(error),
								}
							})?;
							let layout = if route == InstallationStorageRoute::SourceCoexistence {
								RecordManyWriteLayout::Source(source.source())
							} else {
								RecordManyWriteLayout::Relocating(source.relocation())
							};
							(layout, Some(texts))
						}
					};
					let lock = PgRecordManyWriteLock::acquire(RecordManyWriteAcquisition {
						tx,
						layout,
						namespace: &bindings.namespace,
						keys: request.keys().to_vec(),
						source_texts: texts,
					})
					.await?;
					replace(lock, request).await.map_err(RawManyFailure::from)
				}
				.await;
				Ok(result)
			})
			.await
			.map_err(NativeRawManyError::from_operation)?
			.map_err(NativeRawError::Operation)
	})
	.await
}
pub(in crate::kv) async fn write_scope(
	fields: NativeRawScopeManyFields<'_>,
	database: impl WriteDatabase,
	request: RawManySetRequest,
) -> Result<RawManyWriteResult, NativeRawManyError> {
	if request.is_empty() {
		return Ok(RawManyWriteResult::Empty);
	}
	run_transaction_write(database, "kv.native_scope.set_many", async |tx| {
		fields
			.admission
			.execute_write_with_contracts(tx, async |contracts, tx| {
				let declaration = fields.declaration.declaration();
				let _bound = contracts
					.bind_raw_scope(fields.declaration, &declaration, RawRole::Writer)
					.map_err(NativeOperationError::Admission)?;
				let result: Result<_, RawManyFailure> = async {
					let lock = PgRecordManyWriteLock::acquire(RecordManyWriteAcquisition {
						tx,
						layout: RecordManyWriteLayout::Native(fields.storage),
						namespace: fields.declaration.identity().namespace(),
						keys: request.keys().to_vec(),
						source_texts: None,
					})
					.await?;
					replace(lock, request).await.map_err(RawManyFailure::from)
				}
				.await;
				Ok(result)
			})
			.await
			.map_err(NativeRawManyError::from_operation)?
			.map_err(NativeRawError::Operation)
	})
	.await
}

async fn replace(
	lock: PgRecordManyWriteLock<'_, '_>,
	request: RawManySetRequest,
) -> Result<RawManyWriteResult, crate::records::postgres::record_many::write::RecordManyWriteError>
{
	use crate::kv::raw::model::many::{RawWritePreparationFields, prepare_raw_write};
	let outcome = match prepare_raw_write(RawWritePreparationFields {
		observed_at: lock.observed_at(),
		ttl: request.ttl(),
	}) {
		Ok(outcome) => outcome,
		Err(error) => return Err(lock.reject(error.into()).await),
	};
	let count = request.keys().len();
	let records = request
		.into_values()
		.into_iter()
		.map(|body| outcome.encode(body))
		.collect();
	lock.replace(records).await?;
	Ok(RawManyWriteResult::Written { count, outcome })
}
