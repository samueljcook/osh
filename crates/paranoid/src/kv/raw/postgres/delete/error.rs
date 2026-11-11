//! Exact physical identity, candidate rank, and deletion step accompany actual retained causes.
use super::super::{StateStorageError, v10::source_point::SourcePhysicalError};
use crate::db::DatabaseError;
use crate::records::RecordIdentity;
#[derive(Clone, Copy, Debug)]
/// The step of a deletion at which a record failed.
pub(crate) enum RecordDeletionStep {
	/// Finding or first reading the records to delete failed.
	Discovery,
	/// Reading or deleting the record failed.
	Native,
	/// While the namespace is still in v10 storage, reading or deleting the v10 record
	/// failed.
	Source,
	/// Removing a placeholder record that the operation had created failed.
	ReservationCleanup,
}
#[derive(Debug, thiserror::Error)]
pub(crate) enum RawDeleteStorageError {
	#[error("raw deletion database work failed")]
	Database(#[from] DatabaseError),
	#[error("raw deletion native record is invalid")]
	Native(#[from] StateStorageError),
	#[error("raw deletion source record is invalid")]
	Source(#[from] SourcePhysicalError),
	#[error("raw deletion source-domain key is not UTF-8")]
	Utf8(#[from] std::str::Utf8Error),
	#[error("raw deletion source-domain key is not canonical")]
	SourceKey(#[from] crate::kv::KeyError),
	#[error("raw deletion returned an invalid physical shape")]
	Shape,
	#[error("raw deletion returned unknown, repeated, missing, or mismatched identities or tags")]
	Coverage,
	#[error("raw deletion found valued source and native records at the same identity")]
	DuplicatePhysicalValues,
	#[error("raw deletion record failed")]
	Record {
		identity: RecordIdentity,
		candidate_rank: Option<usize>,
		step: RecordDeletionStep,
		#[source]
		source: Box<Self>,
	},
}
impl RawDeleteStorageError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Database(e) => Some(e),
			Self::Native(e) => e.database_error(),
			Self::Record { source, .. } => source.database_error(),
			_ => None,
		}
	}
	pub(crate) fn identity(&self) -> Option<&RecordIdentity> {
		match self {
			Self::Record { identity, .. } => Some(identity),
			_ => None,
		}
	}
	pub(super) fn with_identity(self, fields: RawDeleteRecordErrorFields) -> Self {
		Self::Record {
			identity: fields.identity,
			candidate_rank: fields.candidate_rank,
			step: fields.step,
			source: Box::new(self),
		}
	}
}
pub(super) struct RawDeleteRecordErrorFields {
	pub(super) identity: RecordIdentity,
	pub(super) candidate_rank: Option<usize>,
	pub(super) step: RecordDeletionStep,
}
