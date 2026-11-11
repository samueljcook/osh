//! Failures to build a document query or to read its cursor.

use super::DocumentQueryInnerError;
use crate::{kv::document::schema::IndexContractIdentity, scope::ScopeIdentity};
use std::{error::Error, fmt};

pub(super) struct QueryErrorFields<'a> {
	pub(super) scope: &'a ScopeIdentity,
	pub(super) schema_identifier: &'static str,
	pub(super) index: &'a IndexContractIdentity,
	pub(super) source: DocumentQueryInnerError,
}
struct Diagnostic {
	scope: ScopeIdentity,
	schema_identifier: &'static str,
	index: IndexContractIdentity,
	source: DocumentQueryInnerError,
}
/// A query that could not be built, or a cursor that the query rejected.
pub struct DocumentQueryError {
	diagnostic: Box<Diagnostic>,
}
/// The class of a query failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DocumentQueryErrorKind {
	/// The scope's document code declares no index with the requested identity.
	UndeclaredIndex,
	/// The equality prefix has more values than the index has fields.
	EqualityPrefixTooLong,
	/// The page limit exceeds [`MAX_PAGE_LIMIT`](crate::kv::MAX_PAGE_LIMIT).
	LimitTooLarge,
	/// The index has no field after the equality prefix for the range.
	RangeFieldAbsent,
	/// A predicate or cursor value has a type other than its index field.
	ScalarTypeMismatch,
	/// A cursor has a number of values other than the index's fields.
	TupleLengthMismatch,
	/// The cursor belongs to another scope, schema, index or predicate.
	CursorContextMismatch,
	/// The cursor's position does not satisfy the query's predicate.
	PositionOutsideSelection,
	/// The cursor's format version is not supported.
	UnsupportedCursorVersion,
	/// An integer in the cursor is not a canonical decimal `i64`.
	InvalidCursorDecimal,
	/// The cursor's scope or index could not be read.
	CursorScope,
	/// The cursor is not valid JSON of the cursor format.
	CursorJson,
}
impl DocumentQueryError {
	pub(super) fn new(fields: QueryErrorFields<'_>) -> Self {
		Self {
			diagnostic: Box::new(Diagnostic {
				scope: fields.scope.clone(),
				schema_identifier: fields.schema_identifier,
				index: fields.index.clone(),
				source: fields.source,
			}),
		}
	}
	/// The queried scope.
	pub fn scope(&self) -> &ScopeIdentity {
		&self.diagnostic.scope
	}
	/// The identifier of the scope's document schema.
	pub fn schema_identifier(&self) -> &'static str {
		self.diagnostic.schema_identifier
	}
	/// The queried index.
	pub fn index_identity(&self) -> &IndexContractIdentity {
		&self.diagnostic.index
	}
	/// The class of the failure.
	pub fn kind(&self) -> DocumentQueryErrorKind {
		use DocumentQueryErrorKind as Kind;
		match &self.diagnostic.source {
			DocumentQueryInnerError::DifferentIndexDeclaration => Kind::UndeclaredIndex,
			DocumentQueryInnerError::EqualityPrefixTooLong => Kind::EqualityPrefixTooLong,
			DocumentQueryInnerError::LimitTooLarge { .. } => Kind::LimitTooLarge,
			DocumentQueryInnerError::RangeFieldAbsent => Kind::RangeFieldAbsent,
			DocumentQueryInnerError::ScalarTypeMismatch { .. } => Kind::ScalarTypeMismatch,
			DocumentQueryInnerError::TupleLengthMismatch => Kind::TupleLengthMismatch,
			DocumentQueryInnerError::CursorContextMismatch => Kind::CursorContextMismatch,
			DocumentQueryInnerError::PositionOutsideSelection => Kind::PositionOutsideSelection,
			DocumentQueryInnerError::UnsupportedCursorVersion { .. } => {
				Kind::UnsupportedCursorVersion
			}
			DocumentQueryInnerError::InvalidCursorDecimal => Kind::InvalidCursorDecimal,
			DocumentQueryInnerError::CursorScope(_) => Kind::CursorScope,
			DocumentQueryInnerError::CursorJson { .. } => Kind::CursorJson,
		}
	}
	/// The position of the field with the wrong type, when the kind is `ScalarTypeMismatch`.
	pub fn field_position(&self) -> Option<usize> {
		match &self.diagnostic.source {
			DocumentQueryInnerError::ScalarTypeMismatch { field_index } => Some(*field_index),
			_ => None,
		}
	}
	/// The cursor's format version, when the kind is `UnsupportedCursorVersion`.
	pub fn unsupported_version(&self) -> Option<u32> {
		match &self.diagnostic.source {
			DocumentQueryInnerError::UnsupportedCursorVersion { version } => Some(*version),
			_ => None,
		}
	}
	/// The line and column of the JSON failure, when the kind is `CursorJson`.
	pub fn json_location(&self) -> Option<(usize, usize)> {
		match &self.diagnostic.source {
			DocumentQueryInnerError::CursorJson { line, column, .. } => Some((*line, *column)),
			_ => None,
		}
	}
}
impl fmt::Debug for DocumentQueryError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "DocumentQueryError({:?}): {self}", self.kind())?;
		let mut source = self.source();
		while let Some(error) = source {
			write!(f, ": {error}")?;
			source = error.source();
		}
		Ok(())
	}
}
impl fmt::Display for DocumentQueryError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("document query construction or cursor rejected")
	}
}
impl Error for DocumentQueryError {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		Some(&self.diagnostic.source)
	}
}
