//! One advertised code-support catalog and active references into that exact catalog.

use super::wire::{FormatWire, SchemaSupportWire, SemanticSupportWire};
use super::{PARTICIPATION_COMPONENT, ParticipantStateError};
use crate::installation::contract::{
	ComponentDeclaration, ComponentPolicy, ComponentSettings, ComponentSettingsRequirement,
	SchemaPolicy,
};
use crate::kv::document::schema::IndexContractIdentity;
use crate::scope::payload::{PayloadRole, PayloadRoleSupport, PayloadRoleSupportSet};
use crate::scope::{
	DocumentRole, DocumentRoleSupport, DocumentRoleSupportSet, PrimitiveRole,
	PrimitiveScopeSupport, PrimitiveScopeSupportSet, RawRole, RawRoleSupport, RawRoleSupportSet,
	ScopeError, ScopeIdentity, ScopePolicySet,
};
use std::fmt;

/// Serializable support for one complete component declaration.
/// Only actual declarations and the stored-representation parser create these values.
pub(crate) struct ParticipantComponentSupport {
	pub(super) component_identifier: String,
	pub(super) semantics: Vec<SemanticSupportWire>,
	pub(super) settings_schema: Option<SchemaSupportWire>,
	pub(super) schemas: Vec<SchemaSupportWire>,
}

impl ParticipantComponentSupport {
	pub(crate) fn identifier(&self) -> &str {
		&self.component_identifier
	}
	pub(crate) fn from_declaration<Transitions>(
		declaration: &ComponentDeclaration<'_, Transitions>,
	) -> Self {
		Self {
			component_identifier: declaration.identifier().to_owned(),
			semantics: declaration
				.semantic_defs()
				.iter()
				.map(|semantic| SemanticSupportWire {
					identifier: semantic.identifier.to_owned(),
					settings: semantic.settings.into(),
				})
				.collect(),
			schemas: declaration
				.schemas()
				.iter()
				.copied()
				.map(SchemaSupportWire::from_capabilities)
				.collect(),
			settings_schema: declaration
				.settings_schema()
				.map(SchemaSupportWire::from_capabilities),
		}
	}

	/// Checks a target policy without interpreting any application payload or executing a
	/// transition.
	pub(crate) fn check_policy(
		&self,
		policy: &ComponentPolicy,
	) -> Result<(), ParticipantStateError> {
		self.check_policy_for_access(policy, SupportAccess::Complete)
	}
	pub(crate) fn check_reader_policy(
		&self,
		policy: &ComponentPolicy,
	) -> Result<(), ParticipantStateError> {
		self.check_policy_for_access(policy, SupportAccess::Reader)
	}
	fn check_policy_for_access(
		&self,
		policy: &ComponentPolicy,
		access: SupportAccess,
	) -> Result<(), ParticipantStateError> {
		if self.component_identifier != policy.component_identifier() {
			return Err(ParticipantStateError::WrongComponent);
		}
		let semantic = self
			.semantics
			.iter()
			.find(|semantic| semantic.identifier == policy.semantic_contract())
			.ok_or(ParticipantStateError::UnsupportedSemanticContract)?;
		for (schema_index, required) in policy.schemas().iter().enumerate() {
			let schema = self
				.schemas
				.iter()
				.find(|schema| schema.identifier == required.identifier())
				.ok_or(ParticipantStateError::UnsupportedSchema { schema_index })?;
			check_schema_support(schema, required, schema_index, access)?;
		}
		match (
			ComponentSettingsRequirement::from(semantic.settings),
			policy.settings(),
		) {
			(ComponentSettingsRequirement::Parameterless, ComponentSettings::Parameterless) => {}
			(
				ComponentSettingsRequirement::Required,
				ComponentSettings::Document {
					representation_policy,
					document,
				},
			) => {
				let schema = self
					.settings_schema
					.as_ref()
					.ok_or(ParticipantStateError::UnsupportedSettings)?;
				if schema.identifier != representation_policy.identifier()
					|| schema.identifier != document.schema_identifier()
				{
					return Err(ParticipantStateError::UnsupportedSettings);
				}
				check_schema_support(schema, representation_policy, 0, access)
					.map_err(|error| ParticipantStateError::SettingsCapability(Box::new(error)))?;
				if !schema.representations.iter().any(|representation| {
					representation.version == document.version()
						&& representation.format == FormatWire::Json
				}) {
					return Err(ParticipantStateError::SettingsCapability(Box::new(
						ParticipantStateError::UnsupportedReader {
							schema_index: 0,
							version: document.version(),
						},
					)));
				}
			}
			_ => return Err(ParticipantStateError::UnsupportedSettings),
		}
		for (schema_index, supported) in self.schemas.iter().enumerate() {
			if !policy
				.schemas()
				.iter()
				.any(|schema| schema.identifier() == supported.identifier)
			{
				return Err(ParticipantStateError::MissingPolicySchema { schema_index });
			}
		}
		Ok(())
	}
}

