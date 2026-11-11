//! Protected progress, document, and payload page failures.

use crate::records::postgres::scan::RecordPageError;
use crate::upgrade::progress::{
	ControlProgressError, document_conversion::DocumentConversionPageError,
	document_verification::DocumentVerificationPageError,
	payload_conversion::PayloadConversionPageError,
};

use super::NativeUpgradeStepFailureReason as Reason;

pub(super) fn classify_control_error(error: &ControlProgressError) -> Reason<'_> {
	match error {
		ControlProgressError::Enrollment(error) => super::classify_upgrade_admission_error(error),
		ControlProgressError::Binding(error) => super::classify_binding_error(error),
		ControlProgressError::Pin(error) => super::classify_diagnostic_database_error(error.database_error()),
		ControlProgressError::RejectionCleanup { operation_error, .. } => classify_control_error(operation_error),
		ControlProgressError::Work(error) => super::classify_work_error(error),
		ControlProgressError::WorkAbsent | ControlProgressError::EnrolledWorkMismatch => Reason::ReobserveExactRequest,
		ControlProgressError::PinWaitElapsed => Reason::ProtectedWait,
		ControlProgressError::Creation(crate::upgrade::requests::work::creation_execution::NativeCreateExecutionError::EndingPending) => Reason::WaitForEnding,
		_ => Reason::Prerequisite,
	}
}

pub(super) fn classify_document_error(error: &DocumentConversionPageError) -> Reason<'_> {
	match error {
		DocumentConversionPageError::Admission(error) => {
			super::classify_upgrade_admission_error(error)
		}
		DocumentConversionPageError::Progress(error) => classify_control_error(error),
		DocumentConversionPageError::Work(error) => super::classify_work_error(error),
		DocumentConversionPageError::Placement(error) => super::classify_binding_error(error),
		DocumentConversionPageError::Page(error) => classify_page_error(error),
		DocumentConversionPageError::Storage(error) => {
			super::classify_diagnostic_database_error(error.database_error())
		}
		DocumentConversionPageError::WriterStageNotInstalled
		| DocumentConversionPageError::CheckpointInvariantMismatch => Reason::ReobserveExactRequest,
		_ => Reason::Prerequisite,
	}
}
pub(super) fn classify_verification_error(error: &DocumentVerificationPageError) -> Reason<'_> {
	match error {
		DocumentVerificationPageError::Admission(error) => {
			super::classify_upgrade_admission_error(error)
		}
		DocumentVerificationPageError::Progress(error) => classify_control_error(error),
		DocumentVerificationPageError::Work(error) => super::classify_work_error(error),
		DocumentVerificationPageError::Placement(error) => super::classify_binding_error(error),
		DocumentVerificationPageError::Backend(error) => super::classify_installation_error(error),
		DocumentVerificationPageError::Page(error) => classify_page_error(error),
		DocumentVerificationPageError::WriterStageNotInstalled
		| DocumentVerificationPageError::ConversionNotScanned
		| DocumentVerificationPageError::CheckpointInvariantMismatch
		| DocumentVerificationPageError::IndexNotRecorded => Reason::ReobserveExactRequest,
		DocumentVerificationPageError::DeadlineElapsed => Reason::ProtectedWait,
		_ => Reason::Prerequisite,
	}
}
pub(super) fn classify_payload_error(error: &PayloadConversionPageError) -> Reason<'_> {
	match error {
		PayloadConversionPageError::Admission(error) => {
			super::classify_upgrade_admission_error(error)
		}
		PayloadConversionPageError::Progress(error) => classify_control_error(error),
		PayloadConversionPageError::Work(error) => super::classify_work_error(error),
		PayloadConversionPageError::Placement(error) => super::classify_binding_error(error),
		PayloadConversionPageError::Page(error) => classify_page_error(error),
		PayloadConversionPageError::Storage(error) => {
			super::classify_diagnostic_database_error(error.database_error())
		}
		PayloadConversionPageError::WriterStageNotInstalled
		| PayloadConversionPageError::CheckpointInvariantMismatch => Reason::ReobserveExactRequest,
		PayloadConversionPageError::DeadlineElapsed => Reason::ProtectedWait,
		_ => Reason::Prerequisite,
	}
}
pub(super) fn classify_page_error(error: &RecordPageError) -> Reason<'_> {
	match error {
		RecordPageError::Database(error) => Reason::Database(error),
		RecordPageError::Record(error) => {
			super::classify_diagnostic_database_error(error.database_error())
		}
		_ => Reason::Prerequisite,
	}
}

pub(super) fn classify_queue_job_error(
	error: &crate::upgrade::progress::queue_job_conversion::QueueJobConversionPageError,
) -> Reason<'_> {
	match error {
		crate::upgrade::progress::queue_job_conversion::QueueJobConversionPageError::Admission(
			error,
		) => super::classify_upgrade_admission_error(error),
		crate::upgrade::progress::queue_job_conversion::QueueJobConversionPageError::Progress(
			error,
		) => classify_control_error(error),
		crate::upgrade::progress::queue_job_conversion::QueueJobConversionPageError::Work(
			error,
		) => super::classify_work_error(error),
		crate::upgrade::progress::queue_job_conversion::QueueJobConversionPageError::Page(
			error,
		) => classify_page_error(error),
		crate::upgrade::progress::queue_job_conversion::QueueJobConversionPageError::Storage(
			error,
		) => super::classify_diagnostic_database_error(error.database_error()),
		_ => Reason::Prerequisite,
	}
}
