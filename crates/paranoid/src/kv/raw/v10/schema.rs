use super::catalog::{KvCatalog, KvColumn, KvIndex, RequiredColumn};
use super::operation_names::{
	KV_SCHEMA_VALIDATE_CHECK_CONSTRAINTS_LABEL, KV_SCHEMA_VALIDATE_COLUMNS_LABEL,
	KV_SCHEMA_VALIDATE_EXPIRES_AT_INDEX_LABEL, KV_SCHEMA_VALIDATE_KEY_CONFLICT_ARBITER_LABEL,
	KV_SCHEMA_VALIDATE_KEY_PATTERN_INDEX_LABEL, KV_SCHEMA_VALIDATE_UPDATED_AT_INDEX_LABEL,
};
use super::{ActualColumn, KV_SCHEMA_COMPONENT, KV_SCHEMA_FINGERPRINT, KV_SCHEMA_VERSION};
use crate::db::{
	DatabaseOperationKind, PG_COLUMN_METADATA_SQL, PG_SINGLE_COLUMN_INDEX_EXISTS_SQL,
	PG_USABLE_SINGLE_COLUMN_UNIQUE_INDEX_EXISTS_SQL, PgQualifiedTableName, SchemaError,
	TIMESTAMPTZ_OPERATOR_CLASS, Tx, owned_portable_query_as, owned_portable_query_scalar,
};
use crate::installation::{
	ComponentSchemaVersion, schema_instance_key_for_parts,
	validate_component_schema_version_in_current_transaction,
};

pub(crate) struct KvPhysicalSchemaBinding<'a> {
	pub(crate) table: &'a PgQualifiedTableName,
	pub(crate) requires_updated_at_index: bool,
}

/// Shared actual catalog contract from physical facts, without store policy or admission.
pub(crate) async fn validate_kv_physical_schema_in_current_transaction(
	tx: &mut Tx<'_>,
	binding: KvPhysicalSchemaBinding<'_>,
) -> Result<(), SchemaError> {
	let catalog = KvCatalog::from_table(binding.table.clone());
	validate_required_columns(tx, &catalog).await?;
	validate_key_conflict_arbiter(tx, &catalog).await?;
	validate_required_check_constraints(tx, &catalog).await?;
	validate_expires_at_index(tx, &catalog).await?;
	validate_key_pattern_index(tx, &catalog).await?;
	if binding.requires_updated_at_index {
		validate_updated_at_index(tx, &catalog).await?;
	}
	Ok(())
}

pub(super) async fn validate_required_columns(
	tx: &mut Tx<'_>,
	catalog: &KvCatalog,
) -> Result<(), SchemaError> {
	let quoted_table_name = catalog.quoted_table_name();
	let statement = PG_COLUMN_METADATA_SQL;
	tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		KV_SCHEMA_VALIDATE_COLUMNS_LABEL,
		Some(statement),
	);
	let actual_columns =
		owned_portable_query_as::<(String, String, bool, Option<String>, bool)>(statement)
			.bind_validated(&quoted_table_name)
			.fetch_all(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(KV_SCHEMA_VALIDATE_COLUMNS_LABEL))?
			.into_iter()
			.map(
				|(name, data_type, not_null, collation, writable)| ActualColumn {
					name,
					data_type,
					not_null,
					collation,
					writable,
				},
			)
			.collect::<Vec<_>>();

	for required_column in catalog.required_columns() {
		let Some(actual_column) = actual_columns
			.iter()
			.find(|column| column.name == required_column.name)
		else {
			return Err(SchemaError::mismatch(format!(
				"required column {:?} not found on table {}",
				required_column.name, quoted_table_name
			)));
		};

		validate_required_column(&quoted_table_name, required_column, actual_column)?;
	}

	Ok(())
}

/// Actual KV registration associations, including Fleet's KV-backed state table.
pub(crate) struct RecordedKvSchemaBinding<'a> {
	pub(crate) table: &'a PgQualifiedTableName,
	pub(crate) ledger: &'a PgQualifiedTableName,
}

