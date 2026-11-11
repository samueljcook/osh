//! One admitted transaction owns all deletion windows or a complete relocation attempt.
mod attempt;
mod collection;
mod error;
use super::super::slot::RawRejectionSettlement;
use super::super::{PgPointStorage, StateMutationCompletion, WriteTx};
use super::{
	PgRawDeleteBatch, RawDeleteAcquisitionFields, RawDeleteLayout, RawDeleteStorageError, apply,
	discovery, records::RawDeleteRecords,
};
use crate::kv::raw::model::delete::RecordDeleteBatchRequest;
use crate::kv::raw::model::namespace::{
	RecordSelectionDeleteRequest, RecordSelectionDeleteResult, RecordSelectionDeleteResultFields,
};
use crate::kv::raw::model::page::RecordKeySelection;
use crate::records::{RecordKey, RecordNamespace};
pub(crate) use error::RawNamespaceStorageError;
pub(crate) struct RawNamespaceDeletionFields<'a, 'tx> {
	pub(crate) tx: &'a mut WriteTx<'tx>,
	pub(crate) layout: RawDeleteLayout<'a>,
	pub(crate) namespace: &'a RecordNamespace,
	pub(crate) request: &'a RecordSelectionDeleteRequest,
}
impl<'a> RawNamespaceDeletionFields<'a, '_> {
	fn physical_selection(&self) -> &'a RecordKeySelection {
		self.request.selection()
	}
}
/// Fixed native single-location input; no relocation retry choice is accepted.
pub(crate) struct NativeNamespaceDeletionFields<'a, 'tx> {
	pub(crate) tx: &'a mut WriteTx<'tx>,
	pub(crate) storage: &'a PgPointStorage,
	pub(crate) namespace: &'a RecordNamespace,
	pub(crate) batch: &'a RecordDeleteBatchRequest,
}
struct SingleLocationDeletionFields<'a, 'tx> {
	tx: &'a mut WriteTx<'tx>,
	layout: RawDeleteLayout<'a>,
	namespace: &'a RecordNamespace,
	batch: &'a RecordDeleteBatchRequest,
}
pub(crate) struct PgRawNamespaceDeletion<'a, 'tx> {
	fields: RawNamespaceDeletionFields<'a, 'tx>,
	completion: StateMutationCompletion,
}
impl<'a, 'tx> PgRawNamespaceDeletion<'a, 'tx> {
	/// Owns the same native deletion loop without a source retry input.
	pub(crate) async fn delete_native(
		fields: NativeNamespaceDeletionFields<'a, 'tx>,
	) -> Result<RecordSelectionDeleteResult, RawNamespaceStorageError> {
		let mut completion = fields.tx.track_state_mutation();
		let result = delete_single_location(SingleLocationDeletionFields {
			tx: fields.tx,
			layout: RawDeleteLayout::Native(fields.storage),
			namespace: fields.namespace,
			batch: fields.batch,
		})
		.await;
		if result.is_ok() {
			completion.complete();
		}
		result
	}

	pub(crate) fn new(fields: RawNamespaceDeletionFields<'a, 'tx>) -> Self {
		let completion = fields.tx.track_state_mutation();
		Self { fields, completion }
	}
	pub(crate) async fn delete(
		self,
	) -> Result<RecordSelectionDeleteResult, RawNamespaceStorageError> {
		let Self {
			fields,
			mut completion,
		} = self;
		let result = if matches!(fields.layout, RawDeleteLayout::Relocating(_)) {
			attempt::delete(fields).await
		} else {
			delete_single_location(SingleLocationDeletionFields {
				tx: fields.tx,
				layout: fields.layout,
				namespace: fields.namespace,
				batch: fields.request.batch(),
			})
			.await
		};
		if result.is_ok()
			|| result.as_ref().is_err_and(|error| {
				matches!(error.settlement(), RawRejectionSettlement::KnownRestored)
			}) {
			completion.complete();
		}
		result
	}
}
/// Each window starts after the greatest candidate key of the previous window. The transaction
/// keeps the rows that earlier windows deleted in the key index until it ends, so a window that
/// started at the beginning of the selection would visit all of them again. A window waits for
/// each candidate and may delete as many records as it has candidates, so it retains only
/// candidates that another transaction removed, and no later window needs them.
async fn delete_single_location(
	fields: SingleLocationDeletionFields<'_, '_>,
) -> Result<RecordSelectionDeleteResult, RawNamespaceStorageError> {
	let mut deleted = 0u64;
	let mut windows = 0u64;
	let mut after: Option<RecordKey> = None;
	loop {
		windows = windows
			.checked_add(1)
			.ok_or(RawNamespaceStorageError::CountUnrepresentable)?;
		let mut batch = PgRawDeleteBatch::acquire_namespace_window(
			RawDeleteAcquisitionFields {
				tx: fields.tx,
				layout: fields.layout,
				namespace: fields.namespace,
				request: fields.batch,
			},
			after.as_ref(),
		)
		.await?;
		let prepared = apply::prepare(&batch, None)?;
		let count = u64::try_from(prepared.selection.deleted.len())
			.map_err(|_| RawNamespaceStorageError::CountUnrepresentable)?;
		let next_deleted = deleted
			.checked_add(count)
			.ok_or(RawNamespaceStorageError::CountUnrepresentable)?;
		let selection = apply::apply(&mut batch, prepared).await?;
		batch.completion.complete();
		deleted = next_deleted;
		if let Some(greatest) = batch.records.positions.keys().next_back() {
			after = Some(greatest.clone());
		}
		if !selection.more_candidates && selection.retained.is_empty() {
			return Ok(RecordSelectionDeleteResult::new(
				RecordSelectionDeleteResultFields {
					deleted_count: deleted,
					completed_attempt_count: windows,
				},
			));
		}
	}
}
impl<'a, 'tx> PgRawDeleteBatch<'a, 'tx, ()> {
	async fn acquire_namespace_window(
		fields: RawDeleteAcquisitionFields<'a, 'tx>,
		after: Option<&RecordKey>,
	) -> Result<Self, RawDeleteStorageError> {
		use crate::kv::raw::model::delete::{RecordDeleteContention, RecordDeleteKind};
		let completion = fields.tx.track_state_mutation();
		if !matches!(
			fields.request.kind(),
			RecordDeleteKind::AllValued {
				contention: RecordDeleteContention::Wait
			}
		) || fields.request.limits().max_deletions() != fields.request.limits().max_candidates()
			|| matches!(fields.layout, RawDeleteLayout::Relocating(_))
		{
			return Err(RawDeleteStorageError::Shape);
		}
		let mut batch = Self {
			tx: fields.tx,
			layout: fields.layout,
			records: RawDeleteRecords::new(fields.namespace.clone()),
			request: fields.request,
			more: false,
			completion,
			observed_at: (),
		};
		discovery::discover_window(&mut batch, after).await?;
		Ok(batch)
	}
}
