use super::rows::RowCtx;
use super::schema_model::{
	ActualColumn, QueueColumn, QueueIndexDef, QueueSchemaLayout, QueueSchemaTableBinding,
	QueueTable, RequiredColumn,
};
use super::sql::*;
use crate::db::{
	DatabaseOperationKind, PgIdentifier, PgQualifiedTableName, SchemaError, Tx,
	owned_portable_query, owned_portable_query_as, owned_portable_query_scalar,
};
use crate::queue::{
	GLOBAL_PAUSE_KEY, JOB_ID_LEN, JobStatus, MAX_DEDUPE_KEY_LEN, MAX_TASK_NAME_LEN,
	MAX_WORKER_OWNER_ID_LEN, QUEUE_SCHEMA_VALIDATE_NAMED_INDEX_LABEL,
	QUEUE_SCHEMA_VALIDATE_PRIMARY_KEY_LABEL, QUEUE_SCHEMA_VALIDATE_TABLE_CHECKS_LABEL,
	QUEUE_SCHEMA_VALIDATE_TABLE_COLUMNS_LABEL, WORKER_CLAIM_TOKEN_LEN,
};

pub(super) async fn validate_published_v10_schema_in_current_transaction(
	tx: &mut Tx<'_>,
	tables: &impl QueueSchemaTableBinding,
	source_layout: crate::queue::PublishedV10QueueLayout,
) -> Result<(), SchemaError> {
	validate_physical_contract(
		tx,
		tables,
		QueueSchemaLayout::SourceEpoch,
		source_layout == crate::queue::PublishedV10QueueLayout::PreparedV2,
	)
	.await
}

async fn validate_physical_contract(
	tx: &mut Tx<'_>,
	tables: &impl QueueSchemaTableBinding,
	layout: QueueSchemaLayout,
	require_claim_token: bool,
) -> Result<(), SchemaError> {
	let job_columns: Vec<_> = required_job_columns()
		.into_iter()
		.filter(|column| require_claim_token || column.column != QueueColumn::WorkerClaimToken)
		.collect();
	validate_table_columns(
		tx,
		QueueTable::Jobs.table_name(tables),
		&job_columns,
		layout,
		!require_claim_token,
	)
	.await?;
	validate_table_columns(
		tx,
		QueueTable::DeadLetter.table_name(tables),
		&required_dead_letter_columns(),
		layout,
		false,
	)
	.await?;
	validate_table_columns(
		tx,
		QueueTable::Pause.table_name(tables),
		&required_pause_columns(),
		layout,
		false,
	)
	.await?;
	validate_table_checks(tx, tables, require_claim_token).await?;
	validate_primary_keys(tx, tables).await?;
	validate_required_indexes(tx, tables).await
}

async fn validate_table_columns(
	tx: &mut Tx<'_>,
	table_name: &PgQualifiedTableName,
	required_columns: &[RequiredColumn],
	layout: QueueSchemaLayout,
	forbid_claim_token: bool,
) -> Result<(), SchemaError> {
	let statement = crate::db::PG_COLUMN_METADATA_SQL;
	tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		QUEUE_SCHEMA_VALIDATE_TABLE_COLUMNS_LABEL,
		Some(statement),
	);
	let actual_columns =
		owned_portable_query_as::<(String, String, bool, Option<String>, bool)>(statement)
			.bind_validated(table_name.quoted().to_string())
			.fetch_all(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(QUEUE_SCHEMA_VALIDATE_TABLE_COLUMNS_LABEL))?
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

	if forbid_claim_token
		&& actual_columns
			.iter()
			.any(|column| column.name == QueueColumn::WorkerClaimToken.name())
	{
		return Err(SchemaError::mismatch(
			"published-v10 Queue source v1 has unrecorded claim-token preparation",
		));
	}
	for required_column in required_columns {
		let required_column_name = required_column.column.name_in(layout);
		let Some(actual_column) = actual_columns
			.iter()
			.find(|column| column.name == required_column_name)
		else {
			return Err(SchemaError::mismatch(format!(
				"table {} is missing column {}",
				table_name.quoted(),
				required_column_name
			)));
		};
		validate_required_column(table_name, *required_column, actual_column, layout)?;
	}

	Ok(())
}