#[derive(Clone, Copy)]
enum SupportAccess {
	Complete,
	Reader,
}

fn check_schema_support(
	schema: &SchemaSupportWire,
	required: &SchemaPolicy,
	schema_index: usize,
	access: SupportAccess,
) -> Result<(), ParticipantStateError> {
	let writer = required.writer();
	if matches!(access, SupportAccess::Complete)
		&& !schema.representations.iter().any(|representation| {
			representation.version == writer.version
				&& representation.format
					== FormatWire::from(writer.envelope.representation_format())
				&& representation.writable
		}) {
		return Err(ParticipantStateError::UnsupportedWriter {
			schema_index,
			version: writer.version,
		});
	}
	for reader in required.required_readers() {
		if !schema.representations.iter().any(|representation| {
			representation.version == reader.version
				&& representation.format
					== FormatWire::from(reader.envelope.representation_format())
		}) {
			return Err(ParticipantStateError::UnsupportedReader {
				schema_index,
				version: reader.version,
			});
		}
	}
	Ok(())
}

/// Support derives from actual declarations; advertisements provide no use authority.
pub(crate) struct ParticipantSupportCatalogFields {
	pub(crate) components: Vec<ParticipantComponentSupport>,
	pub(crate) document_roles: Vec<DocumentRoleSupport>,
	pub(crate) raw_roles: Vec<RawRoleSupport>,
	pub(crate) primitive_scopes: Vec<PrimitiveScopeSupport>,
	pub(crate) payload_roles: Vec<PayloadRoleSupport>,
}
pub(crate) struct ParticipantSupportCatalog {
	pub(super) components: Vec<ParticipantComponentSupport>,
	pub(super) document_roles: DocumentRoleSupportSet,
	pub(super) raw_roles: RawRoleSupportSet,
	pub(super) primitive_scopes: PrimitiveScopeSupportSet,
	pub(super) payload_roles: PayloadRoleSupportSet,
}
impl ParticipantSupportCatalog {
	pub(crate) fn new(
		fields: ParticipantSupportCatalogFields,
	) -> Result<Self, ParticipantStateError> {
		let ParticipantSupportCatalogFields {
			components,
			document_roles,
			raw_roles,
			primitive_scopes,
			payload_roles,
		} = fields;
		for entry in &payload_roles {
			if document_roles
				.iter()
				.any(|support| support.identity() == entry.identity())
				|| raw_roles
					.iter()
					.any(|support| support.identity() == entry.identity())
			{
				return Err(ScopeError::WrongOwner.into());
			}
		}
		for entry in &primitive_scopes {
			if document_roles
				.iter()
				.any(|support| support.identity() == entry.identity())
				|| raw_roles
					.iter()
					.any(|support| support.identity() == entry.identity())
			{
				return Err(ScopeError::WrongOwner.into());
			}
		}
		for (index, component) in components.iter().enumerate() {
			if components[..index]
				.iter()
				.any(|prior| prior.component_identifier == component.component_identifier)
			{
				return Err(ParticipantStateError::DuplicateComponent {
					component_index: index,
				});
			}
		}
		let document_roles = DocumentRoleSupportSet::new(document_roles)?;
		Ok(Self {
			components,
			document_roles,
			raw_roles: RawRoleSupportSet::new(raw_roles)?,
			primitive_scopes: PrimitiveScopeSupportSet::new(primitive_scopes)?,
			payload_roles: PayloadRoleSupportSet::new(payload_roles)?,
		})
	}
	pub(crate) fn component(&self, identifier: &str) -> Option<&ParticipantComponentSupport> {
		self.components
			.iter()
			.find(|declared| declared.component_identifier == identifier)
	}
	pub(crate) fn components(&self) -> &[ParticipantComponentSupport] {
		&self.components
	}
	pub(crate) fn document_roles(&self) -> &DocumentRoleSupportSet {
		&self.document_roles
	}
	pub(crate) fn primitive_scopes(&self) -> &PrimitiveScopeSupportSet {
		&self.primitive_scopes
	}
	pub(crate) fn payload_roles(&self) -> &PayloadRoleSupportSet {
		&self.payload_roles
	}
	fn resolves(&self, reference: &ParticipantUseReference) -> bool {
		match reference {
			ParticipantUseReference::Payload { scope, role } => {
				self.payload_roles.lookup(scope, *role).is_ok()
			}
			ParticipantUseReference::Primitive { scope, role } => {
				self.primitive_scopes.lookup(scope, *role).is_ok()
			}
			ParticipantUseReference::Component { identifier } => {
				self.component(identifier).is_some()
			}
			ParticipantUseReference::Document { scope, role } => {
				self.document_roles.lookup(scope, *role).is_ok()
			}
			ParticipantUseReference::Raw { scope, role } => {
				self.raw_roles.lookup(scope, *role).is_ok()
			}
			ParticipantUseReference::DocumentQuery { scope, index } => self
				.document_roles
				.lookup(scope, DocumentRole::Reader)
				.is_ok_and(|reader| {
					reader
						.query_indexes()
						.iter()
						.any(|declared| declared.identity() == index)
				}),
		}
	}
}
impl fmt::Debug for ParticipantSupportCatalog {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("ParticipantSupportCatalog")
			.field("component_count", &self.components.len())
			.field("document_role_count", &self.document_roles.entries().len())
			.field("raw_role_count", &self.raw_roles.entries().len())
			.field(
				"primitive_scope_count",
				&self.primitive_scopes.entries().len(),
			)
			.field("payload_role_count", &self.payload_roles.entries().len())
			.finish()
	}
}

