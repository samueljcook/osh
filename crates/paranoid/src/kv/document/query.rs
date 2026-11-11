//! Document index queries and their cursors.
//!
//! A query is checked against the index's declared fields when it is built. Whether the
//! runtime may run it is checked when it runs.

use std::{cmp::Ordering, fmt, num::NonZeroU32, ops::Bound};

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::kv::MAX_PAGE_LIMIT;
use crate::kv::document::schema::{IndexContract, IndexContractIdentity};
use crate::records::{IndexScalar, RecordKey};
use crate::scope::{DocumentScopeDeclaration, IndexContractDescriptor, ScopeIdentity};

mod error;
mod wire;
pub use error::{DocumentQueryError, DocumentQueryErrorKind};

/// The order of query results: by the index's field values, and then by key.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentQueryDirection {
	/// Ascending order.
	Ascending,
	/// Descending order.
	Descending,
}

/// A range on the first index field after the equality prefix.
#[derive(Clone, Eq, PartialEq)]
pub struct DocumentQueryRange {
	/// The lower bound.
	pub lower: Bound<IndexScalar>,
	/// The upper bound.
	pub upper: Bound<IndexScalar>,
}
impl fmt::Debug for DocumentQueryRange {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("DocumentQueryRange { .. }")
	}
}

#[derive(Clone, Eq, PartialEq)]
struct QueryPredicate {
	equality_prefix: Vec<IndexScalar>,
	range: Option<DocumentQueryRange>,
	direction: DocumentQueryDirection,
}

/// The position after the last document of a query page. A cursor belongs to one scope,
/// schema, index, and predicate, and a query of any other combination rejects it.
pub struct DocumentQueryCursor {
	scope: ScopeIdentity,
	schema_identifier: &'static str,
	index: IndexContractDescriptor,
	predicate: QueryPredicate,
	values: Vec<IndexScalar>,
	key: RecordKey,
}
impl DocumentQueryCursor {
	pub(crate) fn values(&self) -> &[IndexScalar] {
		&self.values
	}
	pub(crate) fn key(&self) -> &RecordKey {
		&self.key
	}
	/// Encodes the cursor as JSON so that the app can store it and pass it to
	/// `ScopedDocumentIndexQuery::resume` later.
	pub fn encode_json(&self) -> Result<Box<RawValue>, DocumentQueryError> {
		wire::encode(self).map_err(|source| {
			DocumentQueryError::new(error::QueryErrorFields {
				scope: &self.scope,
				schema_identifier: self.schema_identifier,
				index: self.index.identity(),
				source,
			})
		})
	}
}
impl fmt::Debug for DocumentQueryCursor {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("DocumentQueryCursor")
			.field("value_count", &self.values.len())
			.finish_non_exhaustive()
	}
}

/// The predicate, the order, and the page size of a query.
pub struct DocumentQuerySelectionFields {
	/// The values that the index's first fields must equal, in the index's field order.
	pub equality_prefix: Vec<IndexScalar>,
	/// A range on the first field after the equality prefix, or `None`.
	pub range: Option<DocumentQueryRange>,
	/// The order of the results.
	pub direction: DocumentQueryDirection,
	/// The largest number of documents on a page, at most [`MAX_PAGE_LIMIT`].
	pub limit: NonZeroU32,
	/// The cursor of the previous page of the same query, or `None` for the first page.
	pub cursor: Option<DocumentQueryCursor>,
}

pub(in crate::kv) struct ScopedDocumentIndexQueryFields<'a, 's, 'd, T, C, P> {
	pub(in crate::kv) declaration: &'a DocumentScopeDeclaration<'s, 'd, T, C, P>,
	pub(in crate::kv) index: &'a IndexContract,
	pub(in crate::kv) selection: DocumentQuerySelectionFields,
}

/// The inputs of a document index query.
pub struct DocumentQueryFields {
	/// The queried index, which the scope's document code declares.
	pub index: IndexContractIdentity,
	/// The predicate, the order, and the page size.
	pub selection: DocumentQuerySelectionFields,
}

