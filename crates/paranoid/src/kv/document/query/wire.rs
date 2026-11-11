//! Strict contextual cursor encoding; parsed positions provide no query or record authority.

use std::borrow::Cow;

use primitives_rs::encoding::decimal::parse_canonical_decimal;
use primitives_rs::serialization::json::JsonDecodeFailureSummary;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use super::{
	Bound, DocumentQueryCursor, DocumentQueryDirection, DocumentQueryInnerError,
	DocumentQueryRange, IndexScalar, QueryPredicate, RecordKey, ScopedDocumentIndexQuery,
};
use crate::records::StateTimestampMicros;
use crate::scope::wire::{IndexWire, ScopeIdentityWire};

const VERSION: u32 = 1;

#[derive(Deserialize)]
struct VersionHeader {
	version: u32,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum CursorKind {
	DocumentIndexPosition,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ValueWire<'a> {
	Boolean { value: bool },
	SignedInteger { decimal: String },
	Timestamp { micros: String },
	BytewiseText { value: Cow<'a, str> },
	Bytes { value: Cow<'a, [u8]> },
}

fn parse_decimal(decimal: &str) -> Result<i64, DocumentQueryInnerError> {
	parse_canonical_decimal(decimal).map_err(|_| DocumentQueryInnerError::InvalidCursorDecimal)
}

impl<'a> ValueWire<'a> {
	fn from_value(value: &'a IndexScalar) -> Self {
		match value {
			IndexScalar::Boolean(value) => Self::Boolean { value: *value },
			IndexScalar::SignedInteger(value) => Self::SignedInteger {
				decimal: value.to_string(),
			},
			IndexScalar::Timestamp(value) => Self::Timestamp {
				micros: value.as_i64().to_string(),
			},
			IndexScalar::BytewiseText(value) => Self::BytewiseText {
				value: Cow::Borrowed(value),
			},
			IndexScalar::Bytes(value) => Self::Bytes {
				value: Cow::Borrowed(value),
			},
		}
	}
	fn into_value(self) -> Result<IndexScalar, DocumentQueryInnerError> {
		Ok(match self {
			Self::Boolean { value } => IndexScalar::Boolean(value),
			Self::SignedInteger { decimal } => IndexScalar::SignedInteger(parse_decimal(&decimal)?),
			Self::Timestamp { micros } => {
				IndexScalar::Timestamp(StateTimestampMicros::new(parse_decimal(&micros)?))
			}
			Self::BytewiseText { value } => IndexScalar::BytewiseText(value.into_owned()),
			Self::Bytes { value } => IndexScalar::Bytes(value.into_owned()),
		})
	}
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum BoundWire<'a> {
	Unbounded {},
	Included { value: ValueWire<'a> },
	Excluded { value: ValueWire<'a> },
}
impl<'a> BoundWire<'a> {
	fn from_bound(bound: &'a Bound<IndexScalar>) -> Self {
		match bound {
			Bound::Unbounded => Self::Unbounded {},
			Bound::Included(value) => Self::Included {
				value: ValueWire::from_value(value),
			},
			Bound::Excluded(value) => Self::Excluded {
				value: ValueWire::from_value(value),
			},
		}
	}
	fn into_bound(self) -> Result<Bound<IndexScalar>, DocumentQueryInnerError> {
		Ok(match self {
			Self::Unbounded {} => Bound::Unbounded,
			Self::Included { value } => Bound::Included(value.into_value()?),
			Self::Excluded { value } => Bound::Excluded(value.into_value()?),
		})
	}
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RangeWire<'a> {
	lower: BoundWire<'a>,
	upper: BoundWire<'a>,
}

/// Explicit None avoids omission/null becoming an undeclared predicate default.
#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum RangeChoiceWire<'a> {
	None {},
	Range { bounds: RangeWire<'a> },
}
impl<'a> RangeChoiceWire<'a> {
	fn from_range(range: &'a Option<DocumentQueryRange>) -> Self {
		match range {
			None => Self::None {},
			Some(range) => Self::Range {
				bounds: RangeWire {
					lower: BoundWire::from_bound(&range.lower),
					upper: BoundWire::from_bound(&range.upper),
				},
			},
		}
	}
	fn into_range(self) -> Result<Option<DocumentQueryRange>, DocumentQueryInnerError> {
		Ok(match self {
			Self::None {} => None,
			Self::Range { bounds } => Some(DocumentQueryRange {
				lower: bounds.lower.into_bound()?,
				upper: bounds.upper.into_bound()?,
			}),
		})
	}
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PredicateWire<'a> {
	equality_prefix: Vec<ValueWire<'a>>,
	range: RangeChoiceWire<'a>,
	direction: DocumentQueryDirection,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CursorWire<'a> {
	kind: CursorKind,
	version: u32,
	scope: ScopeIdentityWire<'a>,
	schema: Cow<'a, str>,
	index: IndexWire<'a>,
	predicate: PredicateWire<'a>,
	values: Vec<ValueWire<'a>>,
	key: Cow<'a, [u8]>,
}

fn json_error(error: serde_json::Error) -> DocumentQueryInnerError {
	let summary = JsonDecodeFailureSummary::from_serde_error(&error);
	DocumentQueryInnerError::CursorJson {
		category: summary.category(),
		line: summary.line(),
		column: summary.column(),
	}
}

pub(super) fn encode(
	cursor: &DocumentQueryCursor,
) -> Result<Box<RawValue>, DocumentQueryInnerError> {
	serde_json::value::to_raw_value(&CursorWire {
		kind: CursorKind::DocumentIndexPosition,
		version: VERSION,
		scope: ScopeIdentityWire::from_identity(&cursor.scope),
		schema: Cow::Borrowed(cursor.schema_identifier),
		index: IndexWire::from_descriptor(&cursor.index),
		predicate: PredicateWire {
			equality_prefix: cursor
				.predicate
				.equality_prefix
				.iter()
				.map(ValueWire::from_value)
				.collect(),
			range: RangeChoiceWire::from_range(&cursor.predicate.range),
			direction: cursor.predicate.direction,
		},
		values: cursor.values.iter().map(ValueWire::from_value).collect(),
		key: Cow::Borrowed(cursor.key.as_bytes()),
	})
	.map_err(json_error)
}

/// The actual query supplies the schema reference only after exact stored context comparison.
pub(super) fn decode<T, C, P>(
	query: &ScopedDocumentIndexQuery<'_, '_, '_, T, C, P>,
	cursor_json: &RawValue,
) -> Result<DocumentQueryCursor, DocumentQueryInnerError> {
	let header: VersionHeader = serde_json::from_str(cursor_json.get()).map_err(json_error)?;
	if header.version != VERSION {
		return Err(DocumentQueryInnerError::UnsupportedCursorVersion {
			version: header.version,
		});
	}
	let wire: CursorWire<'static> = serde_json::from_str(cursor_json.get()).map_err(json_error)?;
	if wire.version != VERSION {
		return Err(DocumentQueryInnerError::UnsupportedCursorVersion {
			version: wire.version,
		});
	}
	let schema = query.declaration.document().schema().identifier();
	if wire.schema != schema {
		return Err(DocumentQueryInnerError::CursorContextMismatch);
	}
	let cursor = DocumentQueryCursor {
		scope: wire.scope.into_identity()?,
		schema_identifier: schema,
		index: wire.index.into_descriptor()?,
		predicate: QueryPredicate {
			equality_prefix: wire
				.predicate
				.equality_prefix
				.into_iter()
				.map(ValueWire::into_value)
				.collect::<Result<_, _>>()?,
			range: wire.predicate.range.into_range()?,
			direction: wire.predicate.direction,
		},
		values: wire
			.values
			.into_iter()
			.map(ValueWire::into_value)
			.collect::<Result<_, _>>()?,
		key: RecordKey::new(wire.key.into_owned()),
	};
	query.require_cursor_context(&cursor)?;
	Ok(cursor)
}
