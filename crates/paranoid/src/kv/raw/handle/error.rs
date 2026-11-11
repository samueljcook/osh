//! Raw operation failures with their scope, namespace, record, and primary kind.

use crate::db::{DatabaseError, TransactionFailure};
use crate::kv::{
	raw::engine::aggregate::NativeRawAggregateError,
	raw::engine::delete::{NativeRawDeleteError, NativeRawSelectionDeleteError},
	raw::engine::error::NativeRawError,
	raw::engine::many::NativeRawManyError,
	raw::engine::page::NativeRawPageError,
	raw::engine::slot::NativeRawSlotError,
	raw::engine::store::{NativeRawKvError, RawSourceRouteError},
	raw::model::cleanup::{
		RawCleanupTimingError, RawExpiredCleanupCommitted, RawExpiredCleanupRunError,
	},
	raw::model::delete::RecordDeleteBatchResult,
	raw::model::page::RecordPageRequestError,
};
use crate::records::{RecordIdentity, RecordNamespace};
use crate::{kv::raw::postgres::slot::RawRejectionSettlement, scope::ScopeIdentity};
use primitives_rs::crypto::encryption::EncryptError;
use std::{convert::Infallible, error::Error, fmt};

mod maintenance;
mod many;
mod point;
mod selection;
mod slot;

pub(super) struct PgRawErrorFields<'a, C> {
	pub(super) scope: Option<&'a ScopeIdentity>,
	pub(super) record: RecordIdentity,
	pub(super) source: NativeRawKvError<C>,
}
pub(super) struct PgRawSelectionErrorFields<'a> {
	pub(super) scope: Option<&'a ScopeIdentity>,
	pub(super) namespace: &'a RecordNamespace,
	pub(super) source: RawOperationCause<Infallible>,
}
pub(super) enum RawOperationCause<C> {
	Point(NativeRawKvError<C>),
	Many(NativeRawManyError),
	Page(NativeRawPageError),
	Count(NativeRawAggregateError),
	Delete(NativeRawDeleteError),
	Namespace(NativeRawSelectionDeleteError),
	Cleanup(RawExpiredCleanupRunError),
	Slot(NativeRawSlotError),
}
enum RawContext {
	Point {
		scope: Option<ScopeIdentity>,
		record: RecordIdentity,
	},
	Selection {
		scope: Option<ScopeIdentity>,
		namespace: RecordNamespace,
	},
}
struct RawDiagnostic<C> {
	context: RawContext,
	source: RawOperationCause<C>,
}

/// A failed Raw operation. `C` is the error type of the app's callbacks.
///
/// Formatting the error omits keys, values, and callback errors.
pub struct PgRawError<C = Infallible> {
	diagnostic: Box<RawDiagnostic<C>>,
}