/// Checks the owning version contract without accessing the source data relation.
pub(crate) async fn validate_recorded_kv_schema_in_current_transaction(
	tx: &mut Tx<'_>,
	binding: RecordedKvSchemaBinding<'_>,
) -> Result<(), SchemaError> {
	let instance_key = kv_schema_instance_key_for_table(binding.table);
	validate_component_schema_version_in_current_transaction(
		tx,
		binding.ledger,
		kv_component_schema_version(&instance_key),
	)
	.await
}

fn kv_schema_instance_key_for_table(table: &PgQualifiedTableName) -> String {
	schema_instance_key_for_parts([("table", table)])
}

fn kv_component_schema_version(instance_key: &str) -> ComponentSchemaVersion<'_> {
	ComponentSchemaVersion {
		component: KV_SCHEMA_COMPONENT,
		instance_key,
		version: KV_SCHEMA_VERSION,
		fingerprint: KV_SCHEMA_FINGERPRINT,
	}
}

pub(super) async fn validate_key_conflict_arbiter(
	tx: &mut Tx<'_>,
	catalog: &KvCatalog,
) -> Result<(), SchemaError> {
	let quoted_table_name = catalog.quoted_table_name();
	let statement = PG_USABLE_SINGLE_COLUMN_UNIQUE_INDEX_EXISTS_SQL;
	tx.record_database_operation(
		DatabaseOperationKind::FetchOne,
		KV_SCHEMA_VALIDATE_KEY_CONFLICT_ARBITER_LABEL,
		Some(statement),
	);
	let has_usable_unique_key = owned_portable_query_scalar::<bool>(statement)
		.bind_validated(&quoted_table_name)
		.bind_validated(KvColumn::Key.name())
		.fetch_one(tx.inner.as_mut())
		.await
		.map_err(|error| {
			error.with_operation_label(KV_SCHEMA_VALIDATE_KEY_CONFLICT_ARBITER_LABEL)
		})?;

	if !has_usable_unique_key {
		let key = KvColumn::Key.name();
		return Err(SchemaError::mismatch(format!(
			r#"column "{key}" must have a unique or primary key constraint usable by ON CONFLICT ({key})"#
		)));
	}

	Ok(())
}

pub(super) async fn validate_required_check_constraints(
	tx: &mut Tx<'_>,
	catalog: &KvCatalog,
) -> Result<(), SchemaError> {
	let quoted_table_name = catalog.quoted_table_name();
	let statement = crate::db::catalog::BUILTIN_CHECK_EXPRESSIONS_SQL;
	tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		KV_SCHEMA_VALIDATE_CHECK_CONSTRAINTS_LABEL,
		Some(statement),
	);
	let normalized_check_expressions = owned_portable_query_scalar::<String>(statement)
		.bind_validated(&quoted_table_name)
		.fetch_all(tx.inner.as_mut())
		.await
		.map_err(|error| error.with_operation_label(KV_SCHEMA_VALIDATE_CHECK_CONSTRAINTS_LABEL))?
		.into_iter()
		.map(|expression| crate::db::catalog::normalize_builtin_check_expression(&expression))
		.collect::<Vec<String>>();

	let required_key_length_expression = catalog.normalized_key_length_check_expression();
	if !normalized_check_expressions
		.iter()
		.any(|expression| expression == &required_key_length_expression)
	{
		let key_length_check = catalog.key_length_check_sql();
		return Err(SchemaError::mismatch(format!(
			r#"table {} must enforce CHECK ({})"#,
			quoted_table_name, key_length_check
		)));
	}

	Ok(())
}

