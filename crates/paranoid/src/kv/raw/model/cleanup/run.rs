//! Fixed library pool batches finish before checked progress and local pacing.

use super::{
	ExpiredCleanupRunError, ExpiredCleanupRunInnerError, RawCleanupTimingError,
	RawExpiredCleanupCommitted, RawExpiredCleanupRunRequest, RawExpiredCleanupRunResult,
};
use crate::kv::raw::model::delete::{RecordDeleteBatchRequest, RecordDeleteBatchResult};
use primitives_rs::time::Deadline;
use std::future::Future;

pub(in crate::kv) struct ExpiredCleanupRunFields<'request, DeleteBatchFn> {
	pub(in crate::kv) request: &'request RawExpiredCleanupRunRequest,
	pub(in crate::kv) delete_committed_batch_fn: DeleteBatchFn,
}

/// Both fixed callers supply actual pool-owned deletion; no joined transaction is retained.
pub(in crate::kv) async fn run_expired_cleanup<'request, E, BatchFuture>(
	mut fields: ExpiredCleanupRunFields<
		'request,
		impl FnMut(&'request RecordDeleteBatchRequest) -> BatchFuture,
	>,
) -> Result<RawExpiredCleanupRunResult, ExpiredCleanupRunError<E>>
where
	BatchFuture: Future<Output = Result<RecordDeleteBatchResult, E>>,
{
	let mut committed = RawExpiredCleanupCommitted::empty();
	loop {
		let batch = (fields.delete_committed_batch_fn)(fields.request.batch())
			.await
			.map_err(|error| {
				ExpiredCleanupRunError::new(
					committed,
					ExpiredCleanupRunInnerError::Batch(Box::new(error)),
				)
			})?;
		let deleted = batch.deleted_count();
		let Some(next) = committed.advance(deleted) else {
			return Err(ExpiredCleanupRunError::new(
				committed,
				ExpiredCleanupRunInnerError::CommittedProgressOverflow {
					committed_batch: Box::new(batch),
				},
			));
		};
		committed = next;
		if deleted < fields.request.batch().limits().max_deletions().get() as usize {
			return Ok(RawExpiredCleanupRunResult::new(committed, batch));
		}
		let delay = fields.request.delay_between_full_batches();
		if !delay.is_zero() {
			let until = Deadline::new_from_base(tokio::time::Instant::now().into_std(), delay)
				.map_err(|_| {
					ExpiredCleanupRunError::new(
						committed,
						ExpiredCleanupRunInnerError::Timing(RawCleanupTimingError::new(delay)),
					)
				})?;
			tokio::time::sleep_until(tokio::time::Instant::from_std(until.instant())).await;
		}
	}
}
