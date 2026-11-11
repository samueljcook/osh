//! Page and count failures map to their primary kinds; page request rejections stay available.
use super::{FailureView, PgRawErrorKind as Kind, route_view};
use crate::kv::raw::postgres::page::RawPageStorageError;
use crate::kv::{
	raw::engine::aggregate::RawAggregateFailure, raw::engine::page::RawPageFailure,
	raw::model::page::RecordPageRequestError,
};

pub(super) fn page_view<C>(error: &RawPageFailure) -> FailureView<'_, C> {
	match error {
		RawPageFailure::Request(e) => {
			let mut v = FailureView::new(
				if matches!(
					e,
					RecordPageRequestError::SourceUtf8(_) | RecordPageRequestError::SourceKey(_)
				) {
					Kind::V10Key
				} else {
					Kind::Request
				},
			);
			v.request = Some(e);
			v
		}
		RawPageFailure::Storage(e) => page_storage_view(e),
		RawPageFailure::Route(e) => route_view(e),
	}
}
pub(super) fn page_storage_view<C>(error: &RawPageStorageError) -> FailureView<'_, C> {
	match error {
		RawPageStorageError::Record { source, .. } => page_storage_view(source),
		RawPageStorageError::SourceUtf8(_) | RawPageStorageError::SourceKey(_) => {
			FailureView::new(Kind::V10Key)
		}
		_ => FailureView::new(Kind::Storage),
	}
}
pub(super) fn count_view<C>(error: &RawAggregateFailure) -> FailureView<'_, C> {
	match error {
		RawAggregateFailure::Input(_) => FailureView::new(Kind::V10Key),
		RawAggregateFailure::Storage(_) => FailureView::new(Kind::Storage),
		RawAggregateFailure::Route(e) => route_view(e),
	}
}
