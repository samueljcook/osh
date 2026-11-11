//! One admitted raw Writer interval owns slot seeding, selection, and the actual ending.
use super::error::{NativeRawError, RawOperationFailure};
use super::store::NativeRawKvBindings;
use super::store::RawSourceRouteError;
use crate::db::{DatabaseError, WriteDatabase, run_transaction_write};
use crate::kv::raw::model::slot::{
	RawSlotCandidates, RawSlotRequest, RawSlotRequestParts, RawSlotResult, RawSlotSelectionRequest,
	RawSlotSelectionRequestParts, SlotRequestError,
};
use crate::kv::raw::model::slot_preparation::{
	EncryptedRawSlotRequest, EncryptedSlotFinishFields, RawSlotPreparationRejectionError,
	SlotPayload, finish_encrypted,
};
use crate::kv::raw::postgres::{
	delete::RawDeleteLayout,
	slot::{PgRawSlotSelection, RawSlotAcquisitionFields, RawSlotStorageError},
};
use crate::records::postgres::PgPointStorage;
use crate::records::{RecordIdentity, RecordNamespace};
use crate::runtime::admission::{NativeOperationError, PgNativeAdmission};
use crate::scope::{RawRole, RawScopeDeclaration};
use std::{num::NonZeroU32, sync::Arc};
pub(in crate::kv) type NativeRawSlotError = NativeRawError<RawSlotFailure>;