/// A CHECK constraint that a Queue table must carry.
struct RequiredCheck {
	/// The constraint's name, or `None` for a column constraint that PostgreSQL names.
	name: Option<PgIdentifier>,
	/// The expression as PostgreSQL prints it when no other object shadows a built-in name.
	expression: String,
}

fn required_checks(tables: &impl QueueSchemaTableBinding, table: QueueTable) -> Vec<RequiredCheck> {
	let id_length = format!("(octet_length(id) = {JOB_ID_LEN})");
	let numeric =
		"((retry_count >= 0) AND (max_retries >= 0) AND (timeout_nanos >= '-1'::integer))"
			.to_owned();
	let task_name_checks = format!(
		"(task_name ~ '^[A-Za-z0-9_][A-Za-z0-9_.-]*$'::text) AND (octet_length(task_name) <= {MAX_TASK_NAME_LEN})"
	);
	let dedupe_key_check = format!(
		"((dedupe_key IS NULL) OR ((dedupe_key <> ''::text) AND (octet_length(dedupe_key) <= {MAX_DEDUPE_KEY_LEN})))"
	);
	let running = JobStatus::Running.as_str();
	let pending = JobStatus::Pending.as_str();
	match table {
		QueueTable::Jobs => vec![
			RequiredCheck {
				name: None,
				expression: id_length,
			},
			RequiredCheck {
				name: Some(job_status_constraint_identifier(tables)),
				expression: "(status = ANY (ARRAY['pending'::text, 'running'::text, 'completed'::text, 'failed'::text]))".to_owned(),
			},
			RequiredCheck {
				name: Some(job_numeric_constraint_identifier(tables)),
				expression: numeric,
			},
			RequiredCheck {
				name: Some(job_claim_token_constraint_identifier(tables)),
				expression: format!(
					"((worker_claim_token IS NULL) OR ((octet_length(worker_claim_token) = {WORKER_CLAIM_TOKEN_LEN}) AND (status = '{running}'::text) AND (worker_id IS NOT NULL)))"
				),
			},
			RequiredCheck {
				name: Some(job_text_constraint_identifier(tables)),
				expression: format!(
					"({task_name_checks} AND {dedupe_key_check} AND ((worker_id IS NULL) OR ((worker_id <> ''::text) AND (octet_length(worker_id) <= {MAX_WORKER_OWNER_ID_LEN}))))"
				),
			},
			RequiredCheck {
				name: Some(job_lifecycle_constraint_identifier(tables)),
				expression: format!(
					"(((status = '{pending}'::text) AND (worker_id IS NULL) AND (claimed_by_worker_at IS NULL) AND (execution_started_at IS NULL) AND (execution_heartbeat_at IS NULL) AND (finished_at IS NULL)) OR ((status = '{running}'::text) AND (worker_id IS NOT NULL) AND (claimed_by_worker_at IS NOT NULL) AND (execution_heartbeat_at IS NOT NULL) AND (finished_at IS NULL)) OR ((status = ANY (ARRAY['completed'::text, 'failed'::text])) AND (worker_id IS NULL) AND (claimed_by_worker_at IS NULL) AND (execution_started_at IS NULL) AND (execution_heartbeat_at IS NULL) AND (finished_at IS NOT NULL)))"
				),
			},
		],
		QueueTable::DeadLetter => vec![
			RequiredCheck {
				name: None,
				expression: id_length,
			},
			RequiredCheck {
				name: None,
				expression: format!("(octet_length(original_job_id) = {JOB_ID_LEN})"),
			},
			RequiredCheck {
				name: Some(dead_letter_numeric_constraint_identifier(tables)),
				expression: numeric,
			},
			RequiredCheck {
				name: Some(dead_letter_text_constraint_identifier(tables)),
				expression: format!("({task_name_checks} AND {dedupe_key_check})"),
			},
			RequiredCheck {
				name: Some(dead_letter_reason_constraint_identifier(tables)),
				expression: "(reason = ANY (ARRAY['max_retries_exceeded'::text, 'permanent_error'::text, 'operator_action'::text, 'execution_expired'::text]))".to_owned(),
			},
		],
		QueueTable::Pause => vec![
			RequiredCheck {
				name: Some(pause_key_task_constraint_identifier(tables)),
				expression: format!(
					"(((key = '{GLOBAL_PAUSE_KEY}'::text) AND (task_name IS NULL)) OR ((task_name IS NOT NULL) AND (key = ('task:'::text || task_name))))"
				),
			},
			RequiredCheck {
				name: Some(pause_text_constraint_identifier(tables)),
				expression: format!(
					"((task_name IS NULL) OR ((task_name IS NOT NULL) AND (task_name ~ '^[A-Za-z0-9_][A-Za-z0-9_.-]*$'::text) AND (octet_length(task_name) <= {MAX_TASK_NAME_LEN})))"
				),
			},
		],
	}
}

