//! Actual header observations remain distinct from body-bearing snapshots.

use super::super::acquisition::{AcquiredNativeRecord, HeldNativeRow, NativeRecordProjection};
use super::RecordSetRejectionError;
use crate::records::{PointRecordRead, StateTimestampMicros, StoredRecord};

/// One held identity after the set's clock observation. A `Full` snapshot records whether
/// the acquisition inserted its reservation row; a `Header` identity keeps only its row state.
pub(super) enum HeldRecord {
	Full(PointRecordRead),
	Header(HeldNativeRow<()>),
}
impl AcquiredNativeRecord {
	pub(super) fn projection(&self) -> NativeRecordProjection {
		match self {
			Self::Full(_) => NativeRecordProjection::Full,
			Self::Header(_) => NativeRecordProjection::Header,
		}
	}
	pub(super) fn full_record(&self) -> Result<Option<&StoredRecord>, RecordSetRejectionError> {
		match self {
			Self::Full(row) => Ok(row.value()),
			Self::Header(_) => Err(RecordSetRejectionError::full_body_required()),
		}
	}
	pub(super) fn is_inserted_reservation(&self) -> bool {
		match self {
			Self::Full(row) => row.is_inserted_reservation(),
			Self::Header(row) => row.is_inserted_reservation(),
		}
	}
	pub(super) fn finish(self, observed_at: StateTimestampMicros) -> HeldRecord {
		match self {
			Self::Full(row) => HeldRecord::Full(PointRecordRead::held(row, observed_at)),
			Self::Header(row) => HeldRecord::Header(row.without_value()),
		}
	}
}
impl HeldRecord {
	pub(super) fn full_snapshot(&self) -> Result<&PointRecordRead, RecordSetRejectionError> {
		match self {
			Self::Full(snapshot) => Ok(snapshot),
			Self::Header(_) => Err(RecordSetRejectionError::full_body_required()),
		}
	}
	pub(super) fn is_inserted_reservation(&self) -> bool {
		match self {
			Self::Full(snapshot) => snapshot.holds_inserted_reservation(),
			Self::Header(row) => row.is_inserted_reservation(),
		}
	}
}
