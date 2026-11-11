//! Explicit paced cleanup requests and known-committed progress diagnostics.

pub(crate) mod run;

use super::delete::{
	RecordDeleteBatchLimits, RecordDeleteBatchRequest, RecordDeleteBatchRequestFields,
	RecordDeleteBatchResult, RecordDeleteKind,
};
use super::page::RecordKeySelection;
use crate::db::{DatabaseError, TransactionFailure};
use crate::kv::raw::engine::delete::NativeRawDeleteError;
use std::time::Duration;
/// The inputs of an expired-record cleanup run.
pub struct RawExpiredCleanupRunRequestFields {
	/// The whole namespace or a nonempty key prefix.
	pub selection: RecordKeySelection,
	/// The limits of each batch.
	pub limits: RecordDeleteBatchLimits,
	/// How long the run waits after each committed batch that deleted `max_deletions`
	/// records, before it starts the next batch.
	pub delay_between_full_batches: Duration,
}
/// A request for a cleanup run, which deletes expired records in batches. Each batch runs in its
/// own transaction on the pool and skips records that other transactions have locked.
#[derive(Clone)]
pub struct RawExpiredCleanupRunRequest {
	batch: RecordDeleteBatchRequest,
	delay_between_full_batches: Duration,
}
impl RawExpiredCleanupRunRequest {
	/// Builds the request without database work.
	pub fn new(fields: RawExpiredCleanupRunRequestFields) -> Self {
		Self {
			batch: RecordDeleteBatchRequest::new(RecordDeleteBatchRequestFields {
				selection: fields.selection,
				kind: RecordDeleteKind::Expired,
				limits: fields.limits,
			}),
			delay_between_full_batches: fields.delay_between_full_batches,
		}
	}
	/// The request of each batch.
	pub fn batch(&self) -> &RecordDeleteBatchRequest {
		&self.batch
	}
	/// How long the run waits after each committed full batch.
	pub fn delay_between_full_batches(&self) -> Duration {
		self.delay_between_full_batches
	}
}
/// The totals of a cleanup run's committed batches.
#[derive(Clone, Copy, Debug)]
pub struct RawExpiredCleanupCommitted {
	deleted_count: u64,
	batch_count: u64,
}
impl RawExpiredCleanupCommitted {
	/// The number of records that the committed batches deleted.
	pub fn deleted_count(self) -> u64 {
		self.deleted_count
	}
	/// The number of committed batches.
	pub fn batch_count(self) -> u64 {
		self.batch_count
	}
	pub(super) fn empty() -> Self {
		Self {
			deleted_count: 0,
			batch_count: 0,
		}
	}
	pub(super) fn advance(self, count: usize) -> Option<Self> {
		Some(Self {
			deleted_count: self.deleted_count.checked_add(u64::try_from(count).ok()?)?,
			batch_count: self.batch_count.checked_add(1)?,
		})
	}
}
/// The result of a cleanup run, which stops when a batch deletes fewer than `max_deletions`
/// records. The last batch can still report kept records or more
/// records to examine, so expired records can remain after the run.
#[derive(Debug)]
pub struct RawExpiredCleanupRunResult {
	committed: RawExpiredCleanupCommitted,
	last_batch: RecordDeleteBatchResult,
}
impl RawExpiredCleanupRunResult {
	pub(super) fn new(
		committed: RawExpiredCleanupCommitted,
		last_batch: RecordDeleteBatchResult,
	) -> Self {
		Self {
			committed,
			last_batch,
		}
	}
	/// The totals of the committed batches, including the last batch.
	pub fn committed(&self) -> RawExpiredCleanupCommitted {
		self.committed
	}
	/// The result of the last batch.
	pub fn last_batch(&self) -> &RecordDeleteBatchResult {
		&self.last_batch
	}
}
#[derive(Debug, thiserror::Error)]
#[error("raw cleanup delay exceeds the local monotonic clock domain")]
/// The delay between batches is too long for the local monotonic clock.
pub struct RawCleanupTimingError {
	delay: Duration,
}
impl RawCleanupTimingError {
	pub(super) fn new(delay: Duration) -> Self {
		Self { delay }
	}
	/// The requested delay.
	pub fn delay(&self) -> Duration {
		self.delay
	}
}
#[derive(Debug, thiserror::Error)]
pub(in crate::kv) enum ExpiredCleanupRunInnerError<E> {
	#[error("raw cleanup batch failed")]
	Batch(#[source] Box<E>),
	#[error("raw cleanup pacing failed after its known committed prefix")]
	Timing(#[source] RawCleanupTimingError),
	#[error("raw cleanup counters cannot represent its additional known committed batch")]
	CommittedProgressOverflow {
		committed_batch: Box<RecordDeleteBatchResult>,
	},
}
/// A failed current ending is separate from every earlier known-committed batch.
#[derive(Debug, thiserror::Error)]
#[error("paced raw cleanup failed after a known committed prefix")]
pub(in crate::kv) struct ExpiredCleanupRunError<E> {
	committed_prefix: RawExpiredCleanupCommitted,
	#[source]
	source: ExpiredCleanupRunInnerError<E>,
}
pub(in crate::kv) type RawExpiredCleanupRunInnerError =
	ExpiredCleanupRunInnerError<NativeRawDeleteError>;
pub(in crate::kv) type RawExpiredCleanupRunError = ExpiredCleanupRunError<NativeRawDeleteError>;

impl<E> ExpiredCleanupRunError<E> {
	pub(super) fn new(
		committed_prefix: RawExpiredCleanupCommitted,
		source: ExpiredCleanupRunInnerError<E>,
	) -> Self {
		Self {
			committed_prefix,
			source,
		}
	}
	pub(in crate::kv) fn committed_prefix(&self) -> RawExpiredCleanupCommitted {
		self.committed_prefix
	}
	pub(crate) fn inner_error(&self) -> &ExpiredCleanupRunInnerError<E> {
		&self.source
	}
	pub(in crate::kv) fn batch_error(&self) -> Option<&E> {
		match &self.source {
			ExpiredCleanupRunInnerError::Batch(e) => Some(e),
			_ => None,
		}
	}
	pub(in crate::kv) fn additional_committed_batch(&self) -> Option<&RecordDeleteBatchResult> {
		match &self.source {
			ExpiredCleanupRunInnerError::CommittedProgressOverflow { committed_batch } => {
				Some(committed_batch)
			}
			_ => None,
		}
	}
}
impl ExpiredCleanupRunError<NativeRawDeleteError> {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		self.batch_error()
			.and_then(NativeRawDeleteError::database_error)
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		self.batch_error()
			.is_some_and(TransactionFailure::commit_outcome_is_unknown)
	}
}
