//! Concrete source wrappers, identified record causes and selected source checkpoint failures.

use crate::fleet::lease::v10::source_store::LeaseSourceStorageError;
use crate::records::postgres::v10::relocating_point::RelocatingPointError;
use crate::upgrade::progress::v10::{
	source_fleet::SourceFleetConversionError,
	source_invariants::{SourceRetirementCheckpointError, SourceRetirementSelectionCauseError},
	source_lease::SourceLeaseConversionError,
	source_queue::SourceQueueConversionError,
	source_raw::SourceRawConversionError,
};

use super::NativeUpgradeStepFailureReason as Reason;

pub(super) fn classify_raw_error(error: &SourceRawConversionError) -> Reason<'_> {
	match error {
		SourceRawConversionError::Progress(error) => super::progress::classify_control_error(error),
		SourceRawConversionError::Enrollment(error) => {
			super::classify_upgrade_admission_error(error)
		}
		SourceRawConversionError::Admission(error) => super::classify_admission_error(error),
		SourceRawConversionError::Work(error) => super::classify_work_error(error),
		SourceRawConversionError::Point { source, .. } => classify_point_error(source),
		SourceRawConversionError::Page(error) => {
			super::classify_diagnostic_database_error(error.database_error())
		}
		_ => Reason::Prerequisite,
	}
}
pub(super) fn classify_lease_error(error: &SourceLeaseConversionError) -> Reason<'_> {
	match error {
		SourceLeaseConversionError::Progress(error) => {
			super::progress::classify_control_error(error)
		}
		SourceLeaseConversionError::Enrollment(error) => {
			super::classify_upgrade_admission_error(error)
		}
		SourceLeaseConversionError::Admission(error) => super::classify_admission_error(error),
		SourceLeaseConversionError::Work(error) => super::classify_work_error(error),
		SourceLeaseConversionError::Lease { source, .. } => classify_lease_storage_error(source),
		SourceLeaseConversionError::Page(error) => {
			super::classify_diagnostic_database_error(error.database_error())
		}
		_ => Reason::Prerequisite,
	}
}
pub(super) fn classify_fleet_error(error: &SourceFleetConversionError) -> Reason<'_> {
	match error {
		SourceFleetConversionError::Record { source, .. } => classify_fleet_error(source),
		SourceFleetConversionError::Progress(error) => {
			super::progress::classify_control_error(error)
		}
		SourceFleetConversionError::Enrollment(error) => {
			super::classify_upgrade_admission_error(error)
		}
		SourceFleetConversionError::Admission(error) => super::classify_admission_error(error),
		SourceFleetConversionError::Binding(error) => super::classify_binding_error(error),
		SourceFleetConversionError::Work(error) => super::classify_work_error(error),
		SourceFleetConversionError::Point { source, .. } => classify_point_error(source),
		SourceFleetConversionError::Discard { source, .. } => {
			super::classify_diagnostic_database_error(source.database_error())
		}
		SourceFleetConversionError::Page(error) => {
			super::classify_diagnostic_database_error(error.database_error())
		}
		_ => Reason::Prerequisite,
	}
}
pub(super) fn classify_queue_error(error: &SourceQueueConversionError) -> Reason<'_> {
	match error {
		SourceQueueConversionError::Progress(error) => {
			super::progress::classify_control_error(error)
		}
		SourceQueueConversionError::Enrollment(error) => {
			super::classify_upgrade_admission_error(error)
		}
		SourceQueueConversionError::Work(error) => super::classify_work_error(error),
		SourceQueueConversionError::Binding(error) => {
			super::queue::classify_conversion_error(error)
		}
		SourceQueueConversionError::Record { source, .. } => {
			super::queue::classify_conversion_error(source)
		}
		SourceQueueConversionError::Page(error) => {
			super::classify_diagnostic_database_error(error.database_error())
		}
		_ => Reason::Prerequisite,
	}
}
fn classify_point_error(error: &RelocatingPointError) -> Reason<'_> {
	match error {
		RelocatingPointError::Schema(error) => super::classify_schema_error(error),
		_ => super::classify_diagnostic_database_error(error.database_error()),
	}
}
fn classify_lease_storage_error(error: &LeaseSourceStorageError) -> Reason<'_> {
	match error {
		LeaseSourceStorageError::CleanupFailed {
			operation_error, ..
		} => classify_lease_storage_error(operation_error),
		LeaseSourceStorageError::Schema(error) => super::classify_schema_error(error),
		_ => super::classify_diagnostic_database_error(error.database_error()),
	}
}
pub(super) fn classify_retirement_checkpoint_error(
	error: &SourceRetirementCheckpointError,
) -> Reason<'_> {
	match error {
		SourceRetirementCheckpointError::Work(error) => super::classify_work_error(error),
		SourceRetirementCheckpointError::Admission(error) => super::classify_admission_error(error),
		SourceRetirementCheckpointError::Selection { source, .. } => match source {
			SourceRetirementSelectionCauseError::Raw(error) => classify_raw_error(error),
			SourceRetirementSelectionCauseError::Fleet(error) => classify_fleet_error(error),
			SourceRetirementSelectionCauseError::Lease(error) => classify_lease_error(error),
			SourceRetirementSelectionCauseError::Queue(error) => classify_queue_error(error),
		},
		_ => Reason::Prerequisite,
	}
}
