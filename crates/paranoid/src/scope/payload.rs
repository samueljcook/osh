//! Actual payload schema roles, independent from intrinsic family codecs and transaction authority.

use super::payload_policy::primitive_policy;
use super::{ScopeError, ScopeIdentity, ScopePolicy};
use crate::fleet::payload::state::{
	FleetPayload, FleetPayloadStateError, decode_payload, encode_payload,
};
use crate::installation::contract::role_binding::{BoundSchemaReader, bind_producer};
use crate::installation::contract::{BoundStateSchema, StateStorageEnvelope};
use crate::records::state_schema::{
	StateRepresentationFormat, StateSchema, StateSchemaCapabilities,
};

use serde::{Deserialize, Serialize};

pub(crate) mod managed;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PayloadRole {
	Reader,
	Producer,
}
/// Actual scope and application schema borrowed without managed-code trait bounds.
pub struct PayloadSchemaDeclarationFields<'scope, 'code, T, C> {
	/// The exact scope identity shared with the intrinsic family declaration.
	pub scope: &'scope ScopeIdentity,
	/// Actual registered application readers and optional encoders.
	pub schema: &'code StateSchema<T, C>,
}
/// Borrowed application payload code for one exact intrinsic scope.
///
/// Construction invokes no application codec and grants no active role or database access.
pub struct PayloadSchemaDeclaration<'scope, 'code, T, C> {
	scope: &'scope ScopeIdentity,
	schema: &'code StateSchema<T, C>,
}
impl<'scope, 'code, T, C> PayloadSchemaDeclaration<'scope, 'code, T, C> {
	/// Borrows the supplied actual scope and schema without additional application bounds.
	pub fn new(fields: PayloadSchemaDeclarationFields<'scope, 'code, T, C>) -> Self {
		Self {
			scope: fields.scope,
			schema: fields.schema,
		}
	}
	/// Borrows the supplied actual schema with its original lifetime.
	pub fn schema(&self) -> &'code StateSchema<T, C> {
		self.schema
	}
	fn policy<'a>(
		&self,
		actual_scope: &ScopeIdentity,
		policy: &'a ScopePolicy,
	) -> Result<&'a crate::installation::contract::SchemaPolicy, ScopeError> {
		if !std::ptr::eq(self.scope, actual_scope) {
			return Err(ScopeError::DifferentScopeDeclaration);
		}
		if policy.identity() != self.scope {
			return Err(ScopeError::ScopeMismatch);
		}
		let schema = primitive_policy(policy)?.payload().schema()?;
		if schema.identifier() != self.schema.identifier() {
			return Err(ScopeError::PayloadSchemaMismatch);
		}
		Ok(schema)
	}
	pub(crate) fn bind_reader<'a>(
		&self,
		actual_scope: &ScopeIdentity,
		policy: &'a ScopePolicy,
	) -> Result<BoundPayloadReader<'a, T, C>, ScopeError>
	where
		'scope: 'a,
		'code: 'a,
	{
		let policy = self.policy(actual_scope, policy)?;
		Ok(BoundPayloadReader {
			scope: self.scope,
			schema: self.schema,
			reader: BoundSchemaReader::bind(self.schema.capabilities(), self.schema, policy)?,
		})
	}
	pub(crate) fn bind_producer<'a>(
		&self,
		actual_scope: &ScopeIdentity,
		policy: &'a ScopePolicy,
	) -> Result<BoundPayloadProducer<'a, T, C>, ScopeError>
	where
		'scope: 'a,
		'code: 'a,
	{
		let policy = self.policy(actual_scope, policy)?;
		if policy.writer().envelope != StateStorageEnvelope::NativeJson {
			return Err(ScopeError::NativeWriterRequired);
		}
		Ok(BoundPayloadProducer {
			scope: self.scope,
			schema: self.schema,
			writer: bind_producer(self.schema.capabilities(), self.schema, policy)?,
		})
	}
	/// Borrows actual schema capabilities for application role declaration without erasing values.
	pub fn support_declaration(&self) -> PayloadSchemaSupportDeclaration<'scope, 'code> {
		PayloadSchemaSupportDeclaration {
			scope: self.scope,
			schema: self.schema.capabilities(),
		}
	}
	pub(crate) fn support(&self, role: PayloadRole) -> Result<PayloadRoleSupport, ScopeError> {
		self.support_declaration().support(role)
	}
}

