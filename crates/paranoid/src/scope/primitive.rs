//! Sealed native family support with one code-derived catalog entry per logical scope.

use super::wire::{ScopeIdentityWire, ScopeWireError};
use super::{ScopeError, ScopeIdentity, ScopeOwner, ScopePolicy};
use crate::fleet::cache::entry_state::CacheEntryComponent;
use crate::fleet::primitive_scope::PrimitiveScopeDeclaration;
use crate::fleet::semaphore::state::SemaphoreComponent;
use crate::fleet::throttler::state::ThrottlerComponent;
use crate::fleet::topic::event_state::TopicEventComponent;
use crate::runtime::participation::state::{
	ComponentSupportWire, ParticipantComponentSupport, ParticipantStateError,
};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PrimitiveRole {
	Reader,
	Writer,
}

/// Advertised roles share one actual component schema/semantic support inventory.
pub(crate) struct PrimitiveScopeSupport {
	scope: ScopeIdentity,
	component: ParticipantComponentSupport,
	roles: Vec<PrimitiveRole>,
}
impl PrimitiveScopeSupport {
	pub(crate) fn from_declaration(
		declaration: &PrimitiveScopeDeclaration<'_, '_>,
		roles: Vec<PrimitiveRole>,
	) -> Result<Self, ScopeError> {
		if roles.contains(&PrimitiveRole::Writer) && !declaration.supports_writer() {
			return Err(ScopeError::PrimitiveWriterUnavailable);
		}
		Self::parse_parts(
			declaration.identity().clone(),
			declaration.component_support(),
			roles,
		)
	}
	fn parse_parts(
		scope: ScopeIdentity,
		component: ParticipantComponentSupport,
		roles: Vec<PrimitiveRole>,
	) -> Result<Self, ScopeError> {
		if component.identifier() != SemaphoreComponent::component_identifier()
			&& component.identifier() != ThrottlerComponent::component_identifier()
			&& component.identifier() != CacheEntryComponent::component_identifier()
			&& component.identifier() != TopicEventComponent::component_identifier()
		{
			return Err(ScopeError::InvalidRoleSupport);
		}
		if roles.is_empty() {
			return Err(ScopeError::InvalidRoleSupport);
		}
		for (index, role) in roles.iter().enumerate() {
			if roles[..index].contains(role) {
				return Err(ScopeError::DuplicateRoleSupport { index });
			}
		}
		Ok(Self {
			scope,
			component,
			roles,
		})
	}
	pub(crate) fn identity(&self) -> &ScopeIdentity {
		&self.scope
	}
	pub(crate) fn check_policy(
		&self,
		role: PrimitiveRole,
		policy: &ScopePolicy,
	) -> Result<(), ScopeError> {
		if !self.roles.contains(&role) {
			return Err(ScopeError::RoleNotRegistered);
		}
		if self.scope != *policy.identity() {
			return Err(ScopeError::ScopeMismatch);
		}
		let ScopeOwner::Primitive(component) = policy.owner() else {
			return Err(ScopeError::WrongOwner);
		};
		if self.component.identifier() == CacheEntryComponent::component_identifier()
			|| self.component.identifier() == TopicEventComponent::component_identifier()
		{
			if matches!(
				component.payload(),
				crate::scope::PrimitivePayloadPolicy::None
			) {
				return Err(ScopeError::PrimitivePayloadRequired);
			}
		} else {
			component.payload().require_none()?;
		}
		match role {
			PrimitiveRole::Reader => self.component.check_reader_policy(component.intrinsic()),
			PrimitiveRole::Writer => self.component.check_policy(component.intrinsic()),
		}
		.map_err(|error| ScopeError::PrimitiveContractUnsupported {
			source: Box::new(error),
		})
	}
}
impl fmt::Debug for PrimitiveScopeSupport {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("PrimitiveScopeSupport")
			.field("roles", &self.roles)
			.finish_non_exhaustive()
	}
}

pub(crate) struct PrimitiveScopeSupportSet {
	entries: Vec<PrimitiveScopeSupport>,
}
impl PrimitiveScopeSupportSet {
	pub(crate) fn into_entries(self) -> Vec<PrimitiveScopeSupport> {
		self.entries
	}

	pub(crate) fn new(entries: Vec<PrimitiveScopeSupport>) -> Result<Self, ScopeError> {
		for (index, entry) in entries.iter().enumerate() {
			if entries[..index]
				.iter()
				.any(|prior| prior.identity() == entry.identity())
			{
				return Err(ScopeError::DuplicateScope { index });
			}
		}
		Ok(Self { entries })
	}
	pub(crate) fn entries(&self) -> &[PrimitiveScopeSupport] {
		&self.entries
	}
	pub(crate) fn lookup(
		&self,
		scope: &ScopeIdentity,
		role: PrimitiveRole,
	) -> Result<&PrimitiveScopeSupport, ScopeError> {
		self.entries
			.iter()
			.find(|entry| entry.identity() == scope && entry.roles.contains(&role))
			.ok_or(ScopeError::RoleNotRegistered)
	}
}
impl fmt::Debug for PrimitiveScopeSupportSet {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("PrimitiveScopeSupportSet")
			.field("scope_count", &self.entries.len())
			.finish()
	}
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PrimitiveScopeSupportWire<'a> {
	version: u32,
	scope: ScopeIdentityWire<'a>,
	component: ComponentSupportWire,
	roles: Vec<PrimitiveRole>,
}
impl<'a> PrimitiveScopeSupportWire<'a> {
	pub(crate) fn from_support(support: &'a PrimitiveScopeSupport) -> Self {
		Self {
			version: 1,
			scope: ScopeIdentityWire::from_identity(&support.scope),
			component: ComponentSupportWire::from_support(&support.component),
			roles: support.roles.clone(),
		}
	}
	pub(crate) fn into_support(self) -> Result<PrimitiveScopeSupport, ParticipantStateError> {
		if self.version != 1 {
			return Err(ScopeWireError::UnsupportedVersion {
				version: self.version,
			}
			.into());
		}
		Ok(PrimitiveScopeSupport::parse_parts(
			self.scope.into_identity()?,
			self.component.into_support()?,
			self.roles,
		)?)
	}
}
