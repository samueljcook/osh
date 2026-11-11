//! Real native slot failures retain member identity independently from cleanup and ending.

use crate::db::DatabaseError;
use crate::records::RecordIdentity;

use crate::kv::raw::postgres::delete::RawDeleteStorageError;
use crate::records::postgres::StateStorageError;

#[derive(Debug, thiserror::Error)]
pub(crate) enum RecordSlotError {
	#[error("native slot database work failed")]
	Database(#[from] DatabaseError),
	#[error("native slot physical record operation failed")]
	Physical(#[from] StateStorageError),
	#[error("native slot final clock frame is invalid")]
	Clock(#[source] RawDeleteStorageError),
	#[error("native slot request or physical output has an invalid shape")]
	Shape,
	#[error("native slot returned unknown, duplicate, missing or mismatched output")]
	Coverage,
	#[error("native slot member failed")]
	Record {
		identity: RecordIdentity,
		input_index: Option<usize>,
		#[source]
		source: Box<Self>,
	},
	#[error("native slot reservation cleanup failed")]
	Cleanup {
		#[source]
		source: Box<Self>,
	},
}
impl RecordSlotError {
	pub(super) fn with_identity(
		self,
		identity: RecordIdentity,
		input_index: Option<usize>,
	) -> Self {
		Self::Record {
			identity,
			input_index,
			source: Box::new(self),
		}
	}
	pub(crate) fn identity(&self) -> Option<&RecordIdentity> {
		match self {
			Self::Record { identity, .. } => Some(identity),
			Self::Cleanup { source } => source.identity(),
			Self::Clock(source) => source.identity(),
			Self::Physical(
				StateStorageError::InvalidRecord { identity, .. }
				| StateStorageError::MutationRowMissing { identity }
				| StateStorageError::MetadataValueAbsent { identity },
			) => Some(identity),
			_ => None,
		}
	}
	pub(crate) fn input_index(&self) -> Option<usize> {
		match self {
			Self::Record { input_index, .. } => *input_index,
			Self::Cleanup { source } => source.input_index(),
			_ => None,
		}
	}
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Database(source) => Some(source),
			Self::Physical(source) => source.database_error(),
			Self::Clock(source) => source.database_error(),
			Self::Record { source, .. } | Self::Cleanup { source } => source.database_error(),
			Self::Shape | Self::Coverage => None,
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		self.database_error()
			.is_some_and(DatabaseError::commit_outcome_is_unknown)
	}
}