#[derive(Debug, thiserror::Error)]
pub(in crate::kv) enum RawSlotFailure {
	#[error("raw slot v10 route failed")]
	Route(#[from] RawSourceRouteError),
	#[error("raw slot source candidate is invalid")]
	SourceKey {
		identity: RecordIdentity,
		input_index: usize,
		#[source]
		source: Box<SlotRequestError>,
	},
	#[error("raw slot physical work failed")]
	Storage(#[from] RawSlotStorageError),
	#[error("raw slot selected-key preparation failed")]
	Preparation(#[source] Box<RawSlotPreparationRejectionError>),
}
impl RawOperationFailure for RawSlotFailure {
	const OPERATION: &'static str = "raw slot";
	fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Route(e) => e.database_error(),
			Self::Storage(e) => e.database_error(),
			Self::Preparation(e) => e
				.cleanup_error()
				.and_then(RawSlotStorageError::database_error),
			Self::SourceKey { .. } => None,
		}
	}
	fn involves_database_failure(&self) -> bool {
		self.database_error().is_some()
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Route(e) => e.commit_outcome_is_unknown(),
			Self::Storage(e) => e.commit_outcome_is_unknown(),
			Self::Preparation(e) => e
				.cleanup_error()
				.is_some_and(RawSlotStorageError::commit_outcome_is_unknown),
			Self::SourceKey { .. } => false,
		}
	}
	fn record_identity(&self) -> Option<&RecordIdentity> {
		match self {
			Self::Storage(e) => e.identity(),
			Self::Preparation(e) => Some(e.identity()),
			Self::SourceKey { identity, .. } => Some(identity),
			Self::Route(_) => None,
		}
	}
	fn input_index(&self) -> Option<usize> {
		match self {
			Self::Storage(e) => e.input_index(),
			Self::Preparation(e) => e.input_index(),
			Self::SourceKey { input_index, .. } => Some(*input_index),
			Self::Route(_) => None,
		}
	}
}
pub(super) async fn acquire_core(
	bindings: &Arc<NativeRawKvBindings>,
	database: impl WriteDatabase,
	request: RawSlotRequest,
) -> Result<RawSlotResult, NativeRawSlotError> {
	let RawSlotRequestParts { selection, value } = request.into_parts();
	acquire_core_prepared(bindings, database, selection, SlotPayload::Bytes(value)).await
}
pub(super) async fn acquire_encrypted_core(
	bindings: &Arc<NativeRawKvBindings>,
	database: impl WriteDatabase,
	request: EncryptedRawSlotRequest<'_>,
) -> Result<RawSlotResult, NativeRawSlotError> {
	let (selection, payload) = request.into_parts();
	acquire_core_prepared(bindings, database, selection, payload).await
}
async fn acquire_core_prepared(
	bindings: &Arc<NativeRawKvBindings>,
	database: impl WriteDatabase,
	selection: RawSlotSelectionRequest,
	payload: SlotPayload<'_>,
) -> Result<RawSlotResult, NativeRawSlotError> {
	if selection.is_empty() {
		return Ok(RawSlotResult::Empty);
	}
	let RawSlotSelectionRequestParts { candidates, ttl } = selection.into_parts();
	run_transaction_write(database, "kv.native_store.acquire_slot", async move |tx| {
		bindings
			.admission
			.execute_write_with_contracts(tx, async |contracts, tx| {
				let declaration = bindings.component.declaration();
				let _bound = contracts
					.bind(&declaration)
					.map_err(NativeOperationError::Admission)?;
				let result: Result<_, RawSlotFailure> = async {
					let layout = super::delete::core_placement(bindings, &contracts)?;
					let source = if matches!(
						layout,
						RawDeleteLayout::Source(_) | RawDeleteLayout::Relocating(_)
					) {
						Some(
							candidates
								.source_keys()
								.map_err(|e| source_error(e, &candidates, &bindings.namespace))?,
						)
					} else {
						None
					};
					let selected = PgRawSlotSelection::acquire(RawSlotAcquisitionFields {
						tx,
						layout,
						namespace: &bindings.namespace,
						candidates: &candidates,
						source_keys: source.as_ref(),
						ttl,
						conflict_attempts: bindings.conflict_attempts,
					})
					.await?;
					finish(selected, None, payload, ttl, &candidates).await
				}
				.await;
				Ok(result)
			})
			.await
			.map_err(NativeRawSlotError::from_operation)?
			.map_err(NativeRawError::Operation)
	})
	.await
}
pub(super) struct NativeRawScopeSlotFields<'a> {
	pub(super) declaration: &'a RawScopeDeclaration,
	pub(super) storage: &'a PgPointStorage,
	pub(super) admission: &'a PgNativeAdmission,
	pub(super) conflict_attempts: NonZeroU32,
}
pub(super) async fn acquire_scope(
	fields: NativeRawScopeSlotFields<'_>,
	database: impl WriteDatabase,
	request: RawSlotRequest,
) -> Result<RawSlotResult, NativeRawSlotError> {
	let RawSlotRequestParts { selection, value } = request.into_parts();
	acquire_scope_prepared(fields, database, selection, SlotPayload::Bytes(value)).await
}
pub(super) async fn acquire_encrypted_scope(
	fields: NativeRawScopeSlotFields<'_>,
	database: impl WriteDatabase,
	request: EncryptedRawSlotRequest<'_>,
) -> Result<RawSlotResult, NativeRawSlotError> {
	let (selection, payload) = request.into_parts();
	acquire_scope_prepared(fields, database, selection, payload).await
}
async fn acquire_scope_prepared(
	fields: NativeRawScopeSlotFields<'_>,
	database: impl WriteDatabase,
	selection: RawSlotSelectionRequest,
	payload: SlotPayload<'_>,
) -> Result<RawSlotResult, NativeRawSlotError> {
	if selection.is_empty() {
		return Ok(RawSlotResult::Empty);
	}
	let RawSlotSelectionRequestParts { candidates, ttl } = selection.into_parts();
	run_transaction_write(database, "kv.native_scope.acquire_slot", async move |tx| {
		fields
			.admission
			.execute_write_with_contracts(tx, async |contracts, tx| {
				let declaration = fields.declaration.declaration();
				let _bound = contracts
					.bind_raw_scope(fields.declaration, &declaration, RawRole::Writer)
					.map_err(NativeOperationError::Admission)?;
				let result: Result<_, RawSlotFailure> = async {
					let selected = PgRawSlotSelection::acquire(RawSlotAcquisitionFields {
						tx,
						layout: RawDeleteLayout::Native(fields.storage),
						namespace: fields.declaration.identity().namespace(),
						candidates: &candidates,
						source_keys: None,
						ttl,
						conflict_attempts: fields.conflict_attempts,
					})
					.await?;
					finish(
						selected,
						Some(fields.declaration.identity()),
						payload,
						ttl,
						&candidates,
					)
					.await
				}
				.await;
				Ok(result)
			})
			.await
			.map_err(NativeRawSlotError::from_operation)?
			.map_err(NativeRawError::Operation)
	})
	.await
}
async fn finish(
	selected: PgRawSlotSelection<'_, '_>,
	scope: Option<&crate::scope::ScopeIdentity>,
	payload: SlotPayload<'_>,
	ttl: crate::kv::Ttl,
	candidates: &RawSlotCandidates,
) -> Result<RawSlotResult, RawSlotFailure> {
	match selected {
		PgRawSlotSelection::Unavailable { observed_at } => {
			Ok(RawSlotResult::Unavailable { observed_at })
		}
		PgRawSlotSelection::Selected(selected) => match payload {
			SlotPayload::Bytes(value) => {
				let key = selected.identity().key().clone();
				let outcome = (*selected).write_bytes(value).await?;
				Ok(RawSlotResult::Acquired { key, outcome })
			}
			SlotPayload::PublishedItem(preparation) => {
				let input_index = candidates.input_index(selected.identity().key());
				finish_encrypted(EncryptedSlotFinishFields {
					selected: *selected,
					scope,
					ttl,
					preparation,
					input_index,
				})
				.await
			}
		},
	}
}
fn source_error(
	source: SlotRequestError,
	candidates: &RawSlotCandidates,
	namespace: &RecordNamespace,
) -> RawSlotFailure {
	let input_index = match &source {
		SlotRequestError::SourceUtf8 { input_index, .. }
		| SlotRequestError::SourceKey { input_index, .. } => *input_index,
		_ => {
			unreachable!("only source key parsing is performed for an admitted parsed slot request")
		}
	};
	let key = candidates
		.keys()
		.iter()
		.find(|key| candidates.input_index(key) == Some(input_index))
		.expect("source key parsing identifies an actual candidate");
	RawSlotFailure::SourceKey {
		identity: RecordIdentity::new(namespace.clone(), key.clone()),
		input_index,
		source: Box::new(source),
	}
}
