//! Fixed source-data observations inside the actual canonical exclusive control guard.

use std::sync::Arc;

use crate::installation::administration::PgNativeAdministration;
use crate::installation::control_state::InstallationControlState;
use crate::installation::v10::{PgV10SourceDataEmptinessStorage, PgV10SourceDescriptor};
use crate::runtime::admission::bindings::{
	InstallationStorageRoute, PgInstallationAdmissionBinding,
};
use crate::runtime::admission::upgrade::NativeUpgradeInvocation;
use crate::upgrade::participants::NativeUpgradeParticipantsError;
use crate::upgrade::participants::v10::source_data::{
	SourceDataRetirementCauseError, SourceDataRetirementRequest,
};
use crate::upgrade::progress::v10::source_fleet::selection::SourceFleetDeclarations;
use crate::upgrade::progress::v10::source_invariants::{
	SourceRetirementCheckpointError, SourceRetirementCheckpointFields,
	SourceRetirementSelectionCauseError, require_exhausted_source_checkpoints,
};
use crate::upgrade::work::{
	UpgradeWork, v10::source_checkpoint::PgV10ConversionProcedure,
	v10::source_retirement::SourceRetirementSelections,
};

use super::PgParticipantControlLock;

pub(in crate::upgrade) struct SourceDataInspectionFields<'a, 'request> {
	pub(crate) current: &'a InstallationControlState,
	pub(crate) work: &'a UpgradeWork,
	pub(crate) request: &'a SourceDataRetirementRequest<'request>,
	pub(crate) administration: &'a Arc<PgNativeAdministration>,
}

/// This owned metadata cannot authorize a write outside its caller's retained guard.
pub(in crate::upgrade) struct SourceDataRetirementInspection {
	pub(crate) selected: SourceRetirementSelections,
}

impl PgParticipantControlLock<'_, '_> {
	/// The concrete owning source-gate runner must already retain SourceExclusive.
	/// Private authentication happens once under this exact held current control snapshot.
	pub(in crate::upgrade) async fn inspect_source_data_retirement(
		&mut self,
		fields: SourceDataInspectionFields<'_, '_>,
	) -> Result<SourceDataRetirementInspection, NativeUpgradeParticipantsError> {
		let (tx, proof) = self.physical.companion_tx_with_proof();
		let mut completion = tx.track_state_mutation();
		let contracts = fields
			.request
			.invocation
			.source_relocation_contracts(
				tx,
				crate::runtime::admission::upgrade::HeldUpgradeControl::new(
					proof,
					fields.current,
					InstallationStorageRoute::SourceRelocation,
				),
				fields.request.resolved.candidate(),
			)
			.await?;
		let code = fields.request.resolved.code();
		let fleet_declarations = SourceFleetDeclarations::new(code);
		let fleet = fleet_declarations.bind(&contracts).map_err(|error| {
			SourceDataRetirementCauseError::Checkpoints(Box::new(
				SourceRetirementCheckpointError::Selection {
					procedure: PgV10ConversionProcedure::FleetStateRelocationV1,
					source: SourceRetirementSelectionCauseError::Fleet(Box::new(error)),
				},
			))
		})?;
		let selected = require_exhausted_source_checkpoints(SourceRetirementCheckpointFields {
			contracts: &contracts,
			work: fields.work,
			resolved: fields.request.resolved,
			administration: fields.administration,
			fleet: &fleet,
		})
		.map_err(|error| SourceDataRetirementCauseError::Checkpoints(Box::new(error)))?;
		let PgInstallationAdmissionBinding::Imported { source, .. } =
			contracts.installation_binding().as_ref()
		else {
			return Err(NativeUpgradeParticipantsError::ImportedInstallationRequired);
		};
		// The existing validator retains relation locks and rejects both inheritance directions.
		source
			.validate_existing(tx)
			.await
			.map_err(SourceDataRetirementCauseError::SourceSchema)?;
		let emptiness = PgV10SourceDataEmptinessStorage::new(source)
			.read(tx)
			.await
			.map_err(SourceDataRetirementCauseError::Emptiness)?;
		if !emptiness.is_empty() {
			return Err(SourceDataRetirementCauseError::NonemptySourceRoles {
				roles: emptiness.nonempty_roles().collect(),
			}
			.into());
		}
		completion.complete();
		Ok(SourceDataRetirementInspection { selected })
	}

	/// Retired data roles are deliberately absent from this fresh postcondition observation.
	pub(in crate::upgrade) async fn observe_retired_source_data_postcondition(
		&mut self,
		current: &InstallationControlState,
		invocation: &NativeUpgradeInvocation<'_>,
		source: &PgV10SourceDescriptor,
	) -> Result<(), NativeUpgradeParticipantsError> {
		let mut completion = self.physical.companion_tx().track_state_mutation();
		self.authenticate_upgrade(current, invocation).await?;
		source
			.validate_retained_protocol(self.physical.companion_tx())
			.await
			.map_err(SourceDataRetirementCauseError::SourceSchema)?;
		source
			.validate_retained_layout(self.physical.companion_tx())
			.await
			.map_err(SourceDataRetirementCauseError::SourceSchema)?;
		completion.complete();
		Ok(())
	}
}
