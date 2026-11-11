use super::catalog::{
	LEASE_FENCING_COUNTER_CHECKED_COLUMNS, LEASE_FENCING_COUNTER_COLUMNS,
	LEASE_STATE_CHECKED_COLUMNS, LEASE_STATE_COLUMNS, LeaseColumn, migration_index_identifier,
};
use crate::db::{
	DatabaseOperationKind, DatabaseOperationObserver, PG_COLUMN_METADATA_SQL,
	PG_SINGLE_COLUMN_INDEX_EXISTS_SQL, PG_USABLE_SINGLE_COLUMN_UNIQUE_INDEX_EXISTS_SQL,
	SchemaError, TIMESTAMPTZ_OPERATOR_CLASS, Tx, owned_portable_query_as,
	owned_portable_query_scalar, pg_table_name_set_contains_duplicate, record_database_operation,
};
use crate::fleet::lease::{
	LEASE_SCHEMA_VALIDATE_CHECK_CONSTRAINTS_LABEL, LEASE_SCHEMA_VALIDATE_COLUMNS_LABEL,
	LEASE_SCHEMA_VALIDATE_EXPIRES_AT_INDEX_LABEL, LEASE_SCHEMA_VALIDATE_KEY_CONFLICT_ARBITER_LABEL,
	StoreConfig,
};
use sqlx::{Executor, Postgres};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RequiredColumn {
	column: LeaseColumn,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ActualColumn {
	name: String,
	data_type: String,
	not_null: bool,
	collation: Option<String>,
	writable: bool,
}

pub(crate) async fn validate_schema_in_current_transaction(
	tx: &mut Tx<'_>,
	config: &StoreConfig,
) -> Result<(), SchemaError> {
	validate_distinct_table_names(config)?;
	validate_lease_table(tx, config).await?;
	validate_expires_at_index_of_lease_table(tx, config).await?;
	validate_fencing_counter_table(tx, config).await
}

async fn validate_lease_table(tx: &mut Tx<'_>, config: &StoreConfig) -> Result<(), SchemaError> {
	let quoted_table_name = config.claim_table.quoted().to_string();
	let observer = tx.database_operation_observer().cloned();
	validate_required_columns(
		tx.inner.as_mut(),
		observer.as_ref(),
		&quoted_table_name,
		&required_state_columns(),
	)
	.await?;
	validate_key_conflict_arbiter(tx.inner.as_mut(), observer.as_ref(), &quoted_table_name).await?;
	validate_required_state_check_constraints(
		tx.inner.as_mut(),
		observer.as_ref(),
		&quoted_table_name,
	)
	.await
}

async fn validate_expires_at_index_of_lease_table(
	tx: &mut Tx<'_>,
	config: &StoreConfig,
) -> Result<(), SchemaError> {
	let quoted_table_name = config.claim_table.quoted().to_string();
	let observer = tx.database_operation_observer().cloned();
	validate_expires_at_index(
		tx.inner.as_mut(),
		observer.as_ref(),
		config,
		&quoted_table_name,
	)
	.await
}

async fn validate_fencing_counter_table(
	tx: &mut Tx<'_>,
	config: &StoreConfig,
) -> Result<(), SchemaError> {
	let quoted_table_name = config.fencing_counter_table.quoted().to_string();
	let observer = tx.database_operation_observer().cloned();
	validate_required_columns(
		tx.inner.as_mut(),
		observer.as_ref(),
		&quoted_table_name,
		&required_fencing_counter_columns(),
	)
	.await?;
	validate_key_conflict_arbiter(tx.inner.as_mut(), observer.as_ref(), &quoted_table_name).await?;
	validate_required_fencing_counter_check_constraints(
		tx.inner.as_mut(),
		observer.as_ref(),
		&quoted_table_name,
	)
	.await
}

pub(in crate::fleet::lease) fn validate_distinct_table_names(
	config: &StoreConfig,
) -> Result<(), SchemaError> {
	if pg_table_name_set_contains_duplicate(&[&config.claim_table, &config.fencing_counter_table]) {
		return Err(SchemaError::mismatch(
			"lease state and fencing counter table names must be distinct",
		));
	}
	Ok(())
}

async fn validate_required_columns<'e, E>(
	executor: E,
	observer: Option<&DatabaseOperationObserver>,
	quoted_table_name: &str,
	required_columns: &[RequiredColumn],
) -> Result<(), SchemaError>
where
	E: Executor<'e, Database = Postgres>,
{
	let statement = PG_COLUMN_METADATA_SQL;
	record_database_operation(
		observer,
		DatabaseOperationKind::FetchAll,
		LEASE_SCHEMA_VALIDATE_COLUMNS_LABEL,
		Some(statement),
	);
	let actual_columns =
		owned_portable_query_as::<(String, String, bool, Option<String>, bool)>(statement)
			.bind_validated(quoted_table_name)
			.fetch_all(executor)
			.await
			.map_err(|error| error.with_operation_label(LEASE_SCHEMA_VALIDATE_COLUMNS_LABEL))?
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

	for required_column in required_columns {
		let required_column_name = required_column.column.name();
		let Some(actual_column) = actual_columns
			.iter()
			.find(|column| column.name == required_column_name)
		else {
			return Err(SchemaError::mismatch(format!(
				"required column {:?} not found on table {}",
				required_column_name, quoted_table_name
			)));
		};

		validate_required_column(quoted_table_name, *required_column, actual_column)?;
	}

	Ok(())
}