pub(in crate::kv) struct DocumentQueryPosition<'a> {
	pub(in crate::kv) values: &'a [IndexScalar],
	pub(in crate::kv) key: &'a RecordKey,
}
pub(in crate::kv) struct DocumentQueryPositionComparison<'a> {
	pub(in crate::kv) left: DocumentQueryPosition<'a>,
	pub(in crate::kv) right: DocumentQueryPosition<'a>,
}

#[derive(Debug, thiserror::Error)]
pub(in crate::kv) enum DocumentQueryInnerError {
	#[error("the document code declares no index with this identity")]
	DifferentIndexDeclaration,
	#[error("document query equality prefix exceeds the declared index fields")]
	EqualityPrefixTooLong,
	#[error("document query limit is {actual}, maximum is {max}")]
	LimitTooLarge { actual: u32, max: u32 },
	#[error("document query range has no following declared field")]
	RangeFieldAbsent,
	#[error("document query field {field_index} has another scalar type")]
	ScalarTypeMismatch { field_index: usize },
	#[error("document query position has another tuple length")]
	TupleLengthMismatch,
	#[error("document query continuation belongs to another query context")]
	CursorContextMismatch,
	#[error("document query continuation position is outside the selected predicate")]
	PositionOutsideSelection,
	#[error("document query cursor representation version {version} is unsupported")]
	UnsupportedCursorVersion { version: u32 },
	#[error("document query cursor integer must be a canonical signed decimal i64")]
	InvalidCursorDecimal,
	#[error("document query cursor scope or index shape is invalid")]
	CursorScope(#[from] crate::scope::wire::ScopeWireError),
	#[error(
		"document query cursor JSON is invalid: {category:?} error at line {line}, column {column}"
	)]
	CursorJson {
		category: primitives_rs::serialization::json::JsonFailureCategory,
		line: usize,
		column: usize,
	},
}

/// A checked query of one index of a document scope. `PgDocument::query` builds it.
pub struct ScopedDocumentIndexQuery<'a, 's, 'd, T, C, P> {
	declaration: &'a DocumentScopeDeclaration<'s, 'd, T, C, P>,
	index: &'a IndexContract,
	predicate: QueryPredicate,
	limit: NonZeroU32,
	cursor: Option<DocumentQueryCursor>,
}

/// Compares only parsed values of the same declared scalar type.
fn compare_same_type((left, right): (&IndexScalar, &IndexScalar)) -> Ordering {
	match (left, right) {
		(IndexScalar::Boolean(a), IndexScalar::Boolean(b)) => a.cmp(b),
		(IndexScalar::SignedInteger(a), IndexScalar::SignedInteger(b)) => a.cmp(b),
		(IndexScalar::Timestamp(a), IndexScalar::Timestamp(b)) => a.cmp(b),
		(IndexScalar::BytewiseText(a), IndexScalar::BytewiseText(b)) => {
			a.as_bytes().cmp(b.as_bytes())
		}
		(IndexScalar::Bytes(a), IndexScalar::Bytes(b)) => a.cmp(b),
		_ => unreachable!("query values were parsed against one immutable scalar definition"),
	}
}

fn range_contains(range: &DocumentQueryRange, value: &IndexScalar) -> bool {
	let lower = match &range.lower {
		Bound::Unbounded => true,
		Bound::Included(lower) => compare_same_type((value, lower)) != Ordering::Less,
		Bound::Excluded(lower) => compare_same_type((value, lower)) == Ordering::Greater,
	};
	let upper = match &range.upper {
		Bound::Unbounded => true,
		Bound::Included(upper) => compare_same_type((value, upper)) != Ordering::Greater,
		Bound::Excluded(upper) => compare_same_type((value, upper)) == Ordering::Less,
	};
	lower && upper
}

fn check_value(
	index: &IndexContract,
	field_index: usize,
	value: &IndexScalar,
) -> Result<(), DocumentQueryInnerError> {
	if index.fields()[field_index].scalar_type().matches(value) {
		Ok(())
	} else {
		Err(DocumentQueryInnerError::ScalarTypeMismatch { field_index })
	}
}

