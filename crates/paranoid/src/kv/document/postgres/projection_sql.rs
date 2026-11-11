//! Typed scalar SQL projection lookups and literals for Document index expressions and queries.

use std::fmt;

use crate::db::AuditedSqlBuilder;
use crate::kv::document::schema::IndexScalarType;
use crate::records::{IndexFields, IndexScalar};

/// A logical projection name; SQL and persisted JSON derive the same canonical hex key.
pub(crate) struct ProjectionFieldKey(String);

impl ProjectionFieldKey {
	pub(crate) fn new(name: String) -> Self {
		Self(name)
	}
	pub(crate) fn lookup<'a>(&self, fields: &'a IndexFields) -> Option<&'a IndexScalar> {
		fields.get(&self.0)
	}
	pub(super) fn append_json_lookup(&self, sql: &mut AuditedSqlBuilder) {
		sql.push_text("pg_catalog.jsonb_extract_path(index_fields, pg_catalog.encode(");
		sql.push_argument(self.0.as_bytes());
		sql.push_text(", 'hex'))");
	}
	pub(super) fn append_value_lookup(&self, sql: &mut AuditedSqlBuilder) {
		sql.push_text("pg_catalog.jsonb_extract_path_text(index_fields, pg_catalog.encode(");
		sql.push_argument(self.0.as_bytes());
		sql.push_text(", 'hex'), 'value')");
	}
}

impl fmt::Debug for ProjectionFieldKey {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("ProjectionFieldKey")
			.field("byte_len", &self.0.len())
			.finish()
	}
}

#[derive(Debug, thiserror::Error)]
#[error("projection scalar has another declared type")]
pub(crate) struct ProjectionScalarTypeMismatchError;

pub(super) fn append_scalar_literal(
	kind: IndexScalarType,
	value: &IndexScalar,
	sql: &mut AuditedSqlBuilder,
) -> Result<(), ProjectionScalarTypeMismatchError> {
	if !kind.matches(value) {
		return Err(ProjectionScalarTypeMismatchError);
	}
	match value {
		IndexScalar::Boolean(value) => sql.push_argument(value),
		IndexScalar::SignedInteger(value) => sql.push_argument(value),
		IndexScalar::Timestamp(value) => sql.push_argument(&value.as_i64()),
		IndexScalar::BytewiseText(value) => {
			sql.push_text("(pg_catalog.encode(");
			sql.push_argument(value.as_bytes());
			sql.push_text(", 'hex') COLLATE pg_catalog.\"C\")");
		}
		IndexScalar::Bytes(value) => sql.push_argument(value),
	}
	Ok(())
}

pub(super) fn append_true_coverage(sql: &mut AuditedSqlBuilder) {
	sql.push_text("'{\"kind\":\"boolean\",\"value\":true}'::pg_catalog.jsonb");
}