/// One CHECK constraint of a table, as the catalog reports it.
struct ActualCheck {
	name: String,
	expression: String,
	/// The constraint is validated and depends on nothing but its own table and pinned
	/// built-ins. The catalog omits dependencies on pinned objects, so any other dependency
	/// is a function, operator, type, or collation that could accept what Paranoid rejects.
	validated_and_built_in: bool,
}

/// Reads the CHECK constraints of each table and requires every one that Paranoid writes,
/// exactly: the same expression, validated, built-in only, and under the same name when
/// Paranoid names it. Other constraints may coexist.
async fn validate_table_checks(
	tx: &mut Tx<'_>,
	tables: &impl QueueSchemaTableBinding,
	require_claim_token: bool,
) -> Result<(), SchemaError> {
	for table in [QueueTable::Jobs, QueueTable::DeadLetter, QueueTable::Pause] {
		let table_name = table.table_name(tables);
		let statement = r#"
        SELECT con.conname::pg_catalog.text,
               pg_catalog.pg_get_expr(con.conbin, con.conrelid),
               con.convalidated AND NOT EXISTS (
                   SELECT 1 FROM pg_catalog.pg_depend AS dep
                   WHERE dep.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_constraint'::pg_catalog.regclass
                     AND dep.objid OPERATOR(pg_catalog.=) con.oid
                     AND NOT (
                         dep.refclassid OPERATOR(pg_catalog.=) 'pg_catalog.pg_class'::pg_catalog.regclass
                         AND dep.refobjid OPERATOR(pg_catalog.=) con.conrelid
                     )
               )
        FROM pg_catalog.pg_constraint AS con
        JOIN pg_catalog.pg_class AS cls ON cls.oid OPERATOR(pg_catalog.=) con.conrelid
        JOIN pg_catalog.pg_namespace AS ns ON ns.oid OPERATOR(pg_catalog.=) cls.relnamespace
        WHERE ns.nspname OPERATOR(pg_catalog.=) $1
          AND cls.relname OPERATOR(pg_catalog.=) $2
          AND con.contype OPERATOR(pg_catalog.=) 'c'
        "#;
		tx.record_database_operation(
			DatabaseOperationKind::FetchAll,
			QUEUE_SCHEMA_VALIDATE_TABLE_CHECKS_LABEL,
			Some(statement),
		);
		let actual = owned_portable_query_as::<(String, Option<String>, bool)>(statement)
			.bind_validated(table_name.schema().as_str())
			.bind_validated(table_name.table().as_str())
			.fetch_all(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(QUEUE_SCHEMA_VALIDATE_TABLE_CHECKS_LABEL))?
			.into_iter()
			.filter_map(|(name, expression, validated_and_built_in)| {
				Some(ActualCheck {
					name,
					expression: normalize_catalog_expression(&expression?),
					validated_and_built_in,
				})
			})
			.collect::<Vec<_>>();
		for required in required_checks(tables, table) {
			if !require_claim_token
				&& table == QueueTable::Jobs
				&& required.name.as_ref() == Some(&job_claim_token_constraint_identifier(tables))
			{
				continue;
			}
			let expected = normalize_catalog_expression(&required.expression);
			let matching = actual.iter().any(|check| {
				check.validated_and_built_in
					&& check.expression == expected
					&& required
						.name
						.as_ref()
						.is_none_or(|name| name.as_str() == check.name)
			});
			if matching {
				continue;
			}
			let Some(name) = &required.name else {
				return Err(SchemaError::mismatch(format!(
					"table {} is missing check constraint {}",
					table_name.quoted(),
					required.expression
				)));
			};
			return Err(SchemaError::mismatch(
				match actual.iter().find(|check| check.name == name.as_str()) {
					None => format!(
						"table {} is missing check constraint {}",
						table_name.quoted(),
						name.quoted()
					),
					Some(check) if !check.validated_and_built_in => format!(
						"table {} check constraint {} is not validated or depends on an object other than its table",
						table_name.quoted(),
						name.quoted()
					),
					Some(_) => format!(
						"table {} check constraint {} has incompatible definition",
						table_name.quoted(),
						name.quoted()
					),
				},
			));
		}
	}
	Ok(())
}