/// Borrowed actual registry capabilities provide support derivation without an owning type bound.
pub struct PayloadSchemaSupportDeclaration<'scope, 'code> {
	scope: &'scope ScopeIdentity,
	schema: StateSchemaCapabilities<'code>,
}
impl PayloadSchemaSupportDeclaration<'_, '_> {
	pub(crate) fn support(&self, role: PayloadRole) -> Result<PayloadRoleSupport, ScopeError> {
		let mut representations = Vec::new();
		self.schema.for_each_representation(|version, capability| {
			if role == PayloadRole::Producer
				&& (!capability.writable || capability.format != StateRepresentationFormat::Json)
			{
				return;
			}
			representations.push(crate::installation::contract::RepresentationPolicy {
				version,
				envelope: match capability.format {
					StateRepresentationFormat::Bytes => StateStorageEnvelope::SourceBytes,
					StateRepresentationFormat::Json => StateStorageEnvelope::NativeJson,
				},
			});
		});
		PayloadRoleSupport::parse_parts(
			self.scope.clone(),
			self.schema.identifier().to_owned(),
			role,
			representations,
		)
	}
}

pub(crate) struct BoundPayloadReader<'a, T, C> {
	scope: &'a ScopeIdentity,
	schema: &'a StateSchema<T, C>,
	reader: BoundSchemaReader<'a, T, C>,
}
impl<T, C> BoundPayloadReader<'_, T, C> {
	pub(crate) fn require_scope_policy(
		&self,
		actual_scope: &ScopeIdentity,
		policy: &ScopePolicy,
	) -> Result<(), ScopeError> {
		if !std::ptr::eq(self.scope, actual_scope) {
			return Err(ScopeError::DifferentScopeDeclaration);
		}
		if policy.identity() != actual_scope
			|| !self
				.reader
				.policy()
				.matches_policy(primitive_policy(policy)?.payload().schema()?)
		{
			return Err(ScopeError::DifferentPayloadPolicy);
		}
		Ok(())
	}
	pub(crate) fn require_declaration(
		&self,
		actual: &PayloadSchemaDeclaration<'_, '_, T, C>,
	) -> Result<(), ScopeError> {
		if !std::ptr::eq(self.scope, actual.scope) || !std::ptr::eq(self.schema, actual.schema) {
			return Err(ScopeError::DifferentPayloadDeclaration);
		}
		Ok(())
	}
	pub(crate) fn decode(&self, payload: &FleetPayload) -> Result<T, FleetPayloadStateError<C>> {
		decode_payload(payload, &self.reader)
	}
}
pub(crate) struct BoundPayloadProducer<'a, T, C> {
	scope: &'a ScopeIdentity,
	schema: &'a StateSchema<T, C>,
	writer: BoundStateSchema<'a, T, C>,
}
impl<T, C> BoundPayloadProducer<'_, T, C> {
	pub(crate) fn require_scope_policy(
		&self,
		actual_scope: &ScopeIdentity,
		policy: &ScopePolicy,
	) -> Result<(), ScopeError> {
		if !std::ptr::eq(self.scope, actual_scope) {
			return Err(ScopeError::DifferentScopeDeclaration);
		}
		if policy.identity() != actual_scope
			|| !self
				.writer
				.policy()
				.matches_policy(primitive_policy(policy)?.payload().schema()?)
		{
			return Err(ScopeError::DifferentPayloadPolicy);
		}
		Ok(())
	}
	pub(crate) fn require_declaration(
		&self,
		actual: &PayloadSchemaDeclaration<'_, '_, T, C>,
	) -> Result<(), ScopeError> {
		if !std::ptr::eq(self.scope, actual.scope) || !std::ptr::eq(self.schema, actual.schema) {
			return Err(ScopeError::DifferentPayloadDeclaration);
		}
		Ok(())
	}
	pub(crate) fn encode(&self, value: &T) -> Result<FleetPayload, FleetPayloadStateError<C>> {
		encode_payload(value, &self.writer)
	}
}

