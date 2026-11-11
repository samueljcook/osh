//! Durable combined policy request data, without promotion or maintenance authority.
//!
//! A controller must bind the recorded target against its actual code declarations again.
//! Stored policy shape and request identity do not prove compatibility or data readiness.

use std::{fmt, num::NonZeroU64};

use primitives_rs::encoding::decimal::parse_canonical_decimal;
use primitives_rs::id::time_sortable::{SORTABLE_ID_LEN, SortableId};
use serde::{Deserialize, Serialize};

use crate::installation::contract::policy_wire::ComponentPolicyWire;
use crate::installation::contract::{ComponentPolicy, StateContractError};
use crate::scope::wire::{ScopePolicySetWire, ScopeWireError};
use crate::scope::{ScopeOwner, ScopePolicy, ScopePolicySet};

use super::InstallationUpgradeCandidate;
use super::scope::DocumentScopeTargetView;

#[derive(Clone)]
pub(crate) struct PolicyUpgradeRequestIdentity {
	pub(crate) request_id: SortableId,
	pub(crate) installation_id: SortableId,
	pub(crate) base_generation: NonZeroU64,
}

/// Structurally parsed request. No capability claim survives its serialization.
#[derive(Clone)]
pub(crate) struct PolicyUpgradeRequest {
	identity: PolicyUpgradeRequestIdentity,
	policies: Vec<ComponentPolicy>,
	scopes: ScopeReplacementRequest,
}

impl PolicyUpgradeRequest {
	pub(crate) fn from_candidate(
		identity: PolicyUpgradeRequestIdentity,
		candidate: &InstallationUpgradeCandidate,
	) -> Self {
		Self {
			identity,
			policies: candidate.core_policies().policies().to_vec(),
			scopes: match candidate.scope_replacements() {
				None => ScopeReplacementRequest::Unchanged,
				Some(scopes) => ScopeReplacementRequest::Replace(scopes.clone()),
			},
		}
	}

	pub(crate) fn identity(&self) -> &PolicyUpgradeRequestIdentity {
		&self.identity
	}
	pub(crate) fn policies(&self) -> &[ComponentPolicy] {
		&self.policies
	}

	pub(crate) fn scopes(&self) -> &ScopeReplacementRequest {
		&self.scopes
	}
	pub(crate) fn scope_replacements(&self) -> Option<&ScopePolicySet> {
		self.scopes.replacements()
	}
	pub(in crate::upgrade) fn document_scopes(&self) -> Option<DocumentScopeTargetView<'_>> {
		self.scope_replacements()
			.and_then(DocumentScopeTargetView::from_scopes)
	}

	/// Exact target equality includes reader requirements, index shapes, and semantic selections.
	pub(crate) fn matches_candidate(&self, candidate: &InstallationUpgradeCandidate) -> bool {
		self.policies.len() == candidate.core_policies().policies().len()
			&& self.policies.iter().all(|stored| {
				candidate
					.core_policies()
					.policies()
					.iter()
					.any(|bound| stored.matches_policy(bound))
			}) && match (self.scope_replacements(), candidate.scope_replacements()) {
			(None, None) => true,
			(Some(stored), Some(bound)) => stored.matches_policies(bound),
			_ => false,
		}
	}

	/// Policy lists are sets by parsed identity; declaration iteration order does not constrain
	/// resume.
	pub(in crate::upgrade) fn matches_request(&self, other: &Self) -> bool {
		self.identity.request_id == other.identity.request_id
			&& self.identity.installation_id == other.identity.installation_id
			&& self.identity.base_generation == other.identity.base_generation
			&& self.policies.len() == other.policies.len()
			&& self.policies.iter().all(|policy| {
				other
					.policies
					.iter()
					.any(|other| policy.matches_policy(other))
			}) && match (self.scope_replacements(), other.scope_replacements()) {
			(None, None) => true,
			(Some(scopes), Some(other)) => scopes.matches_policies(other),
			_ => false,
		}
	}

	pub(in crate::upgrade) fn from_wire_parts(
		request_id: [u8; SORTABLE_ID_LEN],
		installation_id: [u8; SORTABLE_ID_LEN],
		base_generation: String,
		wire_policies: Vec<ComponentPolicyWire<'static>>,
		scopes: ScopeReplacementRequest,
	) -> Result<Self, PolicyUpgradeRequestError> {
		if scopes
			.replacements()
			.is_some_and(|scopes| scopes.policies().is_empty())
		{
			return Err(PolicyUpgradeRequestError::EmptyScopeTarget);
		}
		let generation = parse_canonical_decimal::<u64>(&base_generation)
			.map_err(|_| PolicyUpgradeRequestError::InvalidGeneration)?;
		let mut policies: Vec<ComponentPolicy> = Vec::with_capacity(wire_policies.len());
		for (policy_index, policy) in wire_policies.into_iter().enumerate() {
			let policy = policy.into_policy()?;
			if policies
				.iter()
				.any(|prior| prior.component_identifier() == policy.component_identifier())
			{
				return Err(PolicyUpgradeRequestError::DuplicateComponent { policy_index });
			}
			policies.push(policy);
		}
		if policies.is_empty() {
			return Err(PolicyUpgradeRequestError::EmptyTarget);
		}
		Ok(Self {
			identity: PolicyUpgradeRequestIdentity {
				request_id: SortableId::from_bytes(&request_id)
					.map_err(|_| PolicyUpgradeRequestError::InvalidRequestId)?,
				installation_id: SortableId::from_bytes(&installation_id)
					.map_err(|_| PolicyUpgradeRequestError::InvalidInstallationId)?,
				base_generation: NonZeroU64::new(generation)
					.ok_or(PolicyUpgradeRequestError::InvalidGeneration)?,
			},
			policies,
			scopes,
		})
	}
}