/// Collapses whitespace outside quotes and removes the `pg_catalog.` qualifications that
/// PostgreSQL prints when another object of the same name is visible first, including
/// `OPERATOR(pg_catalog.<operator>)`. Text inside quotes is kept byte for byte. Callers use
/// it only on expressions that depend on no object that is not built in, which makes
/// removing the qualifications sound.
enum CatalogSpacing {
	Adjacent,
	Separated,
}

fn normalize_catalog_expression(expression: &str) -> String {
	const QUALIFIED_OPERATOR: &[char] = &[
		'O', 'P', 'E', 'R', 'A', 'T', 'O', 'R', '(', 'p', 'g', '_', 'c', 'a', 't', 'a', 'l', 'o',
		'g', '.',
	];
	const QUALIFIER: &[char] = &['p', 'g', '_', 'c', 'a', 't', 'a', 'l', 'o', 'g', '.'];
	let characters = expression.chars().collect::<Vec<_>>();
	let mut normalized = String::with_capacity(expression.len());
	let mut index = 0;
	let mut spacing = CatalogSpacing::Adjacent;
	while index < characters.len() {
		let character = characters[index];
		if character.is_whitespace() {
			spacing = CatalogSpacing::Separated;
			index += 1;
			continue;
		}
		let rest = &characters[index..];
		let previous_is_identifier = matches!(spacing, CatalogSpacing::Adjacent)
			&& normalized.chars().last().is_some_and(|previous| {
				previous.is_alphanumeric() || previous == '_' || previous == '$'
			});
		if rest.starts_with(QUALIFIED_OPERATOR) && !previous_is_identifier {
			if matches!(spacing, CatalogSpacing::Separated) {
				normalized.push(' ');
				spacing = CatalogSpacing::Adjacent;
			}
			index += QUALIFIED_OPERATOR.len();
			while index < characters.len() && characters[index] != ')' {
				normalized.push(characters[index]);
				index += 1;
			}
			index += 1;
			continue;
		}
		if rest.starts_with(QUALIFIER) && !previous_is_identifier {
			index += QUALIFIER.len();
			continue;
		}
		if matches!(spacing, CatalogSpacing::Separated) {
			normalized.push(' ');
			spacing = CatalogSpacing::Adjacent;
		}
		normalized.push(character);
		index += 1;
		if character == '\'' || character == '"' {
			while index < characters.len() {
				normalized.push(characters[index]);
				index += 1;
				if characters[index - 1] == character {
					if characters.get(index) == Some(&character) {
						normalized.push(character);
						index += 1;
					} else {
						break;
					}
				}
			}
		}
	}
	normalized
}

