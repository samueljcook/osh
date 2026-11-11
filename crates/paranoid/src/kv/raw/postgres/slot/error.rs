//! Slot failures retain actual physical identity, original input position, and cleanup causes.
use super::super::attempt_boundary::GroupAttemptError;
use super::super::delete::RawDeleteStorageError;
use super::super::v10::source_point::{SourcePhysicalError, SourcePointError};
use super::super::{DatabaseError, StateStorageError};
use crate::kv::raw::model::slot::{RawSlotSeedOverflowError, RawWriteExpirationOverflowError};
use crate::records::RecordIdentity;

#[derive(Debug, thiserror::Error)]
pub(crate) enum RawSlotRetryReasonError {
	#[error("an intended native seed changed or could not be held")]
	NativeSeedChanged { identity: RecordIdentity },
	#[error("an intended seed has a source value")]
	SourceSeedChanged { identity: RecordIdentity },
	#[error("the proposed slot could not be held")]
	CandidateSkipped { identity: RecordIdentity },
	#[error("the proposed source slot disappeared")]
	CandidateDisappeared { identity: RecordIdentity },
	#[error("the proposed slot became live at the final clock")]
	CandidateBecameLive { identity: RecordIdentity },
}
impl RawSlotRetryReasonError {
	pub(super) fn identity(&self) -> &RecordIdentity {
		match self {
			Self::NativeSeedChanged { identity }
			| Self::SourceSeedChanged { identity }
			| Self::CandidateSkipped { identity }
			| Self::CandidateDisappeared { identity }
			| Self::CandidateBecameLive { identity } => identity,
		}
	}
}
#[derive(Debug, thiserror::Error)]
pub(crate) enum RawSlotStorageError {
	#[error("raw slot database work failed")]
	Database(#[from] DatabaseError),
	#[error("raw slot native header is invalid")]
	Native(#[from] StateStorageError),
	#[error("raw slot source header is invalid")]
	Source(#[from] SourcePhysicalError),
	#[error("raw slot source write metadata is invalid")]
	SourceWrite(#[from] SourcePointError),
	#[error("raw slot protected physical set is invalid")]
	Physical(#[from] RawDeleteStorageError),
	#[error("raw slot attempt boundary failed")]
	Boundary(#[from] GroupAttemptError),
	#[error("raw slot write expiration is unrepresentable")]
	Expiration(#[from] RawWriteExpirationOverflowError),
	#[error("raw slot neutral seed time is unrepresentable")]
	Seed(#[from] RawSlotSeedOverflowError),
	#[error("raw slot returned unknown, duplicate, missing, or mismatched physical output")]
	Coverage,
	#[error("raw slot request or physical output shape is invalid")]
	Shape,
	#[error("raw slot has both native and source values")]
	DuplicatePhysicalValues,
	#[error("raw slot record failed")]
	Record {
		identity: RecordIdentity,
		input_index: Option<usize>,
		#[source]
		source: Box<Self>,
	},
	#[error("raw slot attempt requires a new protected proposal")]
	Replan {
		#[source]
		source: RawSlotRetryReasonError,
	},
	#[error("raw slot replan budget was exhausted after restoration")]
	RejectedAfterRestore {
		#[source]
		source: RawSlotRetryReasonError,
	},
	#[error("raw slot preparation was rejected after restoration")]
	Restored {
		#[source]
		primary: Box<Self>,
	},
	#[error("raw slot rejection and restoration both failed")]
	RejectionCleanup {
		#[source]
		primary_error: Box<Self>,
		cleanup_error: Box<GroupAttemptError>,
	},
}
impl RawSlotStorageError {
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
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Database(e) => Some(e),
			Self::Native(e) => e.database_error(),
			Self::SourceWrite(e) => e.database_error(),
			Self::Physical(e) => e.database_error(),
			Self::Boundary(e) => e.database_error(),
			Self::Record { source, .. } => source.database_error(),
			Self::Restored { primary } => primary.database_error(),
			Self::RejectionCleanup {
				primary_error,
				cleanup_error,
			} => primary_error
				.database_error()
				.or_else(|| cleanup_error.database_error()),
			_ => None,
		}
	}
	pub(crate) fn identity(&self) -> Option<&RecordIdentity> {
		match self {
			Self::Record { identity, .. } => Some(identity),
			Self::Physical(e) => e.identity(),
			Self::RejectedAfterRestore { source } | Self::Replan { source } => {
				Some(source.identity())
			}
			Self::Restored { primary }
			| Self::RejectionCleanup {
				primary_error: primary,
				..
			} => primary.identity(),
			_ => None,
		}
	}
	pub(crate) fn input_index(&self) -> Option<usize> {
		match self {
			Self::Record { input_index, .. } => *input_index,
			Self::Restored { primary }
			| Self::RejectionCleanup {
				primary_error: primary,
				..
			} => primary.input_index(),
			_ => None,
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Record { source, .. } => source.commit_outcome_is_unknown(),
			Self::Restored { primary } => primary.commit_outcome_is_unknown(),
			Self::RejectionCleanup {
				primary_error,
				cleanup_error,
			} => {
				primary_error.commit_outcome_is_unknown()
					|| cleanup_error.commit_outcome_is_unknown()
			}
			Self::SourceWrite(e) => e.commit_outcome_is_unknown(),
			_ => self
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
		}
	}
	pub(super) fn known_restored(&self) -> bool {
		match self {
			Self::RejectedAfterRestore { .. } | Self::Restored { .. } => true,
			Self::Record { source, .. } => source.known_restored(),
			_ => false,
		}
	}
}
