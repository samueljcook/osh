//! Scoped opaque-byte uses derived from the actual schema-free raw primitive.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::installation::contract::{ComponentDeclaration, ComponentSettings};
use crate::kv::raw::engine::store::RawKvComponent;

use super::wire::{ScopeIdentityWire, ScopeWireError};
use super::{ScopeError, ScopeIdentity, ScopeOwner, ScopePolicy, require_identifier};

/// One actual raw primitive with an explicit application-selected scope.
pub struct RawScopeDeclaration {
	identity: ScopeIdentity,
	component: RawKvComponent,
}
impl RawScopeDeclaration {
	/// Owns an explicit scope identity for opaque bytes without installing it.
	pub fn new(identity: ScopeIdentity) -> Self {
		Self {
			identity,
			component: RawKvComponent::new(),
		}
	}
	/// Borrows the actual logical scope identity.
	pub fn identity(&self) -> &ScopeIdentity {
		&self.identity
	}
	pub(crate) fn component(&self) -> &RawKvComponent {
		&self.component
	}
	pub(crate) fn declaration(&self) -> ComponentDeclaration<'_, RawKvComponent> {
		self.component.declaration()
	}
	pub(crate) fn support(&self, role: RawRole) -> Result<RawRoleSupport, ScopeError> {
		let declaration = self.declaration();
		RawRoleSupport::parse_parts(
			self.identity.clone(),
			role,
			declaration.identifier().to_owned(),
			declaration
				.semantic_contracts()
				.map(str::to_owned)
				.collect(),
		)
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RawRole {
	Reader,
	Writer,
}

/// Serializable scoped support. Parsed support grants no local or transaction authority.
pub(crate) struct RawRoleSupport {
	scope: ScopeIdentity,
	role: RawRole,
	component_identifier: String,
	semantics: Vec<String>,
}
impl RawRoleSupport {
	fn parse_parts(
		scope: ScopeIdentity,
		role: RawRole,
		component: String,
		semantics: Vec<String>,
	) -> Result<Self, ScopeError> {
		require_identifier(&component)?;
		if semantics.is_empty() {
			return Err(ScopeError::InvalidRoleSupport);
		}
		for (index, semantic) in semantics.iter().enumerate() {
			require_identifier(semantic)?;
			if semantics[..index].contains(semantic) {
				return Err(ScopeError::InvalidRoleSupport);
			}
		}
		Ok(Self {
			scope,
			role,
			component_identifier: component,
			semantics,
		})
	}
	pub(crate) fn identity(&self) -> &ScopeIdentity {
		&self.scope
	}
	pub(crate) fn role(&self) -> RawRole {
		self.role
	}
	pub(crate) fn check_policy(&self, policy: &ScopePolicy) -> Result<(), ScopeError> {
		if self.scope != *policy.identity() {
			return Err(ScopeError::ScopeMismatch);
		}
		let ScopeOwner::Raw(component) = policy.owner() else {
			return Err(ScopeError::WrongOwner);
		};
		if component.component_identifier() != self.component_identifier
			|| !component.schemas().is_empty()
			|| !matches!(component.settings(), ComponentSettings::Parameterless)
			|| !self
				.semantics
				.iter()
				.any(|semantic| semantic == component.semantic_contract())
		{
			return Err(ScopeError::RawContractUnsupported);
		}
		Ok(())
	}
}
impl fmt::Debug for RawRoleSupport {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("RawRoleSupport")
			.field("role", &self.role)
			.finish_non_exhaustive()
	}
}

pub(crate) struct RawRoleSupportSet {
	entries: Vec<RawRoleSupport>,
}
impl RawRoleSupportSet {
	pub(crate) fn into_entries(self) -> Vec<RawRoleSupport> {
		self.entries
	}
	pub(crate) fn new(entries: Vec<RawRoleSupport>) -> Result<Self, ScopeError> {
		for (index, declared) in entries.iter().enumerate() {
			if entries[..index]
				.iter()
				.any(|prior| prior.identity() == declared.identity() && prior.role == declared.role)
			{
				return Err(ScopeError::DuplicateRoleSupport { index });
			}
		}
		Ok(Self { entries })
	}
	pub(crate) fn entries(&self) -> &[RawRoleSupport] {
		&self.entries
	}
	pub(crate) fn lookup(
		&self,
		scope: &ScopeIdentity,
		role: RawRole,
	) -> Result<&RawRoleSupport, ScopeError> {
		self.entries
			.iter()
			.find(|declared| declared.identity() == scope && declared.role == role)
			.ok_or(ScopeError::RoleNotRegistered)
	}
}
impl fmt::Debug for RawRoleSupportSet {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("RawRoleSupportSet")
			.field("use_count", &self.entries.len())
			.finish()
	}
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRoleSupportWire<'a> {
	version: u32,
	scope: ScopeIdentityWire<'a>,
	role: RawRole,
	component: String,
	semantics: Vec<String>,
}
impl<'a> RawRoleSupportWire<'a> {
	pub(crate) fn from_support(support: &'a RawRoleSupport) -> Self {
		Self {
			version: 1,
			scope: ScopeIdentityWire::from_identity(&support.scope),
			role: support.role,
			component: support.component_identifier.clone(),
			semantics: support.semantics.clone(),
		}
	}
	pub(crate) fn into_support(self) -> Result<RawRoleSupport, ScopeWireError> {
		if self.version != 1 {
			return Err(ScopeWireError::UnsupportedVersion {
				version: self.version,
			});
		}
		Ok(RawRoleSupport::parse_parts(
			self.scope.into_identity()?,
			self.role,
			self.component,
			self.semantics,
		)?)
	}
}
