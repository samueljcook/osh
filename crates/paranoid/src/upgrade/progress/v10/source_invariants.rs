//! Exact current source checkpoint prerequisites derived from retained callable code.
//!
//! Exhausted metadata supplies no source emptiness, destination consistency, participant
//! coverage, physical exclusion, readiness, or source-retirement authority.

use std::sync::Arc;

use crate::installation::administration::{NativeAdministrationError, PgNativeAdministration};
use crate::runtime::admission::bindings::{
	InstallationStorageRoute, PgInstallationAdmissionBinding,
};
use crate::runtime::admission::{NativeAdmissionError, upgrade::contracts::NativeUpgradeContracts};
use crate::runtime::targets::ResolvedNativeRuntimeTarget;
use crate::upgrade::work::v10::source_checkpoint::{
	PgV10ConversionProcedure, SourceCheckpointError, SourceConversionInvariant,
};
use crate::upgrade::work::v10::source_retirement::{
	SourceRetirementSelections, SourceRetirementSelectionsFields,
};
use crate::upgrade::work::{
	PgUpgradeStatus, SourceGoal, UpgradeSourceOrigin, UpgradeWork, UpgradeWorkError,
};

use super::source_fleet::{SourceFleetConversionError, selection::BoundSourceFleetSelection};
use super::source_lease::{
	SourceLeaseConversionError,
	selection::{SourceLeaseSelectionFields, bind_source_lease_selection},
};
use super::source_queue::{SourceQueueConversionError, selection::SourceQueueDeclarations};
use super::source_raw::{
	SourceRawConversionError,
	selection::{SourceRawSelectionFields, bind_source_raw_selection},
};

