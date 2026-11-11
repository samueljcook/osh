//! One Primitive owner with separate intrinsic and explicit embedded-payload meaning.

use super::{ScopeError, ScopeOwner, ScopePolicy};
use crate::installation::contract::{
	ComponentPolicy, RepresentationPolicy, SchemaPolicy, StateStorageEnvelope,
};

#[derive(Clone, Debug)]
pub(crate) enum PrimitivePayloadPolicy {
	None,
	Opaque,
	Schema(SchemaPolicy),
}
impl PrimitivePayloadPolicy {
	pub(crate) fn require_none(&self) -> Result<(), ScopeError> {
		if !matches!(self, Self::None) {
			return Err(ScopeError::PrimitivePayloadNotAllowed);
		}
		Ok(())
	}
	pub(crate) fn schema(&self) -> Result<&SchemaPolicy, ScopeError> {
		match self {
			Self::Schema(policy) => Ok(policy),
			_ => Err(ScopeError::PayloadSchemaRequired),
		}
	}
	pub(crate) fn require_representation(
		&self,
		identifier: &str,
		representation: RepresentationPolicy,
	) -> Result<(), ScopeError> {
		let policy = self.schema()?;
		if policy.identifier() != identifier {
			return Err(ScopeError::PayloadSchemaMismatch);
		}
		if !policy.required_readers().contains(&representation) {
			return Err(ScopeError::UnsupportedRepresentation {
				version: representation.version,
			});
		}
		Ok(())
	}
	pub(crate) fn matches_policy(&self, other: &Self) -> bool {
		match (self, other) {
			(Self::None, Self::None) | (Self::Opaque, Self::Opaque) => true,
			(Self::Schema(a), Self::Schema(b)) => a.matches_policy(b),
			_ => false,
		}
	}
	pub(crate) fn permits_compatible_ownership(&self, other: &Self) -> bool {
		match (self, other) {
			(Self::None, Self::None) | (Self::Opaque, Self::Opaque) => true,
			(Self::Schema(a), Self::Schema(b)) => a.identifier() == b.identifier(),
			_ => false,
		}
	}
}

pub(crate) struct PrimitiveScopePolicyFields {
	pub(crate) intrinsic: ComponentPolicy,
	pub(crate) payload: PrimitivePayloadPolicy,
}
#[derive(Clone, Debug)]
pub(crate) struct PrimitiveScopePolicy {
	intrinsic: ComponentPolicy,
	payload: PrimitivePayloadPolicy,
}
impl PrimitiveScopePolicy {
	pub(crate) fn new(fields: PrimitiveScopePolicyFields) -> Result<Self, ScopeError> {
		if let PrimitivePayloadPolicy::Schema(schema) = &fields.payload
			&& schema.writer().envelope != StateStorageEnvelope::NativeJson
		{
			return Err(ScopeError::NativeWriterRequired);
		}
		Ok(Self {
			intrinsic: fields.intrinsic,
			payload: fields.payload,
		})
	}
	pub(crate) fn intrinsic(&self) -> &ComponentPolicy {
		&self.intrinsic
	}
	pub(crate) fn payload(&self) -> &PrimitivePayloadPolicy {
		&self.payload
	}
	pub(crate) fn matches_policy(&self, other: &Self) -> bool {
		self.intrinsic.matches_policy(&other.intrinsic)
			&& self.payload.matches_policy(&other.payload)
	}
}

pub(super) fn primitive_policy(policy: &ScopePolicy) -> Result<&PrimitiveScopePolicy, ScopeError> {
	match policy.owner() {
		ScopeOwner::Primitive(primitive) => Ok(primitive),
		_ => Err(ScopeError::WrongOwner),
	}
}
pub(crate) fn require_no_primitive_payload(policy: &ScopePolicy) -> Result<(), ScopeError> {
	primitive_policy(policy)?.payload().require_none()
}
