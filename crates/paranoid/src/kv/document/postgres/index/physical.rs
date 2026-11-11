//! Closed actual-code DDL and comparison-only catalog expectations.

use crate::db::{AuditedSql, AuditedSqlBuilder};
use crate::kv::document::schema::IndexScalarType;

use super::managed::PgManagedDocumentIndexBinding;
use super::{PgDocumentIndexDescriptor, PgDocumentIndexFormat};

mod catalog;
mod grammar;
mod types;
pub(crate) use catalog::inspect_document_index;
pub(crate) use types::{
	PgDocumentIndexCatalogMismatch, PgDocumentIndexCatalogObservation,
	PgDocumentIndexPhysicalError, PgIndexKeyCapacity,
};

/// Recorded facts cannot construct an executable binding or emit index DDL.
pub(crate) struct PgDocumentIndexExpectedCatalog {
	descriptor: PgDocumentIndexDescriptor,
	fields: Vec<ExpectedIndexField>,
	coverage_key: Vec<u8>,
}
struct ExpectedIndexField {
	kind: IndexScalarType,
	projection_key: Vec<u8>,
}
impl PgDocumentIndexExpectedCatalog {
	pub(crate) fn from_descriptor(
		descriptor: &PgDocumentIndexDescriptor,
	) -> Result<Self, PgDocumentIndexPhysicalError> {
		match descriptor.format() {
			PgDocumentIndexFormat::CompoundExpressionsV1 => {}
		}
		let fields = descriptor
			.index()
			.fields()
			.iter()
			.enumerate()
			.map(|(ordinal, field)| {
				Ok(ExpectedIndexField {
					kind: field.scalar_type(),
					projection_key: descriptor.index().field_key(ordinal)?.into_bytes(),
				})
			})
			.collect::<Result<Vec<_>, PgDocumentIndexPhysicalError>>()?;
		Ok(Self {
			descriptor: descriptor.clone(),
			fields,
			coverage_key: descriptor.index().coverage_field_key().into_bytes(),
		})
	}
	fn key_count(&self) -> Result<usize, PgDocumentIndexPhysicalError> {
		self.fields
			.len()
			.checked_add(1)
			.ok_or(PgDocumentIndexPhysicalError::KeyCountOverflow)
	}
}

/// Generates only this physical format from actual retained code and the owner-chosen name.
/// A fixed executor must separately establish current policy, ledger and mutation authority.
pub(crate) fn create_index_statement(
	binding: &PgManagedDocumentIndexBinding<'_>,
	capacity: &PgIndexKeyCapacity,
) -> Result<AuditedSql, PgDocumentIndexPhysicalError> {
	let count = binding
		.index()
		.fields()
		.len()
		.checked_add(1)
		.ok_or(PgDocumentIndexPhysicalError::KeyCountOverflow)?;
	capacity.require_count(count)?;
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("CREATE INDEX ");
	sql.push_argument(binding.descriptor().index_name());
	sql.push_text(" ON ONLY ");
	sql.push_argument(binding.descriptor().table());
	sql.push_text(" USING btree (");
	for (ordinal, field) in binding.index().fields().iter().enumerate() {
		if ordinal != 0 {
			sql.push_text(", ");
		}
		sql.push_text("(");
		binding
			.expressions()
			.append_field_expression(ordinal, &mut sql)?;
		sql.push_text(") ");
		sql.push_text(scalar_facts(field.scalar_type()).opclass_sql);
		sql.push_text(" ASC NULLS LAST");
	}
	if !binding.index().fields().is_empty() {
		sql.push_text(", ");
	}
	binding.expressions().append_key_expression(&mut sql);
	sql.push_text(" pg_catalog.bytea_ops ASC NULLS LAST) WHERE ");
	binding
		.expressions()
		.append_valued_scope_predicate(&mut sql);
	Ok(sql.finish())
}

struct ScalarFacts {
	type_id: i64,
	type_name: &'static str,
	opclass_name: &'static str,
	opclass_sql: &'static str,
	collatable: bool,
}
fn scalar_facts(kind: IndexScalarType) -> ScalarFacts {
	match kind {
		IndexScalarType::Boolean => ScalarFacts {
			type_id: 16,
			type_name: "bool",
			opclass_name: "bool_ops",
			opclass_sql: "pg_catalog.bool_ops",
			collatable: false,
		},
		IndexScalarType::SignedInteger | IndexScalarType::Timestamp => ScalarFacts {
			type_id: 20,
			type_name: "int8",
			opclass_name: "int8_ops",
			opclass_sql: "pg_catalog.int8_ops",
			collatable: false,
		},
		IndexScalarType::BytewiseText => ScalarFacts {
			type_id: 25,
			type_name: "text",
			opclass_name: "text_ops",
			opclass_sql: "pg_catalog.text_ops",
			collatable: true,
		},
		IndexScalarType::Bytes => ScalarFacts {
			type_id: 17,
			type_name: "bytea",
			opclass_name: "bytea_ops",
			opclass_sql: "pg_catalog.bytea_ops",
			collatable: false,
		},
	}
}
