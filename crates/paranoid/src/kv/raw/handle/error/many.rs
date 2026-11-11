//! Multi-record failures map to their primary kinds.
use super::{FailureView, PgRawErrorKind as Kind, route_view};
use crate::kv::raw::engine::many::RawManyFailure;
use crate::kv::raw::postgres::many::RawManyStorageError;
use crate::kv::raw::postgres::slot::RawRejectionSettlement;
use crate::records::postgres::record_many::write::RecordManyWriteError;

pub(super) fn view<C>(error: &RawManyFailure) -> FailureView<'_, C> {
	match error {
		RawManyFailure::Storage(e) => read_view(e),
		RawManyFailure::Write(e) => write_view(e),
		RawManyFailure::SourceKey { .. } => FailureView::new(Kind::V10Key),
		RawManyFailure::Route(e) => route_view(e),
	}
}
fn read_view<C>(error: &RawManyStorageError) -> FailureView<'_, C> {
	match error {
		RawManyStorageError::Record { source, .. } => read_view(source),
		RawManyStorageError::Physical(e) => super::selection::page_storage_view(e),
		_ => FailureView::new(Kind::Storage),
	}
}
fn write_view<C>(error: &RecordManyWriteError) -> FailureView<'_, C> {
	match error {
		RecordManyWriteError::Record { source, .. } => write_view(source),
		RecordManyWriteError::Cleanup { primary_error, .. } => {
			let mut v = write_view(primary_error);
			v.settlement = Some(RawRejectionSettlement::WholeRollbackRequired);
			v
		}
		RecordManyWriteError::Expiration(_) => {
			let mut v = FailureView::new(Kind::ExpirationOverflow);
			v.settlement = Some(RawRejectionSettlement::KnownRestored);
			v
		}
		_ => FailureView::new(Kind::Storage),
	}
}
