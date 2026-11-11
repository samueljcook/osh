use crate::db::{PgIdentifier, PgQualifiedTableName, migration_object_identifier};
use crate::kv::MAX_KEY_LEN;

pub const EXPIRES_AT_INDEX_SUFFIX: &str = "expires_at";
pub const KEY_PATTERN_INDEX_SUFFIX: &str = "key_pattern";
pub const UPDATED_AT_INDEX_SUFFIX: &str = "updated_at";

const INDEX_KIND: &str = "idx";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KvCatalog {
	table_name: PgQualifiedTableName,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum KvColumn {
	Key,
	Value,
	ExpiresAt,
	UpdatedAt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum KvIndex {
	ExpiresAt,
	KeyPattern,
	UpdatedAt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RequiredColumn {
	pub(super) name: &'static str,
	pub(super) data_type: &'static str,
	pub(super) not_null: bool,
	pub(super) allowed_collations: &'static [&'static str],
}

impl KvCatalog {
	pub(in crate::kv) fn from_table(table_name: PgQualifiedTableName) -> Self {
		Self { table_name }
	}

	pub(super) fn table_name(&self) -> &PgQualifiedTableName {
		&self.table_name
	}

	pub(super) fn quoted_table_name(&self) -> String {
		self.table_name.quoted().to_string()
	}

	pub(super) fn required_columns(&self) -> [RequiredColumn; 4] {
		[
			RequiredColumn {
				name: KvColumn::Key.name(),
				data_type: "text",
				not_null: true,
				allowed_collations: &["C", "POSIX"],
			},
			RequiredColumn {
				name: KvColumn::Value.name(),
				data_type: "bytea",
				not_null: true,
				allowed_collations: &[],
			},
			RequiredColumn {
				name: KvColumn::ExpiresAt.name(),
				data_type: "timestamp with time zone",
				not_null: false,
				allowed_collations: &[],
			},
			RequiredColumn {
				name: KvColumn::UpdatedAt.name(),
				data_type: "timestamp with time zone",
				not_null: true,
				allowed_collations: &[],
			},
		]
	}

	pub(super) fn migration_index_identifier_for_suffix(&self, suffix: &str) -> PgIdentifier {
		migration_index_identifier_for_table(self.table_name(), suffix)
	}

	pub(super) fn migration_index_identifier(&self, index: KvIndex) -> PgIdentifier {
		self.migration_index_identifier_for_suffix(index.suffix())
	}

	pub(super) fn key_length_check_sql(&self) -> String {
		format!(
			"pg_catalog.octet_length({}) OPERATOR(pg_catalog.>) 0 AND pg_catalog.octet_length({}) OPERATOR(pg_catalog.<=) {}",
			KvColumn::Key.sql_identifier(),
			KvColumn::Key.sql_identifier(),
			MAX_KEY_LEN
		)
	}

	pub(super) fn normalized_key_length_check_expression(&self) -> String {
		format!(
			"(octet_length({})>0)AND(octet_length({})<={})",
			KvColumn::Key.sql_identifier(),
			KvColumn::Key.sql_identifier(),
			MAX_KEY_LEN
		)
	}

	pub(in crate::kv) fn append_expired_filter(sql: &mut crate::db::AuditedSqlBuilder) {
		sql.push_text("(");
		sql.push_text(KvColumn::ExpiresAt.sql_identifier());
		sql.push_text(" IS NOT NULL AND ");
		sql.push_text(KvColumn::ExpiresAt.sql_identifier());
		sql.push_text(" OPERATOR(pg_catalog.<=) pg_catalog.statement_timestamp())");
	}

	pub(super) fn expires_at_index_predicate_sql(&self) -> String {
		format!("{} IS NOT NULL", KvColumn::ExpiresAt.sql_identifier())
	}

	pub(super) fn parenthesized_expires_at_index_predicate_sql(&self) -> String {
		format!("({})", self.expires_at_index_predicate_sql())
	}
}

impl KvColumn {
	pub(super) const fn name(self) -> &'static str {
		match self {
			Self::Key => "key",
			Self::Value => "value",
			Self::ExpiresAt => "expires_at",
			Self::UpdatedAt => "updated_at",
		}
	}

	pub(super) const fn sql_identifier(self) -> &'static str {
		self.name()
	}
}

impl KvIndex {
	pub(super) const fn suffix(self) -> &'static str {
		match self {
			Self::ExpiresAt => EXPIRES_AT_INDEX_SUFFIX,
			Self::KeyPattern => KEY_PATTERN_INDEX_SUFFIX,
			Self::UpdatedAt => UPDATED_AT_INDEX_SUFFIX,
		}
	}
}

pub(super) fn migration_index_identifier_for_table(
	table_name: &PgQualifiedTableName,
	suffix: &str,
) -> PgIdentifier {
	migration_object_identifier(INDEX_KIND, table_name, suffix)
}
