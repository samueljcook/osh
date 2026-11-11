//! Strict versioned ownership and role-support codecs, without authority claims.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::installation::contract::policy_wire::{ComponentPolicyWire, SchemaPolicyWire};
use crate::installation::contract::{RepresentationPolicy, StateStorageEnvelope};
use crate::kv::document::schema::{
	IndexContract, IndexContractIdentity, IndexFieldDef, IndexScalarType,
};
use crate::records::RecordNamespace;

use super::support::SupportedRepresentation;
use super::{
	DocumentRole, DocumentRoleSupport, DocumentScopePolicy, IndexContractDescriptor, ScopeError,
	ScopeIdentity, ScopeOwner, ScopePolicy, ScopePolicySet, StorageIdentity,
};

mod payload;
mod policy;
mod support;
pub(crate) use payload::PayloadRoleSupportWire;
pub(crate) use policy::{ScopePolicySetWire, ScopePolicyWire};
pub(crate) use support::DocumentRoleSupportWire;

const WIRE_VERSION: u32 = 1;

#[derive(Deserialize)]
struct VersionHeader {
	version: u32,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IndexIdentityWire<'a> {
	identifier: Cow<'a, str>,
	version: u32,
}
impl<'a> IndexIdentityWire<'a> {
	pub(crate) fn from_identity(identity: &'a IndexContractIdentity) -> Self {
		Self {
			identifier: Cow::Borrowed(identity.identifier()),
			version: identity.version().get(),
		}
	}
	pub(crate) fn into_identity(self) -> Result<IndexContractIdentity, ScopeWireError> {
		IndexContractIdentity::new(self.identifier.into_owned(), self.version)
			.map_err(ScopeError::from)
			.map_err(Into::into)
	}
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScopeIdentityWire<'a> {
	storage: Cow<'a, [u8]>,
	namespace: Cow<'a, [u8]>,
}
impl<'a> ScopeIdentityWire<'a> {
	pub(crate) fn from_identity(identity: &'a ScopeIdentity) -> Self {
		Self {
			storage: Cow::Borrowed(identity.storage().as_bytes()),
			namespace: Cow::Borrowed(identity.namespace().as_bytes()),
		}
	}
	pub(crate) fn into_identity(self) -> Result<ScopeIdentity, ScopeWireError> {
		let namespace = RecordNamespace::new(self.namespace.into_owned())
			.map_err(|_| ScopeWireError::InvalidNamespace)?;
		let storage = StorageIdentity::new(self.storage.into_owned())
			.map_err(|_| ScopeError::EmptyStorageIdentity)?;
		Ok(ScopeIdentity::from_stored(storage, namespace))
	}
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IndexWire<'a> {
	identifier: Cow<'a, str>,
	version: u32,
	fields: Vec<FieldWire<'a>>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FieldWire<'a> {
	name: Cow<'a, str>,
	scalar_type: ScalarWire,
}
#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ScalarWire {
	Boolean,
	SignedInteger,
	Timestamp,
	BytewiseText,
	Bytes,
}
impl From<IndexScalarType> for ScalarWire {
	fn from(scalar: IndexScalarType) -> Self {
		match scalar {
			IndexScalarType::Boolean => Self::Boolean,
			IndexScalarType::SignedInteger => Self::SignedInteger,
			IndexScalarType::Timestamp => Self::Timestamp,
			IndexScalarType::BytewiseText => Self::BytewiseText,
			IndexScalarType::Bytes => Self::Bytes,
		}
	}
}
impl From<ScalarWire> for IndexScalarType {
	fn from(scalar: ScalarWire) -> Self {
		match scalar {
			ScalarWire::Boolean => Self::Boolean,
			ScalarWire::SignedInteger => Self::SignedInteger,
			ScalarWire::Timestamp => Self::Timestamp,
			ScalarWire::BytewiseText => Self::BytewiseText,
			ScalarWire::Bytes => Self::Bytes,
		}
	}
}
impl<'a> IndexWire<'a> {
	pub(crate) fn from_descriptor(descriptor: &'a IndexContractDescriptor) -> Self {
		Self {
			identifier: Cow::Borrowed(descriptor.identity().identifier()),
			version: descriptor.identity().version().get(),
			fields: descriptor
				.fields()
				.iter()
				.map(|field| FieldWire {
					name: Cow::Borrowed(field.name()),
					scalar_type: field.scalar_type().into(),
				})
				.collect(),
		}
	}
	pub(crate) fn into_descriptor(self) -> Result<IndexContractDescriptor, ScopeWireError> {
		let identity = IndexContractIdentity::new(self.identifier.into_owned(), self.version)
			.map_err(ScopeError::from)?;
		let fields = self
			.fields
			.into_iter()
			.map(|field| {
				IndexFieldDef::new(field.name.into_owned(), field.scalar_type.into())
					.map_err(ScopeError::from)
			})
			.collect::<Result<Vec<_>, _>>()?;
		Ok(IndexContractDescriptor(
			IndexContract::new(identity, fields).map_err(ScopeError::from)?,
		))
	}
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SupportedRepresentationWire<'a> {
	representation: RepresentationWire,
	indexes: Vec<IndexWire<'a>>,
}
#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RepresentationWire {
	version: u32,
	envelope: EnvelopeWire,
}
#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum EnvelopeWire {
	SourceBytes,
	NativeJson,
}
#[derive(Clone, Copy, Deserialize, Serialize)]
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
impl From<RepresentationPolicy> for RepresentationWire {
	fn from(representation: RepresentationPolicy) -> Self {
		Self {
			version: representation.version,
			envelope: match representation.envelope {
				StateStorageEnvelope::SourceBytes => EnvelopeWire::SourceBytes,
				StateStorageEnvelope::NativeJson => EnvelopeWire::NativeJson,
			},
		}
	}
}
impl From<RepresentationWire> for RepresentationPolicy {
	fn from(representation: RepresentationWire) -> Self {
		Self {
			version: representation.version,
			envelope: match representation.envelope {
				EnvelopeWire::SourceBytes => StateStorageEnvelope::SourceBytes,
				EnvelopeWire::NativeJson => StateStorageEnvelope::NativeJson,
			},
		}
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ScopeWireError {
	#[error("scope wire version {version} is unsupported")]
	UnsupportedVersion { version: u32 },
	#[error("scope namespace must not be empty")]
	InvalidNamespace,
	#[error("scope policy or support was rejected")]
	Scope(#[from] ScopeError),
}
fn require_version(version: u32) -> Result<(), ScopeWireError> {
	if version != WIRE_VERSION {
		return Err(ScopeWireError::UnsupportedVersion { version });
	}
	Ok(())
}