/// Each table needs a plain primary key: an immediate, ascending, built-in btree index on the
/// one key column, with the column's own collation and no predicate or expression.
async fn validate_primary_keys(
	tx: &mut Tx<'_>,
	tables: &impl QueueSchemaTableBinding,
) -> Result<(), SchemaError> {
	let statement = format!(
		r#"
        SELECT EXISTS (
            SELECT 1
            FROM pg_catalog.pg_index idx
            JOIN pg_catalog.pg_class table_class ON table_class.oid OPERATOR(pg_catalog.=) idx.indrelid
            JOIN pg_catalog.pg_namespace table_namespace ON table_namespace.oid OPERATOR(pg_catalog.=) table_class.relnamespace
            JOIN pg_catalog.pg_class index_class ON index_class.oid OPERATOR(pg_catalog.=) idx.indexrelid
            JOIN pg_catalog.pg_constraint con
              ON con.conindid OPERATOR(pg_catalog.=) idx.indexrelid
             AND con.conrelid OPERATOR(pg_catalog.=) idx.indrelid
            JOIN pg_catalog.pg_attribute attr
              ON attr.attrelid OPERATOR(pg_catalog.=) idx.indrelid
             AND attr.attnum OPERATOR(pg_catalog.=) idx.indkey[0]
             AND NOT attr.attisdropped
            WHERE table_namespace.nspname OPERATOR(pg_catalog.=) $1
              AND table_class.relname OPERATOR(pg_catalog.=) $2
              AND attr.attname OPERATOR(pg_catalog.=) $3
              AND idx.indisprimary
              AND con.contype OPERATOR(pg_catalog.=) 'p'
              AND con.convalidated
              AND NOT con.condeferrable
              AND idx.indnkeyatts OPERATOR(pg_catalog.=) 1
              AND idx.indexprs IS NULL
              AND idx.indpred IS NULL
              AND idx.indoption[0] OPERATOR(pg_catalog.=) 0
              AND idx.indcollation[0] OPERATOR(pg_catalog.=) attr.attcollation
              {builtin_btree_index_conditions}
        )
        "#,
		builtin_btree_index_conditions = crate::db::builtin_btree_index_conditions!()
	);
	for (table, key_column) in [
		(QueueTable::Jobs, QueueColumn::Id),
		(QueueTable::DeadLetter, QueueColumn::Id),
		(QueueTable::Pause, QueueColumn::Key),
	] {
		let table_name = table.table_name(tables);
		tx.record_database_operation(
			DatabaseOperationKind::FetchOne,
			QUEUE_SCHEMA_VALIDATE_PRIMARY_KEY_LABEL,
			Some(statement.as_str()),
		);
		let valid = owned_portable_query_scalar::<bool>(sqlx::AssertSqlSafe(statement.as_str()))
			.bind_validated(table_name.schema().as_str())
			.bind_validated(table_name.table().as_str())
			.bind_validated(key_column.name())
			.fetch_one(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(QUEUE_SCHEMA_VALIDATE_PRIMARY_KEY_LABEL))?;
		if !valid {
			return Err(SchemaError::mismatch(format!(
				"table {} is missing a plain primary key on {}",
				table_name.quoted(),
				key_column.name()
			)));
		}
	}
	Ok(())
}

async fn validate_required_indexes(
	tx: &mut Tx<'_>,
	tables: &impl QueueSchemaTableBinding,
) -> Result<(), SchemaError> {
	for required_index in queue_schema_index_defs()
		.into_iter()
		.map(|def| RequiredQueueIndex::from_def(tables, def))
	{
		validate_required_index(tx, required_index).await?;
	}
	Ok(())
}

struct RequiredQueueIndex<'a> {
	table_name: &'a PgQualifiedTableName,
	index_name: PgIdentifier,
	columns: Vec<&'static str>,
	predicate: Option<String>,
	unique: bool,
}

