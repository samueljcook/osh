//! Canonical PostgreSQL projection wire encoding for native records.
//!
//! Encoded names and text remain lowercase byte hex, including embedded UTF-8 NUL.

use std::collections::BTreeMap;

use primitives_rs::encoding::binary_to_text::{ByteEncoding, DecodeError, DecodeOptions};
use serde::{Deserialize, Serialize};

use crate::records::{IndexFields, IndexScalar, StateTimestampMicros};

#[derive(Deserialize, Serialize)]
#[serde(
	tag = "kind",
	content = "value",
	rename_all = "snake_case",
	deny_unknown_fields
)]
enum ProjectionWire {
	Boolean(bool),
	SignedInteger(i64),
	Timestamp(i64),
	BytewiseText(String),
	Bytes(String),
}

/// Checks true coverage using the strict projection scalar wire owner.
pub(crate) fn is_true_coverage_json(encoded: &str) -> bool {
	matches!(
		serde_json::from_str::<ProjectionWire>(encoded),
		Ok(ProjectionWire::Boolean(true))
	)
}

pub(crate) fn encode_index_fields(fields: &IndexFields) -> String {
	let encoded = fields
		.iter()
		.map(|(name, scalar)| {
			let scalar = match scalar {
				IndexScalar::Boolean(value) => ProjectionWire::Boolean(*value),
				IndexScalar::SignedInteger(value) => ProjectionWire::SignedInteger(*value),
				IndexScalar::Timestamp(value) => ProjectionWire::Timestamp(value.as_i64()),
				IndexScalar::BytewiseText(value) => {
					ProjectionWire::BytewiseText(ByteEncoding::HexLower.encode(value.as_bytes()))
				}
				IndexScalar::Bytes(value) => {
					ProjectionWire::Bytes(ByteEncoding::HexLower.encode(value))
				}
			};
			(ByteEncoding::HexLower.encode(name.as_bytes()), scalar)
		})
		.collect::<BTreeMap<_, _>>();
	serde_json::to_string(&encoded)
		.expect("owned projection scalars and hexadecimal strings serialize as JSON")
}

pub(super) fn decode_index_fields(encoded: &str) -> Result<IndexFields, &'static str> {
	let fields: BTreeMap<String, ProjectionWire> = serde_json::from_str(encoded)
		.map_err(|_| "index map does not contain the declared scalar wire types")?;
	fields
		.into_iter()
		.map(|(name, scalar)| {
			let name = String::from_utf8(decode_hex(&name)?)
				.map_err(|_| "index field name is not UTF-8")?;
			let scalar = match scalar {
				ProjectionWire::Boolean(value) => IndexScalar::Boolean(value),
				ProjectionWire::SignedInteger(value) => IndexScalar::SignedInteger(value),
				ProjectionWire::Timestamp(value) => {
					IndexScalar::Timestamp(StateTimestampMicros::new(value))
				}
				ProjectionWire::BytewiseText(value) => IndexScalar::BytewiseText(
					String::from_utf8(decode_hex(&value)?)
						.map_err(|_| "index text is not UTF-8")?,
				),
				ProjectionWire::Bytes(value) => IndexScalar::Bytes(decode_hex(&value)?),
			};
			Ok((name, scalar))
		})
		.collect()
}

fn decode_hex(encoded: &str) -> Result<Vec<u8>, &'static str> {
	ByteEncoding::HexLower
		.decode(
			encoded,
			DecodeOptions {
				max_decoded_size: usize::MAX,
			},
		)
		.map_err(|error| match error {
			DecodeError::InvalidLength => "index bytes have an incomplete hexadecimal pair",
			DecodeError::InvalidSymbol { .. }
			| DecodeError::NonZeroTrailingBits { .. }
			| DecodeError::InvalidPadding => "index bytes do not use canonical lowercase hexadecimal",
			DecodeError::OutputTooSmall | DecodeError::AllocationFailed => {
				"index bytes exceed maximum decoded size"
			}
		})
}