/// A unique identity resolves to one catalog entry; no capability array is repeated here.
#[derive(Clone, Eq, PartialEq)]
pub(crate) enum ParticipantUseReference {
	Payload {
		scope: ScopeIdentity,
		role: PayloadRole,
	},
	Primitive {
		scope: ScopeIdentity,
		role: PrimitiveRole,
	},
	DocumentQuery {
		scope: ScopeIdentity,
		index: IndexContractIdentity,
	},
	Component {
		identifier: String,
	},
	Document {
		scope: ScopeIdentity,
		role: DocumentRole,
	},
	Raw {
		scope: ScopeIdentity,
		role: RawRole,
	},
}
impl fmt::Debug for ParticipantUseReference {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Payload { role, .. } => f
				.debug_struct("ParticipantUseReference::Payload")
				.field("role", role)
				.finish_non_exhaustive(),
			Self::Primitive { role, .. } => f
				.debug_struct("ParticipantUseReference::Primitive")
				.field("role", role)
				.finish_non_exhaustive(),
			Self::DocumentQuery { .. } => {
				f.write_str("ParticipantUseReference::DocumentQuery { .. }")
			}
			Self::Component { .. } => f.write_str("ParticipantUseReference::Component { .. }"),
			Self::Document { role, .. } => f
				.debug_struct("ParticipantUseReference::Document")
				.field("role", role)
				.finish_non_exhaustive(),
			Self::Raw { role, .. } => f
				.debug_struct("ParticipantUseReference::Raw")
				.field("role", role)
				.finish_non_exhaustive(),
		}
	}
}

