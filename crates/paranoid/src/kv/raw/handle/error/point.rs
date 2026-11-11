//! Point failures map to their primary kinds; a callback failure keeps the app's error.
use super::{FailureView, PgRawErrorKind as Kind};
use crate::db::DatabaseError;
use crate::kv::raw::postgres::slot::RawRejectionSettlement;
use crate::kv::{
	raw::engine::store::{NativeRawKvError, RawKvStorageError},
	raw::state::RecordMutationError,
};

pub(super) fn view<C>(error: &NativeRawKvError<C>) -> FailureView<'_, C> {
	match error {
		NativeRawKvError::RollbackFailed {
			operation_error, ..
		} => view(operation_error),
		NativeRawKvError::Admission(_) => FailureView::new(Kind::Admission),
		NativeRawKvError::Route(e) => super::route_view(e),
		NativeRawKvError::SourceKeyNotUtf8(_) | NativeRawKvError::SourceKeyNotCanonical(_) => {
			FailureView::new(Kind::V10Key)
		}
		NativeRawKvError::Storage(_) => FailureView::new(Kind::Storage),
		NativeRawKvError::Mutation(e) => mutation_view(e),
		NativeRawKvError::Database(_) => FailureView::new(Kind::Database),
		NativeRawKvError::Protocol(_) => FailureView::new(Kind::Protocol),
	}
}
fn mutation_view<C>(error: &RecordMutationError<C, RawKvStorageError>) -> FailureView<'_, C> {
	match error {
		RecordMutationError::Callback(e) => {
			let mut v = FailureView::new(Kind::Callback);
			v.callback = Some(e);
			v.settlement = Some(RawRejectionSettlement::KnownRestored);
			v
		}
		RecordMutationError::KeyNotFound => {
			let mut v = FailureView::new(Kind::NotLive);
			v.settlement = Some(RawRejectionSettlement::KnownRestored);
			v
		}
		RecordMutationError::ExpirationOverflow => {
			let mut v = FailureView::new(Kind::ExpirationOverflow);
			v.settlement = Some(RawRejectionSettlement::KnownRestored);
			v
		}
		RecordMutationError::Storage(_) => FailureView::new(Kind::Storage),
		RecordMutationError::CleanupFailed {
			operation_error, ..
		} => {
			let mut v = mutation_view(operation_error);
			v.settlement = Some(RawRejectionSettlement::WholeRollbackRequired);
			v
		}
	}
}
pub(super) fn database_error<C>(error: &NativeRawKvError<C>) -> Option<&DatabaseError> {
	match error {
		NativeRawKvError::Admission(e) => e.database_error(),
		NativeRawKvError::Route(e) => e.database_error(),
		NativeRawKvError::Storage(e) => storage_database_error(e),
		NativeRawKvError::Mutation(e) => mutation_database_error(e),
		NativeRawKvError::Database(e) => Some(e),
		NativeRawKvError::RollbackFailed {
			operation_error,
			rollback_error,
		} => database_error(operation_error).or(Some(rollback_error)),
		_ => None,
	}
}
fn mutation_database_error<C>(
	error: &RecordMutationError<C, RawKvStorageError>,
) -> Option<&DatabaseError> {
	match error {
		RecordMutationError::Storage(e) => storage_database_error(e),
		RecordMutationError::CleanupFailed {
			operation_error,
			cleanup_error,
		} => mutation_database_error(operation_error)
			.or_else(|| storage_database_error(cleanup_error)),
		_ => None,
	}
}
fn storage_database_error(error: &RawKvStorageError) -> Option<&DatabaseError> {
	match error {
		RawKvStorageError::Native(e) => e.database_error(),
		RawKvStorageError::Source(e) => e.database_error(),
		RawKvStorageError::Relocation(e) => e.database_error(),
	}
}
