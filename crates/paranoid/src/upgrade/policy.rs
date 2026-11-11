//! Code-bound upgrade policies and observations of recorded runtime compatibility.
//!
//! These pure checks provide no promotion, transaction drainage, or durable fencing proof.
//! The controller must perform those operations under exclusive installation control.

use std::fmt;

use primitives_rs::id::time_sortable::SortableId;

use self::scope::{
	ScopeUpgradeCandidatePolicies, ScopedParticipantAssessmentFields,
	ScopedUpgradeCompatibilityError, ScopedUpgradeParticipantDisposition,
};
use crate::installation::contract::{
	BoundComponentContract, ComponentDeclaration, ComponentPolicy, StateContractError,
};
use crate::installation::control_state::{
	InstallationControlComponent, InstallationControlError, InstallationControlState,
};
use crate::records::StateTimestampMicros;
use crate::runtime::participation::state::{
	ParticipantCapabilities, ParticipantSchema, ParticipantState, ParticipantStateError,
	ParticipationSemantics,
};
use crate::scope::ScopePolicySet;

pub(in crate::upgrade) mod assessment;
pub(in crate::upgrade) mod control_replacement;
pub(in crate::upgrade) mod document_reader;
pub(in crate::upgrade) mod document_readiness;
pub(in crate::upgrade) mod document_stage;
pub(crate) mod installed;
pub(in crate::upgrade) mod payload_reader;
pub(in crate::upgrade) mod payload_stage;
pub(crate) mod request;
pub(crate) mod scope;
pub(crate) mod v10;

/// A complete policy checked against an actual code declaration, including its transition.
/// Construction binds actual settings codecs and invokes no application payload or transition
/// function.
pub(crate) struct CandidateComponentPolicy {
	policy: ComponentPolicy,
}

impl CandidateComponentPolicy {
	pub(crate) fn new<Transitions>(
		declaration: &ComponentDeclaration<'_, Transitions>,
		policy: ComponentPolicy,
	) -> Result<Self, UpgradeCompatibilityError> {
		declaration.bind(&policy)?;
		Ok(Self { policy })
	}
	/// Retains an already-bound policy without invoking its settings decoder again.
	pub(crate) fn from_bound<Transitions>(
		bound: &BoundComponentContract<'_, '_, Transitions>,
	) -> Self {
		Self {
			policy: bound.policy().clone(),
		}
	}
}

impl fmt::Debug for CandidateComponentPolicy {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("CandidateComponentPolicy")
			.finish_non_exhaustive()
	}
}

/// One unique set of candidate core policies, including actual participant/control schemas.
/// Application producer, handler, and query roles require their separate scoped protocol.
pub(crate) struct UpgradeCandidatePolicies {
	policies: Vec<ComponentPolicy>,
}

pub(crate) struct UpgradeCandidatePoliciesFields<'a, 'd> {
	pub(crate) administration: &'a ComponentDeclaration<'d, ParticipationSemantics>,
	pub(crate) administrative_policy: ComponentPolicy,
	pub(crate) other_policies: Vec<CandidateComponentPolicy>,
}

impl UpgradeCandidatePolicies {
	pub(crate) fn new(
		fields: UpgradeCandidatePoliciesFields<'_, '_>,
	) -> Result<Self, UpgradeCompatibilityError> {
		let UpgradeCandidatePoliciesFields {
			administration,
			administrative_policy,
			other_policies,
		} = fields;
		administration.bind(&administrative_policy)?;
		let mut policies = Vec::with_capacity(other_policies.len() + 1);
		policies.push(administrative_policy);
		for (candidate_index, candidate) in other_policies.into_iter().enumerate() {
			if policies.iter().any(|prior| {
				prior.component_identifier() == candidate.policy.component_identifier()
			}) {
				return Err(UpgradeCompatibilityError::DuplicateComponent { candidate_index });
			}
			policies.push(candidate.policy);
		}
		Ok(Self { policies })
	}

	pub(crate) fn policies(&self) -> &[ComponentPolicy] {
		&self.policies
	}

	pub(in crate::upgrade) fn administrative_requirements(
		&self,
		administration: &ComponentDeclaration<'_, ParticipationSemantics>,
		participant: &ParticipantSchema,
		control: &InstallationControlComponent,
	) -> Result<AdministrativePolicyRequirements, UpgradeCompatibilityError> {
		let policy = self
			.policies
			.iter()
			.find(|policy| policy.component_identifier() == administration.identifier())
			.ok_or(UpgradeCompatibilityError::AdministrativePolicyAbsent)?;
		let bound = administration.bind(policy)?;
		AdministrativePolicyRequirements::from_bound(&bound, participant, control)
	}

