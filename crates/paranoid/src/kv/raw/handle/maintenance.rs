//! Raw deletion batches, selection deletion, and expired-record cleanup.
use super::{PgRaw, PgRawError, RawHandleOwner, error::RawOperationCause};
use crate::db::{WriteDatabase, WritePool};
use crate::kv::{
	raw::model::cleanup::{RawExpiredCleanupRunRequest, RawExpiredCleanupRunResult},
	raw::model::delete::{RecordDeleteBatchRequest, RecordDeleteBatchResult},
	raw::model::namespace::{RecordSelectionDeleteRequest, RecordSelectionDeleteResult},
};
impl PgRaw<'_> {
	/// Examines at most `max_candidates` records of the selection and deletes at most
	/// `max_deletions` of them, deciding expiry at one database time. The outcome lists the
	/// deleted and the kept records. A batch that deletes fewer records than its limit does not
	/// show that the selection contains no more records to delete.
	///
	/// When the batch fails, `PgRawError::rejection_settlement` reports whether the caller's
	/// transaction can still commit. It returns `KnownRestored` when the batch failed after
	/// Paranoid admitted it and before it read or locked a record, for example because the
	/// selection prefix is not a valid v10 key prefix while the namespace is still in v10
	/// storage. It returns `WholeRollbackRequired` after every other failure; Paranoid then
	/// rejects committing the transaction, and the transaction must roll back. When `database`
	/// is a pool, Paranoid owns the transaction and has already ended it after the failure.
	pub async fn delete_batch(
		&self,
		database: impl WriteDatabase,
		request: &RecordDeleteBatchRequest,
	) -> Result<RecordDeleteBatchResult, PgRawError> {
		let result = match &self.owner {
			RawHandleOwner::Core(store) => store.delete_batch(database, request).await,
			RawHandleOwner::Scope(store) => store.delete_batch(database, request).await,
		};
		result.map_err(|error| self.selection_error(RawOperationCause::Delete(error)))
	}
	/// Deletes every record of the selection in one transaction, in batches of `batch_size`.
	/// Each batch starts after the greatest key of the previous batch, and the deletion ends when
	/// a batch finds no more records. The deletions commit or roll back together. The operation
	/// does not block inserts, so a record that another transaction inserts while the deletion
	/// runs can remain.
	///
	/// After a v10 retirement has stopped v10 writes and before the retirement finishes, the
	/// operation instead reads every key of the selection, locks all of those records, and
	/// deletes them in batches of `batch_size`. If other transactions change the selection
	/// meanwhile, the operation undoes its deletions and tries again with every key that it has
	/// found, and after a fixed number of attempts it fails with the kind
	/// `PgRawErrorKind::Contention`.
	///
	/// When the deletion fails, `PgRawError::rejection_settlement` reports whether the caller's
	/// transaction can still commit. It returns `KnownRestored` when the deletion failed after
	/// Paranoid admitted it and before it read or locked a record, for example because the
	/// selection prefix is not a valid v10 key prefix while the namespace is still in v10
	/// storage. It also returns `KnownRestored` after a `Contention` failure for which Paranoid
	/// restored every record of the selection. It returns `WholeRollbackRequired` after every
	/// other failure; Paranoid then rejects committing the transaction, and the transaction must
	/// roll back. When `database` is a pool, Paranoid owns the transaction and has already ended
	/// it after the failure.
	pub async fn delete_selection_atomically(
		&self,
		database: impl WriteDatabase,
		request: &RecordSelectionDeleteRequest,
	) -> Result<RecordSelectionDeleteResult, PgRawError> {
		let result = match &self.owner {
			RawHandleOwner::Core(store) => {
				store.delete_selection_atomically(database, request).await
			}
			RawHandleOwner::Scope(store) => {
				store.delete_selection_atomically(database, request).await
			}
		};
		result.map_err(|error| self.selection_error(RawOperationCause::Namespace(error)))
	}
	/// Deletes expired records in batches until a batch deletes fewer than `max_deletions`
	/// records. Each batch runs in its own transaction on the pool, and the run waits the
	/// request's delay after each full batch. Batches that committed stay committed when a later
	/// batch fails or the caller cancels the run; `PgRawError::committed_prefix` reports them.
	pub async fn cleanup_expired_with_delay(
		&self,
		pool: &WritePool,
		request: &RawExpiredCleanupRunRequest,
	) -> Result<RawExpiredCleanupRunResult, PgRawError> {
		let result = match &self.owner {
			RawHandleOwner::Core(store) => store.cleanup_expired_with_delay(pool, request).await,
			RawHandleOwner::Scope(store) => store.cleanup_expired_with_delay(pool, request).await,
		};
		result.map_err(|error| self.selection_error(RawOperationCause::Cleanup(error)))
	}
}
