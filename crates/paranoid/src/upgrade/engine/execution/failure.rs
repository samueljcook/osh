//! Closed primary-cause classification; actual ending and local settlement remain driver duties.

use crate::db::{DatabaseError, SchemaError};
use crate::installation::v10::source_gate::NativeSourceRetirementError;
use crate::runtime::admission::{NativeAdmissionError, upgrade::NativeUpgradeAdmissionError};
use crate::runtime::local_admission::RuntimeAdmissionError;
use crate::upgrade::participants::{
	NativeUpgradeParticipantsError, ParticipantInspectionCoverage, ParticipantInspectionSummary,
};
use crate::upgrade::requests::batch::NativeUpgradeBatchError;
use crate::upgrade::requests::{
	NativeUpgradeRequestError, work::creation_execution::NativeCreateExecutionError,
};
use crate::upgrade::work::UpgradeWorkError;

use super::error::NativeUpgradeStepExecutionCauseError;

mod participants;
mod progress;
mod queue;
mod v10;

/// A reason never permits retry before the actual ending and owner continuation are settled.
#[derive(Debug)]
pub(in crate::upgrade) enum NativeUpgradeStepFailureReason<'a> {
	Database(&'a DatabaseError),
	LiveParticipants,
	ProofBudget(NativeUpgradeProofBudget),
	ProtectedWait,
	ReobserveExactRequest,
	WaitForEnding,
	OwnerStopped,
	OwnershipFailure,
	Prerequisite,
}

#[derive(Clone, Copy, Debug)]
pub(in crate::upgrade) enum NativeUpgradeProofBudget {
	Participants,
}

pub(super) fn classify_cause(
	error: &NativeUpgradeStepExecutionCauseError,
) -> NativeUpgradeStepFailureReason<'_> {
	use NativeUpgradeStepExecutionCauseError as E;
	match error {
		E::Admission(error) => classify_upgrade_admission_error(error),
		E::Participants(error) => participants::classify(error),
		E::SourceWriter(error) | E::SourceData(error) => classify_source_retirement_error(error),
		E::Batch(error) => classify_batch_error(error),
		E::WorkAbsent => NativeUpgradeStepFailureReason::ReobserveExactRequest,
		E::IndexRequest(error) => participants::classify_index_error(error),
		E::ReadyRequest(error) => participants::classify_ready_error(error),
		E::ContextMismatch | E::IndexPlacement(_) => NativeUpgradeStepFailureReason::Prerequisite,
	}
}

fn classify_batch_error(error: &NativeUpgradeBatchError) -> NativeUpgradeStepFailureReason<'_> {
	use NativeUpgradeBatchError as E;
	use NativeUpgradeStepFailureReason as Reason;
	match error {
		E::Admission(error) => classify_upgrade_admission_error(error),
		E::Progress(error) => progress::classify_control_error(error),
		E::Raw(error) => v10::classify_raw_error(error),
		E::Fleet(error) => v10::classify_fleet_error(error),
		E::Lease(error) => v10::classify_lease_error(error),
		E::Queue(error) => v10::classify_queue_error(error),
		E::QueueJobConversion(error) => progress::classify_queue_job_error(error),
		E::DocumentConversion(error) => progress::classify_document_error(error),
		E::DocumentVerification(error) => progress::classify_verification_error(error),
		E::PayloadConversion(error) => progress::classify_payload_error(error),
		E::Database(error) => Reason::Database(error),
		E::RollbackFailed {
			operation_error, ..
		} => classify_batch_error(operation_error),
		E::BatchWaitElapsed => Reason::ProtectedWait,
		_ => Reason::Prerequisite,
	}
}

/// Initial read/create/recovery uses the same primary precedence without guessing a target.
pub(in crate::upgrade) fn classify_request_error(
	error: &NativeUpgradeRequestError,
) -> NativeUpgradeStepFailureReason<'_> {
	use NativeUpgradeRequestError as E;
	match error {
		E::Enrollment(error) => classify_upgrade_admission_error(error),
		E::Progress(error) => progress::classify_control_error(error),
		E::Work(error) => classify_work_error(error),
		E::Database(error) => NativeUpgradeStepFailureReason::Database(error),
		E::RollbackFailed {
			operation_error, ..
		} => classify_request_error(operation_error),
		E::PinWaitElapsed | E::RecoveryWaitElapsed => NativeUpgradeStepFailureReason::ProtectedWait,
		E::Pin(error) => classify_diagnostic_database_error(error.database_error()),
		E::Binding(error) => classify_binding_error(error),
		E::Creation(NativeCreateExecutionError::EndingPending) => {
			NativeUpgradeStepFailureReason::WaitForEnding
		}
		_ => NativeUpgradeStepFailureReason::Prerequisite,
	}
}

pub(super) fn source_retirement_database_error(
	error: &NativeSourceRetirementError<NativeUpgradeParticipantsError>,
) -> Option<&DatabaseError> {
	match error {
		NativeSourceRetirementError::Schema(error) => error.database_error(),
		NativeSourceRetirementError::Controller(error) => error.database_error(),
		NativeSourceRetirementError::Database(error) => Some(error),
		NativeSourceRetirementError::RollbackFailed {
			operation_error,
			rollback_error,
		} => source_retirement_database_error(operation_error).or(Some(rollback_error)),
		_ => None,
	}
}