pub(super) async fn validate_expires_at_index(
	tx: &mut Tx<'_>,
	catalog: &KvCatalog,
) -> Result<(), SchemaError> {
	let quoted_table_name = catalog.quoted_table_name();
	let index_name = catalog.migration_index_identifier(KvIndex::ExpiresAt);
	let statement = concat!(
		r#"
        SELECT EXISTS (
            SELECT 1
            FROM pg_catalog.pg_index idx
            JOIN pg_catalog.pg_class index_class ON index_class.oid OPERATOR(pg_catalog.=) idx.indexrelid
            JOIN pg_catalog.pg_attribute attr
              ON attr.attrelid OPERATOR(pg_catalog.=) idx.indrelid
             AND attr.attnum OPERATOR(pg_catalog.=) idx.indkey[0]
             AND NOT attr.attisdropped
            JOIN pg_catalog.pg_opclass opclass
              ON opclass.oid OPERATOR(pg_catalog.=) idx.indclass[0]
             AND opclass.opcmethod OPERATOR(pg_catalog.=) index_class.relam
             AND opclass.opcnamespace OPERATOR(pg_catalog.=) 'pg_catalog'::pg_catalog.regnamespace
            WHERE idx.indrelid OPERATOR(pg_catalog.=) pg_catalog.to_regclass($1)
              AND index_class.relname OPERATOR(pg_catalog.=) $2
              AND idx.indnkeyatts OPERATOR(pg_catalog.=) 1
              AND idx.indexprs IS NULL
              AND attr.attname OPERATOR(pg_catalog.=) $3
              AND pg_catalog.pg_get_expr(idx.indpred, idx.indrelid) OPERATOR(pg_catalog.=) ANY (ARRAY[$4, $5]::pg_catalog.text[])
              AND opclass.opcname OPERATOR(pg_catalog.=) $6
              AND idx.indoption[0] OPERATOR(pg_catalog.=) 0
              AND idx.indcollation[0] OPERATOR(pg_catalog.=) attr.attcollation"#,
		crate::db::builtin_btree_index_conditions!(),
		r#"
        )
        "#
	);
	tx.record_database_operation(
		DatabaseOperationKind::FetchOne,
		KV_SCHEMA_VALIDATE_EXPIRES_AT_INDEX_LABEL,
		Some(statement),
	);
	let has_index = owned_portable_query_scalar::<bool>(statement)
		.bind_validated(&quoted_table_name)
		.bind_validated(index_name.as_str())
		.bind_validated(KvColumn::ExpiresAt.name())
		.bind_validated(catalog.expires_at_index_predicate_sql())
		.bind_validated(catalog.parenthesized_expires_at_index_predicate_sql())
		.bind_validated(TIMESTAMPTZ_OPERATOR_CLASS)
		.fetch_one(tx.inner.as_mut())
		.await
		.map_err(|error| error.with_operation_label(KV_SCHEMA_VALIDATE_EXPIRES_AT_INDEX_LABEL))?;

	if !has_index {
		let expires_at = KvColumn::ExpiresAt.name();
		return Err(SchemaError::mismatch(format!(
			"table {} must have partial {} index {:?}",
			quoted_table_name,
			expires_at,
			index_name.as_str()
		)));
	}

	Ok(())
}

pub(super) async fn validate_key_pattern_index(
	tx: &mut Tx<'_>,
	catalog: &KvCatalog,
) -> Result<(), SchemaError> {
	let quoted_table_name = catalog.quoted_table_name();
	let index_name = catalog.migration_index_identifier(KvIndex::KeyPattern);
	let statement = concat!(
		r#"
        SELECT EXISTS (
            SELECT 1
            FROM pg_catalog.pg_index idx
            JOIN pg_catalog.pg_class index_class ON index_class.oid OPERATOR(pg_catalog.=) idx.indexrelid
            JOIN pg_catalog.pg_attribute attr
              ON attr.attrelid OPERATOR(pg_catalog.=) idx.indrelid
             AND attr.attnum OPERATOR(pg_catalog.=) idx.indkey[0]
             AND NOT attr.attisdropped
            JOIN pg_catalog.pg_opclass opclass
              ON opclass.oid OPERATOR(pg_catalog.=) idx.indclass[0]
             AND opclass.opcmethod OPERATOR(pg_catalog.=) index_class.relam
             AND opclass.opcnamespace OPERATOR(pg_catalog.=) 'pg_catalog'::pg_catalog.regnamespace
            WHERE idx.indrelid OPERATOR(pg_catalog.=) pg_catalog.to_regclass($1)
              AND index_class.relname OPERATOR(pg_catalog.=) $2
              AND idx.indnkeyatts OPERATOR(pg_catalog.=) 1
              AND idx.indpred IS NULL
              AND idx.indexprs IS NULL
              AND attr.attname OPERATOR(pg_catalog.=) $3
              AND opclass.opcname OPERATOR(pg_catalog.=) 'text_pattern_ops'
              AND idx.indoption[0] OPERATOR(pg_catalog.=) 0
              AND idx.indcollation[0] OPERATOR(pg_catalog.=) attr.attcollation"#,
		crate::db::builtin_btree_index_conditions!(),
		r#"
        )
        "#
	);
	tx.record_database_operation(
		DatabaseOperationKind::FetchOne,
		KV_SCHEMA_VALIDATE_KEY_PATTERN_INDEX_LABEL,
		Some(statement),
	);
	let has_index = owned_portable_query_scalar::<bool>(statement)
		.bind_validated(&quoted_table_name)
		.bind_validated(index_name.as_str())
		.bind_validated(KvColumn::Key.name())
		.fetch_one(tx.inner.as_mut())
		.await
		.map_err(|error| error.with_operation_label(KV_SCHEMA_VALIDATE_KEY_PATTERN_INDEX_LABEL))?;

	if !has_index {
		let key = KvColumn::Key.name();
		return Err(SchemaError::mismatch(format!(
			"table {} must have {} text_pattern_ops index {:?}",
			quoted_table_name,
			key,
			index_name.as_str()
		)));
	}

	Ok(())
}

