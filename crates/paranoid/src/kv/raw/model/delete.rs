//! Parsed bounded deletion requests and independent selection results.

use crate::kv::{MAX_DELETE_BATCH_SIZE, raw::model::page::RecordKeySelection};
use crate::records::{RecordKey, StateTimestampMicros};
use std::num::NonZeroU32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Which records a deletion batch deletes.
pub enum RecordDeleteKind {
	/// Only records that are expired at the batch's database time. The batch skips a record
	/// that another transaction has locked and reports it in the outcome's kept records.
	Expired,
	/// Every record that has a value, live or expired.
	AllValued {
		/// What the batch does with a record that another transaction has locked.
		contention: RecordDeleteContention,
	},
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// What an `AllValued` deletion batch does with a record that another transaction has locked.
pub enum RecordDeleteContention {
	/// The batch skips the record and reports it in the outcome's kept records.
	SkipLocked,
	/// The batch waits for the lock.
	Wait,
}
#[derive(Clone, Copy, Debug)]
/// The validated limits of one deletion batch.
pub struct RecordDeleteBatchLimits {
	max_deletions: NonZeroU32,
	max_candidates: NonZeroU32,
}
/// The inputs of the limits of one deletion batch.
pub struct RecordDeleteBatchLimitsFields {
	/// The maximum number of records that the batch deletes. It must be positive.
	pub max_deletions: NonZeroU32,
	/// The maximum number of records that the batch examines, at least
	/// `max_deletions`.
	pub max_candidates: NonZeroU32,
}
#[derive(Debug, thiserror::Error)]
/// A deletion request that Paranoid rejected.
pub enum RecordDeleteRequestError {
	#[error("raw deletion counts must not exceed the deletion batch limit")]
	/// A limit exceeds the largest supported batch size.
	LimitTooLarge,
	#[error("raw deletion count exceeds its candidate window")]
	/// `max_deletions` exceeds `max_candidates`.
	MaxDeletionsAboveMaxCandidates,
}
impl RecordDeleteBatchLimits {
	/// Validates the limits without database work.
	pub fn new(fields: RecordDeleteBatchLimitsFields) -> Result<Self, RecordDeleteRequestError> {
		if fields.max_deletions.get() > MAX_DELETE_BATCH_SIZE
			|| fields.max_candidates.get() > MAX_DELETE_BATCH_SIZE
		{
			return Err(RecordDeleteRequestError::LimitTooLarge);
		}
		if fields.max_deletions > fields.max_candidates {
			return Err(RecordDeleteRequestError::MaxDeletionsAboveMaxCandidates);
		}
		Ok(Self {
			max_deletions: fields.max_deletions,
			max_candidates: fields.max_candidates,
		})
	}
	/// The largest number of records that the batch deletes.
	pub fn max_deletions(self) -> NonZeroU32 {
		self.max_deletions
	}
	/// The largest number of records that the batch examines.
	pub fn max_candidates(self) -> NonZeroU32 {
		self.max_candidates
	}
}
/// The inputs of a deletion batch request.
pub struct RecordDeleteBatchRequestFields {
	/// The whole namespace or a nonempty key prefix.
	pub selection: RecordKeySelection,
	/// Which records the batch deletes, and for `AllValued`, what the batch does with a record
	/// that another transaction has locked.
	pub kind: RecordDeleteKind,
	/// The batch's limits.
	pub limits: RecordDeleteBatchLimits,
}
#[derive(Clone, Debug)]
/// A request for one deletion batch.
pub struct RecordDeleteBatchRequest {
	selection: RecordKeySelection,
	kind: RecordDeleteKind,
	limits: RecordDeleteBatchLimits,
}
impl RecordDeleteBatchRequest {
	/// Builds the request without database work.
	pub fn new(fields: RecordDeleteBatchRequestFields) -> Self {
		Self {
			selection: fields.selection,
			kind: fields.kind,
			limits: fields.limits,
		}
	}
	/// The keys that the batch covers.
	pub fn selection(&self) -> &RecordKeySelection {
		&self.selection
	}
	/// Which records the batch deletes.
	pub fn kind(&self) -> RecordDeleteKind {
		self.kind
	}
	/// What the batch does with a record that another transaction has locked. An `Expired`
	/// batch skips it.
	pub fn contention(&self) -> RecordDeleteContention {
		match self.kind {
			RecordDeleteKind::Expired => RecordDeleteContention::SkipLocked,
			RecordDeleteKind::AllValued { contention } => contention,
		}
	}
	/// The batch's limits.
	pub fn limits(&self) -> RecordDeleteBatchLimits {
		self.limits
	}
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Why a deletion batch kept a record that it examined.
pub enum RecordDeleteRetainedReason {
	/// The batch could not lock the record because another transaction held it or changed
	/// it, so the record may still exist.
	NativeUnheld,
	/// While the namespace is still in v10 storage, the batch could not lock the v10
	/// record, so the record may still exist.
	SourceUnheld,
	/// The record changed or was deleted before the batch locked it.
	ChangedOrGone,
	/// The record was live at the batch's database time, so an `Expired` batch kept it.
	BecameLive,
	/// The batch had already deleted `max_deletions` records.
	MaxDeletionsReached,
}
#[derive(Debug)]
/// A record that a deletion batch examined and kept, and the reason.
pub struct RecordDeleteRetained {
	key: RecordKey,
	reason: RecordDeleteRetainedReason,
}
impl RecordDeleteRetained {
	pub(crate) fn new(key: RecordKey, reason: RecordDeleteRetainedReason) -> Self {
		Self { key, reason }
	}
	/// The record's key.
	pub fn key(&self) -> &RecordKey {
		&self.key
	}
	/// Why the batch kept the record.
	pub fn reason(&self) -> RecordDeleteRetainedReason {
		self.reason
	}
}
/// The result of one deletion batch. A batch that deletes fewer records than its limit does
/// not show that the selection holds no more records to delete.
#[derive(Debug)]
pub struct RecordDeleteBatchResult {
	observed_at: StateTimestampMicros,
	deleted: Vec<RecordKey>,
	examined: usize,
	retained: Vec<RecordDeleteRetained>,
	more_candidates: bool,
}
pub(crate) struct RecordDeleteBatchOutcomeFields {
	pub(crate) observed_at: StateTimestampMicros,
	pub(crate) deleted: Vec<RecordKey>,
	pub(crate) examined: usize,
	pub(crate) retained: Vec<RecordDeleteRetained>,
	pub(crate) more_candidates: bool,
}
impl RecordDeleteBatchResult {
	pub(crate) fn new(fields: RecordDeleteBatchOutcomeFields) -> Self {
		Self {
			observed_at: fields.observed_at,
			deleted: fields.deleted,
			examined: fields.examined,
			retained: fields.retained,
			more_candidates: fields.more_candidates,
		}
	}
	/// The database time at which the batch decided which records to delete.
	pub fn observed_at(&self) -> StateTimestampMicros {
		self.observed_at
	}
	/// The keys of the deleted records.
	pub fn deleted_keys(&self) -> &[RecordKey] {
		&self.deleted
	}
	/// The number of deleted records.
	pub fn deleted_count(&self) -> usize {
		self.deleted.len()
	}
	/// The number of records that the batch examined.
	pub fn examined_count(&self) -> usize {
		self.examined
	}
	/// The examined records that the batch kept.
	pub fn retained(&self) -> &[RecordDeleteRetained] {
		&self.retained
	}
	/// Whether the selection held more records than the batch examined. Another batch can
	/// find more records even when this is false because other transactions can write.
	pub fn more_candidates(&self) -> bool {
		self.more_candidates
	}
}
