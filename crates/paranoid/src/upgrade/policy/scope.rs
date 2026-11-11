//! Actual owner-specific scope candidates and scoped participant compatibility.
//!
//! These values provide no source assignment, maintenance barrier, or promotion proof.
//! A controller must perform those procedures under the exact installed generation.

use std::fmt;

use primitives_rs::id::time_sortable::SortableId;

use crate::fleet::primitive_scope::{PrimitiveScopeBindingError, PrimitiveScopeDeclaration};
use crate::scope::{
	ComponentScopeDeclaration, ComponentScopeKind, DocumentScopeDeclaration, ScopeError,
	ScopeIdentity, ScopeOwner, ScopePolicy, ScopePolicySet,
};
use crate::{records::StateTimestampMicros, runtime::participation::state::ParticipantState};

/// A target checked against the actual producer and complete retained projection functions.
pub(crate) struct CandidateDocumentScopePolicy {
	policy: ScopePolicy,
}

impl CandidateDocumentScopePolicy {
	pub(crate) fn new<T, C, P>(
		declaration: &DocumentScopeDeclaration<'_, '_, T, C, P>,
		policy: ScopePolicy,
	) -> Result<Self, ScopedUpgradeCompatibilityError> {
		declaration.bind_producer(&policy)?;
		declaration.bind_maintenance(&policy)?;
		Ok(Self { policy })
	}
	pub(crate) fn policy(&self) -> &ScopePolicy {
		&self.policy
	}
}

impl fmt::Debug for CandidateDocumentScopePolicy {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("CandidateDocumentScopePolicy")
			.finish_non_exhaustive()
	}
}

/// Actual full component binding establishes candidate meaning, not scoped role or execution
/// support.
pub(crate) struct CandidateRawScopePolicy {
	policy: ScopePolicy,
}
pub(crate) struct CandidatePrimitiveScopePolicy {
	policy: ScopePolicy,
}
pub(crate) struct CandidateClosedScopePolicy {
	policy: ScopePolicy,
}

pub(crate) enum CandidateScopePolicy {
	Document(CandidateDocumentScopePolicy),
	Raw(CandidateRawScopePolicy),
	Primitive(CandidatePrimitiveScopePolicy),
	Closed(CandidateClosedScopePolicy),
}
impl CandidateScopePolicy {
	pub(crate) fn policy(&self) -> &ScopePolicy {
		match self {
			Self::Document(candidate) => &candidate.policy,
			Self::Raw(candidate) => &candidate.policy,
			Self::Primitive(candidate) => &candidate.policy,
			Self::Closed(candidate) => &candidate.policy,
		}
	}
	pub(crate) fn from_document(candidate: CandidateDocumentScopePolicy) -> Self {
		Self::Document(candidate)
	}
	pub(crate) fn from_component<T>(
		declaration: &ComponentScopeDeclaration<'_, '_, T>,
		policy: ScopePolicy,
	) -> Result<Self, ScopedUpgradeCompatibilityError> {
		if declaration.kind() != ComponentScopeKind::Raw {
			return Err(ScopeError::WrongOwner.into());
		}
		declaration.bind(declaration.identity(), &policy)?;
		Ok(Self::Raw(CandidateRawScopePolicy { policy }))
	}
	/// Binds complete actual intrinsic code; structured payloads require their separate code owner.
	pub(crate) fn from_primitive(
		declaration: &PrimitiveScopeDeclaration<'_, '_>,
		policy: ScopePolicy,
	) -> Result<Self, ScopedUpgradeCompatibilityError> {
		if matches!(policy.owner(), ScopeOwner::Primitive(primitive) if matches!(primitive.payload(), crate::scope::PrimitivePayloadPolicy::Schema(_)))
		{
			return Err(ScopedUpgradeCompatibilityError::ActualPayloadCodeRequired {
				scope: policy.identity().clone(),
			});
		}
		declaration.require_writer(&policy)?;
		Ok(Self::Primitive(CandidatePrimitiveScopePolicy { policy }))
	}
	pub(crate) fn from_payload_primitive<T, C>(
		declaration: &PrimitiveScopeDeclaration<'_, '_>,
		payload: &crate::scope::payload::PayloadSchemaDeclaration<'_, '_, T, C>,
		policy: ScopePolicy,
	) -> Result<Self, ScopedUpgradeCompatibilityError> {
		declaration.require_writer(&policy)?;
		payload.bind_reader(declaration.identity(), &policy)?;
		payload.bind_producer(declaration.identity(), &policy)?;
		Ok(Self::Primitive(CandidatePrimitiveScopePolicy { policy }))
	}

