//! Named atomic Work preparation for effective physical source-data retirement.
//!
//! Selected metadata and exhausted cursors grant no IO, route, source emptiness,
//! native consistency, participant inspection, transaction exclusion or commit authority.

use std::fmt;

use super::super::{
	PgUpgradeStatus, SourceGoal, UpgradeSourceOrigin, UpgradeWork, UpgradeWorkError,
	UpgradeWorkGenerationChange,
};
use super::source_checkpoint::{
	PgV10ConversionProcedure, SourceCheckpointContext, SourceCheckpointError,
	SourceConversionInvariant,
};

/// The actual selector must derive each complete invariant from its current retained code.
pub(in crate::upgrade) struct SourceRetirementSelectionsFields {
	pub(crate) raw_bytes: SourceConversionInvariant,
	pub(crate) fleet_state: SourceConversionInvariant,
	pub(crate) lease_aggregate: SourceConversionInvariant,
	pub(crate) queue_jobs: SourceConversionInvariant,
	pub(crate) queue_dead_letters: SourceConversionInvariant,
	pub(crate) queue_pauses: SourceConversionInvariant,
}

/// Owned parsed meaning; construction establishes no selection sufficiency or physical proof.
pub(in crate::upgrade) struct SourceRetirementSelections {
	fields: SourceRetirementSelectionsFields,
}
impl SourceRetirementSelections {
	pub(crate) fn new(fields: SourceRetirementSelectionsFields) -> Self {
		Self { fields }
	}
	fn require_exhausted(&self, work: &UpgradeWork) -> Result<(), SourceCheckpointError> {
		for (procedure, invariant) in [
			(
				PgV10ConversionProcedure::RawBytesRelocationV1,
				&self.fields.raw_bytes,
			),
			(
				PgV10ConversionProcedure::FleetStateRelocationV1,
				&self.fields.fleet_state,
			),
			(
				PgV10ConversionProcedure::LeaseAggregateRelocationV1,
				&self.fields.lease_aggregate,
			),
			(
				PgV10ConversionProcedure::QueueJobRelocationV1,
				&self.fields.queue_jobs,
			),
			(
				PgV10ConversionProcedure::QueueDeadLetterRelocationV1,
				&self.fields.queue_dead_letters,
			),
			(
				PgV10ConversionProcedure::QueuePauseRelocationV1,
				&self.fields.queue_pauses,
			),
		] {
			work.require_selected_source_checkpoint(procedure, invariant)?;
		}
		Ok(())
	}
}
impl fmt::Debug for SourceRetirementSelections {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("SourceRetirementSelections")
			.finish_non_exhaustive()
	}
}

pub(in crate::upgrade) struct SourceDataRetirementWorkFields<'a> {
	pub(crate) context: SourceCheckpointContext<'a>,
	pub(crate) selected: &'a SourceRetirementSelections,
}
impl UpgradeWork {
	/// The fixed executor must atomically persist its actual DataRetired phase and this update.
	/// Every fallible check precedes mutation; this value alone proves no effective completion.
	pub(in crate::upgrade) fn complete_source_data_retirement(
		&mut self,
		fields: SourceDataRetirementWorkFields<'_>,
	) -> Result<UpgradeWorkGenerationChange, UpgradeWorkError> {
		let context = fields.context;
		self.require_checkpoint_context(
			context.control,
			context.candidate,
			context.origin,
			context.expected,
		)?;
		if matches!(
			self.status(),
			PgUpgradeStatus::Completed | PgUpgradeStatus::Cancelled
		) {
			return Err(UpgradeWorkError::TerminalWork);
		}
		if context.origin != UpgradeSourceOrigin::Imported {
			return Err(SourceCheckpointError::ImportedRequired.into());
		}
		if self.target.source_goal() != SourceGoal::DataRetired {
			return Err(SourceCheckpointError::DataRetiredGoalRequired.into());
		}
		self.require_source_checkpoint_invariants(
			context.control.component_policies(),
			context.control.scope_policies(),
		)?;
		self.require_document_checkpoint_invariants(context.control.scope_policies())?;
		self.require_document_verification_invariants(context.control.scope_policies())?;
		self.require_payload_checkpoint_invariants(context.control.scope_policies())?;
		fields.selected.require_exhausted(self)?;
		let result = UpgradeWorkGenerationChange {
			expected: self.owned_generation(),
			next: self.next_build_generation()?,
		};
		let progress = self.building_progress_mut()?;
		progress.owned_generation = result.next;
		progress.source_checkpoints.clear();
		Ok(result)
	}
}