	/// Observes one exact installation's registration at the caller's authoritative clock.
	/// Expiry requires durable exclusion; it does not imply that the process has ended.
	pub(in crate::upgrade) fn assess_participant(
		&self,
		participant: &ParticipantState,
		installation_id: SortableId,
		observed_at: StateTimestampMicros,
	) -> Result<UpgradeParticipantDisposition, UpgradeCompatibilityError> {
		if participant.installation_id() != installation_id {
			return Err(UpgradeCompatibilityError::ParticipantInstallationMismatch);
		}
		if !participant.is_live_at(observed_at) {
			return Ok(UpgradeParticipantDisposition::RequiresDurableFencing);
		}
		Ok(
			match participant
				.capabilities()
				.check_installed_policies(&self.policies)
			{
				Ok(()) => UpgradeParticipantDisposition::Compatible,
				Err(_) => UpgradeParticipantDisposition::Unsupported,
			},
		)
	}
}

impl fmt::Debug for UpgradeCandidatePolicies {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("UpgradeCandidatePolicies")
			.field("component_count", &self.policies.len())
			.finish()
	}
}

/// Codec and working-state requirements of the actual selected administrative policy.
pub(in crate::upgrade) struct AdministrativePolicyRequirements {
	require_capabilities_fn: fn(&ParticipantCapabilities) -> Result<(), ParticipantStateError>,
}
impl AdministrativePolicyRequirements {
	fn from_bound(
		bound: &BoundComponentContract<'_, '_, ParticipationSemantics>,
		participant: &ParticipantSchema,
		control: &InstallationControlComponent,
	) -> Result<Self, UpgradeCompatibilityError> {
		let participant_schema = bound.bind_schema(participant.schema())?;
		participant.require_bound_writer(&participant_schema)?;
		let control_schema = bound.bind_schema(control.schema())?;
		control.require_bound_writer(&control_schema)?;
		Ok(Self {
			require_capabilities_fn: bound.transitions().require_capabilities_fn,
		})
	}
	fn require_participant_state(
		&self,
		capabilities: &ParticipantCapabilities,
	) -> Result<(), ParticipantStateError> {
		(self.require_capabilities_fn)(capabilities)
	}
}

/// Explicit current metadata for one complete candidate comparison.
pub(in crate::upgrade) struct InstallationUpgradeParticipantAssessmentFields<'a, 'd> {
	pub(crate) participant: &'a ParticipantState,
	pub(crate) control: &'a InstallationControlState,
	pub(crate) administration: &'a ComponentDeclaration<'d, ParticipationSemantics>,
	pub(crate) participant_schema: &'a ParticipantSchema,
	pub(crate) control_component: &'a InstallationControlComponent,
	pub(crate) observed_at: StateTimestampMicros,
}

/// One target uses one pending slot for core policies and explicit scope replacements.
pub(crate) struct InstallationUpgradeCandidate {
	core: UpgradeCandidatePolicies,
	scopes: Option<ScopeUpgradeCandidatePolicies>,
}

impl InstallationUpgradeCandidate {
	pub(crate) fn core(core: UpgradeCandidatePolicies) -> Self {
		Self { core, scopes: None }
	}
	pub(in crate::upgrade) fn core_policies(&self) -> &UpgradeCandidatePolicies {
		&self.core
	}
	pub(crate) fn core_and_scopes(
		core: UpgradeCandidatePolicies,
		scopes: ScopeUpgradeCandidatePolicies,
	) -> Self {
		Self {
			core,
			scopes: Some(scopes),
		}
	}
	pub(crate) fn scope_replacements(&self) -> Option<&ScopePolicySet> {
		self.scopes
			.as_ref()
			.map(ScopeUpgradeCandidatePolicies::targets)
	}
	pub(in crate::upgrade) fn require_control_target(
		&self,
		installed: &InstallationControlState,
		administration: &ComponentDeclaration<'_, ParticipationSemantics>,
		participant: &ParticipantSchema,
		control: &InstallationControlComponent,
	) -> Result<(), UpgradeCompatibilityError> {
		self.core
			.administrative_requirements(administration, participant, control)?;
		self.require_scope_ownership(installed.scope_policies())
	}

