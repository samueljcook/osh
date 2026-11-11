//! Fixed physical expressions borrowed from an actual code-owned index contract.
//!
//! Only typed or managed binding construction can create this view. It owns no schema
//! decoder, encoder, projector, physical registration, or readiness permission.

use crate::kv::document::schema::{IndexContract, IndexFieldDef, IndexScalarType};
use crate::records::{IndexFields, IndexScalar};
use crate::scope::ScopeIdentity;

use super::super::projection_sql::{self as projection, ProjectionFieldKey};
use super::{AuditedSqlBuilder, PgDocumentIndexError, PgDocumentIndexProjectionError};

pub(crate) struct PgDocumentIndexExpressions<'a> {
	scope: &'a ScopeIdentity,
	index: &'a IndexContract,
	coverage_key: ProjectionFieldKey,
	fields: Vec<PgDocumentIndexField<'a>>,
}
struct PgDocumentIndexField<'a> {
	def: &'a IndexFieldDef,
	key: ProjectionFieldKey,
}

impl<'a> PgDocumentIndexExpressions<'a> {
	pub(super) fn new(
		scope: &'a ScopeIdentity,
		index: &'a IndexContract,
	) -> Result<Self, PgDocumentIndexError> {
		let fields = index
			.fields()
			.iter()
			.enumerate()
			.map(|(field_index, def)| {
				Ok(PgDocumentIndexField {
					def,
					key: ProjectionFieldKey::new(index.field_key(field_index)?),
				})
			})
			.collect::<Result<Vec<_>, crate::kv::document::schema::DocumentDeclarationError>>()?;
		Ok(Self {
			scope,
			index,
			coverage_key: ProjectionFieldKey::new(index.coverage_field_key()),
			fields,
		})
	}
	pub(crate) fn index(&self) -> &IndexContract {
		self.index
	}
	/// Reads this actual contract's stored tuple without a decoder, encoder, or projector.
	pub(crate) fn projected_tuple(
		&self,
		fields: &IndexFields,
	) -> Result<Vec<IndexScalar>, PgDocumentIndexProjectionError> {
		match self.coverage_key.lookup(fields) {
			Some(IndexScalar::Boolean(true)) => {}
			Some(IndexScalar::Boolean(false)) => {
				return Err(PgDocumentIndexProjectionError::CoverageFalse);
			}
			Some(_) => return Err(PgDocumentIndexProjectionError::CoverageType),
			None => return Err(PgDocumentIndexProjectionError::CoverageMissing),
		}
		self.fields
			.iter()
			.enumerate()
			.map(|(field_index, field)| {
				let value = field
					.key
					.lookup(fields)
					.ok_or(PgDocumentIndexProjectionError::FieldMissing { field_index })?;
				if !field.def.scalar_type().matches(value) {
					return Err(PgDocumentIndexProjectionError::FieldType { field_index });
				}
				Ok(value.clone())
			})
			.collect()
	}
	fn field(&self, field_index: usize) -> Result<&PgDocumentIndexField<'a>, PgDocumentIndexError> {
		self.fields
			.get(field_index)
			.ok_or(PgDocumentIndexError::FieldAbsent { field_index })
	}
	pub(crate) fn append_field_expression(
		&self,
		field_index: usize,
		sql: &mut AuditedSqlBuilder,
	) -> Result<(), PgDocumentIndexError> {
		let field = self.field(field_index)?;
		match field.def.scalar_type() {
			IndexScalarType::Boolean => {
				sql.push_text("(");
				field.key.append_value_lookup(sql);
				sql.push_text(")::pg_catalog.bool");
			}
			IndexScalarType::SignedInteger | IndexScalarType::Timestamp => {
				sql.push_text("(");
				field.key.append_value_lookup(sql);
				sql.push_text(")::pg_catalog.int8");
			}
			IndexScalarType::BytewiseText => {
				sql.push_text("(");
				field.key.append_value_lookup(sql);
				sql.push_text(" COLLATE pg_catalog.\"C\")");
			}
			IndexScalarType::Bytes => {
				sql.push_text("pg_catalog.decode(");
				field.key.append_value_lookup(sql);
				sql.push_text(", 'hex')");
			}
		}
		Ok(())
	}
	pub(crate) fn append_field_literal(
		&self,
		field_index: usize,
		value: &IndexScalar,
		sql: &mut AuditedSqlBuilder,
	) -> Result<(), PgDocumentIndexError> {
		let field = self.field(field_index)?;
		projection::append_scalar_literal(field.def.scalar_type(), value, sql)
			.map_err(|error| PgDocumentIndexError::FieldScalarType { field_index, error })
	}
	pub(crate) fn append_key_expression(&self, sql: &mut AuditedSqlBuilder) {
		sql.push_text("key");
	}
	pub(crate) fn append_coverage_predicate(&self, sql: &mut AuditedSqlBuilder) {
		self.coverage_key.append_json_lookup(sql);
		sql.push_text(" OPERATOR(pg_catalog.=) ");
		projection::append_true_coverage(sql);
	}
	pub(crate) fn append_valued_scope_predicate(&self, sql: &mut AuditedSqlBuilder) {
		sql.push_text("namespace OPERATOR(pg_catalog.=) ");
		sql.push_argument(self.scope.namespace().as_bytes());
		sql.push_text(" AND body IS NOT NULL AND ");
		self.append_coverage_predicate(sql);
	}
}