/// Active uses borrow their single advertised catalog; advertisements alone grant no role.
pub(crate) struct ParticipantCapabilities {
	pub(super) advertised: ParticipantSupportCatalog,
	pub(super) active_uses: Vec<ParticipantUseReference>,
}
impl ParticipantCapabilities {
	/// Strict catalog/use construction requires uniquely resolved references and active
	/// administration.
	pub(crate) fn new(
		advertised: ParticipantSupportCatalog,
		active_uses: Vec<ParticipantUseReference>,
	) -> Result<Self, ParticipantStateError> {
		for (index, reference) in active_uses.iter().enumerate() {
			if active_uses[..index].contains(reference) {
				return Err(ParticipantStateError::DuplicateActiveUse { use_index: index });
			}
			if !advertised.resolves(reference) {
				return Err(ParticipantStateError::ActiveUseNotAdvertised { use_index: index });
			}
		}
		let capabilities = Self {
			advertised,
			active_uses,
		};
		capabilities.require_active_administration()?;
		Ok(capabilities)
	}
	pub(crate) fn advertised(&self) -> &ParticipantSupportCatalog {
		&self.advertised
	}
	pub(crate) fn active_uses(&self) -> &[ParticipantUseReference] {
		&self.active_uses
	}
	pub(super) fn require_active_administration(&self) -> Result<(), ParticipantStateError> {
		self.component(PARTICIPATION_COMPONENT)
			.ok_or(ParticipantStateError::AdministrativeSupportAbsent)?;
		Ok(())
	}
	pub(crate) fn document_roles(&self) -> ParticipantDocumentUses<'_> {
		ParticipantDocumentUses {
			catalog: &self.advertised.document_roles,
			active: &self.active_uses,
		}
	}
	pub(crate) fn raw_roles(&self) -> ParticipantRawUses<'_> {
		ParticipantRawUses {
			catalog: &self.advertised.raw_roles,
			active: &self.active_uses,
		}
	}
	pub(crate) fn primitive_roles(&self) -> ParticipantPrimitiveUses<'_> {
		ParticipantPrimitiveUses {
			catalog: &self.advertised.primitive_scopes,
			active: &self.active_uses,
		}
	}
	pub(crate) fn payload_roles(&self) -> ParticipantPayloadUses<'_> {
		ParticipantPayloadUses {
			catalog: &self.advertised.payload_roles,
			active: &self.active_uses,
		}
	}
	pub(crate) fn document_queries(&self) -> ParticipantDocumentQueryUses<'_> {
		ParticipantDocumentQueryUses {
			catalog: &self.advertised.document_roles,
			active: &self.active_uses,
		}
	}
	pub(crate) fn check_scope_policies(
		&self,
		policies: &ScopePolicySet,
	) -> Result<(), ParticipantStateError> {
		self.document_roles().check_policies(policies)?;
		self.document_queries().check_policies(policies)?;
		self.raw_roles().check_policies(policies)?;
		self.primitive_roles().check_policies(policies)?;
		self.payload_roles().check_policies(policies)
	}
	pub(crate) fn component(&self, identifier: &str) -> Option<&ParticipantComponentSupport> {
		if !self.active_uses.iter().any(|reference| match reference {
			ParticipantUseReference::Component { identifier: active } => active == identifier,
			ParticipantUseReference::Document { .. }
			| ParticipantUseReference::Raw { .. }
			| ParticipantUseReference::Primitive { .. }
			| ParticipantUseReference::Payload { .. }
			| ParticipantUseReference::DocumentQuery { .. } => false,
		}) {
			return None;
		}
		self.advertised.component(identifier)
	}
	pub(crate) fn check_administrative_policy(
		&self,
		policy: &ComponentPolicy,
	) -> Result<(), ParticipantStateError> {
		self.component(PARTICIPATION_COMPONENT)
			.ok_or(ParticipantStateError::AdministrativeSupportAbsent)?
			.check_policy(policy)
	}
	/// Only active optional dependencies constrain policy; mandatory administration remains
	/// required.
	pub(crate) fn check_installed_policies(
		&self,
		policies: &[ComponentPolicy],
	) -> Result<(), ParticipantStateError> {
		let administrative_policy = policies
			.iter()
			.find(|policy| policy.component_identifier() == PARTICIPATION_COMPONENT)
			.ok_or(ParticipantStateError::AdministrativePolicyAbsent)?;
		self.check_administrative_policy(administrative_policy)?;
		for (component_index, component) in self.advertised.components.iter().enumerate() {
			if component.component_identifier == PARTICIPATION_COMPONENT
				|| self.component(&component.component_identifier).is_none()
			{
				continue;
			}
			let policy = policies
				.iter()
				.find(|policy| policy.component_identifier() == component.component_identifier)
				.ok_or(ParticipantStateError::ComponentNotInstalled { component_index })?;
			component.check_policy(policy)?;
		}
		Ok(())
	}
}

