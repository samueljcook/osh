//! Typed Queue preparation, canonical storage, and transaction-ending failures.

use super::{
	DatabaseError, NativeAdmissionError, NativeOperationError, QueueGroupError, TransactionFailure,
};
use crate::installation::ProtocolError;
use crate::records::postgres::StateStorageError;
use crate::records::postgres::group_attempt::{GroupAttemptError, GroupAttemptRejectionError};
use crate::records::{GroupMutationError, InvalidGroupMutationError};
use crate::runtime::admission::queue::NativeQueuePlacementError;

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeQueueGroupRejectionError {
	#[error("source Queue held requests were rejected")]
	SourceRequest(#[source] crate::queue::v10::source_group_storage::QueueSourceGroupStorageError),
	#[error("source Queue preparation was rejected after completed cleanup")]
	Source(
		#[source] crate::queue::v10::source_group_storage::apply::QueueSourceApplyRejectionError,
	),
	#[error("native Queue group preparation failed")]
	Preparation(#[source] QueueGroupError),
	#[error("native Queue whole-set acquisition was rejected")]
	Acquisition(#[source] GroupAttemptRejectionError),
	#[error("native Queue group action plan was rejected")]
	Plan(#[source] InvalidGroupMutationError),
	#[error("native-only Queue plan requested a source deletion")]
	SourceDeletionInNativePlan,
	#[error("single-record Queue pause preparation requested another identity")]
	UnexpectedPauseDiscovery,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeQueueCleanupError {
	#[error("native Queue attempt restoration failed")]
	Attempt(#[source] GroupAttemptError),
	#[error("native Queue reservation cleanup failed")]
	Storage(#[source] StateStorageError),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeQueueGroupError {
	#[error("source Queue fixed pause operation failed")]
	SourcePause(#[source] crate::queue::v10::source_pause::QueueSourcePauseError),
	#[error("source Queue route has no storage and import implementations")]
	SourceMappingAbsent,
	#[error("source Queue protected observation failed")]
	SourceObservation(
		#[source] crate::queue::v10::source_group_storage::QueueSourceGroupStorageError,
	),
	#[error("source Queue atomic application failed")]
	SourceApply(
		#[source] crate::queue::v10::source_group_storage::apply::QueueSourceGroupApplyError,
	),
	#[error("native Queue placement binding failed")]
	Placement(#[from] NativeQueuePlacementError),
	#[error("native Queue transaction admission failed")]
	Admission(#[source] NativeAdmissionError),
	#[error("native Queue component policy binding failed")]
	PolicyBinding(#[source] QueueGroupError),
	#[error("native Queue operation was rejected after completed cleanup")]
	Rejected(#[source] NativeQueueGroupRejectionError),
	#[error("native Queue rejection cleanup failed")]
	RejectionCleanup {
		#[source]
		rejection_error: NativeQueueGroupRejectionError,
		cleanup_error: Box<NativeQueueCleanupError>,
	},
	#[error("native Queue whole-set attempt failed")]
	Attempt(#[from] GroupAttemptError),
	#[error("native Queue canonical storage failed")]
	Storage(#[from] StateStorageError),
	#[error("reading a Queue record batch failed")]
	RecordBatchRead(#[source] crate::records::postgres::record_many::RecordManyStorageError),
	#[error("native Queue canonical group mutation failed")]
	GroupMutation(#[source] GroupMutationError<StateStorageError>),
	#[error("native Queue database work failed")]
	Database(DatabaseError),
	#[error("a native Queue job key is not a job identifier")]
	InvalidJobKey,
	#[error("a native Queue job record cannot be read")]
	JobRecord(#[source] crate::queue::state::QueueRepresentationError),
	#[error("reading a page of the Queue's dead letters failed")]
	DeadLetterPage(#[source] crate::records::postgres::scan::RecordPageError),
	#[error("a native Queue dead letter cannot be read")]
	DeadLetterRecord(#[source] crate::queue::auxiliary_state::AuxiliaryRepresentationError),
	#[error(
		"the v10 Queue tables have schema version 2, and native Queue operations run on the v10 tables only at schema version 1"
	)]
	CoexistenceLayoutUnsupported,
	#[error("a row of the v10 Queue tables cannot be read")]
	CoexistenceRow(#[source] crate::queue::PersistedRowError),
	#[error(
		"an enqueue with a dedupe key conflicted with concurrent enqueues of the same key more times than the conflict budget allows"
	)]
	CoexistenceDedupeContention,
	#[error("a Queue value exceeds the range of its column in the v10 Queue tables")]
	CoexistenceValueOutOfRange,
	#[error(
		"the move of the failed job {job_id} to the v10 dead letters deleted no job; a trigger or a row security policy on the v10 Queue tables can cause this"
	)]
	CoexistenceDeadLetterMoveRejected { job_id: crate::queue::JobId },
	#[error("native Queue operation returned an unexpected outcome: {0}")]
	UnexpectedOutcome(String),
	#[error("native Queue transaction protocol failed")]
	Protocol(ProtocolError),
	#[error("native Queue work failed and rollback also failed")]
	RollbackFailed {
		#[source]
		operation_error: Box<Self>,
		rollback_error: DatabaseError,
	},
}

impl NativeQueueGroupError {
	/// Returns an actual retained driver cause without replacing the primary typed failure.
	pub(crate) fn database_error(&self) -> Option<&crate::db::DatabaseError> {
		match self {
			Self::SourcePause(error) => error.database_error(),
			Self::SourceObservation(error) => error.database_error(),
			Self::SourceApply(error) => error.database_error(),
			Self::Admission(error) => error.database_error(),
			Self::RejectionCleanup {
				rejection_error,
				cleanup_error,
			} => {
				let primary = match rejection_error {
					NativeQueueGroupRejectionError::SourceRequest(error) => error.database_error(),
					_ => None,
				};
				primary.or_else(|| match cleanup_error.as_ref() {
					NativeQueueCleanupError::Attempt(error) => error.database_error(),
					NativeQueueCleanupError::Storage(error) => error.database_error(),
				})
			}
			Self::Attempt(error) => error.database_error(),
			Self::Storage(error) => error.database_error(),
			Self::RecordBatchRead(error) => error.database_error(),
			Self::DeadLetterPage(error) => dead_letter_page_database_error(error),
			Self::GroupMutation(error) => match error {
				GroupMutationError::Backend(error)
				| GroupMutationError::RejectionCleanup {
					cleanup_error: error,
					..
				} => error.database_error(),
				GroupMutationError::Rejected(_) => None,
			},
			Self::Database(error) => Some(error),
			Self::RollbackFailed {
				operation_error,
				rollback_error,
			} => operation_error.database_error().or(Some(rollback_error)),
			Self::Rejected(NativeQueueGroupRejectionError::SourceRequest(error)) => {
				error.database_error()
			}
			_ => None,
		}
	}

	pub(super) fn from_operation(error: NativeOperationError<Self>) -> Self {
		match error {
			NativeOperationError::Admission(error) => Self::Admission(error),
			NativeOperationError::Operation(error) => error,
		}
	}
}

impl TransactionFailure for NativeQueueGroupError {
	fn from_protocol_error(error: ProtocolError) -> Self {
		Self::Protocol(error)
	}
	fn from_database_error(error: DatabaseError) -> Self {
		Self::Database(error)
	}
	fn from_rollback_failure(operation_error: Self, rollback_error: DatabaseError) -> Self {
		Self::RollbackFailed {
			operation_error: Box::new(operation_error),
			rollback_error,
		}
	}
	fn involves_database_failure(&self) -> bool {
		self.classify_database(DatabaseClassification::Failure)
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		self.classify_database(DatabaseClassification::UnknownCommit)
	}
}

impl NativeQueueGroupError {
	/// Whether the failure involves a database failure that `classification` selects. A variant
	/// with one retained database failure classifies the failure that
	/// [`database_error`](Self::database_error) returns; a variant that can retain several
	/// classifies each of them.
	fn classify_database(&self, classification: DatabaseClassification) -> bool {
		match self {
			Self::SourcePause(error) => match classification {
				DatabaseClassification::Failure => error.involves_database_failure(),
				DatabaseClassification::UnknownCommit => error.commit_outcome_is_unknown(),
			},
			Self::SourceObservation(error) => match classification {
				DatabaseClassification::Failure => error.involves_database_failure(),
				DatabaseClassification::UnknownCommit => error.commit_outcome_is_unknown(),
			},
			Self::SourceApply(error) => match classification {
				DatabaseClassification::Failure => error.involves_database_failure(),
				DatabaseClassification::UnknownCommit => error.commit_outcome_is_unknown(),
			},
			Self::Admission(error) => match classification {
				DatabaseClassification::Failure => error.involves_database_failure(),
				DatabaseClassification::UnknownCommit => error.commit_outcome_is_unknown(),
			},
			Self::RejectionCleanup {
				rejection_error,
				cleanup_error,
			} => {
				rejection_database(rejection_error, classification)
					|| match cleanup_error.as_ref() {
						NativeQueueCleanupError::Attempt(error) => {
							attempt_database(error, classification)
						}
						NativeQueueCleanupError::Storage(error) => error
							.database_error()
							.is_some_and(|error| classification.check(error)),
					}
			}
			Self::Attempt(error) => attempt_database(error, classification),
			Self::RollbackFailed {
				operation_error,
				rollback_error,
			} => {
				operation_error.classify_database(classification)
					|| classification.check(rollback_error)
			}
			Self::Rejected(rejection) => rejection_database(rejection, classification),
			Self::Storage(_)
			| Self::RecordBatchRead(_)
			| Self::DeadLetterPage(_)
			| Self::GroupMutation(_)
			| Self::Database(_)
			| Self::Placement(_)
			| Self::PolicyBinding(_)
			| Self::Protocol(_)
			| Self::SourceMappingAbsent
			| Self::InvalidJobKey
			| Self::JobRecord(_)
			| Self::DeadLetterRecord(_)
			| Self::CoexistenceLayoutUnsupported
			| Self::CoexistenceRow(_)
			| Self::CoexistenceDedupeContention
			| Self::CoexistenceValueOutOfRange
			| Self::CoexistenceDeadLetterMoveRejected { .. }
			| Self::UnexpectedOutcome(_) => self
				.database_error()
				.is_some_and(|error| classification.check(error)),
		}
	}
}

/// Shared classification of [`NativeQueueGroupError`] across [`crate::queue::PgQueueError`] and
/// [`crate::queue::QueueRuntimeError`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::queue) enum NativeQueueErrorClass {
	CommitOutcomeUnknown,
	Database,
	Admission,
	Contention,
	NativeDedupeConflict,
	V10DedupeConflict,
	V10,
	State,
}

impl NativeQueueGroupError {
	pub(in crate::queue) fn classify(&self) -> NativeQueueErrorClass {
		use crate::queue::v10::source_group_storage::apply::{
			QueueSourceApplyRejectionError, QueueSourceGroupApplyError,
		};
		if self.commit_outcome_is_unknown() {
			return NativeQueueErrorClass::CommitOutcomeUnknown;
		}
		if self.involves_database_failure() {
			return NativeQueueErrorClass::Database;
		}
		match self {
			Self::Placement(_)
			| Self::Admission(_)
			| Self::PolicyBinding(_)
			| Self::SourceMappingAbsent
			| Self::Protocol(_)
			| Self::CoexistenceLayoutUnsupported => NativeQueueErrorClass::Admission,
			Self::Attempt(_) | Self::CoexistenceDedupeContention => {
				NativeQueueErrorClass::Contention
			}
			Self::Rejected(rejection)
			| Self::RejectionCleanup {
				rejection_error: rejection,
				..
			} => classify_rejection(rejection),
			Self::SourceApply(QueueSourceGroupApplyError::RejectionCleanup {
				rejection_error,
				..
			}) if matches!(
				rejection_error.as_ref(),
				QueueSourceApplyRejectionError::Preparation {
					error: QueueGroupError::DedupeConflict,
					..
				}
			) =>
			{
				NativeQueueErrorClass::V10DedupeConflict
			}
			Self::SourcePause(_)
			| Self::SourceObservation(_)
			| Self::SourceApply(_)
			| Self::CoexistenceRow(_)
			| Self::CoexistenceValueOutOfRange
			| Self::CoexistenceDeadLetterMoveRejected { .. } => NativeQueueErrorClass::V10,
			Self::Storage(_)
			| Self::RecordBatchRead(_)
			| Self::GroupMutation(_)
			| Self::InvalidJobKey
			| Self::JobRecord(_)
			| Self::DeadLetterPage(_)
			| Self::DeadLetterRecord(_)
			| Self::UnexpectedOutcome(_) => NativeQueueErrorClass::State,
			Self::Database(_) => NativeQueueErrorClass::Database,
			Self::RollbackFailed {
				operation_error, ..
			} => operation_error.classify(),
		}
	}
}

fn classify_rejection(rejection: &NativeQueueGroupRejectionError) -> NativeQueueErrorClass {
	use crate::queue::v10::source_group_storage::apply::QueueSourceApplyRejectionError;
	match rejection {
		NativeQueueGroupRejectionError::Acquisition(_) => NativeQueueErrorClass::Contention,
		NativeQueueGroupRejectionError::Preparation(QueueGroupError::DedupeConflict) => {
			NativeQueueErrorClass::NativeDedupeConflict
		}
		NativeQueueGroupRejectionError::Source(QueueSourceApplyRejectionError::Preparation {
			error: QueueGroupError::DedupeConflict,
			..
		}) => NativeQueueErrorClass::V10DedupeConflict,
		NativeQueueGroupRejectionError::SourceRequest(_)
		| NativeQueueGroupRejectionError::Source(_) => NativeQueueErrorClass::V10,
		NativeQueueGroupRejectionError::Preparation(_)
		| NativeQueueGroupRejectionError::Plan(_)
		| NativeQueueGroupRejectionError::SourceDeletionInNativePlan
		| NativeQueueGroupRejectionError::UnexpectedPauseDiscovery => NativeQueueErrorClass::State,
	}
}

fn dead_letter_page_database_error(
	error: &crate::records::postgres::scan::RecordPageError,
) -> Option<&DatabaseError> {
	use crate::records::postgres::scan::RecordPageError;
	match error {
		RecordPageError::Database(error) => Some(error),
		RecordPageError::Record(error) => error.database_error(),
		_ => None,
	}
}

fn rejection_database(
	rejection: &NativeQueueGroupRejectionError,
	classification: DatabaseClassification,
) -> bool {
	match rejection {
		NativeQueueGroupRejectionError::SourceRequest(error) => match classification {
			DatabaseClassification::Failure => error.involves_database_failure(),
			DatabaseClassification::UnknownCommit => error.commit_outcome_is_unknown(),
		},
		_ => false,
	}
}
fn attempt_database(error: &GroupAttemptError, classification: DatabaseClassification) -> bool {
	match error {
		GroupAttemptError::Storage(error) => error
			.database_error()
			.is_some_and(|error| classification.check(error)),
		GroupAttemptError::Boundary { error, .. } => classification.check(error),
		GroupAttemptError::RejectionCleanup {
			cleanup_error: cleanup,
			..
		}
		| GroupAttemptError::ReacquisitionCleanup {
			cleanup_error: cleanup,
			..
		} => attempt_database(cleanup, classification),
	}
}

#[derive(Clone, Copy)]
enum DatabaseClassification {
	Failure,
	UnknownCommit,
}
impl DatabaseClassification {
	fn check(self, error: &DatabaseError) -> bool {
		match self {
			Self::Failure => true,
			Self::UnknownCommit => error.commit_outcome_is_unknown(),
		}
	}
}