	/// Intrinsic no-access meaning grants no permission to discard or adopt existing data.
	pub(crate) fn closed(policy: ScopePolicy) -> Result<Self, ScopedUpgradeCompatibilityError> {
		if !matches!(policy.owner(), ScopeOwner::Closed) {
			return Err(ScopeError::WrongOwner.into());
		}
		Ok(Self::Closed(CandidateClosedScopePolicy { policy }))
	}
	pub(crate) fn into_policy(self) -> ScopePolicy {
		match self {
			Self::Document(candidate) => candidate.policy,
			Self::Raw(candidate) => candidate.policy,
			Self::Primitive(candidate) => candidate.policy,
			Self::Closed(candidate) => candidate.policy,
		}
	}
}

/// Explicit comparison context for one participant and the candidate's actual scope overlay.
pub(in crate::upgrade) struct ScopedParticipantAssessmentFields<'a> {
	pub(crate) participant: &'a ParticipantState,
	pub(crate) installation_id: SortableId,
	pub(crate) installed: &'a ScopePolicySet,
	pub(crate) observed_at: StateTimestampMicros,
}

/// One actual candidate set. Omitted installed policies remain unchanged.
pub(crate) struct ScopeUpgradeCandidatePolicies {
	targets: ScopePolicySet,
}
impl ScopeUpgradeCandidatePolicies {
	pub(crate) fn new(
		candidates: Vec<CandidateScopePolicy>,
	) -> Result<Self, ScopedUpgradeCompatibilityError> {
		if candidates.is_empty() {
			return Err(ScopedUpgradeCompatibilityError::EmptyTarget);
		}
		Ok(Self {
			targets: ScopePolicySet::new(
				candidates
					.into_iter()
					.map(CandidateScopePolicy::into_policy)
					.collect(),
			)?,
		})
	}
	pub(crate) fn targets(&self) -> &ScopePolicySet {
		&self.targets
	}
	/// Ownership and state-schema reassignment require an actual adoption procedure.
	pub(in crate::upgrade) fn require_compatible_ownership(
		&self,
		installed: &ScopePolicySet,
	) -> Result<(), ScopedUpgradeCompatibilityError> {
		for target in self.targets.policies() {
			if let Some(current) = installed
				.policies()
				.iter()
				.find(|policy| policy.identity() == target.identity())
			{
				let components = match (current.owner(), target.owner()) {
					(ScopeOwner::Document(current), ScopeOwner::Document(target)) => {
						if current.schema().identifier() != target.schema().identifier() {
							return Err(
								ScopedUpgradeCompatibilityError::SchemaReassignmentRequired,
							);
						}
						None
					}
					(ScopeOwner::Raw(current), ScopeOwner::Raw(target)) => Some((current, target)),
					(ScopeOwner::Primitive(current), ScopeOwner::Primitive(proposed)) => {
						if !current
							.payload()
							.permits_compatible_ownership(proposed.payload())
						{
							return Err(ScopedUpgradeCompatibilityError::PayloadAdoptionRequired {
								scope: target.identity().clone(),
							});
						}
						Some((current.intrinsic(), proposed.intrinsic()))
					}
					(ScopeOwner::Closed, ScopeOwner::Closed) => None,
					_ => return Err(ScopedUpgradeCompatibilityError::OwnershipConversionRequired),
				};
				if let Some((current, target)) = components {
					if current.component_identifier() != target.component_identifier() {
						return Err(ScopedUpgradeCompatibilityError::ComponentReassignmentRequired);
					}
					if current.schemas().len() != target.schemas().len()
						|| current.schemas().iter().any(|current| {
							!target
								.schemas()
								.iter()
								.any(|target| current.identifier() == target.identifier())
						}) {
						return Err(ScopedUpgradeCompatibilityError::SchemaReassignmentRequired);
					}
				}
			}
		}
		Ok(())
	}
	pub(in crate::upgrade) fn assess_participant(
		&self,
		fields: ScopedParticipantAssessmentFields<'_>,
	) -> Result<ScopedUpgradeParticipantDisposition, ScopedUpgradeCompatibilityError> {
		let ScopedParticipantAssessmentFields {
			participant,
			installation_id,
			installed,
			observed_at,
		} = fields;
		if participant.installation_id() != installation_id {
			return Err(ScopedUpgradeCompatibilityError::ParticipantInstallationMismatch);
		}
		if !participant.is_live_at(observed_at) {
			return Ok(ScopedUpgradeParticipantDisposition::RequiresDurableFencing);
		}
		let overlaid = installed.with_replacements(&self.targets)?;
		if participant
			.capabilities()
			.check_scope_policies(&overlaid)
			.is_err()
		{
			return Ok(ScopedUpgradeParticipantDisposition::Unsupported);
		}
		Ok(ScopedUpgradeParticipantDisposition::Compatible)
	}
}
impl fmt::Debug for ScopeUpgradeCandidatePolicies {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("ScopeUpgradeCandidatePolicies")
			.field("scope_count", &self.targets.policies().len())
			.finish()
	}
}

