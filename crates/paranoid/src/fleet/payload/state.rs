//! Explicit opaque, assigned-source and native-document payload transport.
//!
//! Parsing an encoding tag supplies no application reader capability or storage permission.

use std::{borrow::Cow, fmt};

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::installation::contract::role_binding::BoundSchemaReader;
use crate::installation::contract::{
	BoundStateSchema, ContractStateReadError, RepresentationPolicy, StateStorageEnvelope,
};
use crate::records::state_schema::{NativeDocumentError, NativeStateDocument, StateSchemaError};
use crate::scope::{PrimitivePayloadPolicy, ScopeError};
use primitives_rs::encoding::binary_to_text::{ByteEncoding, DecodeError, DecodeOptions};

enum FleetPayloadBody {
	OpaqueJson(Box<RawValue>),
	OpaqueBytes(Vec<u8>),
	SourceSchema {
		identifier: String,
		version: u32,
		bytes: Vec<u8>,
	},
	NativeSchemaDocument(NativeStateDocument),
}

pub(crate) struct FleetPayload {
	body: FleetPayloadBody,
}
pub(crate) struct SourceFleetPayloadFields {
	pub(crate) identifier: String,
	pub(crate) version: u32,
	pub(crate) bytes: Vec<u8>,
}

pub(crate) enum FleetPayloadRef<'a> {
	OpaqueJson(&'a RawValue),
	OpaqueBytes(&'a [u8]),
	SourceSchema {
		identifier: &'a str,
		version: u32,
		bytes: &'a [u8],
	},
	NativeSchemaDocument(&'a NativeStateDocument),
}
impl FleetPayload {
	pub(crate) fn opaque_json(value: Box<RawValue>) -> Self {
		Self {
			body: FleetPayloadBody::OpaqueJson(value),
		}
	}
	pub(crate) fn opaque_bytes(bytes: Vec<u8>) -> Self {
		Self {
			body: FleetPayloadBody::OpaqueBytes(bytes),
		}
	}
	pub(crate) fn source_schema(
		fields: SourceFleetPayloadFields,
	) -> Result<Self, FleetPayloadError> {
		if fields.identifier.is_empty() {
			return Err(FleetPayloadError::EmptySchema);
		}
		if fields.version == 0 {
			return Err(FleetPayloadError::VersionIsZero);
		}
		Ok(Self {
			body: FleetPayloadBody::SourceSchema {
				identifier: fields.identifier,
				version: fields.version,
				bytes: fields.bytes,
			},
		})
	}
	pub(crate) fn native_document(document: NativeStateDocument) -> Self {
		Self {
			body: FleetPayloadBody::NativeSchemaDocument(document),
		}
	}
	pub(crate) fn as_ref(&self) -> FleetPayloadRef<'_> {
		match &self.body {
			FleetPayloadBody::OpaqueJson(value) => FleetPayloadRef::OpaqueJson(value),
			FleetPayloadBody::OpaqueBytes(bytes) => FleetPayloadRef::OpaqueBytes(bytes),
			FleetPayloadBody::SourceSchema {
				identifier,
				version,
				bytes,
			} => FleetPayloadRef::SourceSchema {
				identifier,
				version: *version,
				bytes,
			},
			FleetPayloadBody::NativeSchemaDocument(document) => {
				FleetPayloadRef::NativeSchemaDocument(document)
			}
		}
	}
	pub(crate) fn matches_payload(&self, other: &Self) -> bool {
		match (self.as_ref(), other.as_ref()) {
			(FleetPayloadRef::OpaqueJson(a), FleetPayloadRef::OpaqueJson(b)) => a.get() == b.get(),
			(FleetPayloadRef::OpaqueBytes(a), FleetPayloadRef::OpaqueBytes(b)) => a == b,
			(
				FleetPayloadRef::SourceSchema {
					identifier: a,
					version: av,
					bytes: ab,
				},
				FleetPayloadRef::SourceSchema {
					identifier: b,
					version: bv,
					bytes: bb,
				},
			) => a == b && av == bv && ab == bb,
			(
				FleetPayloadRef::NativeSchemaDocument(a),
				FleetPayloadRef::NativeSchemaDocument(b),
			) => a.matches_document(b),
			_ => false,
		}
	}
}
impl fmt::Debug for FleetPayload {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self.body {
			FleetPayloadBody::OpaqueJson(_) => "FleetPayload::OpaqueJson { .. }",
			FleetPayloadBody::OpaqueBytes(_) => "FleetPayload::OpaqueBytes { .. }",
			FleetPayloadBody::SourceSchema { .. } => "FleetPayload::SourceSchema { .. }",
			FleetPayloadBody::NativeSchemaDocument(_) => {
				"FleetPayload::NativeSchemaDocument { .. }"
			}
		})
	}
}