impl<'a> RequiredQueueIndex<'a> {
	fn from_def(tables: &'a impl QueueSchemaTableBinding, def: QueueIndexDef) -> Self {
		let table_name = def.table.table_name(tables);
		Self {
			table_name,
			index_name: migration_index_identifier(def.kind, table_name, def.suffix),
			columns: def.columns.iter().map(|column| column.name()).collect(),
			predicate: def
				.predicate
				.catalog_expression()
				.map(normalize_catalog_expression),
			unique: def.unique,
		}
	}
}

#[derive(Clone, Copy)]
enum QueueIndexValidationField {
	Column0,
	Column1,
	Column2,
	Predicate,
}

impl QueueIndexValidationField {
	const fn name(self) -> &'static str {
		match self {
			Self::Column0 => "column_0",
			Self::Column1 => "column_1",
			Self::Column2 => "column_2",
			Self::Predicate => "predicate",
		}
	}
}

/// Checks each required B-tree index's physical contract through catalog reads.
/// Each key must use its type's built-in default operator class to support the queue's
/// comparisons and ordering. Unique indexes must enforce uniqueness immediately so that
/// PostgreSQL can use them as `ON CONFLICT` arbiters.
async fn validate_required_index(
	tx: &mut Tx<'_>,
	required_index: RequiredQueueIndex<'_>,
) -> Result<(), SchemaError> {
	let column_0 = QueueIndexValidationField::Column0.name();
	let column_1 = QueueIndexValidationField::Column1.name();
	let column_2 = QueueIndexValidationField::Column2.name();
	let predicate = QueueIndexValidationField::Predicate.name();
	let statement = format!(
		r#"
        SELECT
            attr0.attname AS {column_0},
            attr1.attname AS {column_1},
            attr2.attname AS {column_2},
            pg_catalog.pg_get_expr(idx.indpred, idx.indrelid) AS {predicate}
        FROM pg_catalog.pg_index idx
        JOIN pg_catalog.pg_class table_class ON table_class.oid OPERATOR(pg_catalog.=) idx.indrelid
        JOIN pg_catalog.pg_namespace table_namespace ON table_namespace.oid OPERATOR(pg_catalog.=) table_class.relnamespace
        JOIN pg_catalog.pg_class index_class ON index_class.oid OPERATOR(pg_catalog.=) idx.indexrelid
        LEFT JOIN pg_catalog.pg_attribute attr0
          ON attr0.attrelid OPERATOR(pg_catalog.=) idx.indrelid
         AND attr0.attnum OPERATOR(pg_catalog.=) idx.indkey[0]
         AND NOT attr0.attisdropped
        LEFT JOIN pg_catalog.pg_attribute attr1
          ON attr1.attrelid OPERATOR(pg_catalog.=) idx.indrelid
         AND attr1.attnum OPERATOR(pg_catalog.=) idx.indkey[1]
         AND NOT attr1.attisdropped
        LEFT JOIN pg_catalog.pg_attribute attr2
          ON attr2.attrelid OPERATOR(pg_catalog.=) idx.indrelid
         AND attr2.attnum OPERATOR(pg_catalog.=) idx.indkey[2]
         AND NOT attr2.attisdropped
        WHERE table_namespace.nspname OPERATOR(pg_catalog.=) $1
          AND table_class.relname OPERATOR(pg_catalog.=) $2
          AND index_class.relname OPERATOR(pg_catalog.=) $3
          AND idx.indisunique OPERATOR(pg_catalog.=) $4
          AND (NOT idx.indisunique OR idx.indimmediate)
          AND idx.indnkeyatts OPERATOR(pg_catalog.=) $5
          AND idx.indexprs IS NULL
          AND NOT EXISTS (
              SELECT 1
              FROM pg_catalog.generate_series(0, idx.indnkeyatts OPERATOR(pg_catalog.-) 1) AS key_position(position)
              JOIN pg_catalog.pg_attribute AS key_attr
                ON key_attr.attrelid OPERATOR(pg_catalog.=) idx.indrelid
               AND key_attr.attnum OPERATOR(pg_catalog.=) idx.indkey[key_position.position]
              LEFT JOIN pg_catalog.pg_opclass AS opclass
                ON opclass.oid OPERATOR(pg_catalog.=) idx.indclass[key_position.position]
              LEFT JOIN pg_catalog.pg_namespace AS opclass_namespace
                ON opclass_namespace.oid OPERATOR(pg_catalog.=) opclass.opcnamespace
              WHERE idx.indoption[key_position.position] OPERATOR(pg_catalog.<>) 0
                 OR idx.indcollation[key_position.position] OPERATOR(pg_catalog.<>) key_attr.attcollation
                 OR opclass.oid IS NULL
                 OR opclass_namespace.nspname IS NULL
                 OR opclass_namespace.nspname OPERATOR(pg_catalog.<>) 'pg_catalog'
                 OR opclass.opcmethod OPERATOR(pg_catalog.<>) index_class.relam
                 OR opclass.opcintype OPERATOR(pg_catalog.<>) key_attr.atttypid
                 OR NOT opclass.opcdefault
          )
          {builtin_btree_index_conditions}
        "#,
		builtin_btree_index_conditions = crate::db::builtin_btree_index_conditions!()
	);
	tx.record_database_operation(
		DatabaseOperationKind::FetchOptional,
		QUEUE_SCHEMA_VALIDATE_NAMED_INDEX_LABEL,
		Some(statement.as_str()),
	);
	let row = owned_portable_query(sqlx::AssertSqlSafe(statement.as_str()))
		.bind_validated(required_index.table_name.schema().as_str())
		.bind_validated(required_index.table_name.table().as_str())
		.bind_validated(required_index.index_name.as_str())
		.bind_validated(required_index.unique)
		.bind_validated(
			i16::try_from(required_index.columns.len())
				.expect("required queue index arity must fit in i16"),
		)
		.fetch_optional(tx.inner.as_mut())
		.await
		.map_err(|error| error.with_operation_label(QUEUE_SCHEMA_VALIDATE_NAMED_INDEX_LABEL))?;

	let Some(row) = row else {
		return Err(SchemaError::mismatch(format!(
			"table {} is missing required index {}",
			required_index.table_name.quoted(),
			required_index.index_name.quoted()
		)));
	};

	let ctx = RowCtx {
		operation: QUEUE_SCHEMA_VALIDATE_NAMED_INDEX_LABEL,
		statement: statement.as_str(),
	};
	let actual_columns = [
		ctx.column::<Option<String>>(&row, QueueIndexValidationField::Column0.name())?,
		ctx.column::<Option<String>>(&row, QueueIndexValidationField::Column1.name())?,
		ctx.column::<Option<String>>(&row, QueueIndexValidationField::Column2.name())?,
	];
	let actual_columns = actual_columns
		.iter()
		.filter_map(Option::as_deref)
		.collect::<Vec<_>>();
	if actual_columns.as_slice() != required_index.columns.as_slice() {
		return Err(SchemaError::mismatch(format!(
			"index {} on table {} has incompatible columns",
			required_index.index_name.quoted(),
			required_index.table_name.quoted()
		)));
	}

	let actual_predicate = ctx
		.column::<Option<String>>(&row, QueueIndexValidationField::Predicate.name())?
		.map(|predicate| normalize_catalog_expression(&predicate));
	// The query proved that the index depends on no object that is not built in, which makes
	// removing the catalog's qualifications sound.
	if actual_predicate != required_index.predicate {
		return Err(SchemaError::mismatch(format!(
			"index {} on table {} has an incompatible predicate",
			required_index.index_name.quoted(),
			required_index.table_name.quoted()
		)));
	}

	Ok(())
}

