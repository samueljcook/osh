//! Complete first-native scope policies with explicit maintained and ready sets.

use primitives_rs::serialization::json::JsonDecodeFailureSummary;
use serde_json::value::RawValue;

use super::{
	ComponentPolicyWire, Deserialize, DocumentScopePolicy, IndexIdentityWire, IndexWire,
	SchemaPolicyWire, ScopeError, ScopeIdentityWire, ScopeOwner, ScopePolicy, ScopePolicySet,
	ScopeWireError, Serialize, VersionHeader, WIRE_VERSION, require_version,
};
use crate::scope::{PrimitivePayloadPolicy, PrimitiveScopePolicy, PrimitiveScopePolicyFields};

#[cfg(test)]
mod tests;

pub(crate) enum ScopePolicySetWire<'a> {
	Current(PolicySetWire<'a>),
	Unsupported(u32),
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PolicySetWire<'a> {
	version: u32,
	scopes: Vec<PolicyWire<'a>>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PolicyWire<'a> {
	scope: ScopeIdentityWire<'a>,
	owner: OwnerWire<'a>,
}
// External tagging is required: Raw and Primitive owners contain component settings documents.
// Their raw JSON cannot be decoded from the buffer that serde uses for internal tagging.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum OwnerWire<'a> {
	Raw {
		component: ComponentPolicyWire<'a>,
	},
	Document {
		schema: SchemaPolicyWire<'a>,
		maintenance_indexes: Vec<IndexWire<'a>>,
		ready_indexes: Vec<IndexIdentityWire<'a>>,
	},
	Primitive {
		intrinsic: ComponentPolicyWire<'a>,
		payload: PayloadPolicyWire<'a>,
	},
	Closed {},
}
#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum PayloadPolicyWire<'a> {
	None {},
	Opaque {},
	Schema { schema: SchemaPolicyWire<'a> },
}
impl<'a> PayloadPolicyWire<'a> {
	fn from_policy(policy: &'a PrimitivePayloadPolicy) -> Self {
		match policy {
			PrimitivePayloadPolicy::None => Self::None {},
			PrimitivePayloadPolicy::Opaque => Self::Opaque {},
			PrimitivePayloadPolicy::Schema(schema) => Self::Schema {
				schema: SchemaPolicyWire::from_policy(schema),
			},
		}
	}
	fn into_policy(self) -> Result<PrimitivePayloadPolicy, ScopeWireError> {
		match self {
			Self::None {} => Ok(PrimitivePayloadPolicy::None),
			Self::Opaque {} => Ok(PrimitivePayloadPolicy::Opaque),
			Self::Schema { schema } => Ok(PrimitivePayloadPolicy::Schema(
				schema.into_policy().map_err(ScopeError::from)?,
			)),
		}
	}
}

