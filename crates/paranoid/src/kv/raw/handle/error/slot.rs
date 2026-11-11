//! Slot failures map to their primary kinds; a preparation failure reports whether the
//! caller's transaction can still commit.
use super::{FailureView, PgRawErrorKind as Kind, route_view};
use crate::kv::raw::postgres::slot::RawSlotStorageError;
use crate::kv::{
	raw::engine::slot::RawSlotFailure, raw::model::slot_preparation::RawSlotPreparationError,
};

pub(super) fn view<C>(error: &RawSlotFailure) -> FailureView<'_, C> {
	match error {
		RawSlotFailure::Route(e) => route_view(e),
		RawSlotFailure::SourceKey { .. } => FailureView::new(Kind::V10Key),
		RawSlotFailure::Storage(e) => storage_view(e),
		RawSlotFailure::Preparation(e) => {
			let mut v = FailureView::new(match e.preparation_error() {
				RawSlotPreparationError::Expiration(_) => Kind::ExpirationOverflow,
				_ => Kind::Preparation,
			});
			v.settlement = e.settlement();
			if let RawSlotPreparationError::Encryption(source) = e.preparation_error() {
				v.encryption = Some(source);
			}
			v
		}
	}
}
fn storage_view<C>(error: &RawSlotStorageError) -> FailureView<'_, C> {
	match error {
		RawSlotStorageError::Record { source, .. } => storage_view(source),
		RawSlotStorageError::Restored { primary }
		| RawSlotStorageError::RejectionCleanup {
			primary_error: primary,
			..
		} => storage_view(primary),
		RawSlotStorageError::Physical(e) => super::maintenance::physical_view(e),
		RawSlotStorageError::Boundary(e) => super::maintenance::group_view(e),
		RawSlotStorageError::Expiration(_) => FailureView::new(Kind::ExpirationOverflow),
		RawSlotStorageError::RejectedAfterRestore { .. } => FailureView::new(Kind::Contention),
		_ => FailureView::new(Kind::Storage),
	}
}
