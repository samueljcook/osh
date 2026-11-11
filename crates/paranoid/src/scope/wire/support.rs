//! Actual document role support including separately selected query descriptors.

use primitives_rs::serialization::json::JsonDecodeFailureSummary;
use serde_json::value::RawValue;

use super::{
	Cow, Deserialize, DocumentRoleSupport, DocumentRoleWire, IndexWire, ScopeIdentityWire,
	ScopeWireError, Serialize, SupportedRepresentation, SupportedRepresentationWire, VersionHeader,
	WIRE_VERSION, require_version,
};

pub(crate) enum DocumentRoleSupportWire<'a> {
	Current(SupportWire<'a>),
	Unsupported(u32),
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SupportWire<'a> {
	version: u32,
	scope: ScopeIdentityWire<'a>,
	schema: Cow<'a, str>,
	role: DocumentRoleWire,
	representations: Vec<SupportedRepresentationWire<'a>>,
	query_indexes: Vec<IndexWire<'a>>,
}
impl<'a> DocumentRoleSupportWire<'a> {
	pub(crate) fn from_support(support: &'a DocumentRoleSupport) -> Self {
		Self::Current(SupportWire {
			version: WIRE_VERSION,
			scope: ScopeIdentityWire::from_identity(support.identity()),
			schema: Cow::Borrowed(&support.schema_identifier),
			role: support.role.into(),
			representations: support
				.representations
				.iter()
				.map(|supported| SupportedRepresentationWire {
					representation: supported.representation.into(),
					indexes: supported
						.indexes
						.iter()
						.map(IndexWire::from_descriptor)
						.collect(),
				})
				.collect(),
			query_indexes: support
				.query_indexes()
				.iter()
				.map(IndexWire::from_descriptor)
				.collect(),
		})
	}
	pub(crate) fn into_support(self) -> Result<DocumentRoleSupport, ScopeWireError> {
		match self {
			Self::Unsupported(version) => Err(ScopeWireError::UnsupportedVersion { version }),
			Self::Current(wire) => {
				require_version(wire.version)?;
				Ok(DocumentRoleSupport::parse_parts(
					wire.scope.into_identity()?,
					wire.schema.into_owned(),
					wire.role.into(),
					wire.representations
						.into_iter()
						.map(|supported| {
							Ok(SupportedRepresentation {
								representation: supported.representation.into(),
								indexes: supported
									.indexes
									.into_iter()
									.map(IndexWire::into_descriptor)
									.collect::<Result<Vec<_>, ScopeWireError>>()?,
							})
						})
						.collect::<Result<Vec<_>, ScopeWireError>>()?,
					wire.query_indexes
						.into_iter()
						.map(IndexWire::into_descriptor)
						.collect::<Result<Vec<_>, _>>()?,
				)?)
			}
		}
	}
}
impl<'de, 'a> Deserialize<'de> for DocumentRoleSupportWire<'a> {
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
impl Serialize for DocumentRoleSupportWire<'_> {
	fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		match self {
			Self::Current(wire) => wire.serialize(serializer),
			Self::Unsupported(_) => Err(serde::ser::Error::custom(
				"unsupported document support version",
			)),
		}
	}
}