pub(in crate::upgrade) struct SourceRetirementCheckpointFields<'a, 'current, 'code> {
	pub(in crate::upgrade) contracts: &'a NativeUpgradeContracts<'current>,
	pub(in crate::upgrade) work: &'a UpgradeWork,
	pub(in crate::upgrade) resolved: &'a ResolvedNativeRuntimeTarget,
	pub(in crate::upgrade) administration: &'a Arc<PgNativeAdministration>,
	pub(in crate::upgrade) fleet: &'a BoundSourceFleetSelection<'a, 'code>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SourceRetirementSelectionCauseError {
	#[error("Raw source selection failed")]
	Raw(#[source] Box<SourceRawConversionError>),
	#[error("Fleet source selection failed")]
	Fleet(#[source] Box<SourceFleetConversionError>),
	#[error("Lease source selection failed")]
	Lease(#[source] Box<SourceLeaseConversionError>),
	#[error("Queue source selection failed")]
	Queue(#[source] Box<SourceQueueConversionError>),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SourceRetirementCheckpointError {
	#[error("source retirement code and administration owners differ")]
	AdministrationAssociationMismatch,
	#[error("source retirement code differs from the authenticated immutable installation")]
	InstallationAssociationMismatch,
	#[error("source retirement Fleet selection differs from its code or authenticated context")]
	FleetSelectionAssociationMismatch,
	#[error("source retirement requires the imported relocation route")]
	SourceRelocationRequired,
	#[error("source retirement resolved target differs from the complete recorded target")]
	TargetMismatch,
	#[error("source retirement complete work context failed")]
	Work(#[from] UpgradeWorkError),
	#[error("source retirement requires a complete current checkpoint invariant")]
	Checkpoint(#[from] SourceCheckpointError),
	#[error("source retirement administration support failed")]
	Admission(#[from] NativeAdmissionError),
	#[error("source retirement administration selected writers failed")]
	Administration(#[from] NativeAdministrationError),
	#[error("source retirement selection failed for {procedure:?}")]
	Selection {
		procedure: PgV10ConversionProcedure,
		#[source]
		source: SourceRetirementSelectionCauseError,
	},
}

impl SourceRetirementCheckpointError {
	/// Returns an actual retained driver cause without replacing the primary typed failure.
	pub(crate) fn database_error(&self) -> Option<&crate::db::DatabaseError> {
		match self {
			Self::Admission(error) => error.database_error(),
			Self::Selection { source, .. } => match source {
				SourceRetirementSelectionCauseError::Raw(error) => error.database_error(),
				SourceRetirementSelectionCauseError::Fleet(error) => error.database_error(),
				SourceRetirementSelectionCauseError::Lease(error) => error.database_error(),
				SourceRetirementSelectionCauseError::Queue(error) => error.database_error(),
			},
			_ => None,
		}
	}

	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::Admission(source) => source.involves_database_failure(),
			Self::Selection { source, .. } => match source {
				SourceRetirementSelectionCauseError::Raw(source) => {
					source.involves_database_failure()
				}
				SourceRetirementSelectionCauseError::Fleet(source) => {
					source.involves_database_failure()
				}
				SourceRetirementSelectionCauseError::Lease(source) => {
					source.involves_database_failure()
				}
				SourceRetirementSelectionCauseError::Queue(source) => {
					source.involves_database_failure()
				}
			},
			_ => false,
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Admission(source) => source.commit_outcome_is_unknown(),
			Self::Selection { source, .. } => match source {
				SourceRetirementSelectionCauseError::Raw(source) => {
					source.commit_outcome_is_unknown()
				}
				SourceRetirementSelectionCauseError::Fleet(source) => {
					source.commit_outcome_is_unknown()
				}
				SourceRetirementSelectionCauseError::Lease(source) => {
					source.commit_outcome_is_unknown()
				}
				SourceRetirementSelectionCauseError::Queue(source) => {
					source.commit_outcome_is_unknown()
				}
			},
			_ => false,
		}
	}
}

/// Rechecks exact slot metadata and all six actual selections without granting an effective write.
pub(in crate::upgrade) fn require_exhausted_source_checkpoints(
	fields: SourceRetirementCheckpointFields<'_, '_, '_>,
) -> Result<SourceRetirementSelections, SourceRetirementCheckpointError> {
	let code = fields.resolved.code();
	if !Arc::ptr_eq(code.administration(), fields.administration) {
		return Err(SourceRetirementCheckpointError::AdministrationAssociationMismatch);
	}
	if !Arc::ptr_eq(code.installation(), fields.contracts.installation_binding()) {
		return Err(SourceRetirementCheckpointError::InstallationAssociationMismatch);
	}
	if !fields.fleet.matches_context(code, fields.contracts) {
		return Err(SourceRetirementCheckpointError::FleetSelectionAssociationMismatch);
	}
	if fields.contracts.storage_route() != InstallationStorageRoute::SourceRelocation
		|| !matches!(
			code.installation().as_ref(),
			PgInstallationAdmissionBinding::Imported { .. }
		) {
		return Err(SourceRetirementCheckpointError::SourceRelocationRequired);
	}
	if !fields
		.work
		.target()
		.matches_target(fields.resolved.target())
	{
		return Err(SourceRetirementCheckpointError::TargetMismatch);
	}
	let control = fields.contracts.current_control();
	fields.work.require_checkpoint_context(
		control,
		fields.resolved.candidate(),
		UpgradeSourceOrigin::Imported,
		&fields.work.checkpoint_context(),
	)?;
	if matches!(
		fields.work.status(),
		PgUpgradeStatus::Completed | PgUpgradeStatus::Cancelled
	) {
		return Err(UpgradeWorkError::TerminalWork.into());
	}
	if fields.work.target().source_goal() != SourceGoal::DataRetired {
		return Err(SourceCheckpointError::DataRetiredGoalRequired.into());
	}
	let administration = fields.administration.declaration();
	let bound = fields.contracts.bind(&administration)?;
	fields.administration.require_bound_policy(&bound)?;
	fields.work.require_source_checkpoint_invariants(
		control.component_policies(),
		control.scope_policies(),
	)?;
	fields
		.work
		.require_document_checkpoint_invariants(control.scope_policies())?;
	fields
		.work
		.require_document_verification_invariants(control.scope_policies())?;
	fields
		.work
		.require_payload_checkpoint_invariants(control.scope_policies())?;

	let raw = code.raw_kv();
	let declaration = raw.component.declaration();
	let procedure = PgV10ConversionProcedure::RawBytesRelocationV1;
	let selected = bind_source_raw_selection(SourceRawSelectionFields {
		resolved: fields.resolved,
		contracts: fields.contracts,
		declaration: &declaration,
	})
	.map_err(|error| SourceRetirementCheckpointError::Selection {
		procedure,
		source: SourceRetirementSelectionCauseError::Raw(Box::new(error)),
	})?;
	require_checkpoint(fields.work, procedure, &selected.invariant)?;
	let raw_bytes = selected.invariant;

	let procedure = PgV10ConversionProcedure::FleetStateRelocationV1;
	require_checkpoint(fields.work, procedure, fields.fleet.invariant())?;
	let fleet_state = fields.fleet.invariant().clone();

	let leases = code.leases();
	let source =
		leases
			.source
			.as_ref()
			.ok_or_else(|| SourceRetirementCheckpointError::Selection {
				procedure: PgV10ConversionProcedure::LeaseAggregateRelocationV1,
				source: SourceRetirementSelectionCauseError::Lease(Box::new(
					SourceLeaseConversionError::SourceStorageAbsent,
				)),
			})?;
	let native_declaration = leases.component.declaration();
	let bridge_declaration = source.bridge.declaration();
	let procedure = PgV10ConversionProcedure::LeaseAggregateRelocationV1;
	let selected = bind_source_lease_selection(SourceLeaseSelectionFields {
		code: leases,
		contracts: fields.contracts,
		native_declaration: &native_declaration,
		bridge_declaration: &bridge_declaration,
	})
	.map_err(|error| SourceRetirementCheckpointError::Selection {
		procedure,
		source: SourceRetirementSelectionCauseError::Lease(Box::new(error)),
	})?;
	require_checkpoint(fields.work, procedure, &selected.invariant)?;
	let lease_aggregate = selected.invariant;

	let procedure = PgV10ConversionProcedure::QueueJobRelocationV1;
	let declarations = SourceQueueDeclarations::new(code).map_err(|error| {
		SourceRetirementCheckpointError::Selection {
			procedure,
			source: SourceRetirementSelectionCauseError::Queue(Box::new(error)),
		}
	})?;
	let selected = declarations.bind(fields.contracts).map_err(|error| {
		SourceRetirementCheckpointError::Selection {
			procedure,
			source: SourceRetirementSelectionCauseError::Queue(Box::new(error)),
		}
	})?;
	for procedure in [
		PgV10ConversionProcedure::QueueJobRelocationV1,
		PgV10ConversionProcedure::QueueDeadLetterRelocationV1,
		PgV10ConversionProcedure::QueuePauseRelocationV1,
	] {
		require_checkpoint(fields.work, procedure, &selected.invariant)?;
	}
	Ok(SourceRetirementSelections::new(
		SourceRetirementSelectionsFields {
			raw_bytes,
			fleet_state,
			lease_aggregate,
			queue_jobs: selected.invariant.clone(),
			queue_dead_letters: selected.invariant.clone(),
			queue_pauses: selected.invariant,
		},
	))
}

fn require_checkpoint(
	work: &UpgradeWork,
	procedure: PgV10ConversionProcedure,
	invariant: &SourceConversionInvariant,
) -> Result<(), SourceRetirementCheckpointError> {
	work.require_selected_source_checkpoint(procedure, invariant)
		.map_err(Into::into)
}
