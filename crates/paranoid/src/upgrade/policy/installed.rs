//! Compatibility with the exact installed policy during a request-bound intermediate inspection.
//!
//! This borrowed assessment supplies no admission, drainage, source phase, or promotion proof.

use super::assessment::{ParticipantPolicyAssessment, ParticipantPolicyAssessmentFields};
use super::{
	ComponentDeclaration, InstallationControlComponent, InstallationControlState,
	InstallationUpgradeParticipantDisposition, ParticipantSchema, ParticipantState,
	ParticipationSemantics, StateTimestampMicros, UpgradeCompatibilityError,
};

pub(in crate::upgrade) struct InstalledParticipantPolicyAssessment<'a>(
	ParticipantPolicyAssessment<'a>,
);

pub(in crate::upgrade) struct InstalledParticipantPolicyAssessmentFields<'a, 'd> {
	pub(crate) control: &'a InstallationControlState,
	pub(crate) administration: &'a ComponentDeclaration<'d, ParticipationSemantics>,
	pub(crate) participant: &'a ParticipantSchema,
	pub(crate) control_component: &'a InstallationControlComponent,
}

impl<'a> InstalledParticipantPolicyAssessment<'a> {
	pub(crate) fn new(
		fields: InstalledParticipantPolicyAssessmentFields<'a, '_>,
	) -> Result<Self, UpgradeCompatibilityError> {
		Ok(Self(ParticipantPolicyAssessment::new(
			ParticipantPolicyAssessmentFields {
				administration: fields.administration,
				installation_id: fields.control.installation_id(),
				components: fields.control.component_policies(),
				scopes: fields.control.scope_policies(),
				participant: fields.participant,
				control: fields.control_component,
			},
		)?))
	}

	pub(in crate::upgrade) fn assess(
		&self,
		participant: &ParticipantState,
		observed_at: StateTimestampMicros,
	) -> Result<InstallationUpgradeParticipantDisposition, UpgradeCompatibilityError> {
		self.0.assess(participant, observed_at)
	}
}
