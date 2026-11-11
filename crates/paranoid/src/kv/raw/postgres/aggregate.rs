//! Exact raw count transport. Callers bind actual ownership and phase before this IO.

use super::page::RawReadLayout;
use super::v10::relocating_point::PgRelocatingPointStorage;
use super::v10::source_point::PgSourcePointStorage;
use super::{DatabaseError, PgPointStorage, Tx};
use crate::kv::raw::model::aggregate::RecordLiveCount;
use crate::kv::raw::model::page::RecordKeySelection;
use crate::records::{RecordIdentity, RecordNamespace};

mod count;
pub(crate) struct RawCountSelection<'a> {
	pub(crate) namespace: &'a RecordNamespace,
	pub(crate) selection: &'a RecordKeySelection,
}
#[derive(Debug, thiserror::Error)]
pub(crate) enum RawAggregateStorageError {
	#[error("raw aggregate database work failed")]
	Database(#[from] DatabaseError),
	#[error("raw aggregate output has an invalid count or observation shape")]
	InvalidObservation,
	#[error("raw aggregate found valued source and native records at the same identity")]
	DuplicatePhysicalValues { identity: RecordIdentity },
	#[error("raw aggregate cannot interpret source expiry for the identified record")]
	SourceExpiry { identity: RecordIdentity },
}
impl RawAggregateStorageError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Database(error) => Some(error),
			_ => None,
		}
	}
	pub(crate) fn identity(&self) -> Option<&RecordIdentity> {
		match self {
			Self::DuplicatePhysicalValues { identity } | Self::SourceExpiry { identity } => {
				Some(identity)
			}
			_ => None,
		}
	}
}
impl PgPointStorage {
	pub(crate) async fn count_raw_live(
		&self,
		tx: &mut Tx<'_>,
		fields: RawCountSelection<'_>,
	) -> Result<RecordLiveCount, RawAggregateStorageError> {
		count::count(tx, fields, RawReadLayout::Native(self)).await
	}
}
impl PgSourcePointStorage {
	pub(crate) async fn count_raw_live(
		&self,
		tx: &mut Tx<'_>,
		fields: RawCountSelection<'_>,
	) -> Result<RecordLiveCount, RawAggregateStorageError> {
		count::count(tx, fields, RawReadLayout::Source(self)).await
	}
}
impl PgRelocatingPointStorage {
	pub(crate) async fn count_raw_live(
		&self,
		tx: &mut Tx<'_>,
		fields: RawCountSelection<'_>,
	) -> Result<RecordLiveCount, RawAggregateStorageError> {
		count::count(tx, fields, RawReadLayout::Relocating(self)).await
	}
}
