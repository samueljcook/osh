//! Parsed whole-transaction record deletion, without predicate exclusion or expiry decisions.
use super::delete::{
	RecordDeleteBatchLimits, RecordDeleteBatchLimitsFields, RecordDeleteBatchRequest,
	RecordDeleteBatchRequestFields, RecordDeleteContention, RecordDeleteKind,
	RecordDeleteRequestError,
};
use super::page::RecordKeySelection;
use std::num::NonZeroU32;
#[derive(Clone, Debug)]
/// A validated request that deletes every record in a selection in one transaction.
pub struct RecordSelectionDeleteRequest {
	batch: RecordDeleteBatchRequest,
}
/// The inputs of a request that deletes every record in a selection in one transaction.
pub struct RecordSelectionDeleteRequestFields {
	/// The whole namespace or a nonempty key prefix.
	pub selection: RecordKeySelection,
	/// The maximum number of records that one statement deletes. It must be positive.
	pub batch_size: NonZeroU32,
}
impl RecordSelectionDeleteRequest {
	/// Validates the request without database work.
	pub fn new(
		fields: RecordSelectionDeleteRequestFields,
	) -> Result<Self, RecordDeleteRequestError> {
		let limits = RecordDeleteBatchLimits::new(RecordDeleteBatchLimitsFields {
			max_deletions: fields.batch_size,
			max_candidates: fields.batch_size,
		})?;
		Ok(Self {
			batch: RecordDeleteBatchRequest::new(RecordDeleteBatchRequestFields {
				selection: fields.selection,
				kind: RecordDeleteKind::AllValued {
					contention: RecordDeleteContention::Wait,
				},
				limits,
			}),
		})
	}
	/// The keys that the request covers.
	pub fn selection(&self) -> &RecordKeySelection {
		self.batch.selection()
	}
	/// The largest number of records that one statement deletes.
	pub fn batch_size(&self) -> NonZeroU32 {
		self.batch.limits().max_candidates()
	}
	/// The largest number of times that a deletion on the relocating route tries to delete the
	/// whole selection.
	pub(crate) fn whole_set_attempts(&self) -> NonZeroU32 {
		crate::runtime::parameters::WHOLE_SET_ATTEMPTS
	}
	pub(crate) fn batch(&self) -> &RecordDeleteBatchRequest {
		&self.batch
	}
}
/// The result of a selection deletion. Every deletion commits or rolls back together, and a
/// record that another transaction inserts while the deletion runs can remain.
#[derive(Debug)]
pub struct RecordSelectionDeleteResult {
	deleted_count: u64,
	completed_attempt_count: u64,
}
pub(crate) struct RecordSelectionDeleteResultFields {
	pub(crate) deleted_count: u64,
	pub(crate) completed_attempt_count: u64,
}
impl RecordSelectionDeleteResult {
	pub(crate) fn new(fields: RecordSelectionDeleteResultFields) -> Self {
		Self {
			deleted_count: fields.deleted_count,
			completed_attempt_count: fields.completed_attempt_count,
		}
	}
	/// The number of deleted records.
	pub fn deleted_count(&self) -> u64 {
		self.deleted_count
	}
	/// The number of batches that the deletion ran. After a v10 retirement has stopped v10
	/// writes and before the retirement finishes, this is instead the number of times that the
	/// deletion tried to delete the whole selection.
	pub fn completed_attempt_count(&self) -> u64 {
		self.completed_attempt_count
	}
}
