//! Actual PostgreSQL document index expressions and configured physical association.
//!
//! Construction resolves configuration only. It neither inspects an installed index nor
//! authenticates a participant, query role, coverage, or readiness.

use primitives_rs::bytes::write_length_prefixed;
use primitives_rs::crypto::algorithms::blake3;
use std::fmt;

use crate::kv::document::query::ScopedDocumentIndexQuery;
use crate::kv::document::schema::{DocumentBindingError, DocumentDeclarationError, IndexContract};
use crate::records::{IndexFields, IndexScalar};
use crate::scope::{DocumentScopeDeclaration, IndexContractDescriptor, ScopeIdentity};

use super::projection_sql::ProjectionScalarTypeMismatchError;
use crate::db::{AuditedSqlBuilder, PgIdentifier, PgQualifiedTableName};
use crate::records::postgres::PgPointStorage;

mod expressions;
pub(crate) use expressions::PgDocumentIndexExpressions;
pub(crate) mod managed;
pub(crate) mod physical;

const COMPOUND_EXPRESSIONS_V1: &str = "paranoid.pg.document-index.compound-expressions.v1";

/// The physical name of one document index: `paranoid_index_` and 32 hexadecimal digits of a
/// hash of the scope and the index identity. An upgrade and the opening that creates an
/// installation both name a new index this way.
pub(crate) fn derived_index_name(
	scope: &ScopeIdentity,
	index: &crate::kv::document::schema::IndexContractIdentity,
) -> PgIdentifier {
	let mut hasher = blake3::Hasher::new();
	for part in [
		&b"paranoid.document-index-name.v1"[..],
		scope.storage().as_bytes(),
		scope.namespace().as_bytes(),
		index.identifier().as_bytes(),
		&index.version().get().to_be_bytes(),
	] {
		write_length_prefixed(part, |chunk| {
			hasher.update(chunk);
		});
	}
	let digest = hasher.finalize().to_hex();
	PgIdentifier::new(format!("paranoid_index_{}", &digest[..32]))
		.expect("a derived index name is a valid identifier")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PgDocumentIndexFormat {
	CompoundExpressionsV1,
}
impl PgDocumentIndexFormat {
	pub(crate) fn identifier(self) -> &'static str {
		match self {
			Self::CompoundExpressionsV1 => COMPOUND_EXPRESSIONS_V1,
		}
	}
	pub(crate) fn from_identifier(identifier: &str) -> Option<Self> {
		match identifier {
			COMPOUND_EXPRESSIONS_V1 => Some(Self::CompoundExpressionsV1),
			_ => None,
		}
	}
}

/// Comparison data derived from actual code and owner-selected placement, never executable code.
#[derive(Clone)]
pub(crate) struct PgDocumentIndexDescriptor {
	format: PgDocumentIndexFormat,
	scope: ScopeIdentity,
	schema_identifier: String,
	index: IndexContractDescriptor,
	table: PgQualifiedTableName,
	index_name: PgIdentifier,
}
/// Parsed comparison fields. These fields grant no declaration or physical authority.
pub(crate) struct PgDocumentIndexDescriptorFields {
	pub(crate) format: PgDocumentIndexFormat,
	pub(crate) scope: ScopeIdentity,
	pub(crate) schema_identifier: String,
	pub(crate) index: IndexContractDescriptor,
	pub(crate) table: PgQualifiedTableName,
	pub(crate) index_name: PgIdentifier,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PgDocumentIndexDescriptorError {
	#[error("PostgreSQL document index schema identifier must not be empty")]
	EmptySchemaIdentifier,
}
impl PgDocumentIndexDescriptor {
	pub(crate) fn recorded(
		fields: PgDocumentIndexDescriptorFields,
	) -> Result<Self, PgDocumentIndexDescriptorError> {
		if fields.schema_identifier.is_empty() {
			return Err(PgDocumentIndexDescriptorError::EmptySchemaIdentifier);
		}
		Ok(Self {
			format: fields.format,
			scope: fields.scope,
			schema_identifier: fields.schema_identifier,
			index: fields.index,
			table: fields.table,
			index_name: fields.index_name,
		})
	}
	pub(crate) fn format(&self) -> PgDocumentIndexFormat {
		self.format
	}
	pub(crate) fn scope(&self) -> &ScopeIdentity {
		&self.scope
	}
	pub(crate) fn schema_identifier(&self) -> &str {
		&self.schema_identifier
	}
	pub(crate) fn index(&self) -> &IndexContractDescriptor {
		&self.index
	}
	pub(crate) fn table(&self) -> &PgQualifiedTableName {
		&self.table
	}
	pub(crate) fn index_name(&self) -> &PgIdentifier {
		&self.index_name
	}
	pub(crate) fn matches(&self, other: &Self) -> bool {
		self.format == other.format
			&& self.scope == other.scope
			&& self.schema_identifier == other.schema_identifier
			&& self.table == other.table
			&& self.index_name == other.index_name
			&& self.index.identity() == other.index.identity()
			&& self.index.fields().len() == other.index.fields().len()
			&& self
				.index
				.fields()
				.iter()
				.zip(other.index.fields())
				.all(|(a, b)| a.name() == b.name() && a.scalar_type() == b.scalar_type())
	}
}
impl fmt::Debug for PgDocumentIndexDescriptor {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("PgDocumentIndexDescriptor")
			.field("format", &self.format)
			.field("scope", &self.scope)
			.field("field_count", &self.index.fields().len())
			.finish_non_exhaustive()
	}
}

