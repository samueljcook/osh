//! Shared native scalar JSON, permanent metadata, and exact bound-schema mechanics.

use std::error::Error as StdError;
use std::fmt;

use primitives_rs::encoding::decimal::parse_canonical_decimal;
use primitives_rs::serialization::json::JsonDecodeFailureSummary;
use serde::{Deserialize, Serialize};

use crate::installation::contract::{
	BoundComponentContract, BoundStateSchema, ContractStateReadError, RepresentationPolicy,
	SchemaPolicy, StateContractError, StateStorageEnvelope,
};
use crate::records::state_schema::{
	StateDecoder, StateEncoder, StateRepresentationFormat, StateSchema, StateSchemaError,
	StateVersion,
};
use crate::records::{EncodedRecord, IndexFields, RecordMetadata, StoredRecord};

/// A stored scalar state value could not be encoded or decoded.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ValueError {
	/// A stored value is not in a format that its state family's representation accepts.
	#[error("state representation rejected a value: {reason}")]
	Representation {
		/// What the representation rejected.
		reason: String,
	},
	/// Paranoid's decoder of a stored value failed.
	#[error("state representation decoding failed")]
	RepresentationDecode(#[source] RepresentationDecodeError),
}

/// The error of Paranoid's decoder of a stored value. Its `Debug` output shows no stored values.
pub(crate) struct RepresentationDecodeError {
	source: Box<dyn StdError + Send + Sync>,
}

impl RepresentationDecodeError {
	pub(crate) fn new(source: impl StdError + Send + Sync + 'static) -> Self {
		Self {
			source: Box::new(source),
		}
	}
}

impl fmt::Debug for RepresentationDecodeError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("RepresentationDecodeError { typed_cause_retained: true }")
	}
}

impl fmt::Display for RepresentationDecodeError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("state representation decoder failed")
	}
}

impl StdError for RepresentationDecodeError {
	fn source(&self) -> Option<&(dyn StdError + 'static)> {
		Some(self.source.as_ref())
	}
}

const NATIVE_REPRESENTATION_VERSION: u32 = 1;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct IntegerPayload {
	value: String,
}

pub(super) fn decode_integer(bytes: &[u8]) -> Result<i64, ValueError> {
	let payload: IntegerPayload = serde_json::from_slice(bytes).map_err(json_error)?;
	parse_integer(&payload.value)
}
pub(super) fn encode_integer(value: &i64) -> Result<Vec<u8>, ValueError> {
	serde_json::to_vec(&IntegerPayload {
		value: value.to_string(),
	})
	.map_err(json_error)
}
pub(super) fn parse_integer(text: &str) -> Result<i64, ValueError> {
	parse_canonical_decimal(text)
		.map_err(|_| invalid_value("scalar integer must be a canonical signed 64-bit decimal"))
}
pub(super) fn require_nonnegative(value: i64) -> Result<i64, ValueError> {
	if value < 0 {
		return Err(invalid_value("topic scalar must not be negative"));
	}
	Ok(value)
}
pub(super) fn decode_nonnegative_integer(bytes: &[u8]) -> Result<i64, ValueError> {
	require_nonnegative(decode_integer(bytes)?)
}
pub(super) fn encode_nonnegative_integer(value: &i64) -> Result<Vec<u8>, ValueError> {
	require_nonnegative(*value)?;
	encode_integer(value)
}
pub(super) fn invalid_value(reason: &'static str) -> ValueError {
	ValueError::Representation {
		reason: reason.to_owned(),
	}
}
pub(super) fn json_error(error: serde_json::Error) -> ValueError {
	ValueError::Representation {
		reason: JsonDecodeFailureSummary::from(&error).to_string(),
	}
}

