//! Complete native participation catalog, active references, queries and settings support.

use super::catalog::{
	ParticipantCapabilities, ParticipantComponentSupport, ParticipantSupportCatalog,
	ParticipantSupportCatalogFields, ParticipantUseReference,
};
use super::{
	PARTICIPANT_TOKEN_LEN, ParticipantState, ParticipantStateError, ParticipantToken, invalid,
	json_error,
};
use crate::installation::contract::ComponentSettingsRequirement;
use crate::records::StateTimestampMicros;
use crate::records::state_schema::{StateRepresentationFormat, StateSchemaCapabilities};
use crate::scope::wire::{DocumentRoleSupportWire, IndexIdentityWire, ScopeIdentityWire};
use crate::scope::{DocumentRole, PrimitiveRole, RawRole};
use crate::scope::{
	payload::PayloadRole, primitive::PrimitiveScopeSupportWire, raw::RawRoleSupportWire,
	wire::PayloadRoleSupportWire,
};
use primitives_rs::encoding::decimal::parse_canonical_decimal;
use primitives_rs::id::time_sortable::{SORTABLE_ID_LEN, SortableId};
use primitives_rs::secret_bytes::SecretToken32;
use primitives_rs::serialization::json::deserialize_required_option;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use zeroize::Zeroize;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RegistrationWire {
	installation_id: [u8; SORTABLE_ID_LEN],
	registration_id: [u8; SORTABLE_ID_LEN],
	token: [u8; PARTICIPANT_TOKEN_LEN],
	expires_at: String,
}
impl Drop for RegistrationWire {
	fn drop(&mut self) {
		self.token.zeroize();
	}
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ComponentSupportWire {
	identifier: String,
	semantics: Vec<SemanticSupportWire>,
	schemas: Vec<SchemaSupportWire>,
	#[serde(deserialize_with = "deserialize_required_option")]
	settings_schema: Option<SchemaSupportWire>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SemanticSupportWire {
	pub(super) identifier: String,
	pub(super) settings: SettingsRequirementWire,
}
#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum SettingsRequirementWire {
	Parameterless {},
	Required {},
}
impl From<ComponentSettingsRequirement> for SettingsRequirementWire {
	fn from(requirement: ComponentSettingsRequirement) -> Self {
		match requirement {
			ComponentSettingsRequirement::Parameterless => Self::Parameterless {},
			ComponentSettingsRequirement::Required => Self::Required {},
		}
	}
}
impl From<SettingsRequirementWire> for ComponentSettingsRequirement {
	fn from(requirement: SettingsRequirementWire) -> Self {
		match requirement {
			SettingsRequirementWire::Parameterless {} => Self::Parameterless,
			SettingsRequirementWire::Required {} => Self::Required,
		}
	}
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SchemaSupportWire {
	pub(super) identifier: String,
	pub(super) representations: Vec<RepresentationSupportWire>,
}
impl SchemaSupportWire {
	pub(super) fn from_capabilities(schema: StateSchemaCapabilities<'_>) -> Self {
		let mut representations = Vec::new();
		schema.for_each_representation(|version, capability| {
			representations.push(RepresentationSupportWire {
				version,
				format: capability.format.into(),
				writable: capability.writable,
			})
		});
		Self {
			identifier: schema.identifier().to_owned(),
			representations,
		}
	}
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RepresentationSupportWire {
	pub(super) version: u32,
	pub(super) format: FormatWire,
	pub(super) writable: bool,
}
#[derive(Clone, Copy, Deserialize, Serialize, Eq, PartialEq)]
pub(super) enum FormatWire {
	Bytes,
	Json,
}
impl From<StateRepresentationFormat> for FormatWire {
	fn from(format: StateRepresentationFormat) -> Self {
		match format {
			StateRepresentationFormat::Bytes => Self::Bytes,
			StateRepresentationFormat::Json => Self::Json,
		}
	}
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CatalogWire<'a> {
	components: Vec<ComponentSupportWire>,
	/// Published `paranoid 0.0.1-pre.8` processes write an empty `queue_tasks` array in every
	/// `paranoid.runtime-participation` record and require that field when they read participant
	/// records during rolling coexistence.
	#[serde(default)]
	queue_tasks: [(); 0],
	document_roles: Vec<DocumentRoleSupportWire<'a>>,
	raw_roles: Vec<RawRoleSupportWire<'a>>,
	primitive_scopes: Vec<PrimitiveScopeSupportWire<'a>>,
	payload_roles: Vec<PayloadRoleSupportWire<'a>>,
}
impl<'a> CatalogWire<'a> {
	fn from_catalog(catalog: &'a ParticipantSupportCatalog) -> Self {
		Self {
			components: component_wires(catalog),
			queue_tasks: [],
			payload_roles: catalog
				.payload_roles
				.entries()
				.iter()
				.map(PayloadRoleSupportWire::from_support)
				.collect(),
			primitive_scopes: catalog
				.primitive_scopes
				.entries()
				.iter()
				.map(PrimitiveScopeSupportWire::from_support)
				.collect(),
			document_roles: catalog
				.document_roles
				.entries()
				.iter()
				.map(DocumentRoleSupportWire::from_support)
				.collect(),
			raw_roles: catalog
				.raw_roles
				.entries()
				.iter()
				.map(RawRoleSupportWire::from_support)
				.collect(),
		}
	}
	fn into_catalog(self) -> Result<ParticipantSupportCatalog, ParticipantStateError> {
		ParticipantSupportCatalog::new(ParticipantSupportCatalogFields {
			components: parse_components(self.components)?,
			payload_roles: self
				.payload_roles
				.into_iter()
				.map(PayloadRoleSupportWire::into_support)
				.collect::<Result<Vec<_>, _>>()?,
			document_roles: self
				.document_roles
				.into_iter()
				.map(DocumentRoleSupportWire::into_support)
				.collect::<Result<Vec<_>, _>>()?,
			raw_roles: self
				.raw_roles
				.into_iter()
				.map(RawRoleSupportWire::into_support)
				.collect::<Result<Vec<_>, _>>()?,
			primitive_scopes: self
				.primitive_scopes
				.into_iter()
				.map(PrimitiveScopeSupportWire::into_support)
				.collect::<Result<Vec<_>, _>>()?,
		})
	}
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ParticipantWire {
	registration: RegistrationWire,
	advertised: CatalogWire<'static>,
	active_uses: Vec<ActiveUseWire<'static>>,
}
#[derive(Serialize)]
struct ParticipantWireRef<'a> {
	registration: RegistrationWire,
	advertised: CatalogWire<'a>,
	active_uses: Vec<ActiveUseWire<'a>>,
}

pub(super) fn decode_state(bytes: &[u8]) -> Result<ParticipantState, ParticipantStateError> {
	let wire: ParticipantWire = serde_json::from_slice(bytes).map_err(json_error)?;
	let capabilities = ParticipantCapabilities::new(
		wire.advertised.into_catalog()?,
		wire.active_uses
			.into_iter()
			.map(ActiveUseWire::into_reference)
			.collect::<Result<Vec<_>, _>>()?,
	)?;
	let registration = wire.registration;
	Ok(ParticipantState {
		installation_id: SortableId::from_bytes(&registration.installation_id)
			.expect("parsed fixed-size identifier"),
		registration_id: SortableId::from_bytes(&registration.registration_id)
			.expect("parsed fixed-size identifier"),
		token: ParticipantToken(SecretToken32::from_array(registration.token)),
		expires_at: parse_expiry(&registration.expires_at)?,
		capabilities,
	})
}
pub(super) fn encode_state(state: &ParticipantState) -> Result<Vec<u8>, ParticipantStateError> {
	state.capabilities.require_active_administration()?;
	serde_json::to_vec(&ParticipantWireRef {
		registration: RegistrationWire {
			installation_id: *state.installation_id.as_bytes(),
			registration_id: *state.registration_id.as_bytes(),
			token: *state.token.0.expose_secret(),
			expires_at: state.expires_at.as_i64().to_string(),
		},
		advertised: CatalogWire::from_catalog(state.capabilities.advertised()),
		active_uses: state
			.capabilities
			.active_uses()
			.iter()
			.map(ActiveUseWire::from_reference)
			.collect(),
	})
	.map_err(json_error)
}

impl ComponentSupportWire {
	pub(crate) fn from_support(component: &ParticipantComponentSupport) -> Self {
		Self {
			identifier: component.component_identifier.clone(),
			semantics: component.semantics.clone(),
			schemas: component.schemas.clone(),
			settings_schema: component.settings_schema.clone(),
		}
	}
	pub(crate) fn into_support(self) -> Result<ParticipantComponentSupport, ParticipantStateError> {
		if self.identifier.is_empty() || self.semantics.is_empty() {
			return Err(invalid(
				"component requires an identifier and actual semantic support",
			));
		}
		for (index, semantic) in self.semantics.iter().enumerate() {
			if semantic.identifier.is_empty()
				|| self.semantics[..index]
					.iter()
					.any(|previous| previous.identifier == semantic.identifier)
			{
				return Err(invalid(
					"semantic identifiers must be nonempty and distinct",
				));
			}
			if matches!(semantic.settings, SettingsRequirementWire::Required {})
				&& self.settings_schema.is_none()
			{
				return Err(invalid(
					"required semantic settings need an actual schema declaration",
				));
			}
		}
		for (index, schema) in self.schemas.iter().enumerate() {
			check_schema_support_wire(schema)?;
			if self.schemas[..index]
				.iter()
				.any(|previous| previous.identifier == schema.identifier)
			{
				return Err(invalid("fixed schema identifiers must be distinct"));
			}
		}
		if let Some(schema) = &self.settings_schema {
			check_schema_support_wire(schema)?;
			if self
				.schemas
				.iter()
				.any(|fixed| fixed.identifier == schema.identifier)
			{
				return Err(invalid(
					"settings and fixed schema identities must be distinct",
				));
			}
		}
		Ok(ParticipantComponentSupport {
			component_identifier: self.identifier,
			semantics: self.semantics,
			schemas: self.schemas,
			settings_schema: self.settings_schema,
		})
	}
}
fn parse_components(
	components: Vec<ComponentSupportWire>,
) -> Result<Vec<ParticipantComponentSupport>, ParticipantStateError> {
	components
		.into_iter()
		.map(ComponentSupportWire::into_support)
		.collect()
}
pub(super) fn check_schema_support_wire(
	schema: &SchemaSupportWire,
) -> Result<(), ParticipantStateError> {
	if schema.identifier.is_empty() || schema.representations.is_empty() {
		return Err(invalid(
			"schema requires an identifier and actual representations",
		));
	}
	for (index, representation) in schema.representations.iter().enumerate() {
		if representation.version == 0
			|| schema.representations[..index]
				.iter()
				.any(|previous| previous.version == representation.version)
		{
			return Err(invalid(
				"representation versions must be positive and distinct",
			));
		}
	}
	Ok(())
}
fn component_wires(catalog: &ParticipantSupportCatalog) -> Vec<ComponentSupportWire> {
	catalog
		.components
		.iter()
		.map(ComponentSupportWire::from_support)
		.collect()
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ActiveUseWire<'a> {
	Payload {
		scope: ScopeIdentityWire<'a>,
		role: PayloadRole,
	},
	Primitive {
		scope: ScopeIdentityWire<'a>,
		role: PrimitiveRole,
	},
	DocumentQuery {
		scope: ScopeIdentityWire<'a>,
		index: IndexIdentityWire<'a>,
	},
	Component {
		identifier: Cow<'a, str>,
	},
	Document {
		scope: ScopeIdentityWire<'a>,
		role: DocumentRoleWire,
	},
	Raw {
		scope: ScopeIdentityWire<'a>,
		role: RawRole,
	},
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum DocumentRoleWire {
	Reader,
	Producer,
	Maintenance,
}
impl From<DocumentRole> for DocumentRoleWire {
	fn from(role: DocumentRole) -> Self {
		match role {
			DocumentRole::Reader => Self::Reader,
			DocumentRole::Producer => Self::Producer,
			DocumentRole::Maintenance => Self::Maintenance,
		}
	}
}
impl From<DocumentRoleWire> for DocumentRole {
	fn from(role: DocumentRoleWire) -> Self {
		match role {
			DocumentRoleWire::Reader => Self::Reader,
			DocumentRoleWire::Producer => Self::Producer,
			DocumentRoleWire::Maintenance => Self::Maintenance,
		}
	}
}
impl<'a> ActiveUseWire<'a> {
	fn from_reference(reference: &'a ParticipantUseReference) -> Self {
		match reference {
			ParticipantUseReference::Payload { scope, role } => Self::Payload {
				scope: ScopeIdentityWire::from_identity(scope),
				role: *role,
			},
			ParticipantUseReference::Primitive { scope, role } => Self::Primitive {
				scope: ScopeIdentityWire::from_identity(scope),
				role: *role,
			},
			ParticipantUseReference::DocumentQuery { scope, index } => Self::DocumentQuery {
				scope: ScopeIdentityWire::from_identity(scope),
				index: IndexIdentityWire::from_identity(index),
			},
			ParticipantUseReference::Component { identifier } => Self::Component {
				identifier: Cow::Borrowed(identifier),
			},
			ParticipantUseReference::Document { scope, role } => Self::Document {
				scope: ScopeIdentityWire::from_identity(scope),
				role: (*role).into(),
			},
			ParticipantUseReference::Raw { scope, role } => Self::Raw {
				scope: ScopeIdentityWire::from_identity(scope),
				role: *role,
			},
		}
	}
	fn into_reference(self) -> Result<ParticipantUseReference, ParticipantStateError> {
		Ok(match self {
			Self::Payload { scope, role } => ParticipantUseReference::Payload {
				scope: scope.into_identity()?,
				role,
			},
			Self::Primitive { scope, role } => ParticipantUseReference::Primitive {
				scope: scope.into_identity()?,
				role,
			},
			Self::DocumentQuery { scope, index } => ParticipantUseReference::DocumentQuery {
				scope: scope.into_identity()?,
				index: index.into_identity()?,
			},
			Self::Component { identifier } => ParticipantUseReference::Component {
				identifier: identifier.into_owned(),
			},
			Self::Document { scope, role } => ParticipantUseReference::Document {
				scope: scope.into_identity()?,
				role: role.into(),
			},
			Self::Raw { scope, role } => ParticipantUseReference::Raw {
				scope: scope.into_identity()?,
				role,
			},
		})
	}
}
fn parse_expiry(value: &str) -> Result<StateTimestampMicros, ParticipantStateError> {
	let timestamp = parse_canonical_decimal::<i64>(value)
		.map_err(|_| invalid("expiration must use canonical signed decimal encoding"))?;
	Ok(StateTimestampMicros::new(timestamp))
}
