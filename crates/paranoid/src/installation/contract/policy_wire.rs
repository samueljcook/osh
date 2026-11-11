//! One strict structural policy codec for active control and durable upgrade requests.
//!
//! Parsing establishes policy shape. Actual code declarations must bind a parsed policy
//! before an operation uses it; serialization supplies no runtime compatibility authority.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use super::settings::ComponentSettings;
use super::{
	ComponentPolicy, ComponentPolicyFields, RepresentationPolicy, SchemaPolicy, StateContractError,
	StateStorageEnvelope,
};
use crate::records::state_schema::NativeStateDocument;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ComponentPolicyWire<'a> {
	component: Cow<'a, str>,
	semantic_contract: Cow<'a, str>,
	schemas: Vec<SchemaPolicyWire<'a>>,
	settings: ComponentSettingsWire<'a>,
}

// External tagging is required: serde buffers an internally tagged enum before it selects the
// variant, and the document's raw JSON body cannot be decoded from that buffer.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum ComponentSettingsWire<'a> {
	Parameterless {},
	Document {
		representation_policy: SchemaPolicyWire<'a>,
		document: Cow<'a, NativeStateDocument>,
	},
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SchemaPolicyWire<'a> {
	identifier: Cow<'a, str>,
	writer: RepresentationPolicyWire,
	required_readers: Vec<RepresentationPolicyWire>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RepresentationPolicyWire {
	version: u32,
	envelope: EnvelopeWire,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum EnvelopeWire {
	SourceBytes,
	NativeJson,
}

impl<'a> ComponentPolicyWire<'a> {
	pub(crate) fn from_policy(policy: &'a ComponentPolicy) -> Self {
		Self {
			component: Cow::Borrowed(policy.component_identifier()),
			semantic_contract: Cow::Borrowed(policy.semantic_contract()),
			schemas: policy
				.schemas()
				.iter()
				.map(SchemaPolicyWire::from_policy)
				.collect(),
			settings: match policy.settings() {
				ComponentSettings::Parameterless => ComponentSettingsWire::Parameterless {},
				ComponentSettings::Document {
					representation_policy,
					document,
				} => ComponentSettingsWire::Document {
					representation_policy: SchemaPolicyWire::from_policy(representation_policy),
					document: Cow::Borrowed(document),
				},
			},
		}
	}

	pub(crate) fn into_policy(self) -> Result<ComponentPolicy, StateContractError> {
		let schemas = self
			.schemas
			.into_iter()
			.map(SchemaPolicyWire::into_policy)
			.collect::<Result<Vec<_>, _>>()?;
		ComponentPolicy::new(ComponentPolicyFields {
			component_identifier: self.component.into_owned(),
			semantic_contract: self.semantic_contract.into_owned(),
			schemas,
			settings: match self.settings {
				ComponentSettingsWire::Parameterless {} => ComponentSettings::Parameterless,
				ComponentSettingsWire::Document {
					representation_policy,
					document,
				} => ComponentSettings::Document {
					representation_policy: representation_policy.into_policy()?,
					document: document.into_owned(),
				},
			},
		})
	}
}

impl<'a> SchemaPolicyWire<'a> {
	pub(crate) fn from_policy(policy: &'a SchemaPolicy) -> Self {
		Self {
			identifier: Cow::Borrowed(policy.identifier()),
			writer: representation_wire(policy.writer()),
			required_readers: policy
				.required_readers()
				.iter()
				.copied()
				.map(representation_wire)
				.collect(),
		}
	}

	pub(crate) fn into_policy(self) -> Result<SchemaPolicy, StateContractError> {
		SchemaPolicy::new(
			self.identifier.into_owned(),
			representation_policy(self.writer),
			self.required_readers
				.into_iter()
				.map(representation_policy)
				.collect(),
		)
	}
}

fn representation_policy(wire: RepresentationPolicyWire) -> RepresentationPolicy {
	RepresentationPolicy {
		version: wire.version,
		envelope: match wire.envelope {
			EnvelopeWire::SourceBytes => StateStorageEnvelope::SourceBytes,
			EnvelopeWire::NativeJson => StateStorageEnvelope::NativeJson,
		},
	}
}

fn representation_wire(policy: RepresentationPolicy) -> RepresentationPolicyWire {
	RepresentationPolicyWire {
		version: policy.version,
		envelope: match policy.envelope {
			StateStorageEnvelope::SourceBytes => EnvelopeWire::SourceBytes,
			StateStorageEnvelope::NativeJson => EnvelopeWire::NativeJson,
		},
	}
}