#[derive(Clone)]
pub(crate) enum ScopeReplacementRequest {
	Unchanged,
	Replace(ScopePolicySet),
}
impl ScopeReplacementRequest {
	pub(crate) fn replacements(&self) -> Option<&ScopePolicySet> {
		match self {
			Self::Unchanged => None,
			Self::Replace(scopes) => Some(scopes),
		}
	}
}

pub(in crate::upgrade) fn matches_document_scope(
	policy: &ScopePolicy,
	candidate: &ScopePolicy,
) -> bool {
	matches!(policy.owner(), ScopeOwner::Document(_)) && policy.matches_policy(candidate)
}

impl fmt::Debug for PolicyUpgradeRequest {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("PolicyUpgradeRequest")
			.field("base_generation", &self.identity.base_generation)
			.field("component_count", &self.policies.len())
			.finish_non_exhaustive()
	}
}

// External tagging is required: replaced scopes contain raw JSON, which cannot be decoded from
// the buffer that serde uses for internal tagging.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(in crate::upgrade) enum ScopesWire<'a> {
	Unchanged {},
	Replace { scopes: ScopePolicySetWire<'a> },
}
impl<'a> ScopesWire<'a> {
	pub(crate) fn from_request(request: &'a ScopeReplacementRequest) -> Self {
		match request {
			ScopeReplacementRequest::Unchanged => Self::Unchanged {},
			ScopeReplacementRequest::Replace(scopes) => Self::Replace {
				scopes: ScopePolicySetWire::from_policies(scopes),
			},
		}
	}
	pub(in crate::upgrade) fn into_request(
		self,
	) -> Result<ScopeReplacementRequest, PolicyUpgradeRequestError> {
		match self {
			Self::Unchanged {} => Ok(ScopeReplacementRequest::Unchanged),
			Self::Replace { scopes } => {
				let scopes = scopes.into_policies()?;
				if scopes.policies().is_empty() {
					return Err(PolicyUpgradeRequestError::EmptyScopeTarget);
				}
				Ok(ScopeReplacementRequest::Replace(scopes))
			}
		}
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PolicyUpgradeRequestError {
	#[error("upgrade request contains no explicitly replaced scopes")]
	EmptyScopeTarget,
	#[error("upgrade request scope shape is invalid")]
	Scope(#[from] ScopeWireError),
	#[error("upgrade request identity is invalid")]
	InvalidRequestId,
	#[error("upgrade request installation identity is invalid")]
	InvalidInstallationId,
	#[error("upgrade request base generation must be a canonical positive decimal u64")]
	InvalidGeneration,
	#[error("upgrade request contains no target policies")]
	EmptyTarget,
	#[error("upgrade request policy component occurs more than once at index {policy_index}")]
	DuplicateComponent { policy_index: usize },
	#[error("upgrade request policy shape is invalid")]
	Policy(#[from] StateContractError),
}