fn validate_required_column(
	table_name: &PgQualifiedTableName,
	required_column: RequiredColumn,
	actual_column: &ActualColumn,
	layout: QueueSchemaLayout,
) -> Result<(), SchemaError> {
	let required_column_name = required_column.column.name_in(layout);
	let required_data_type = required_column.column.validation_type();
	if actual_column.data_type != required_data_type {
		return Err(SchemaError::mismatch(format!(
			"table {} column {} has type {}, expected {}",
			table_name.quoted(),
			required_column_name,
			actual_column.data_type,
			required_data_type
		)));
	}
	if !actual_column.writable {
		return Err(SchemaError::mismatch(format!(
			"table {} column {} must accept explicit writes",
			table_name.quoted(),
			required_column_name
		)));
	}
	if actual_column.not_null != required_column.not_null {
		return Err(SchemaError::mismatch(format!(
			"table {} column {} nullability does not match queue schema",
			table_name.quoted(),
			required_column_name
		)));
	}
	if required_column.column.requires_bytewise_collation()
		&& !matches!(actual_column.collation.as_deref(), Some("C" | "POSIX"))
	{
		return Err(SchemaError::mismatch(format!(
			"table {} column {} must use C/POSIX collation",
			table_name.quoted(),
			required_column_name
		)));
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::normalize_catalog_expression;

	#[test]
	fn qualifications_and_whitespace_outside_quotes_do_not_change_an_expression() {
		let declared = "retry_count >= 0 AND max_retries >= 0 AND timeout_nanos >= '-1'::integer";
		let printed = "(((retry_count OPERATOR(pg_catalog.>=) 0) AND (max_retries\n  >= 0)) AND (timeout_nanos >= '-1'::pg_catalog.integer))";
		assert_eq!(
			normalize_catalog_expression(
				"key = ('task:'::pg_catalog.text OPERATOR(pg_catalog.||) task_name)"
			),
			normalize_catalog_expression("key = ('task:'::text || task_name)")
		);
		assert_eq!(
			normalize_catalog_expression("pg_catalog.octet_length(id) OPERATOR(pg_catalog.=) 16"),
			normalize_catalog_expression("octet_length(id) = 16")
		);
		assert_eq!(
			normalize_catalog_expression("a   =\t1"),
			normalize_catalog_expression("a = 1")
		);
		assert_ne!(
			normalize_catalog_expression(declared),
			normalize_catalog_expression(printed),
			"parentheses that PostgreSQL prints are part of the expression text"
		);
	}

	#[test]
	fn text_inside_quotes_is_kept_byte_for_byte() {
		for (required, incompatible) in [
			("key = 'task:'", "key = 'task: '"),
			("key = 'can''t'", "key = 'cant'"),
			("status = 'pending'", "\"STATUS\" = 'pending'"),
			("key = 'a  b'", "key = 'a b'"),
			("key = 'pg_catalog.x'", "key = 'x'"),
		] {
			assert_ne!(
				normalize_catalog_expression(required),
				normalize_catalog_expression(incompatible),
				"{required} must differ from {incompatible}"
			);
		}
		assert_eq!(
			normalize_catalog_expression("key = 'can''t   pg_catalog.x'"),
			"key = 'can''t   pg_catalog.x'"
		);
	}

	#[test]
	fn objects_other_than_the_built_ins_keep_their_qualification() {
		for (required, incompatible) in [
			("octet_length(id) = 16", "custom.octet_length(id) = 16"),
			(
				"octet_length(id) = 16",
				"custom_pg_catalog.octet_length(id) = 16",
			),
			(
				"octet_length(id) = 16",
				"octet_length(id) OPERATOR(custom.=) 16",
			),
			("a = 1 AND b = 2", "a = 1 OR b = 2"),
			("a = 1 AND (b = 2 OR c = 3)", "(a = 1 AND b = 2) OR c = 3"),
			("octet_length(id) = 16", "octet_length(id) = 16 OR TRUE"),
			("timeout_nanos >= -1", "timeout_nanos >= -2"),
			("worker_id IS NULL", "worker_id IS NOT NULL"),
		] {
			assert_ne!(
				normalize_catalog_expression(required),
				normalize_catalog_expression(incompatible),
				"{required} must differ from {incompatible}"
			);
		}
	}
}