/// This borrowed view exposes only active reader/producer/maintenance entries, without copying
/// support.
pub(crate) struct ParticipantDocumentUses<'a> {
	catalog: &'a DocumentRoleSupportSet,
	active: &'a [ParticipantUseReference],
}
impl<'a> ParticipantDocumentUses<'a> {
	pub(crate) fn uses(&self) -> impl Iterator<Item = &'a DocumentRoleSupport> + 'a + use<'a> {
		let active = self.active;
		self.catalog.entries().iter().filter(move |support| {
			active.iter().any(|reference| match reference {
				ParticipantUseReference::Document { scope, role } => {
					scope == support.identity() && *role == support.role()
				}
				ParticipantUseReference::Component { .. }
				| ParticipantUseReference::Raw { .. }
				| ParticipantUseReference::Primitive { .. }
				| ParticipantUseReference::Payload { .. }
				| ParticipantUseReference::DocumentQuery { .. } => false,
			})
		})
	}
	pub(crate) fn lookup(
		&self,
		scope: &ScopeIdentity,
		role: DocumentRole,
	) -> Result<&'a DocumentRoleSupport, ScopeError> {
		self.uses()
			.find(|support| support.identity() == scope && support.role() == role)
			.ok_or(ScopeError::RoleNotRegistered)
	}
	pub(crate) fn check_policies(&self, policies: &ScopePolicySet) -> Result<(), ScopeError> {
		for support in self.uses() {
			support.check_policy(policies.lookup(support.identity())?)?;
		}
		Ok(())
	}
}

/// One query use resolves a descriptor and retained readers from the same advertised entry.
pub(crate) struct ParticipantDocumentQueryUse<'a> {
	reader: &'a DocumentRoleSupport,
	index: &'a crate::scope::IndexContractDescriptor,
}
impl ParticipantDocumentQueryUse<'_> {
	pub(crate) fn identity(&self) -> &ScopeIdentity {
		self.reader.identity()
	}
	pub(crate) fn index(&self) -> &crate::scope::IndexContractDescriptor {
		self.index
	}
	pub(crate) fn check_policy(
		&self,
		policy: &crate::scope::ScopePolicy,
	) -> Result<(), ScopeError> {
		self.reader
			.check_query_policy(policy, self.index.identity())
	}
}
pub(crate) struct ParticipantDocumentQueryUses<'a> {
	catalog: &'a DocumentRoleSupportSet,
	active: &'a [ParticipantUseReference],
}
impl<'a> ParticipantDocumentQueryUses<'a> {
	pub(crate) fn uses(
		&self,
	) -> impl Iterator<Item = ParticipantDocumentQueryUse<'a>> + 'a + use<'a> {
		let catalog = self.catalog;
		self.active.iter().filter_map(move |reference| {
			let ParticipantUseReference::DocumentQuery { scope, index } = reference else {
				return None;
			};
			let reader = catalog
				.lookup(scope, DocumentRole::Reader)
				.expect("active query resolves its immutable reader catalog");
			let index = reader
				.query_indexes()
				.iter()
				.find(|declared| declared.identity() == index)
				.expect("active query resolves its immutable index declaration");
			Some(ParticipantDocumentQueryUse { reader, index })
		})
	}
	pub(crate) fn lookup(
		&self,
		scope: &ScopeIdentity,
		index: &IndexContractIdentity,
	) -> Result<ParticipantDocumentQueryUse<'a>, ScopeError> {
		self.uses()
			.find(|query| query.identity() == scope && query.index.identity() == index)
			.ok_or(ScopeError::QueryUseNotRegistered)
	}
	pub(crate) fn check_policies(&self, policies: &ScopePolicySet) -> Result<(), ScopeError> {
		for query in self.uses() {
			query.check_policy(policies.lookup(query.identity())?)?;
		}
		Ok(())
	}
}

