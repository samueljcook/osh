//! Exact placed record causes with redacted default diagnostics.

use crate::db::DatabaseError;
use crate::records::InvalidGroupMutationError;

use super::{PgPlacedRecordIdentity, StateStorageError};

/// Actual mutation parsing or unavailable body projection at one held physical identity.
#[derive(Debug)]
pub(crate) struct RecordSetRejectionError {
	source: RecordSetRejectionCause,
}
#[derive(Debug)]
enum RecordSetRejectionCause {
	Mutation(Box<InvalidGroupMutationError>),
	FullBodyRequired,
}
impl RecordSetRejectionError {
	pub(super) fn mutation(source: InvalidGroupMutationError) -> Self {
		Self {
			source: RecordSetRejectionCause::Mutation(Box::new(source)),
		}
	}
	pub(super) fn full_body_required() -> Self {
		Self {
			source: RecordSetRejectionCause::FullBodyRequired,
		}
	}
	pub(crate) fn mutation_source(&self) -> Option<&InvalidGroupMutationError> {
		match &self.source {
			RecordSetRejectionCause::Mutation(source) => Some(source),
			RecordSetRejectionCause::FullBodyRequired => None,
		}
	}
}
impl std::fmt::Display for RecordSetRejectionError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(match self.source {
			RecordSetRejectionCause::Mutation(_) => "native placed record mutation was rejected",
			RecordSetRejectionCause::FullBodyRequired => {
				"native placed record body was not acquired"
			}
		})
	}
}
impl std::error::Error for RecordSetRejectionError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		self.mutation_source().map(|error| error as _)
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RecordSetError {
	#[error("native placed record Header cannot be upgraded to Full after acquisition")]
	HeaderToFullUpgrade {
		identity: Box<PgPlacedRecordIdentity>,
	},
	#[error("native placed record extension does not follow canonical lock order")]
	NonCanonicalExtension {
		identity: Box<PgPlacedRecordIdentity>,
	},
	#[error("native placed record attempt boundary failed")]
	Boundary(#[source] Box<super::super::attempt_boundary::GroupAttemptError>),
	#[error("native placed record acquisition requires at least one target")]
	EmptySet,
	#[error("native placed record storage work failed")]
	Storage {
		identity: Box<PgPlacedRecordIdentity>,
		#[source]
		source: Box<StateStorageError>,
	},
	#[error("native placed record authoritative clock observation failed")]
	Clock(#[source] StateStorageError),
	#[error("native placed record mutation was rejected and reservation cleanup failed")]
	RejectionCleanup {
		#[source]
		rejection_error: Box<RecordSetRejectionError>,
		cleanup_error: Box<Self>,
	},
}

impl RecordSetError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Storage { source, .. } => source.database_error(),
			Self::Boundary(error) => error.database_error(),
			Self::Clock(source) => source.database_error(),
			Self::RejectionCleanup { cleanup_error, .. } => cleanup_error.database_error(),
			Self::EmptySet
			| Self::NonCanonicalExtension { .. }
			| Self::HeaderToFullUpgrade { .. } => None,
		}
	}
	pub(crate) fn involves_database_failure(&self) -> bool {
		self.database_error().is_some()
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		self.database_error()
			.is_some_and(DatabaseError::commit_outcome_is_unknown)
	}
}

impl From<super::super::attempt_boundary::GroupAttemptError> for RecordSetError {
	fn from(source: super::super::attempt_boundary::GroupAttemptError) -> Self {
		Self::Boundary(Box::new(source))
	}
}