impl<'a, 's, 'd, T, C, P> ScopedDocumentIndexQuery<'a, 's, 'd, T, C, P> {
	/// Builds the query for `fields.index` of `declaration`'s document code.
	pub(in crate::kv) fn new_for_index(
		declaration: &'a DocumentScopeDeclaration<'s, 'd, T, C, P>,
		fields: DocumentQueryFields,
	) -> Result<Self, DocumentQueryError> {
		let Some(index) = declaration
			.document()
			.indexes()
			.iter()
			.find(|index| index.identity() == &fields.index)
			.copied()
		else {
			return Err(DocumentQueryError::new(error::QueryErrorFields {
				scope: declaration.identity(),
				schema_identifier: declaration.document().schema().identifier(),
				index: &fields.index,
				source: DocumentQueryInnerError::DifferentIndexDeclaration,
			}));
		};
		Self::new(ScopedDocumentIndexQueryFields {
			declaration,
			index,
			selection: fields.selection,
		})
	}
	/// Parses an actual declared index and typed choices without IO or role activation.
	pub(in crate::kv) fn new(
		fields: ScopedDocumentIndexQueryFields<'a, 's, 'd, T, C, P>,
	) -> Result<Self, DocumentQueryError> {
		let declaration = fields.declaration;
		let index = fields.index;
		Self::new_checked(fields).map_err(|source| {
			DocumentQueryError::new(error::QueryErrorFields {
				scope: declaration.identity(),
				schema_identifier: declaration.document().schema().identifier(),
				index: index.identity(),
				source,
			})
		})
	}
	fn new_checked(
		input: ScopedDocumentIndexQueryFields<'a, 's, 'd, T, C, P>,
	) -> Result<Self, DocumentQueryInnerError> {
		let ScopedDocumentIndexQueryFields {
			declaration,
			index,
			selection: fields,
		} = input;
		declaration
			.document()
			.require_index_declaration(index)
			.map_err(|_| DocumentQueryInnerError::DifferentIndexDeclaration)?;
		if fields.equality_prefix.len() > index.fields().len() {
			return Err(DocumentQueryInnerError::EqualityPrefixTooLong);
		}
		if fields.limit.get() > MAX_PAGE_LIMIT {
			return Err(DocumentQueryInnerError::LimitTooLarge {
				actual: fields.limit.get(),
				max: MAX_PAGE_LIMIT,
			});
		}
		for (field_index, value) in fields.equality_prefix.iter().enumerate() {
			check_value(index, field_index, value)?;
		}
		if let Some(range) = &fields.range {
			let field_index = fields.equality_prefix.len();
			if field_index == index.fields().len() {
				return Err(DocumentQueryInnerError::RangeFieldAbsent);
			}
			for bound in [&range.lower, &range.upper] {
				if let Bound::Included(value) | Bound::Excluded(value) = bound {
					check_value(index, field_index, value)?;
				}
			}
		}
		let query = Self {
			declaration,
			index,
			predicate: QueryPredicate {
				equality_prefix: fields.equality_prefix,
				range: fields.range,
				direction: fields.direction,
			},
			limit: fields.limit,
			cursor: fields.cursor,
		};
		if let Some(cursor) = &query.cursor {
			query.require_cursor_context(cursor)?;
		}
		Ok(query)
	}
	/// Continues the query after a cursor that `DocumentQueryCursor::encode_json` encoded.
	///
	/// The cursor must belong to this query's scope, schema, index, and predicate. A cursor
	/// does not mean that another document exists.
	pub fn resume(mut self, cursor_json: &RawValue) -> Result<Self, DocumentQueryError> {
		let cursor = self.decode_cursor(cursor_json).map_err(|source| {
			DocumentQueryError::new(error::QueryErrorFields {
				scope: self.declaration.identity(),
				schema_identifier: self.declaration.document().schema().identifier(),
				index: self.index.identity(),
				source,
			})
		})?;
		self.cursor = Some(cursor);
		Ok(self)
	}