#[derive(Deserialize, Serialize)]
#[serde(
	rename_all = "snake_case",
	deny_unknown_fields,
	bound(deserialize = "'de: 'a")
)]
pub(crate) enum FleetPayloadWire<'a> {
	OpaqueJson {
		#[serde(borrow)]
		value: &'a RawValue,
	},
	OpaqueBytes {
		bytes: Cow<'a, str>,
	},
	SourceSchema {
		identifier: Cow<'a, str>,
		version: u32,
		bytes: Cow<'a, str>,
	},
	NativeSchemaDocument {
		document: Cow<'a, NativeStateDocument>,
	},
}
impl<'a> FleetPayloadWire<'a> {
	pub(crate) fn from_payload(payload: &'a FleetPayload) -> Self {
		match payload.as_ref() {
			FleetPayloadRef::OpaqueJson(value) => Self::OpaqueJson { value },
			FleetPayloadRef::OpaqueBytes(bytes) => Self::OpaqueBytes {
				bytes: Cow::Owned(ByteEncoding::HexLower.encode(bytes)),
			},
			FleetPayloadRef::SourceSchema {
				identifier,
				version,
				bytes,
			} => Self::SourceSchema {
				identifier: Cow::Borrowed(identifier),
				version,
				bytes: Cow::Owned(ByteEncoding::HexLower.encode(bytes)),
			},
			FleetPayloadRef::NativeSchemaDocument(document) => Self::NativeSchemaDocument {
				document: Cow::Borrowed(document),
			},
		}
	}
	pub(crate) fn into_payload(self) -> Result<FleetPayload, FleetPayloadError> {
		match self {
			Self::OpaqueJson { value } => Ok(FleetPayload::opaque_json(value.to_owned())),
			Self::OpaqueBytes { bytes } => {
				Ok(FleetPayload::opaque_bytes(ByteEncoding::HexLower.decode(
					&bytes,
					DecodeOptions {
						max_decoded_size: bytes.len() / 2,
					},
				)?))
			}
			Self::SourceSchema {
				identifier,
				version,
				bytes,
			} => FleetPayload::source_schema(SourceFleetPayloadFields {
				identifier: identifier.into_owned(),
				version,
				bytes: ByteEncoding::HexLower.decode(
					&bytes,
					DecodeOptions {
						max_decoded_size: bytes.len() / 2,
					},
				)?,
			}),
			Self::NativeSchemaDocument { document } => {
				Ok(FleetPayload::native_document(document.into_owned()))
			}
		}
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum FleetPayloadError {
	#[error("source payload schema identifier must not be empty")]
	EmptySchema,
	#[error("source payload representation version must be positive")]
	VersionIsZero,
	#[error("payload byte transport is not canonical")]
	Bytes(#[from] DecodeError),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum FleetPayloadPolicyError {
	#[error("the installed primitive policy does not allow an embedded payload")]
	PayloadNotAllowed,
	#[error("the installed opaque payload policy requires an opaque transport")]
	OpaqueRequired,
	#[error("the installed structured payload policy requires an assigned schema transport")]
	SchemaRequired,
	#[error("the payload identity or representation differs from its installed policy")]
	Policy(#[from] ScopeError),
}

/// Checks structural transport meaning without invoking an application decoder.
pub(crate) fn require_payload_policy(
	payload: &FleetPayload,
	policy: &PrimitivePayloadPolicy,
) -> Result<(), FleetPayloadPolicyError> {
	match (policy, payload.as_ref()) {
		(PrimitivePayloadPolicy::None, _) => Err(FleetPayloadPolicyError::PayloadNotAllowed),
		(
			PrimitivePayloadPolicy::Opaque,
			FleetPayloadRef::OpaqueJson(_) | FleetPayloadRef::OpaqueBytes(_),
		) => Ok(()),
		(PrimitivePayloadPolicy::Opaque, _) => Err(FleetPayloadPolicyError::OpaqueRequired),
		(
			PrimitivePayloadPolicy::Schema(_),
			FleetPayloadRef::SourceSchema {
				identifier,
				version,
				bytes: _,
			},
		) => {
			policy.require_representation(
				identifier,
				RepresentationPolicy {
					version,
					envelope: StateStorageEnvelope::SourceBytes,
				},
			)?;
			Ok(())
		}
		(PrimitivePayloadPolicy::Schema(_), FleetPayloadRef::NativeSchemaDocument(document)) => {
			policy.require_representation(
				document.schema_identifier(),
				RepresentationPolicy {
					version: document.version(),
					envelope: StateStorageEnvelope::NativeJson,
				},
			)?;
			Ok(())
		}
		(PrimitivePayloadPolicy::Schema(_), _) => Err(FleetPayloadPolicyError::SchemaRequired),
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum FleetPayloadStateError<E> {
	#[error("opaque Fleet payload has no assigned typed interpretation")]
	UnassignedSchema,
	#[error("source Fleet payload belongs to another bound application schema")]
	SourceSchemaMismatch,
	#[error("Fleet payload decoding failed under the bound reader")]
	Decode(#[source] ContractStateReadError<E>),
	#[error("Fleet payload encoding failed under the selected writer")]
	Encode(#[source] StateSchemaError<E>),
	#[error("selected Fleet payload writer did not produce a strict native document")]
	NativeDocument(#[from] NativeDocumentError),
}

pub(crate) fn decode_payload<T, E>(
	payload: &FleetPayload,
	reader: &BoundSchemaReader<'_, T, E>,
) -> Result<T, FleetPayloadStateError<E>> {
	match payload.as_ref() {
		FleetPayloadRef::OpaqueJson(_) | FleetPayloadRef::OpaqueBytes(_) => {
			Err(FleetPayloadStateError::UnassignedSchema)
		}
		FleetPayloadRef::SourceSchema {
			identifier,
			version,
			bytes,
		} => {
			if identifier != reader.policy().identifier() {
				return Err(FleetPayloadStateError::SourceSchemaMismatch);
			}
			reader
				.decode_source(version, bytes)
				.map_err(FleetPayloadStateError::Decode)
		}
		FleetPayloadRef::NativeSchemaDocument(document) => reader
			.decode_document(document.as_bytes())
			.map(|(_, value)| value)
			.map_err(FleetPayloadStateError::Decode),
	}
}

pub(crate) fn encode_payload<T, E>(
	value: &T,
	writer: &BoundStateSchema<'_, T, E>,
) -> Result<FleetPayload, FleetPayloadStateError<E>> {
	let bytes = writer
		.encode(value)
		.map_err(FleetPayloadStateError::Encode)?;
	match writer.policy().writer().envelope {
		StateStorageEnvelope::SourceBytes => Ok(FleetPayload {
			body: FleetPayloadBody::SourceSchema {
				identifier: writer.policy().identifier().to_owned(),
				version: writer.policy().writer().version,
				bytes,
			},
		}),
		StateStorageEnvelope::NativeJson => Ok(FleetPayload::native_document(
			NativeStateDocument::from_encoded(bytes)?,
		)),
	}
}
