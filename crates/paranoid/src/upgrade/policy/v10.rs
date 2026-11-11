//! The explicit native Counter writer required at the published source retirement boundary.
//!
//! This pure policy retains source readers and the installed semantic contract. It supplies
//! no source lock, participant inspection, transaction drainage, conversion or retirement proof.

use std::fmt;

use crate::fleet::counter::state::{CounterComponent, CounterRepresentationError};
use crate::installation::contract::{
	ComponentDeclaration, ComponentPolicy, ComponentPolicyFields, SchemaPolicy, StateContractError,
};
use crate::installation::control_state::{InstallationControlComponent, InstallationControlState};
use crate::runtime::participation::state::{ParticipantSchema, ParticipationSemantics};

use super::assessment::{ParticipantPolicyAssessment, ParticipantPolicyAssessmentFields};
use super::{InstallationUpgradeCandidate, UpgradeCompatibilityError};

pub(in crate::upgrade) struct SourceWriterRetirementStageFields<'a> {
	pub(crate) control: &'a InstallationControlState,
	pub(crate) final_candidate: &'a InstallationUpgradeCandidate,
	pub(crate) counter: &'a CounterComponent,
}

pub(in crate::upgrade) struct SourceWriterRetirementAssessmentFields<'a, 'd> {
	pub(crate) administration: &'a ComponentDeclaration<'d, ParticipationSemantics>,
	pub(crate) participant: &'a ParticipantSchema,
	pub(crate) control_component: &'a InstallationControlComponent,
}

/// A proposed intermediate policy anchored to one current snapshot, separate from final work.
/// Only Counter changes; all other core, scope and ready-index policies retain their meaning.
pub(in crate::upgrade) struct SourceWriterRetirementStage<'a> {
	control: &'a InstallationControlState,
	policies: Vec<ComponentPolicy>,
}

impl<'a> SourceWriterRetirementStage<'a> {
	pub(crate) fn new(
		fields: SourceWriterRetirementStageFields<'a>,
	) -> Result<Self, SourceWriterRetirementStageError> {
		let declaration = fields.counter.declaration();
		let installed = fields
			.control
			.component(declaration.identifier())
			.ok_or(SourceWriterRetirementStageError::InstalledCounterAbsent)?;
		let target = fields
			.final_candidate
			.core_policies()
			.policies()
			.iter()
			.find(|policy| policy.component_identifier() == declaration.identifier())
			.ok_or(SourceWriterRetirementStageError::TargetCounterAbsent)?;
		let barrier = CounterSourceWriterBarrier::new(CounterSourceWriterBarrierFields {
			component: fields.counter,
			installed,
			target,
		})?;
		let mut policies = fields.control.component_policies().to_vec();
		let counter = policies
			.iter_mut()
			.find(|policy| policy.component_identifier() == declaration.identifier())
			.expect("installed Counter policy was checked");
		*counter = barrier.into_policy();
		Ok(Self {
			control: fields.control,
			policies,
		})
	}

	pub(crate) fn policies(&self) -> &[ComponentPolicy] {
		&self.policies
	}

	/// Derives the selected administrative checks once for this exact stage inspection.
	pub(in crate::upgrade) fn assessment<'b>(
		&'b self,
		fields: SourceWriterRetirementAssessmentFields<'b, '_>,
	) -> Result<ParticipantPolicyAssessment<'b>, UpgradeCompatibilityError> {
		ParticipantPolicyAssessment::new(ParticipantPolicyAssessmentFields {
			administration: fields.administration,
			installation_id: self.control.installation_id(),
			components: &self.policies,
			scopes: self.control.scope_policies(),
			participant: fields.participant,
			control: fields.control_component,
		})
	}

	pub(crate) fn into_policies(self) -> Vec<ComponentPolicy> {
		self.policies
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SourceWriterRetirementStageError {
	#[error("source writer retirement current Counter policy is absent")]
	InstalledCounterAbsent,
	#[error("source writer retirement final Counter policy is absent")]
	TargetCounterAbsent,
	#[error("source writer retirement Counter stage does not bind code")]
	Counter(#[from] CounterSourceWriterBarrierError),
}

pub(in crate::upgrade) struct CounterSourceWriterBarrierFields<'a> {
	pub(crate) component: &'a CounterComponent,
	pub(crate) installed: &'a ComponentPolicy,
	pub(crate) target: &'a ComponentPolicy,
}

/// A code-checked Counter policy with the target's explicit writer and retained reader union.
pub(in crate::upgrade) struct CounterSourceWriterBarrier {
	policy: ComponentPolicy,
}

impl CounterSourceWriterBarrier {
	pub(crate) fn new(
		fields: CounterSourceWriterBarrierFields<'_>,
	) -> Result<Self, CounterSourceWriterBarrierError> {
		let declaration = fields.component.declaration();
		let installed = declaration.bind(fields.installed)?;
		let target = declaration.bind(fields.target)?;
		fields
			.component
			.bind_schema(&installed)?
			.require_source_writer()?;
		fields
			.component
			.bind_schema(&target)?
			.require_native_writer()?;
		let source = installed.bind_schema(fields.component.schema())?;
		let destination = target.bind_schema(fields.component.schema())?;
		let mut readers = source.policy().required_readers().to_vec();
		for reader in destination.policy().required_readers() {
			if let Some(prior) = readers.iter().find(|prior| prior.version == reader.version) {
				if prior != reader {
					return Err(CounterSourceWriterBarrierError::ReaderEnvelopeConflict {
						version: reader.version,
					});
				}
			} else {
				readers.push(*reader);
			}
		}
		let policy = ComponentPolicy::new(ComponentPolicyFields {
			settings: crate::installation::contract::ComponentSettings::Parameterless,
			component_identifier: fields.installed.component_identifier().to_owned(),
			semantic_contract: fields.installed.semantic_contract().to_owned(),
			schemas: vec![SchemaPolicy::new(
				fields.component.schema().identifier().to_owned(),
				destination.policy().writer(),
				readers,
			)?],
		})?;
		// The installed transition must support the derived writer and complete reader union.
		let bound = declaration.bind(&policy)?;
		fields
			.component
			.bind_schema(&bound)?
			.require_native_writer()?;
		Ok(Self { policy })
	}

	pub(crate) fn into_policy(self) -> ComponentPolicy {
		self.policy
	}
}

impl fmt::Debug for CounterSourceWriterBarrier {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("CounterSourceWriterBarrier")
			.field("policy", &self.policy)
			.finish()
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CounterSourceWriterBarrierError {
	#[error("source Counter writer barrier does not bind its schema or semantics")]
	Contract(#[from] StateContractError),
	#[error("source Counter writer barrier requires the published source and native writers")]
	Representation(#[from] CounterRepresentationError),
	#[error("source Counter reader version {version} has conflicting physical envelopes")]
	ReaderEnvelopeConflict { version: u32 },
}