/// The class of a failed Raw operation. Several kinds can involve a database failure, so check
/// `PgRawError::database_error` to detect every database failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgRawErrorKind {
	/// The namespace is still in v10 storage, and the installation records no v10 location
	/// for it. This is a defect in Paranoid or a damaged installation.
	V10LocationAbsent,
	/// The installation's recorded v10 location of the namespace does not match its storage.
	/// This is a defect in Paranoid or a damaged installation.
	StorageMappingMismatch,
	/// The namespace is still in v10 storage, and a requested key or prefix is not a valid
	/// v10 key.
	V10Key,
	/// The runtime cannot run the operation, for one of these reasons: the installation's
	/// recorded policy or this runtime's declared roles do not allow it; this runtime has
	/// stopped accepting operations, for example because its participation ended; or reading
	/// the installation's control state or this runtime's registration failed, and
	/// `PgRawError::database_error` returns the database error.
	Admission,
	/// The app's decision or initializer callback returned an error.
	/// `PgRawError::callback_error` returns it.
	Callback,
	/// The operation requires a live record, and the record is absent or expired.
	NotLive,
	/// The requested expiry is later than the latest representable time.
	ExpirationOverflow,
	/// Reading or writing a stored record failed, for example because a database statement
	/// failed, other transactions kept changing the record until Paranoid used up its
	/// attempts, or a stored record is invalid. For a failed statement,
	/// `PgRawError::database_error` returns the database error.
	Storage,
	/// Beginning, committing, or rolling back the operation's own transaction failed.
	/// `PgRawError::database_error` returns the database error.
	Database,
	/// The caller's transaction was not in a state in which the operation can run.
	Protocol,
	/// The page cursor came from a page that a handle of another runtime read, or from a page of
	/// another namespace, selection, or visibility.
	/// `PgRawError::request_error` returns the rejection.
	Request,
	/// Preparing the value for the selected slot key failed, for example because encryption
	/// failed. `PgRawError::rejection_settlement` reports whether the transaction can still
	/// commit.
	Preparation,
	/// Other transactions kept changing the selected records, and Paranoid stopped after its
	/// limit of attempts.
	Contention,
	/// The cleanup run's delay between batches is too long for the local monotonic clock.
	LocalClockDomain,
	/// A cleanup run committed a batch, and its totals no longer fit in a `u64`.
	/// `PgRawError::additional_committed_batch` returns that batch.
	CommittedProgressOverflow,
}

struct FailureView<'a, C> {
	kind: PgRawErrorKind,
	callback: Option<&'a C>,
	request: Option<&'a RecordPageRequestError>,
	encryption: Option<&'a EncryptError>,
	settlement: Option<RawRejectionSettlement>,
	timing: Option<&'a RawCleanupTimingError>,
}
impl<C> FailureView<'_, C> {
	fn new(kind: PgRawErrorKind) -> Self {
		Self {
			kind,
			callback: None,
			request: None,
			encryption: None,
			settlement: None,
			timing: None,
		}
	}
}

/// The kind of an admission or transaction failure, or of the operation's own failure, which
/// `operation_view` classifies.
fn transaction_view<'a, E, C>(
	error: &'a NativeRawError<E>,
	operation_view: fn(&'a E) -> FailureView<'a, C>,
) -> FailureView<'a, C> {
	match error {
		NativeRawError::Admission(_) => FailureView::new(PgRawErrorKind::Admission),
		NativeRawError::Operation(e) => operation_view(e),
		NativeRawError::Database(_) => FailureView::new(PgRawErrorKind::Database),
		NativeRawError::Protocol(_) => FailureView::new(PgRawErrorKind::Protocol),
		NativeRawError::RollbackFailed {
			operation_error, ..
		} => transaction_view(operation_error, operation_view),
	}
}

fn route_view<'a, C>(error: &RawSourceRouteError) -> FailureView<'a, C> {
	FailureView::new(match error {
		RawSourceRouteError::SourceMappingAbsent => PgRawErrorKind::V10LocationAbsent,
		RawSourceRouteError::StorageMappingMismatch => PgRawErrorKind::StorageMappingMismatch,
		RawSourceRouteError::Admission(_) => PgRawErrorKind::Admission,
	})
}

