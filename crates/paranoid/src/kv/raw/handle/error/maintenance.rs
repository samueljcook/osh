//! Deletion, namespace deletion, and cleanup failures map to their primary kinds.
use super::{FailureView, PgRawErrorKind as Kind, route_view, transaction_view};
use crate::kv::raw::postgres::delete::{
	RawDeleteStorageError, namespace::RawNamespaceStorageError,
};
use crate::kv::{
	raw::engine::delete::{RawDeleteFailure, RawSelectionDeleteFailure},
	raw::model::cleanup::{RawExpiredCleanupRunError, RawExpiredCleanupRunInnerError},
};
use crate::records::postgres::group_attempt::GroupAttemptError;

pub(super) fn delete_view<C>(error: &RawDeleteFailure) -> FailureView<'_, C> {
	match error {
		RawDeleteFailure::Input(_) => FailureView::new(Kind::V10Key),
		RawDeleteFailure::Route(e) => route_view(e),
		RawDeleteFailure::Storage(e) => physical_view(e),
	}
}
pub(super) fn physical_view<C>(error: &RawDeleteStorageError) -> FailureView<'_, C> {
	match error {
		RawDeleteStorageError::Record { source, .. } => physical_view(source),
		RawDeleteStorageError::Utf8(_) | RawDeleteStorageError::SourceKey(_) => {
			FailureView::new(Kind::V10Key)
		}
		_ => FailureView::new(Kind::Storage),
	}
}
pub(super) fn namespace_view<C>(error: &RawSelectionDeleteFailure) -> FailureView<'_, C> {
	match error {
		RawSelectionDeleteFailure::Input(_) => FailureView::new(Kind::V10Key),
		RawSelectionDeleteFailure::Route(e) => route_view(e),
		RawSelectionDeleteFailure::Storage(e) => namespace_storage_view(e),
	}
}
fn namespace_storage_view<C>(error: &RawNamespaceStorageError) -> FailureView<'_, C> {
	match error {
		RawNamespaceStorageError::Storage(e) => physical_view(e),
		RawNamespaceStorageError::Rejected(_)
		| RawNamespaceStorageError::RejectionCleanup { .. } => FailureView::new(Kind::Contention),
		RawNamespaceStorageError::Boundary(e) => group_view(e),
		_ => FailureView::new(Kind::Storage),
	}
}
pub(super) fn group_view<C>(_error: &GroupAttemptError) -> FailureView<'_, C> {
	FailureView::new(Kind::Storage)
}
pub(super) fn cleanup_view<C>(error: &RawExpiredCleanupRunError) -> FailureView<'_, C> {
	match error.inner_error() {
		RawExpiredCleanupRunInnerError::Batch(e) => transaction_view(e, delete_view),
		RawExpiredCleanupRunInnerError::Timing(e) => {
			let mut v = FailureView::new(Kind::LocalClockDomain);
			v.timing = Some(e);
			v
		}
		RawExpiredCleanupRunInnerError::CommittedProgressOverflow { .. } => {
			FailureView::new(Kind::CommittedProgressOverflow)
		}
	}
}