async fn validate_key_conflict_arbiter<'e, E>(
	executor: E,
	observer: Option<&DatabaseOperationObserver>,
	quoted_table_name: &str,
) -> Result<(), SchemaError>
where
	E: Executor<'e, Database = Postgres>,
{
	let statement = PG_USABLE_SINGLE_COLUMN_UNIQUE_INDEX_EXISTS_SQL;
	record_database_operation(
		observer,
		DatabaseOperationKind::FetchOne,
		LEASE_SCHEMA_VALIDATE_KEY_CONFLICT_ARBITER_LABEL,
		Some(statement),
	);
	let has_usable_unique_key = owned_portable_query_scalar::<bool>(statement)
		.bind_validated(quoted_table_name)
		.bind_validated(LeaseColumn::LeaseKey.name())
		.fetch_one(executor)
		.await
		.map_err(|error| {
			error.with_operation_label(LEASE_SCHEMA_VALIDATE_KEY_CONFLICT_ARBITER_LABEL)
		})?;

	if !has_usable_unique_key {
		let key = LeaseColumn::LeaseKey.name();
		return Err(SchemaError::mismatch(format!(
			r#"column "{key}" must have a unique or primary key constraint usable by ON CONFLICT ({key})"#
		)));
	}

	Ok(())
}

async fn fetch_normalized_check_constraint_expressions<'e, E>(
	executor: E,
	observer: Option<&DatabaseOperationObserver>,
	quoted_table_name: &str,
) -> Result<Vec<String>, SchemaError>
where
	E: Executor<'e, Database = Postgres>,
{
	let statement = crate::db::catalog::BUILTIN_CHECK_EXPRESSIONS_SQL;
	record_database_operation(
		observer,
		DatabaseOperationKind::FetchAll,
		LEASE_SCHEMA_VALIDATE_CHECK_CONSTRAINTS_LABEL,
		Some(statement),
	);
	let normalized_check_expressions = owned_portable_query_scalar::<String>(statement)
		.bind_validated(quoted_table_name)
		.fetch_all(executor)
		.await
		.map_err(|error| error.with_operation_label(LEASE_SCHEMA_VALIDATE_CHECK_CONSTRAINTS_LABEL))?
		.into_iter()
		.map(|expression| crate::db::catalog::normalize_builtin_check_expression(&expression))
		.collect::<Vec<String>>();

	Ok(normalized_check_expressions)
}

async fn validate_required_state_check_constraints<'e, E>(
	executor: E,
	observer: Option<&DatabaseOperationObserver>,
	quoted_table_name: &str,
) -> Result<(), SchemaError>
where
	E: Executor<'e, Database = Postgres>,
{
	let normalized_check_expressions =
		fetch_normalized_check_constraint_expressions(executor, observer, quoted_table_name)
			.await?;

	for column in LEASE_STATE_CHECKED_COLUMNS {
		validate_required_check_constraint(
			quoted_table_name,
			&normalized_check_expressions,
			column,
		)?;
	}

	Ok(())
}

