//! Actual effective actor causes and inspection limits, with primary-before-cleanup precedence.

use crate::upgrade::participants::NativeUpgradeParticipantsError;
use crate::upgrade::participants::{
	document_index::DocumentIndexRegistrationError, document_readiness::DocumentReadyError,
	scope_addition::ScopeAdditionError, scope_reader::ScopeFinalizationError,
	target_completion::TargetCompletionError, v10::source_data::SourceDataRetirementCauseError,
};

use super::NativeUpgradeStepFailureReason as Reason;

pub(super) fn classify(error: &NativeUpgradeParticipantsError) -> Reason<'_> {
	use NativeUpgradeParticipantsError as E;
	match error {
		E::RollbackFailed {
			operation_error, ..
		} => classify(operation_error),
		E::Admission(error) => super::classify_upgrade_admission_error(error),
		E::Work(error) => super::classify_work_error(error),
		E::Begin(error) | E::Commit(error) | E::Database(error) => Reason::Database(error),
		E::InstallationBinding(error) => super::classify_binding_error(error),
		E::ControlPin(error) => super::classify_diagnostic_database_error(error.database_error()),
		E::Storage(error) => super::classify_diagnostic_database_error(error.database_error()),
		E::Page(error) => super::progress::classify_page_error(error),
		E::SourceWriterRegistration(error) => super::classify_installation_error(error),
		E::ScopeAddition(error) => classify_additive_error(error),
		E::SourceData(error) => classify_source_data_error(error),
		E::DocumentIndex(error) => classify_index_error(error),
		E::DocumentReadiness(error) => classify_ready_error(error),
		E::ScopeReader(error) => classify_reader_error(error),
		E::TargetCompletion(error) => classify_target_error(error),
		E::QueueJobInspection { summary }
		| E::SourceWriterInspection { summary }
		| E::DocumentWriterInspection { summary, .. }
		| E::PayloadWriterInspection { summary, .. } => super::classify_inspection_error(summary),

		// Actual fixed actors retain the complete summary in their named ParticipantInspection variants.
		E::ControlPinWaitElapsed => Reason::ProtectedWait,
		E::WorkAbsent
		| E::DocumentCheckpointConflict { .. }
		| E::PayloadCheckpointConflict { .. } => Reason::ReobserveExactRequest,
		_ => Reason::Prerequisite,
	}
}

fn classify_additive_error(error: &ScopeAdditionError) -> Reason<'_> {
	match error {
		ScopeAdditionError::InstallationWaitElapsed => Reason::ProtectedWait,
		ScopeAdditionError::ParticipantInspection { summary, .. } => {
			super::classify_inspection_error(summary)
		}
		ScopeAdditionError::Backend(error) => super::classify_installation_error(error),
		ScopeAdditionError::Namespace { source, .. } => {
			super::progress::classify_page_error(source)
		}
		_ => Reason::Prerequisite,
	}
}

pub(super) fn classify_index_error(error: &DocumentIndexRegistrationError) -> Reason<'_> {
	match error {
		DocumentIndexRegistrationError::ParticipantInspection { summary } => {
			super::classify_inspection_error(summary)
		}
		DocumentIndexRegistrationError::WaitElapsed => Reason::ProtectedWait,
		DocumentIndexRegistrationError::Backend(error)
		| DocumentIndexRegistrationError::StorageValidation(error) => {
			super::classify_installation_error(error)
		}
		DocumentIndexRegistrationError::Physical(error) => {
			super::classify_diagnostic_database_error(error.database_error())
		}
		DocumentIndexRegistrationError::IndexNotMaintained
		| DocumentIndexRegistrationError::ReadyLedgerAbsent => Reason::ReobserveExactRequest,
		_ => Reason::Prerequisite,
	}
}

pub(super) fn classify_ready_error(error: &DocumentReadyError) -> Reason<'_> {
	match error {
		DocumentReadyError::ParticipantInspection { summary } => {
			super::classify_inspection_error(summary)
		}
		DocumentReadyError::WaitElapsed | DocumentReadyError::DeadlineElapsed => {
			Reason::ProtectedWait
		}
		DocumentReadyError::Backend(error) => super::classify_installation_error(error),
		DocumentReadyError::Inspection(error) => match error.as_ref() {
			crate::upgrade::participants::PgDocumentIndexInspectionError::StorageValidation(
				error,
			) => super::classify_installation_error(error),
			crate::upgrade::participants::PgDocumentIndexInspectionError::Physical(error) => {
				super::classify_diagnostic_database_error(error.database_error())
			}
		},
		DocumentReadyError::WriterStageNotInstalled
		| DocumentReadyError::VerificationNotCurrent
		| DocumentReadyError::IndexNotRecorded => Reason::ReobserveExactRequest,
		_ => Reason::Prerequisite,
	}
}

fn classify_reader_error(error: &ScopeFinalizationError) -> Reason<'_> {
	match error {
		ScopeFinalizationError::ParticipantInspection { summary, .. } => {
			super::classify_inspection_error(summary)
		}
		ScopeFinalizationError::RetirementWaitElapsed => Reason::ProtectedWait,
		_ => Reason::Prerequisite,
	}
}
fn classify_target_error(error: &TargetCompletionError) -> Reason<'_> {
	match error {
		TargetCompletionError::ParticipantInspection { summary } => {
			super::classify_inspection_error(summary)
		}
		TargetCompletionError::CompletionWaitElapsed => Reason::ProtectedWait,
		TargetCompletionError::ScopeNotFinal { .. }
		| TargetCompletionError::SourceGoalNotSatisfied { .. } => Reason::ReobserveExactRequest,
		_ => Reason::Prerequisite,
	}
}
fn classify_source_data_error(error: &SourceDataRetirementCauseError) -> Reason<'_> {
	match error {
		SourceDataRetirementCauseError::ParticipantInspection { summary } => {
			super::classify_inspection_error(summary)
		}
		SourceDataRetirementCauseError::RetirementWaitElapsed => Reason::ProtectedWait,
		SourceDataRetirementCauseError::Checkpoints(error) => {
			super::v10::classify_retirement_checkpoint_error(error)
		}
		SourceDataRetirementCauseError::SourceSchema(error) => super::classify_schema_error(error),
		SourceDataRetirementCauseError::Backend(error) => super::classify_installation_error(error),
		SourceDataRetirementCauseError::Emptiness(error) => {
			super::classify_diagnostic_database_error(error.database_error())
		}
		_ => Reason::Prerequisite,
	}
}
