//! Actual Queue source failures retain preparation/refusal precedence over restoration errors.

use crate::queue::engine::{
	NativeQueueGroupError, NativeQueueGroupRejectionError, conversion::QueueSourceConversionError,
};
use crate::queue::v10::source_group_storage::{
	QueueSourceGroupStorageError, apply::QueueSourceGroupApplyError,
};
use crate::queue::v10::source_pause::QueueSourcePauseError;
use crate::records::GroupMutationError;
use crate::records::postgres::group_attempt::GroupAttemptError;
use crate::runtime::admission::queue::NativeQueueSourceBindingError;

use super::NativeUpgradeStepFailureReason as Reason;

pub(super) fn classify_conversion_error(error: &QueueSourceConversionError) -> Reason<'_> {
	match error {
		QueueSourceConversionError::Admission(error) => super::classify_admission_error(error),
		QueueSourceConversionError::Queue(error) => classify_group_error(error),
		_ => Reason::Prerequisite,
	}
}
fn classify_group_error(error: &NativeQueueGroupError) -> Reason<'_> {
	match error {
		NativeQueueGroupError::RollbackFailed {
			operation_error, ..
		} => classify_group_error(operation_error),
		NativeQueueGroupError::RejectionCleanup {
			rejection_error: primary,
			..
		}
		| NativeQueueGroupError::Rejected(primary) => classify_rejection_error(primary),
		NativeQueueGroupError::SourcePause(error) => classify_pause_error(error),
		NativeQueueGroupError::SourceObservation(error) => classify_observation_error(error),
		NativeQueueGroupError::SourceApply(error) => classify_application_error(error),
		NativeQueueGroupError::Admission(error) => super::classify_admission_error(error),
		NativeQueueGroupError::Attempt(error) => classify_attempt_error(error),
		NativeQueueGroupError::Storage(error) => {
			super::classify_diagnostic_database_error(error.database_error())
		}
		NativeQueueGroupError::Database(error) => Reason::Database(error),
		NativeQueueGroupError::GroupMutation(GroupMutationError::Backend(error)) => {
			super::classify_diagnostic_database_error(error.database_error())
		}
		NativeQueueGroupError::GroupMutation(
			GroupMutationError::Rejected(_) | GroupMutationError::RejectionCleanup { .. },
		) => Reason::Prerequisite,
		_ => Reason::Prerequisite,
	}
}
fn classify_rejection_error(error: &NativeQueueGroupRejectionError) -> Reason<'_> {
	match error {
		NativeQueueGroupRejectionError::SourceRequest(error) => classify_observation_error(error),
		_ => Reason::Prerequisite,
	}
}
fn classify_observation_error(error: &QueueSourceGroupStorageError) -> Reason<'_> {
	match error {
		QueueSourceGroupStorageError::Binding(NativeQueueSourceBindingError::Admission(error)) => {
			super::classify_admission_error(error)
		}
		QueueSourceGroupStorageError::Schema(error) => super::classify_schema_error(error),
		QueueSourceGroupStorageError::Database(error) => Reason::Database(error),
		_ => Reason::Prerequisite,
	}
}
fn classify_application_error(error: &QueueSourceGroupApplyError) -> Reason<'_> {
	match error {
		QueueSourceGroupApplyError::Native(error) => {
			super::classify_diagnostic_database_error(error.database_error())
		}
		QueueSourceGroupApplyError::Database(error) => Reason::Database(error),
		QueueSourceGroupApplyError::Boundary(error) => classify_attempt_error(error),
		QueueSourceGroupApplyError::RejectionCleanup { .. } => Reason::Prerequisite,
		_ => Reason::Prerequisite,
	}
}
fn classify_pause_error(error: &QueueSourcePauseError) -> Reason<'_> {
	match error {
		QueueSourcePauseError::Native(error) => {
			super::classify_diagnostic_database_error(error.database_error())
		}
		QueueSourcePauseError::Observation(error) => classify_observation_error(error),
		QueueSourcePauseError::Application(error) => classify_application_error(error),
		QueueSourcePauseError::ObservationCleanup { .. }
		| QueueSourcePauseError::RejectionCleanup { .. }
		| QueueSourcePauseError::UnexpectedObservation { .. } => Reason::Prerequisite,
	}
}
fn classify_attempt_error(error: &GroupAttemptError) -> Reason<'_> {
	match error {
		GroupAttemptError::Storage(error) => {
			super::classify_diagnostic_database_error(error.database_error())
		}
		GroupAttemptError::Boundary { error, .. } => Reason::Database(error),
		GroupAttemptError::RejectionCleanup { .. }
		| GroupAttemptError::ReacquisitionCleanup { .. } => Reason::Prerequisite,
	}
}