pub(crate) struct PgDocumentQueryBindingFields<'a, 's, 'd, T, C, P> {
	pub(crate) declaration: &'a DocumentScopeDeclaration<'s, 'd, T, C, P>,
	pub(crate) index: &'a IndexContract,
	pub(crate) storage: &'a PgPointStorage,
}

pub(crate) struct PgDocumentQueryBinding<'a, 's, 'd, T, C, P> {
	declaration: &'a DocumentScopeDeclaration<'s, 'd, T, C, P>,
	storage: &'a PgPointStorage,
	expressions: PgDocumentIndexExpressions<'a>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PgDocumentIndexError {
	#[error("PostgreSQL document index uses another declaration")]
	DifferentIndexDeclaration,
	#[error("PostgreSQL document query uses another scope declaration")]
	DifferentScopeDeclaration,
	#[error("PostgreSQL document index binding was rejected")]
	Binding(#[from] DocumentBindingError),
	#[error("PostgreSQL document index declaration was rejected")]
	Declaration(#[from] DocumentDeclarationError),
	#[error("PostgreSQL document index has no field {field_index}")]
	FieldAbsent { field_index: usize },
	#[error("PostgreSQL document index field {field_index} has another scalar type")]
	FieldScalarType {
		field_index: usize,
		#[source]
		error: ProjectionScalarTypeMismatchError,
	},
}

/// Selected projection integrity, separate from missing readiness or actual SQL failure.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PgDocumentIndexProjectionError {
	#[error("selected document index coverage is missing")]
	CoverageMissing,
	#[error("selected document index coverage is false")]
	CoverageFalse,
	#[error("selected document index coverage is not a Boolean")]
	CoverageType,
	#[error("selected document index field {field_index} is missing")]
	FieldMissing { field_index: usize },
	#[error("selected document index field {field_index} has another scalar type")]
	FieldType { field_index: usize },
}

impl<'a, 's, 'd, T, C, P> PgDocumentQueryBinding<'a, 's, 'd, T, C, P> {
	pub(crate) fn new(
		fields: PgDocumentQueryBindingFields<'a, 's, 'd, T, C, P>,
	) -> Result<Self, PgDocumentIndexError> {
		fields
			.declaration
			.document()
			.require_index_declaration(fields.index)?;
		let expressions =
			PgDocumentIndexExpressions::new(fields.declaration.identity(), fields.index)?;
		Ok(Self {
			declaration: fields.declaration,
			storage: fields.storage,
			expressions,
		})
	}
	pub(crate) fn declaration(&self) -> &DocumentScopeDeclaration<'s, 'd, T, C, P> {
		self.declaration
	}
	pub(crate) fn index(&self) -> &IndexContract {
		self.expressions.index()
	}
	pub(crate) fn storage(&self) -> &PgPointStorage {
		self.storage
	}
	/// Reads the actual contract's stored tuple without application code.
	pub(crate) fn projected_tuple(
		&self,
		fields: &IndexFields,
	) -> Result<Vec<IndexScalar>, PgDocumentIndexProjectionError> {
		self.expressions.projected_tuple(fields)
	}
	/// Each handle of a scope has its own declaration, so the declarations are compared by scope
	/// identity. The index contract is compared by address: the handles of one runtime share it,
	/// and a query built with code that the handle does not share fails.
	pub(crate) fn require_query(
		&self,
		query: &ScopedDocumentIndexQuery<'_, 's, 'd, T, C, P>,
	) -> Result<(), PgDocumentIndexError> {
		if self.declaration.identity() != query.declaration().identity() {
			return Err(PgDocumentIndexError::DifferentScopeDeclaration);
		}
		if !std::ptr::eq(self.index(), query.index()) {
			return Err(PgDocumentIndexError::DifferentIndexDeclaration);
		}
		Ok(())
	}

	pub(crate) fn append_field_expression(
		&self,
		field_index: usize,
		sql: &mut AuditedSqlBuilder,
	) -> Result<(), PgDocumentIndexError> {
		self.expressions.append_field_expression(field_index, sql)
	}
	pub(crate) fn append_field_literal(
		&self,
		field_index: usize,
		value: &IndexScalar,
		sql: &mut AuditedSqlBuilder,
	) -> Result<(), PgDocumentIndexError> {
		self.expressions
			.append_field_literal(field_index, value, sql)
	}
	pub(crate) fn append_key_expression(&self, sql: &mut AuditedSqlBuilder) {
		self.expressions.append_key_expression(sql);
	}
	pub(crate) fn append_valued_scope_predicate(&self, sql: &mut AuditedSqlBuilder) {
		self.expressions.append_valued_scope_predicate(sql);
	}
}
impl<T, C, P> fmt::Debug for PgDocumentQueryBinding<'_, '_, '_, T, C, P> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("PgDocumentQueryBinding")
			.field("scope", self.declaration.identity())
			.finish_non_exhaustive()
	}
}