/// Borrowed document view of one complete scope replacement set; no policy inventory is copied.
#[derive(Clone, Copy)]
pub(in crate::upgrade) struct DocumentScopeTargetView<'a> {
	targets: &'a ScopePolicySet,
}
impl<'a> DocumentScopeTargetView<'a> {
	pub(in crate::upgrade) fn from_scopes(targets: &'a ScopePolicySet) -> Option<Self> {
		targets
			.policies()
			.iter()
			.any(|policy| matches!(policy.owner(), ScopeOwner::Document(_)))
			.then_some(Self { targets })
	}
	pub(crate) fn policies(&self) -> impl Iterator<Item = &'a ScopePolicy> + use<'a> {
		self.targets
			.policies()
			.iter()
			.filter(|policy| matches!(policy.owner(), ScopeOwner::Document(_)))
	}
	pub(crate) fn lookup(&self, identity: &ScopeIdentity) -> Result<&'a ScopePolicy, ScopeError> {
		self.policies()
			.find(|policy| policy.identity() == identity)
			.ok_or(ScopeError::ScopeNotInstalled)
	}
}

#[derive(Debug)]
pub(in crate::upgrade) enum ScopedUpgradeParticipantDisposition {
	Compatible,
	Unsupported,
	RequiresDurableFencing,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ScopedUpgradeCompatibilityError {
	#[error("structured Primitive target requires its payload reader and producer")]
	ActualPayloadCodeRequired { scope: crate::scope::ScopeIdentity },
	#[error("Primitive payload mode or schema family requires an adoption procedure")]
	PayloadAdoptionRequired { scope: crate::scope::ScopeIdentity },
	#[error("scope upgrade target requires a component-family reassignment procedure")]
	ComponentReassignmentRequired,
	#[error("scope upgrade target requires an ownership conversion procedure")]
	OwnershipConversionRequired,
	#[error("scope upgrade target requires a schema reassignment procedure")]
	SchemaReassignmentRequired,
	#[error("scope upgrade target contains no scopes")]
	EmptyTarget,
	#[error("scope upgrade target or role does not match scope declarations")]
	Scope(#[from] ScopeError),
	#[error("scope upgrade target does not bind its primitive family writer")]
	Primitive(#[from] PrimitiveScopeBindingError),
	#[error("scope upgrade participant belongs to another installation")]
	ParticipantInstallationMismatch,
}