/// The same complete scope parser supplies checkpoint invariants.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScopePolicyWire<'a> {
	version: u32,
	policy: PolicyWire<'a>,
}
impl<'a> ScopePolicyWire<'a> {
	pub(crate) fn from_policy(policy: &'a ScopePolicy) -> Self {
		Self {
			version: WIRE_VERSION,
			policy: PolicyWire::from_policy(policy),
		}
	}
	pub(crate) fn into_policy(self) -> Result<ScopePolicy, ScopeWireError> {
		require_version(self.version)?;
		self.policy.into_policy()
	}
}
impl<'a> PolicyWire<'a> {
	fn from_policy(policy: &'a ScopePolicy) -> Self {
		let owner = match policy.owner() {
			ScopeOwner::Raw(component) => OwnerWire::Raw {
				component: ComponentPolicyWire::from_policy(component),
			},
			ScopeOwner::Primitive(primitive) => OwnerWire::Primitive {
				intrinsic: ComponentPolicyWire::from_policy(primitive.intrinsic()),
				payload: PayloadPolicyWire::from_policy(primitive.payload()),
			},
			ScopeOwner::Closed => OwnerWire::Closed {},
			ScopeOwner::Document(document) => OwnerWire::Document {
				schema: SchemaPolicyWire::from_policy(document.schema()),
				maintenance_indexes: document
					.maintenance_indexes()
					.map(IndexWire::from_descriptor)
					.collect(),
				ready_indexes: document
					.ready_indexes()
					.map(IndexIdentityWire::from_identity)
					.collect(),
			},
		};
		Self {
			scope: ScopeIdentityWire::from_identity(policy.identity()),
			owner,
		}
	}
	fn into_policy(self) -> Result<ScopePolicy, ScopeWireError> {
		let owner = match self.owner {
			OwnerWire::Raw { component } => {
				ScopeOwner::Raw(component.into_policy().map_err(ScopeError::from)?)
			}
			OwnerWire::Primitive { intrinsic, payload } => {
				ScopeOwner::Primitive(PrimitiveScopePolicy::new(PrimitiveScopePolicyFields {
					intrinsic: intrinsic.into_policy().map_err(ScopeError::from)?,
					payload: payload.into_policy()?,
				})?)
			}
			OwnerWire::Closed {} => ScopeOwner::Closed,
			OwnerWire::Document {
				schema,
				maintenance_indexes,
				ready_indexes,
			} => ScopeOwner::Document(DocumentScopePolicy::new_with_ready_indexes(
				schema.into_policy().map_err(ScopeError::from)?,
				maintenance_indexes
					.into_iter()
					.map(IndexWire::into_descriptor)
					.collect::<Result<Vec<_>, _>>()?,
				ready_indexes
					.into_iter()
					.map(IndexIdentityWire::into_identity)
					.collect::<Result<Vec<_>, _>>()?,
			)?),
		};
		Ok(ScopePolicy::new(self.scope.into_identity()?, owner))
	}
}
impl<'a> ScopePolicySetWire<'a> {
	pub(crate) fn from_policies(policies: &'a ScopePolicySet) -> Self {
		Self::Current(PolicySetWire {
			version: WIRE_VERSION,
			scopes: policies
				.policies()
				.iter()
				.map(PolicyWire::from_policy)
				.collect(),
		})
	}
	pub(crate) fn into_policies(self) -> Result<ScopePolicySet, ScopeWireError> {
		match self {
			Self::Unsupported(version) => Err(ScopeWireError::UnsupportedVersion { version }),
			Self::Current(wire) => {
				require_version(wire.version)?;
				Ok(ScopePolicySet::new(
					wire.scopes
						.into_iter()
						.map(PolicyWire::into_policy)
						.collect::<Result<Vec<_>, _>>()?,
				)?)
			}
		}
	}
}
impl<'de, 'a> Deserialize<'de> for ScopePolicySetWire<'a> {
	fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
		// Raw JSON keeps nested settings documents byte-exact. This wire must therefore never be
		// nested inside an internally tagged, untagged or flattened serde shape because serde
		// buffers those shapes and raw JSON cannot be decoded from that buffer.
		let raw = Box::<RawValue>::deserialize(deserializer)?;
		let header: VersionHeader = serde_json::from_str(raw.get()).map_err(|error| {
			serde::de::Error::custom(JsonDecodeFailureSummary::from_serde_error(&error))
		})?;
		if header.version != WIRE_VERSION {
			return Ok(Self::Unsupported(header.version));
		}
		serde_json::from_str(raw.get())
			.map(Self::Current)
			.map_err(|error| {
				serde::de::Error::custom(JsonDecodeFailureSummary::from_serde_error(&error))
			})
	}
}
impl Serialize for ScopePolicySetWire<'_> {
	fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		match self {
			Self::Current(wire) => wire.serialize(serializer),
			Self::Unsupported(_) => Err(serde::ser::Error::custom(
				"unsupported scope policy version",
			)),
		}
	}
}