/// This borrowed view exposes only active opaque-byte access entries, without copying support.
pub(crate) struct ParticipantRawUses<'a> {
	catalog: &'a RawRoleSupportSet,
	active: &'a [ParticipantUseReference],
}
impl<'a> ParticipantRawUses<'a> {
	pub(crate) fn uses(&self) -> impl Iterator<Item = &'a RawRoleSupport> + 'a + use<'a> {
		let active = self.active;
		self.catalog.entries().iter().filter(move |support| {
			active.iter().any(|reference| match reference {
				ParticipantUseReference::Raw { scope, role } => {
					scope == support.identity() && *role == support.role()
				}
				ParticipantUseReference::Component { .. }
				| ParticipantUseReference::Document { .. }
				| ParticipantUseReference::Primitive { .. }
				| ParticipantUseReference::Payload { .. }
				| ParticipantUseReference::DocumentQuery { .. } => false,
			})
		})
	}
	pub(crate) fn lookup(
		&self,
		scope: &ScopeIdentity,
		role: RawRole,
	) -> Result<&'a RawRoleSupport, ScopeError> {
		self.uses()
			.find(|support| support.identity() == scope && support.role() == role)
			.ok_or(ScopeError::RoleNotRegistered)
	}
	pub(crate) fn check_policies(&self, policies: &ScopePolicySet) -> Result<(), ScopeError> {
		for support in self.uses() {
			support.check_policy(policies.lookup(support.identity())?)?;
		}
		Ok(())
	}
}

/// Each active role borrows the one shared scope support entry.
pub(crate) struct ParticipantPrimitiveUse<'a> {
	support: &'a PrimitiveScopeSupport,
	role: PrimitiveRole,
}
impl ParticipantPrimitiveUse<'_> {
	pub(crate) fn identity(&self) -> &ScopeIdentity {
		self.support.identity()
	}
	pub(crate) fn check_policy(
		&self,
		policy: &crate::scope::ScopePolicy,
	) -> Result<(), ScopeError> {
		self.support.check_policy(self.role, policy)
	}
}
pub(crate) struct ParticipantPrimitiveUses<'a> {
	catalog: &'a PrimitiveScopeSupportSet,
	active: &'a [ParticipantUseReference],
}
impl<'a> ParticipantPrimitiveUses<'a> {
	pub(crate) fn uses(&self) -> impl Iterator<Item = ParticipantPrimitiveUse<'a>> + 'a + use<'a> {
		let catalog = self.catalog;
		self.active.iter().filter_map(move |reference| {
			let ParticipantUseReference::Primitive { scope, role } = reference else {
				return None;
			};
			let support = catalog
				.lookup(scope, *role)
				.expect("active Primitive role resolves its immutable catalog");
			Some(ParticipantPrimitiveUse {
				support,
				role: *role,
			})
		})
	}
	pub(crate) fn lookup(
		&self,
		scope: &ScopeIdentity,
		role: PrimitiveRole,
	) -> Result<ParticipantPrimitiveUse<'a>, ScopeError> {
		self.uses()
			.find(|entry| entry.identity() == scope && entry.role == role)
			.ok_or(ScopeError::RoleNotRegistered)
	}
	pub(crate) fn check_policies(&self, policies: &ScopePolicySet) -> Result<(), ScopeError> {
		for entry in self.uses() {
			entry.check_policy(policies.lookup(entry.identity())?)?;
		}
		Ok(())
	}
}

/// Active payload roles borrow actual derived schema support without intrinsic resource permission.
pub(crate) struct ParticipantPayloadUses<'a> {
	catalog: &'a PayloadRoleSupportSet,
	active: &'a [ParticipantUseReference],
}
impl<'a> ParticipantPayloadUses<'a> {
	pub(crate) fn uses(&self) -> impl Iterator<Item = &'a PayloadRoleSupport> + 'a + use<'a> {
		let active = self.active;
		self.catalog.entries().iter().filter(move |support| {
			active.iter().any(|reference| {
				matches!(reference,
				ParticipantUseReference::Payload { scope, role }
				if scope == support.identity() && *role == support.role())
			})
		})
	}
	pub(crate) fn lookup(
		&self,
		scope: &ScopeIdentity,
		role: PayloadRole,
	) -> Result<&'a PayloadRoleSupport, ScopeError> {
		self.uses()
			.find(|support| support.identity() == scope && support.role() == role)
			.ok_or(ScopeError::RoleNotRegistered)
	}
	pub(crate) fn check_policies(
		&self,
		policies: &ScopePolicySet,
	) -> Result<(), ParticipantStateError> {
		for support in self.uses() {
			policies
				.lookup(support.identity())
				.and_then(|policy| support.check_policy(policy))
				.map_err(|error| ParticipantStateError::PayloadScope {
					scope: support.identity().clone(),
					source: Box::new(error),
				})?;
		}
		Ok(())
	}
}