	fn require_scope_ownership(
		&self,
		installed: &ScopePolicySet,
	) -> Result<(), UpgradeCompatibilityError> {
		if let Some(scopes) = &self.scopes {
			scopes.require_compatible_ownership(installed)?;
		}
		Ok(())
	}
	pub(in crate::upgrade) fn assess_participant(
		&self,
		fields: InstallationUpgradeParticipantAssessmentFields<'_, '_>,
	) -> Result<InstallationUpgradeParticipantDisposition, UpgradeCompatibilityError> {
		let InstallationUpgradeParticipantAssessmentFields {
			participant,
			control,
			administration,
			participant_schema,
			control_component,
			observed_at,
		} = fields;
		match self
			.core
			.assess_participant(participant, control.installation_id(), observed_at)?
		{
			UpgradeParticipantDisposition::RequiresDurableFencing => {
				return Ok(InstallationUpgradeParticipantDisposition::RequiresDurableFencing);
			}
			UpgradeParticipantDisposition::Unsupported => {
				return Ok(InstallationUpgradeParticipantDisposition::UnsupportedCore);
			}
			UpgradeParticipantDisposition::Compatible => {}
		}
		let requirements = self.core.administrative_requirements(
			administration,
			participant_schema,
			control_component,
		)?;
		self.require_scope_ownership(control.scope_policies())?;
		if requirements
			.require_participant_state(participant.capabilities())
			.is_err()
		{
			return Ok(InstallationUpgradeParticipantDisposition::UnsupportedCore);
		}
		if let Some(scopes) = &self.scopes {
			return Ok(
				match scopes.assess_participant(ScopedParticipantAssessmentFields {
					participant,
					installation_id: control.installation_id(),
					installed: control.scope_policies(),
					observed_at,
				})? {
					ScopedUpgradeParticipantDisposition::Compatible => {
						InstallationUpgradeParticipantDisposition::Compatible
					}
					ScopedUpgradeParticipantDisposition::RequiresDurableFencing => {
						InstallationUpgradeParticipantDisposition::RequiresDurableFencing
					}
					ScopedUpgradeParticipantDisposition::Unsupported => {
						InstallationUpgradeParticipantDisposition::UnsupportedScope
					}
				},
			);
		}
		Ok(
			match participant
				.capabilities()
				.check_scope_policies(control.scope_policies())
			{
				Ok(()) => InstallationUpgradeParticipantDisposition::Compatible,
				Err(_) => InstallationUpgradeParticipantDisposition::UnsupportedCore,
			},
		)
	}
}

impl fmt::Debug for InstallationUpgradeCandidate {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("InstallationUpgradeCandidate")
			.field("core", &self.core)
			.field("scopes", &self.scopes)
			.finish()
	}
}

#[derive(Debug)]
pub(in crate::upgrade) enum InstallationUpgradeParticipantDisposition {
	Compatible,
	UnsupportedCore,
	UnsupportedScope,
	RequiresDurableFencing,
}

/// A single recorded participant's compatibility, without authority or completion proof.
#[derive(Debug)]
pub(in crate::upgrade) enum UpgradeParticipantDisposition {
	Compatible,
	Unsupported,
	RequiresDurableFencing,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum UpgradeCompatibilityError {
	#[error("effective control replacement does not preserve installed ownership")]
	ControlReplacement(#[source] Box<control_replacement::EffectiveControlReplacementError>),
	#[error("upgrade candidate mandatory administrative policy is absent")]
	AdministrativePolicyAbsent,
	#[error("upgrade target administrative control writer is unavailable")]
	Control(#[from] InstallationControlError),
	#[error("upgrade target participant writer is unavailable")]
	Participant(#[from] ParticipantStateError),
	#[error("upgrade document target or scoped participant is invalid")]
	Scope(#[from] ScopedUpgradeCompatibilityError),
	#[error("upgrade candidate policy does not match its code declaration")]
	Contract(#[from] StateContractError),
	#[error("upgrade candidate component {candidate_index} is declared more than once")]
	DuplicateComponent { candidate_index: usize },
	#[error("upgrade participant belongs to another installation")]
	ParticipantInstallationMismatch,
}

impl From<control_replacement::EffectiveControlReplacementError> for UpgradeCompatibilityError {
	fn from(error: control_replacement::EffectiveControlReplacementError) -> Self {
		Self::ControlReplacement(Box::new(error))
	}
}