/// Derived schema-role support carries no intrinsic family permission or active-use grant.
pub(crate) struct PayloadRoleSupport {
	pub(super) scope: ScopeIdentity,
	pub(super) schema_identifier: String,
	pub(super) role: PayloadRole,
	pub(super) representations: Vec<crate::installation::contract::RepresentationPolicy>,
}
impl PayloadRoleSupport {
	pub(super) fn parse_parts(
		scope: ScopeIdentity,
		schema: String,
		role: PayloadRole,
		representations: Vec<crate::installation::contract::RepresentationPolicy>,
	) -> Result<Self, ScopeError> {
		if schema.is_empty() || representations.is_empty() {
			return Err(ScopeError::InvalidRoleSupport);
		}
		for (index, representation) in representations.iter().enumerate() {
			if representation.version == 0
				|| representations[..index]
					.iter()
					.any(|prior| prior.version == representation.version)
				|| (role == PayloadRole::Producer
					&& representation.envelope != StateStorageEnvelope::NativeJson)
			{
				return Err(ScopeError::InvalidRoleSupport);
			}
		}
		Ok(Self {
			scope,
			schema_identifier: schema,
			role,
			representations,
		})
	}
	pub(crate) fn identity(&self) -> &ScopeIdentity {
		&self.scope
	}
	pub(crate) fn role(&self) -> PayloadRole {
		self.role
	}
	pub(crate) fn check_policy(&self, policy: &ScopePolicy) -> Result<(), ScopeError> {
		if policy.identity() != &self.scope {
			return Err(ScopeError::ScopeMismatch);
		}
		let schema = primitive_policy(policy)?.payload().schema()?;
		if schema.identifier() != self.schema_identifier {
			return Err(ScopeError::PayloadSchemaMismatch);
		}
		let writer = schema.writer();
		let required = match self.role {
			PayloadRole::Reader => schema.required_readers(),
			PayloadRole::Producer => std::slice::from_ref(&writer),
		};
		for representation in required {
			if !self.representations.contains(representation) {
				return Err(ScopeError::UnsupportedRepresentation {
					version: representation.version,
				});
			}
		}
		Ok(())
	}
}

/// One derived payload role entry per scope and role; advertisements establish no active use.
pub(crate) struct PayloadRoleSupportSet {
	entries: Vec<PayloadRoleSupport>,
}
impl PayloadRoleSupportSet {
	pub(crate) fn new(entries: Vec<PayloadRoleSupport>) -> Result<Self, ScopeError> {
		for (index, entry) in entries.iter().enumerate() {
			for prior in &entries[..index] {
				if prior.scope == entry.scope {
					if prior.role == entry.role {
						return Err(ScopeError::DuplicateRoleSupport { index });
					}
					if prior.schema_identifier != entry.schema_identifier {
						return Err(ScopeError::PayloadSchemaMismatch);
					}
				}
			}
		}
		Ok(Self { entries })
	}
	pub(crate) fn into_entries(self) -> Vec<PayloadRoleSupport> {
		self.entries
	}
	pub(crate) fn entries(&self) -> &[PayloadRoleSupport] {
		&self.entries
	}
	pub(crate) fn lookup(
		&self,
		scope: &ScopeIdentity,
		role: PayloadRole,
	) -> Result<&PayloadRoleSupport, ScopeError> {
		self.entries
			.iter()
			.find(|entry| entry.identity() == scope && entry.role() == role)
			.ok_or(ScopeError::RoleNotRegistered)
	}
}
impl std::fmt::Debug for PayloadRoleSupportSet {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("PayloadRoleSupportSet")
			.field("role_count", &self.entries.len())
			.finish()
	}
}
impl std::fmt::Debug for PayloadRoleSupport {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("PayloadRoleSupport")
			.field("role", &self.role)
			.field("representation_count", &self.representations.len())
			.finish_non_exhaustive()
	}
}