impl PgRawError {
	pub(super) fn selection(fields: PgRawSelectionErrorFields<'_>) -> Self {
		Self {
			diagnostic: Box::new(RawDiagnostic {
				context: RawContext::Selection {
					scope: fields.scope.cloned(),
					namespace: fields.namespace.clone(),
				},
				source: fields.source,
			}),
		}
	}
}
impl<C> PgRawError<C> {
	pub(super) fn new(fields: PgRawErrorFields<'_, C>) -> Self {
		Self {
			diagnostic: Box::new(RawDiagnostic {
				context: RawContext::Point {
					scope: fields.scope.cloned(),
					record: fields.record,
				},
				source: RawOperationCause::Point(fields.source),
			}),
		}
	}
	fn view(&self) -> FailureView<'_, C> {
		match &self.diagnostic.source {
			RawOperationCause::Point(e) => point::view(e),
			RawOperationCause::Many(e) => transaction_view(e, many::view),
			RawOperationCause::Page(e) => transaction_view(e, selection::page_view),
			RawOperationCause::Count(e) => transaction_view(e, selection::count_view),
			RawOperationCause::Delete(e) => transaction_view(e, maintenance::delete_view),
			RawOperationCause::Namespace(e) => transaction_view(e, maintenance::namespace_view),
			RawOperationCause::Cleanup(e) => maintenance::cleanup_view(e),
			RawOperationCause::Slot(e) => transaction_view(e, slot::view),
		}
	}
	/// The handle's app scope, or `None` for the core Raw namespace.
	pub fn scope(&self) -> Option<&ScopeIdentity> {
		match &self.diagnostic.context {
			RawContext::Point { scope, .. } | RawContext::Selection { scope, .. } => scope.as_ref(),
		}
	}
	/// The handle's namespace.
	pub fn namespace(&self) -> &RecordNamespace {
		match &self.diagnostic.context {
			RawContext::Point { record, .. } => record.namespace(),
			RawContext::Selection { namespace, .. } => namespace,
		}
	}
	/// The requested record of a one-record operation, or the failed record of an operation on
	/// several records when Paranoid identified it.
	pub fn record_identity(&self) -> Option<&RecordIdentity> {
		if let RawContext::Point { record, .. } = &self.diagnostic.context {
			return Some(record);
		}
		match &self.diagnostic.source {
			RawOperationCause::Point(_) => None,
			RawOperationCause::Many(e) => e.record_identity(),
			RawOperationCause::Page(e) => e.record_identity(),
			RawOperationCause::Count(e) => e.record_identity(),
			RawOperationCause::Delete(e) => e.record_identity(),
			RawOperationCause::Namespace(e) => e.record_identity(),
			RawOperationCause::Cleanup(e) => e
				.batch_error()
				.and_then(NativeRawDeleteError::record_identity),
			RawOperationCause::Slot(e) => e.record_identity(),
		}
	}
	/// The failed entry's position in a many-record or slot request, when Paranoid identified
	/// it.
	pub fn input_index(&self) -> Option<usize> {
		match &self.diagnostic.source {
			RawOperationCause::Many(e) => e.input_index(),
			RawOperationCause::Slot(e) => e.input_index(),
			_ => None,
		}
	}
	/// The class of the failure.
	pub fn kind(&self) -> PgRawErrorKind {
		self.view().kind
	}
	/// For `Callback`, the error that the app's callback returned.
	pub fn callback_error(&self) -> Option<&C> {
		self.view().callback
	}
	/// For `Request` and for an invalid v10 key in a page request, the request rejection.
	pub fn request_error(&self) -> Option<&RecordPageRequestError> {
		self.view().request
	}
	/// The encryption failure of an encrypted slot request.
	pub fn encryption_error(&self) -> Option<&EncryptError> {
		self.view().encryption
	}
	/// Whether the caller's transaction can still commit after this failure, for the
	/// operations that report it:
	///
	/// - For a failed `delete_batch` or `delete_selection_atomically`, this is always `Some`:
	///   `KnownRestored` when Paranoid finished tracking the deletion and the transaction can
	///   commit, and `WholeRollbackRequired` when Paranoid rejects committing the transaction.
	/// - For a point write or mutation rejection (`Callback`, `NotLive`, or
	///   `ExpirationOverflow`) and for a `set_many` rejection (`ExpirationOverflow`), this is
	///   `Some(KnownRestored)` when Paranoid cleaned up its reservations and
	///   `Some(WholeRollbackRequired)` when reservation cleanup failed.
	/// - For a slot preparation failure, this is `Some` when Paranoid settled the slot attempt.
	///   It is `None` when undoing the slot attempt's changes failed; the transaction must then
	///   roll back.
	/// - For every other failure, this is `None`, and the error does not report whether the
	///   transaction can commit.
	///
	/// When the operation ran on a pool, Paranoid owned the transaction and has already ended
	/// it, so the value asks nothing of the caller.
	pub fn rejection_settlement(&self) -> Option<RawRejectionSettlement> {
		match &self.diagnostic.source {
			RawOperationCause::Delete(e) => Some(e.rejection_settlement()),
			RawOperationCause::Namespace(e) => Some(e.rejection_settlement()),
			_ => self.view().settlement,
		}
	}
	/// For a failed cleanup run, the totals of the batches that committed before the failure.
	pub fn committed_prefix(&self) -> Option<RawExpiredCleanupCommitted> {
		match &self.diagnostic.source {
			RawOperationCause::Cleanup(e) => Some(e.committed_prefix()),
			_ => None,
		}
	}
	/// For `CommittedProgressOverflow`, the committed batch with counts that did not fit.
	pub fn additional_committed_batch(&self) -> Option<&RecordDeleteBatchResult> {
		match &self.diagnostic.source {
			RawOperationCause::Cleanup(e) => e.additional_committed_batch(),
			_ => None,
		}
	}
	/// For `LocalClockDomain`, the rejected delay.
	pub fn timing_error(&self) -> Option<&RawCleanupTimingError> {
		self.view().timing
	}
	/// The database error of the operation, or of its cleanup or rollback when the operation
	/// itself has none.
	pub fn database_error(&self) -> Option<&DatabaseError> {
		match &self.diagnostic.source {
			RawOperationCause::Point(e) => point::database_error(e),
			RawOperationCause::Many(e) => e.database_error(),
			RawOperationCause::Page(e) => e.database_error(),
			RawOperationCause::Count(e) => e.database_error(),
			RawOperationCause::Delete(e) => e.database_error(),
			RawOperationCause::Namespace(e) => e.database_error(),
			RawOperationCause::Cleanup(e) => e.database_error(),
			RawOperationCause::Slot(e) => e.database_error(),
		}
	}
	/// Whether the transaction's own `COMMIT` failed, which means that the operation's changes
	/// may or may not have been stored.
	pub fn commit_outcome_is_unknown(&self) -> bool {
		match &self.diagnostic.source {
			RawOperationCause::Point(e) => e.commit_outcome_is_unknown(),
			RawOperationCause::Many(e) => e.commit_outcome_is_unknown(),
			RawOperationCause::Page(e) => e.commit_outcome_is_unknown(),
			RawOperationCause::Count(e) => e.commit_outcome_is_unknown(),
			RawOperationCause::Delete(e) => e.commit_outcome_is_unknown(),
			RawOperationCause::Namespace(e) => e.commit_outcome_is_unknown(),
			RawOperationCause::Cleanup(e) => e.commit_outcome_is_unknown(),
			RawOperationCause::Slot(e) => e.commit_outcome_is_unknown(),
		}
	}
}
impl<C> fmt::Debug for PgRawError<C> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("PgRawError")
			.field("kind", &self.kind())
			.finish_non_exhaustive()
	}
}
impl<C> fmt::Display for PgRawError<C> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("the Raw operation failed")
	}
}
impl<C: Error + 'static> Error for PgRawError<C> {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		Some(match &self.diagnostic.source {
			RawOperationCause::Point(e) => e,
			RawOperationCause::Many(e) => e,
			RawOperationCause::Page(e) => e,
			RawOperationCause::Count(e) => e,
			RawOperationCause::Delete(e) => e,
			RawOperationCause::Namespace(e) => e,
			RawOperationCause::Cleanup(e) => e,
			RawOperationCause::Slot(e) => e,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::kv::raw::{
		engine::{many::RawManyFailure, store::RawKvStorageError},
		state::{RawWriteExpirationOverflowError, RecordMutationError},
	};
	use crate::records::{
		RecordKey,
		postgres::{StateStorageError, record_many::write::RecordManyWriteError},
	};

	fn test_identity() -> RecordIdentity {
		RecordIdentity::new(
			RecordNamespace::new(b"test-ns".to_vec()).expect("a valid namespace"),
			RecordKey::new(b"test-key::".to_vec()),
		)
	}

	fn point_mutation_error<C>(error: RecordMutationError<C, RawKvStorageError>) -> PgRawError<C> {
		PgRawError::new(PgRawErrorFields {
			scope: None,
			record: test_identity(),
			source: NativeRawKvError::Mutation(error),
		})
	}

	fn many_write_error(error: RecordManyWriteError) -> PgRawError {
		let identity = test_identity();
		PgRawError::selection(PgRawSelectionErrorFields {
			scope: None,
			namespace: identity.namespace(),
			source: RawOperationCause::Many(NativeRawError::Operation(RawManyFailure::Write(
				error,
			))),
		})
	}

	#[test]
	fn point_mutation_rejections_report_expected_settlement() {
		let callback = point_mutation_error(RecordMutationError::Callback("rejected"));
		assert_eq!(callback.kind(), PgRawErrorKind::Callback);
		assert_eq!(callback.callback_error(), Some(&"rejected"));
		assert_eq!(
			callback.rejection_settlement(),
			Some(RawRejectionSettlement::KnownRestored)
		);

		let not_live = point_mutation_error::<Infallible>(RecordMutationError::KeyNotFound);
		assert_eq!(not_live.kind(), PgRawErrorKind::NotLive);
		assert_eq!(
			not_live.rejection_settlement(),
			Some(RawRejectionSettlement::KnownRestored)
		);

		let overflow = point_mutation_error::<Infallible>(RecordMutationError::ExpirationOverflow);
		assert_eq!(overflow.kind(), PgRawErrorKind::ExpirationOverflow);
		assert_eq!(
			overflow.rejection_settlement(),
			Some(RawRejectionSettlement::KnownRestored)
		);

		let storage = point_mutation_error::<Infallible>(RecordMutationError::Storage(
			RawKvStorageError::Native(StateStorageError::ConflictBudgetExhausted),
		));
		assert_eq!(storage.kind(), PgRawErrorKind::Storage);
		assert_eq!(storage.rejection_settlement(), None);

		let cleanup_failed = point_mutation_error(RecordMutationError::CleanupFailed {
			operation_error: Box::new(RecordMutationError::Callback("rejected")),
			cleanup_error: RawKvStorageError::Native(StateStorageError::ConflictBudgetExhausted),
		});
		assert_eq!(cleanup_failed.kind(), PgRawErrorKind::Callback);
		assert_eq!(cleanup_failed.callback_error(), Some(&"rejected"));
		assert_eq!(
			cleanup_failed.rejection_settlement(),
			Some(RawRejectionSettlement::WholeRollbackRequired)
		);
	}

	#[test]
	fn many_write_rejections_report_expected_settlement() {
		let settled_overflow = many_write_error(RecordManyWriteError::Expiration(
			RawWriteExpirationOverflowError,
		));
		assert_eq!(settled_overflow.kind(), PgRawErrorKind::ExpirationOverflow);
		assert_eq!(
			settled_overflow.rejection_settlement(),
			Some(RawRejectionSettlement::KnownRestored)
		);

		let cleanup_failed = many_write_error(RecordManyWriteError::Cleanup {
			primary_error: Box::new(RecordManyWriteError::Expiration(
				RawWriteExpirationOverflowError,
			)),
			cleanup_error: Box::new(RecordManyWriteError::Shape),
		});
		assert_eq!(cleanup_failed.kind(), PgRawErrorKind::ExpirationOverflow);
		assert_eq!(
			cleanup_failed.rejection_settlement(),
			Some(RawRejectionSettlement::WholeRollbackRequired)
		);

		let storage_error = many_write_error(RecordManyWriteError::Shape);
		assert_eq!(storage_error.kind(), PgRawErrorKind::Storage);
		assert_eq!(storage_error.rejection_settlement(), None);
	}
}
