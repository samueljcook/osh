//! Actual placed Topic rejection and cleanup causes with redacted identities.

use crate::db::DatabaseError;
use crate::fleet::scalar_codec::ScalarStateError;
use crate::records::postgres::attempt_boundary::GroupAttemptRejectionError;
use crate::records::postgres::record_set::{
	PgPlacedRecordIdentity, RecordSetError, RecordSetRejectionError,
};

#[derive(Debug, thiserror::Error)]
pub(in crate::fleet::topic) enum TopicPublicationRejectionError {
	#[error("native Topic sequence and event placement overlap")]
	OverlappingPlacement,
	#[error("native Topic acquisition exceeds its explicit budget")]
	Budget(#[source] GroupAttemptRejectionError),
	#[error("native Topic sequence cannot be interpreted or incremented")]
	Sequence {
		identity: Box<PgPlacedRecordIdentity>,
		#[source]
		source: Box<ScalarStateError>,
	},
	#[error("native Topic next event already has a physical valued record")]
	OccupiedEvent {
		identity: Box<PgPlacedRecordIdentity>,
	},
	#[error("native Topic physical plan was rejected")]
	Plan(#[source] RecordSetRejectionError),
}

#[derive(Debug, thiserror::Error)]
pub(in crate::fleet::topic) enum TopicPublicationStorageError {
	#[error("native Topic placed storage failed")]
	Storage(#[from] RecordSetError),
	#[error("native Topic preparation was rejected and cleanup failed")]
	RejectionCleanup {
		#[source]
		rejection_error: Box<TopicPublicationRejectionError>,
		cleanup_error: Box<RecordSetError>,
	},
}
impl TopicPublicationStorageError {
	pub(in crate::fleet::topic) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Storage(error) => error.database_error(),
			Self::RejectionCleanup { cleanup_error, .. } => cleanup_error.database_error(),
		}
	}
	pub(in crate::fleet::topic) fn commit_outcome_is_unknown(&self) -> bool {
		self.database_error()
			.is_some_and(DatabaseError::commit_outcome_is_unknown)
	}
}
