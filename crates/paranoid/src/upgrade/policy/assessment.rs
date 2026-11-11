//! Pure participant compatibility with explicit effective or proposed policies.
//!
//! A proposed policy is distinct from transaction-observed control. These checks establish
//! no admission, stage ownership, source exclusion, data coverage or promotion authority.

use super::{
	AdministrativePolicyRequirements, ComponentDeclaration, ComponentPolicy,
	InstallationControlComponent, InstallationUpgradeParticipantDisposition, ParticipantSchema,
	ParticipantState, ParticipationSemantics, ScopePolicySet, SortableId, StateTimestampMicros,
	UpgradeCompatibilityError,
};

pub(in crate::upgrade) struct ParticipantPolicyAssessmentFields<'a, 'd> {
	pub(crate) administration: &'a ComponentDeclaration<'d, ParticipationSemantics>,
	pub(crate) installation_id: SortableId,
	pub(crate) components: &'a [ComponentPolicy],
	pub(crate) scopes: &'a ScopePolicySet,
	pub(crate) participant: &'a ParticipantSchema,
	pub(crate) control: &'a InstallationControlComponent,
}

pub(in crate::upgrade) struct ParticipantPolicyAssessment<'a> {
	installation_id: SortableId,
	components: &'a [ComponentPolicy],
	scopes: &'a ScopePolicySet,
	administration: AdministrativePolicyRequirements,
}

impl<'a> ParticipantPolicyAssessment<'a> {
	pub(crate) fn new(
		fields: ParticipantPolicyAssessmentFields<'a, '_>,
	) -> Result<Self, UpgradeCompatibilityError> {
		let declaration = fields.administration;
		let policy = fields
			.components
			.iter()
			.find(|policy| policy.component_identifier() == declaration.identifier())
			.ok_or(UpgradeCompatibilityError::AdministrativePolicyAbsent)?;
		let bound = declaration.bind(policy)?;
		let administration = AdministrativePolicyRequirements::from_bound(
			&bound,
			fields.participant,
			fields.control,
		)?;
		Ok(Self {
			installation_id: fields.installation_id,
			components: fields.components,
			scopes: fields.scopes,
			administration,
		})
	}

	pub(in crate::upgrade) fn assess(
		&self,
		participant: &ParticipantState,
		observed_at: StateTimestampMicros,
	) -> Result<InstallationUpgradeParticipantDisposition, UpgradeCompatibilityError> {
		if participant.installation_id() != self.installation_id {
			return Err(UpgradeCompatibilityError::ParticipantInstallationMismatch);
		}
		if !participant.is_live_at(observed_at) {
			return Ok(InstallationUpgradeParticipantDisposition::RequiresDurableFencing);
		}
		let capabilities = participant.capabilities();
		if capabilities
			.check_installed_policies(self.components)
			.and_then(|()| self.administration.require_participant_state(capabilities))
			.is_err()
		{
			return Ok(InstallationUpgradeParticipantDisposition::UnsupportedCore);
		}
		if capabilities.check_scope_policies(self.scopes).is_err() {
			return Ok(InstallationUpgradeParticipantDisposition::UnsupportedScope);
		}
		Ok(InstallationUpgradeParticipantDisposition::Compatible)
	}
}