pub(super) fn native_schema<T>(
	identifier: &'static str,
	decode: StateDecoder<T, ValueError>,
	encode: StateEncoder<T, ValueError>,
) -> StateSchema<T, ValueError> {
	StateSchema::new(crate::records::state_schema::StateSchemaFields {
		identifier,
		versions: vec![StateVersion {
			version: NATIVE_REPRESENTATION_VERSION,
			format: StateRepresentationFormat::Json,
			decoder: decode,
			encoder: Some(encode),
		}],
	})
	.expect("native scalar schema declares one positive JSON representation")
}

/// Derives the fixed native representation from the actual scalar schema owner.
pub(super) fn initial_native_schema_policy<T>(
	schema: &StateSchema<T, ValueError>,
) -> Result<SchemaPolicy, StateContractError> {
	let representation = RepresentationPolicy {
		version: NATIVE_REPRESENTATION_VERSION,
		envelope: StateStorageEnvelope::NativeJson,
	};
	SchemaPolicy::new(
		schema.identifier().to_owned(),
		representation,
		vec![representation],
	)
}

/// Checks metadata before interpreting any permanent native or scalar source body.
pub(super) fn require_permanent(metadata: RecordMetadata) -> Result<(), ScalarStateError> {
	if metadata.expires_at.is_some() {
		return Err(ScalarStateError::PermanentMetadataRequired);
	}
	Ok(())
}

/// The actual primitive transition and schema instance are checked before this is constructed.
pub(crate) struct ScalarSchemaBinding<'a, T> {
	schema: BoundStateSchema<'a, T, ValueError>,
}
pub(super) fn bind_native_scalar<'a, T, C>(
	component: &'a C,
	contract: &'a BoundComponentContract<'_, '_, C>,
	schema: &'a StateSchema<T, ValueError>,
) -> Result<ScalarSchemaBinding<'a, T>, ScalarStateError> {
	if !std::ptr::eq(component, contract.transitions()) {
		return Err(ScalarStateError::TransitionInstanceMismatch);
	}
	let schema = contract.bind_schema(schema)?;
	if schema.policy().writer().envelope != StateStorageEnvelope::NativeJson {
		return Err(ScalarStateError::NativeWriterRequired);
	}
	Ok(ScalarSchemaBinding { schema })
}
impl<T> ScalarSchemaBinding<'_, T> {
	pub(crate) fn decode_record(&self, record: &StoredRecord) -> Result<T, ScalarStateError> {
		require_permanent(record.metadata())?;
		self.schema
			.decode_document(record.body())
			.map(|(_, value)| value)
			.map_err(ScalarStateError::Read)
	}
	pub(crate) fn encode_record(
		&self,
		value: &T,
		metadata: RecordMetadata,
	) -> Result<EncodedRecord, ScalarStateError> {
		require_permanent(metadata)?;
		let body = self.schema.encode(value)?;
		EncodedRecord::derive(body, metadata, |body, actual_metadata| {
			require_permanent(actual_metadata)?;
			self.schema.decode_document(body)?;
			Ok(IndexFields::new())
		})
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ScalarStateError {
	#[error("native scalar contract does not bind its declaration")]
	Contract(#[from] StateContractError),
	#[error("native scalar transition belongs to another component instance")]
	TransitionInstanceMismatch,
	#[error("native scalar storage requires its explicitly selected native JSON writer")]
	NativeWriterRequired,
	#[error("permanent scalar state must not have an outer expiration")]
	PermanentMetadataRequired,
	#[error("native scalar is not readable under the installed policy")]
	Read(#[from] ContractStateReadError<ValueError>),
	#[error("native scalar cannot encode the selected representation")]
	Encode(#[from] StateSchemaError<ValueError>),
	#[error("source scalar value is not supported by its selected family reader")]
	Source(#[source] ValueError),
	#[error("source scalar import belongs to another component or semantic declaration")]
	SourceBindingMismatch,
	#[error("scalar arithmetic overflowed the signed 64-bit domain")]
	ArithmeticOverflow,
	#[error("scalar input is outside its primitive domain")]
	Domain(#[source] ValueError),
}