fn classify_source_retirement_error(
	error: &NativeSourceRetirementError<NativeUpgradeParticipantsError>,
) -> NativeUpgradeStepFailureReason<'_> {
	match error {
		NativeSourceRetirementError::Schema(error) => classify_schema_error(error),
		NativeSourceRetirementError::Controller(error) => participants::classify(error),
		NativeSourceRetirementError::Database(error) => {
			NativeUpgradeStepFailureReason::Database(error)
		}
		NativeSourceRetirementError::RollbackFailed {
			operation_error, ..
		} => classify_source_retirement_error(operation_error),
		NativeSourceRetirementError::SourceLockWaitElapsed => {
			NativeUpgradeStepFailureReason::ProtectedWait
		}
		_ => NativeUpgradeStepFailureReason::Prerequisite,
	}
}

fn classify_schema_error(error: &SchemaError) -> NativeUpgradeStepFailureReason<'_> {
	match error {
		SchemaError::Database(error) => NativeUpgradeStepFailureReason::Database(error),
		SchemaError::RollbackFailed {
			operation_error, ..
		} => classify_schema_error(operation_error),
		_ => NativeUpgradeStepFailureReason::Prerequisite,
	}
}

fn classify_diagnostic_database_error(
	error: Option<&DatabaseError>,
) -> NativeUpgradeStepFailureReason<'_> {
	error.map_or(
		NativeUpgradeStepFailureReason::Prerequisite,
		NativeUpgradeStepFailureReason::Database,
	)
}

fn classify_work_error(error: &UpgradeWorkError) -> NativeUpgradeStepFailureReason<'_> {
	match error {
		UpgradeWorkError::WorkNotCurrent
		| UpgradeWorkError::CheckpointMismatch
		| UpgradeWorkError::GenerationMismatch
		| UpgradeWorkError::TargetMismatch
		| UpgradeWorkError::TerminalWork => NativeUpgradeStepFailureReason::ReobserveExactRequest,
		_ => NativeUpgradeStepFailureReason::Prerequisite,
	}
}

fn classify_upgrade_admission_error(
	error: &NativeUpgradeAdmissionError,
) -> NativeUpgradeStepFailureReason<'_> {
	match error {
		NativeUpgradeAdmissionError::Work(error) => classify_work_error(error),
		NativeUpgradeAdmissionError::WorkAbsent
		| NativeUpgradeAdmissionError::WorkTargetMismatch => {
			NativeUpgradeStepFailureReason::ReobserveExactRequest
		}
		NativeUpgradeAdmissionError::PreviousEndingPending => {
			NativeUpgradeStepFailureReason::WaitForEnding
		}
		NativeUpgradeAdmissionError::Local(error) => classify_local_error(error),
		NativeUpgradeAdmissionError::Admission(error) => classify_admission_error(error),
		NativeUpgradeAdmissionError::AlreadyEnrolled
		| NativeUpgradeAdmissionError::TransactionMismatch => {
			NativeUpgradeStepFailureReason::OwnershipFailure
		}
		_ => NativeUpgradeStepFailureReason::Prerequisite,
	}
}

fn classify_admission_error(error: &NativeAdmissionError) -> NativeUpgradeStepFailureReason<'_> {
	match error {
		NativeAdmissionError::Local(error) => classify_local_error(error),
		NativeAdmissionError::InstallationBinding(error) => classify_binding_error(error),
		NativeAdmissionError::GenerationChanged => {
			NativeUpgradeStepFailureReason::ReobserveExactRequest
		}
		_ => classify_diagnostic_database_error(error.database_error()),
	}
}

fn classify_local_error(error: &RuntimeAdmissionError) -> NativeUpgradeStepFailureReason<'_> {
	match error {
		RuntimeAdmissionError::Stopped { .. } => NativeUpgradeStepFailureReason::OwnerStopped,
		_ => NativeUpgradeStepFailureReason::OwnershipFailure,
	}
}

fn classify_inspection_error(
	summary: &ParticipantInspectionSummary,
) -> NativeUpgradeStepFailureReason<'_> {
	match summary.coverage {
		ParticipantInspectionCoverage::Incomplete { .. } => {
			NativeUpgradeStepFailureReason::ProofBudget(NativeUpgradeProofBudget::Participants)
		}
		ParticipantInspectionCoverage::Complete if !summary.deferred.is_empty() => {
			NativeUpgradeStepFailureReason::LiveParticipants
		}
		ParticipantInspectionCoverage::Complete => NativeUpgradeStepFailureReason::Prerequisite,
	}
}

fn classify_binding_error(
	error: &crate::runtime::admission::bindings::InstallationAdmissionBindingError,
) -> NativeUpgradeStepFailureReason<'_> {
	use crate::runtime::admission::bindings::InstallationAdmissionBindingError as E;
	match error {
		E::SourceGate(error) => classify_schema_error(error),
		E::Registration(error) => classify_installation_error(error),
		_ => NativeUpgradeStepFailureReason::Prerequisite,
	}
}
fn classify_installation_error(
	error: &crate::installation::creation::NativeInstallationError,
) -> NativeUpgradeStepFailureReason<'_> {
	use crate::installation::creation::NativeInstallationError as E;
	match error {
		E::RollbackFailed {
			operation_error, ..
		} => classify_installation_error(operation_error),
		E::Schema(error) => classify_schema_error(error),
		E::Database(error) => NativeUpgradeStepFailureReason::Database(error),
		E::Bootstrap(crate::installation::BootstrapError::Database(error)) => {
			NativeUpgradeStepFailureReason::Database(error)
		}
		E::ControlPin(error) => classify_diagnostic_database_error(error.database_error()),
		_ => NativeUpgradeStepFailureReason::Prerequisite,
	}
}