async fn validate_required_fencing_counter_check_constraints<'e, E>(
	executor: E,
	observer: Option<&DatabaseOperationObserver>,
	quoted_table_name: &str,
) -> Result<(), SchemaError>
where
	E: Executor<'e, Database = Postgres>,
{
	let normalized_check_expressions =
		fetch_normalized_check_constraint_expressions(executor, observer, quoted_table_name)
			.await?;

	for column in LEASE_FENCING_COUNTER_CHECKED_COLUMNS {
		validate_required_check_constraint(
			quoted_table_name,
			&normalized_check_expressions,
			column,
		)?;
	}

	Ok(())
}

fn validate_required_check_constraint(
	quoted_table_name: &str,
	normalized_check_expressions: &[String],
	column: LeaseColumn,
) -> Result<(), SchemaError> {
	let required_expression = column
		.normalized_check_constraint()
		.expect("checked lease column must define a constraint");
	if normalized_check_expressions
		.iter()
		.any(|expression| expression == &required_expression)
	{
		return Ok(());
	}

	let expected_check = column
		.human_check_constraint()
		.expect("checked lease column must define a constraint");
	Err(SchemaError::mismatch(format!(
		"table {} must enforce CHECK ({})",
		quoted_table_name, expected_check
	)))
}

async fn validate_expires_at_index<'e, E>(
	executor: E,
	observer: Option<&DatabaseOperationObserver>,
	config: &StoreConfig,
	quoted_table_name: &str,
) -> Result<(), SchemaError>
where
	E: Executor<'e, Database = Postgres>,
{
	let expires_at = LeaseColumn::ExpiresAt.name();
	let index_name = migration_index_identifier(config, expires_at);
	let statement = PG_SINGLE_COLUMN_INDEX_EXISTS_SQL;
	record_database_operation(
		observer,
		DatabaseOperationKind::FetchOne,
		LEASE_SCHEMA_VALIDATE_EXPIRES_AT_INDEX_LABEL,
		Some(statement),
	);
	let has_index = owned_portable_query_scalar::<bool>(statement)
		.bind_validated(quoted_table_name)
		.bind_validated(index_name.as_str())
		.bind_validated(expires_at)
		.bind_validated(TIMESTAMPTZ_OPERATOR_CLASS)
		.fetch_one(executor)
		.await
		.map_err(|error| {
			error.with_operation_label(LEASE_SCHEMA_VALIDATE_EXPIRES_AT_INDEX_LABEL)
		})?;

	if !has_index {
		return Err(SchemaError::mismatch(format!(
			"table {} must have {} index {:?}",
			quoted_table_name,
			expires_at,
			index_name.as_str()
		)));
	}

	Ok(())
}

fn validate_required_column(
	quoted_table_name: &str,
	required_column: RequiredColumn,
	actual_column: &ActualColumn,
) -> Result<(), SchemaError> {
	let column_name = required_column.column.name();
	let required_data_type = required_column.column.validation_type();
	let allowed_collations = required_column.column.allowed_collations();

	if actual_column.data_type != required_data_type {
		return Err(SchemaError::mismatch(format!(
			"column {:?} on table {} must be type {:?}, got {:?}",
			column_name, quoted_table_name, required_data_type, actual_column.data_type
		)));
	}

	if !actual_column.writable {
		return Err(SchemaError::mismatch(format!(
			"column {:?} on table {} must accept explicit writes",
			column_name, quoted_table_name
		)));
	}

	if !actual_column.not_null {
		let expected_nullability = "NOT NULL";
		let actual_nullability = if actual_column.not_null {
			"NOT NULL"
		} else {
			"NULL"
		};
		return Err(SchemaError::mismatch(format!(
			"column {:?} on table {} must be {}, got {}",
			column_name, quoted_table_name, expected_nullability, actual_nullability
		)));
	}

	if !allowed_collations.is_empty() {
		let actual_collation = actual_column.collation.as_deref().unwrap_or("<none>");
		if !allowed_collations.contains(&actual_collation) {
			return Err(SchemaError::mismatch(format!(
				"column {:?} on table {} must use one of collations {:?}, got {:?}",
				column_name, quoted_table_name, allowed_collations, actual_collation
			)));
		}
	}

	Ok(())
}

fn required_state_columns() -> [RequiredColumn; 6] {
	LEASE_STATE_COLUMNS.map(|column| RequiredColumn { column })
}

fn required_fencing_counter_columns() -> [RequiredColumn; 3] {
	LEASE_FENCING_COUNTER_COLUMNS.map(|column| RequiredColumn { column })
}