	pub(crate) fn declaration(&self) -> &DocumentScopeDeclaration<'s, 'd, T, C, P> {
		self.declaration
	}
	pub(crate) fn index(&self) -> &IndexContract {
		self.index
	}
	pub(in crate::kv) fn equality_prefix(&self) -> &[IndexScalar] {
		&self.predicate.equality_prefix
	}
	pub(crate) fn range(&self) -> Option<&DocumentQueryRange> {
		self.predicate.range.as_ref()
	}
	pub(crate) fn direction(&self) -> DocumentQueryDirection {
		self.predicate.direction
	}
	pub(crate) fn limit(&self) -> NonZeroU32 {
		self.limit
	}
	pub(crate) fn cursor(&self) -> Option<&DocumentQueryCursor> {
		self.cursor.as_ref()
	}
	pub(in crate::kv) fn decode_cursor(
		&self,
		cursor_json: &RawValue,
	) -> Result<DocumentQueryCursor, DocumentQueryInnerError> {
		wire::decode(self, cursor_json)
	}
	fn require_cursor_context(
		&self,
		cursor: &DocumentQueryCursor,
	) -> Result<(), DocumentQueryInnerError> {
		if cursor.scope != *self.declaration.identity()
			|| cursor.schema_identifier != self.declaration.document().schema().identifier()
			|| !cursor.index.matches_declaration(self.index)
			|| cursor.predicate != self.predicate
		{
			return Err(DocumentQueryInnerError::CursorContextMismatch);
		}
		self.check_tuple(cursor.values())?;
		if !self.matches_predicate(cursor.values()) {
			return Err(DocumentQueryInnerError::PositionOutsideSelection);
		}
		Ok(())
	}
	fn check_tuple(&self, values: &[IndexScalar]) -> Result<(), DocumentQueryInnerError> {
		if values.len() != self.index.fields().len() {
			return Err(DocumentQueryInnerError::TupleLengthMismatch);
		}
		for (field_index, value) in values.iter().enumerate() {
			check_value(self.index, field_index, value)?;
		}
		Ok(())
	}
	fn matches_predicate(&self, values: &[IndexScalar]) -> bool {
		values.starts_with(&self.predicate.equality_prefix)
			&& self.predicate.range.as_ref().is_none_or(|range| {
				range_contains(range, &values[self.predicate.equality_prefix.len()])
			})
	}
	/// Complete tuple ordering uses bytewise logical keys as its final tie breaker.
	pub(in crate::kv) fn compare_positions(
		&self,
		positions: DocumentQueryPositionComparison<'_>,
	) -> Result<Ordering, DocumentQueryInnerError> {
		let DocumentQueryPositionComparison { left, right } = positions;
		self.check_tuple(left.values)?;
		self.check_tuple(right.values)?;
		let ordering = left
			.values
			.iter()
			.zip(right.values)
			.map(compare_same_type)
			.find(|order| *order != Ordering::Equal)
			.unwrap_or_else(|| left.key.cmp(right.key));
		Ok(match self.direction() {
			DocumentQueryDirection::Ascending => ordering,
			DocumentQueryDirection::Descending => ordering.reverse(),
		})
	}
	pub(in crate::kv) fn matches_position(
		&self,
		values: &[IndexScalar],
		key: &RecordKey,
	) -> Result<bool, DocumentQueryInnerError> {
		self.check_tuple(values)?;
		if !self.matches_predicate(values) {
			return Ok(false);
		}
		match self.cursor() {
			None => Ok(true),
			Some(cursor) => self
				.compare_positions(DocumentQueryPositionComparison {
					left: DocumentQueryPosition { values, key },
					right: DocumentQueryPosition {
						values: cursor.values(),
						key: cursor.key(),
					},
				})
				.map(|order| order == Ordering::Greater),
		}
	}
	/// Constructs a position only after parsing the complete selected tuple and key.
	pub(in crate::kv) fn cursor_after(
		&self,
		values: Vec<IndexScalar>,
		key: RecordKey,
	) -> Result<DocumentQueryCursor, DocumentQueryInnerError> {
		if !self.matches_position(&values, &key)? {
			return Err(DocumentQueryInnerError::PositionOutsideSelection);
		}
		Ok(DocumentQueryCursor {
			scope: self.declaration.identity().clone(),
			schema_identifier: self.declaration.document().schema().identifier(),
			index: IndexContractDescriptor::from_declaration(self.index),
			predicate: self.predicate.clone(),
			values,
			key,
		})
	}
}