pub(super) async fn validate_updated_at_index(
	tx: &mut Tx<'_>,
	catalog: &KvCatalog,
) -> Result<(), SchemaError> {
	let quoted_table_name = catalog.quoted_table_name();
	let index_name = catalog.migration_index_identifier(KvIndex::UpdatedAt);
	let statement = PG_SINGLE_COLUMN_INDEX_EXISTS_SQL;
	tx.record_database_operation(
		DatabaseOperationKind::FetchOne,
		KV_SCHEMA_VALIDATE_UPDATED_AT_INDEX_LABEL,
		Some(statement),
	);
	let has_index = owned_portable_query_scalar::<bool>(statement)
		.bind_validated(&quoted_table_name)
		.bind_validated(index_name.as_str())
		.bind_validated(KvColumn::UpdatedAt.name())
		.bind_validated(TIMESTAMPTZ_OPERATOR_CLASS)
		.fetch_one(tx.inner.as_mut())
		.await
		.map_err(|error| error.with_operation_label(KV_SCHEMA_VALIDATE_UPDATED_AT_INDEX_LABEL))?;

	if !has_index {
		let updated_at = KvColumn::UpdatedAt.name();
		return Err(SchemaError::mismatch(format!(
			"table {} must have {} index {:?}",
			quoted_table_name,
			updated_at,
			index_name.as_str()
		)));
	}

	Ok(())
}

pub(super) fn validate_required_column(
	quoted_table_name: &str,
	required_column: RequiredColumn,
	actual_column: &ActualColumn,
) -> Result<(), SchemaError> {
	if actual_column.data_type != required_column.data_type {
		return Err(SchemaError::mismatch(format!(
			"column {:?} on table {} must be type {:?}, got {:?}",
			required_column.name,
			quoted_table_name,
			required_column.data_type,
			actual_column.data_type
		)));
	}

	if !actual_column.writable {
		return Err(SchemaError::mismatch(format!(
			"column {:?} on table {} must accept explicit writes",
			required_column.name, quoted_table_name
		)));
	}

	if actual_column.not_null != required_column.not_null {
		let expected_nullability = if required_column.not_null {
			"NOT NULL"
		} else {
			"NULL"
		};
		let actual_nullability = if actual_column.not_null {
			"NOT NULL"
		} else {
			"NULL"
		};
		return Err(SchemaError::mismatch(format!(
			"column {:?} on table {} must be {}, got {}",
			required_column.name, quoted_table_name, expected_nullability, actual_nullability
		)));
	}

	if !required_column.allowed_collations.is_empty() {
		let actual_collation = actual_column.collation.as_deref().unwrap_or("<none>");
		if !required_column
			.allowed_collations
			.contains(&actual_collation)
		{
			return Err(SchemaError::mismatch(format!(
				"column {:?} on table {} must use one of collations {:?}, got {:?}",
				required_column.name,
				quoted_table_name,
				required_column.allowed_collations,
				actual_collation
			)));
		}
	}

	Ok(())
}
