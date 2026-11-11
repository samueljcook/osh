//! Actual attempt rejection, physical identity, and restoration causes remain distinct.
use super::super::super::attempt_boundary::GroupAttemptError;
use super::super::super::slot::RawRejectionSettlement;
use super::super::RawDeleteStorageError;
use crate::db::DatabaseError;
use crate::records::RecordIdentity;
#[derive(Debug, thiserror::Error)]
pub(crate) enum RawNamespaceRetryReasonError {
	#[error("a native identity disappeared before exclusion")]
	NativeRacedAbsent { identity: RecordIdentity },
	#[error("a source identity was not held after the non-skip acquisition")]
	SourceUnheld { identity: RecordIdentity },
	#[error("the selected namespace gained {additional_count} identities")]
	Expanded { additional_count: usize },
}
impl RawNamespaceRetryReasonError {
	fn identity(&self) -> Option<&RecordIdentity> {
		match self {
			Self::NativeRacedAbsent { identity } | Self::SourceUnheld { identity } => {
				Some(identity)
			}
			Self::Expanded { .. } => None,
		}
	}
}
#[derive(Debug, thiserror::Error)]
#[error("raw namespace whole-set attempt budget is exhausted")]
pub(crate) struct RawNamespaceRejectionError {
	pub(crate) source: RawNamespaceRetryReasonError,
}
#[derive(Debug, thiserror::Error)]
pub(crate) enum RawNamespaceStorageError {
	#[error("raw namespace physical deletion failed")]
	Storage(#[from] RawDeleteStorageError),
	#[error("raw namespace attempt boundary failed")]
	Boundary(#[from] GroupAttemptError),
	#[error("raw namespace retry was rejected after known restoration")]
	Rejected(#[source] RawNamespaceRejectionError),
	#[error("raw namespace rejection and restoration both failed")]
	RejectionCleanup {
		rejection_error: Box<RawNamespaceRejectionError>,
		#[source]
		cleanup_error: Box<GroupAttemptError>,
	},
	#[error("raw namespace deletion count or ordinal is unrepresentable")]
	CountUnrepresentable,
	#[error("a fully covered held identity still has a valued representation")]
	CoveredIdentityReappeared { identity: RecordIdentity },
}
impl RawNamespaceStorageError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Storage(e) => e.database_error(),
			Self::Boundary(e) => e.database_error(),
			Self::RejectionCleanup { cleanup_error, .. } => cleanup_error.database_error(),
			_ => None,
		}
	}
	pub(crate) fn identity(&self) -> Option<&RecordIdentity> {
		match self {
			Self::Storage(e) => e.identity(),
			Self::Rejected(e) => e.source.identity(),
			Self::RejectionCleanup {
				rejection_error, ..
			} => rejection_error.source.identity(),
			Self::CoveredIdentityReappeared { identity } => Some(identity),
			_ => None,
		}
	}
	/// Whether the deletion guard finishes its tracking after this failure. Only a rejection after
	/// Paranoid restored the selection lets the caller's transaction commit.
	pub(crate) fn settlement(&self) -> RawRejectionSettlement {
		match self {
			Self::Rejected(_) => RawRejectionSettlement::KnownRestored,
			_ => RawRejectionSettlement::WholeRollbackRequired,
		}
	}
}
