//! Selection and listing of Queue jobs from their maintained projection fields, and the indexes
//! that selection uses.
//!
//! The due-job selection skips jobs that another transaction holds, which means that concurrent
//! workers select different jobs, and it keeps no lock; every other selection takes no lock. No
//! selection grants anything. The operation that follows reacquires each candidate and checks
//! its state again. The due-job query compares due times with the stable
//! statement start time, which lets PostgreSQL bound the index scan; a claim checks again with a
//! later database clock reading, at which every selected job is still due.
//!
//! Projection field names and text values are stored as lowercase hexadecimal. The queries name
//! the fields as literal constants so that PostgreSQL can match their expressions to the
//! indexes: `737461747573` is `status`, `72756e5f61745f6f725f6166746572` is
//! `run_at_or_after`, `657865637574696f6e5f6865617274626561745f6174` is
//! `execution_heartbeat_at`, `776f726b65725f6f776e65725f6964` is `worker_owner_id`,
//! `7461736b5f6e616d65` is `task_name`, `66696e69736865645f6174` is `finished_at`, and
//! `646561645f6c657474657265645f6174` is `dead_lettered_at`.

use crate::records::{RecordIdentity, RecordKey, RecordNamespace, StoredRecord};
use primitives_rs::encoding::binary_to_text::ByteEncoding;

use crate::db::PgIdentifier;
use crate::db::bytea::Bytea;
use crate::db::{
	AuditedSql, DatabaseError, DatabaseOperationKind, PgQualifiedTableName, SchemaError, Tx,
	WriteTx, owned_portable_query, owned_portable_query_as,
};
use crate::records::postgres::{PgPointStorage, StateStorageError, decode_physical_record};

const SELECT_DUE_QUEUE_JOBS_LABEL: &str = "db.state_storage.select_due_queue_jobs";
const SELECT_RUNNING_QUEUE_JOBS_LABEL: &str = "db.state_storage.select_running_queue_jobs";
const SELECT_RUNNING_QUEUE_JOBS_BY_OWNER_LABEL: &str =
	"db.state_storage.select_running_queue_jobs_by_owner";
const LIST_QUEUE_JOBS_LABEL: &str = "db.state_storage.list_queue_jobs";
const SELECT_FINISHED_QUEUE_JOBS_LABEL: &str = "db.state_storage.select_finished_queue_jobs";
const SELECT_EXPIRED_QUEUE_DEAD_LETTERS_LABEL: &str =
	"db.state_storage.select_expired_queue_dead_letters";
const CREATE_QUEUE_SELECTION_INDEX_LABEL: &str = "db.state_storage.create_queue_selection_index";
const VALIDATE_QUEUE_SELECTION_INDEX_LABEL: &str =
	"db.state_storage.validate_queue_selection_index";

/// Due pending jobs of the given tasks, excluding every job while the queue is paused and the
/// jobs of a paused task. A pause is a record in the pause namespace `$5`, under the key
/// `0x0100` for the queue or `0x0101` followed by the task name's bytes for a task. The pause
/// reads take no lock, as v10's claim reads its pauses. The selection locks inside a
/// savepoint that it rolls back: `SKIP LOCKED` leaves out the jobs that another transaction
/// holds, and the rollback releases the locks that the selection took, which means that the
/// claim then acquires each job's records in their canonical order.
fn due_query(table: &PgQualifiedTableName) -> AuditedSql {
	crate::audited_sql!(
		r#"
SAVEPOINT paranoid_due_selection;
SELECT key
FROM ONLY {table} AS job
WHERE namespace OPERATOR(pg_catalog.=) $1::pg_catalog.bytea
  AND (index_fields OPERATOR(pg_catalog.->) '737461747573'::pg_catalog.text
      OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text)
      OPERATOR(pg_catalog.=) $2::pg_catalog.text
  AND ((index_fields OPERATOR(pg_catalog.->) '72756e5f61745f6f725f6166746572'::pg_catalog.text
      OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text))::pg_catalog.int8
      OPERATOR(pg_catalog.<=) pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.statement_timestamp())
          OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8
  AND body IS NOT NULL
  AND (index_fields OPERATOR(pg_catalog.->) '7461736b5f6e616d65'::pg_catalog.text
      OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text)
      OPERATOR(pg_catalog.=) ANY ($3::pg_catalog.text[])
  AND NOT EXISTS (
      SELECT 1 FROM ONLY {table} AS pause
      WHERE pause.namespace OPERATOR(pg_catalog.=) $5::pg_catalog.bytea
        AND pause.body IS NOT NULL
        AND (pause.key OPERATOR(pg_catalog.=) pg_catalog.decode('0100', 'hex')
            OR pause.key OPERATOR(pg_catalog.=) (pg_catalog.decode('0101', 'hex')
                OPERATOR(pg_catalog.||) pg_catalog.decode(
                    job.index_fields OPERATOR(pg_catalog.->) '7461736b5f6e616d65'::pg_catalog.text
                        OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text,
                    'hex'))))
ORDER BY ((index_fields OPERATOR(pg_catalog.->) '72756e5f61745f6f725f6166746572'::pg_catalog.text
      OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text))::pg_catalog.int8,
  key USING OPERATOR(pg_catalog.<)
LIMIT $4::pg_catalog.int8
FOR UPDATE OF job SKIP LOCKED;
ROLLBACK TO SAVEPOINT paranoid_due_selection;
RELEASE SAVEPOINT paranoid_due_selection
"#,
		table = table
	)
}

fn running_query(table: &PgQualifiedTableName) -> AuditedSql {
	crate::audited_sql!(
		r#"
SELECT key
FROM ONLY {table}
WHERE namespace OPERATOR(pg_catalog.=) $1::pg_catalog.bytea
  AND (index_fields OPERATOR(pg_catalog.->) '737461747573'::pg_catalog.text
      OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text)
      OPERATOR(pg_catalog.=) $2::pg_catalog.text
  AND ((index_fields OPERATOR(pg_catalog.->) '657865637574696f6e5f6865617274626561745f6174'::pg_catalog.text
      OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text))::pg_catalog.int8
      OPERATOR(pg_catalog.<) GREATEST(-9223372036854775808::pg_catalog.numeric, (pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.statement_timestamp())
          OPERATOR(pg_catalog.*) 1000000) OPERATOR(pg_catalog.-) $4::pg_catalog.numeric))::pg_catalog.int8
  AND body IS NOT NULL
ORDER BY ((index_fields OPERATOR(pg_catalog.->) '657865637574696f6e5f6865617274626561745f6174'::pg_catalog.text
      OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text))::pg_catalog.int8,
  key USING OPERATOR(pg_catalog.<)
LIMIT $3::pg_catalog.int8
"#,
		table = table
	)
}

fn running_by_owner_query(table: &PgQualifiedTableName) -> AuditedSql {
	crate::audited_sql!(
		r#"
SELECT key
FROM ONLY {table}
WHERE namespace OPERATOR(pg_catalog.=) $1::pg_catalog.bytea
  AND (index_fields OPERATOR(pg_catalog.->) '737461747573'::pg_catalog.text
      OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text)
      OPERATOR(pg_catalog.=) $2::pg_catalog.text
  AND body IS NOT NULL
  AND (index_fields OPERATOR(pg_catalog.->) '776f726b65725f6f776e65725f6964'::pg_catalog.text
      OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text)
      OPERATOR(pg_catalog.=) $3::pg_catalog.text
ORDER BY key USING OPERATOR(pg_catalog.<)
"#,
		table = table
	)
}

fn finished_query(table: &PgQualifiedTableName) -> AuditedSql {
	crate::audited_sql!(
		r#"
SELECT key
FROM ONLY {table}
WHERE namespace OPERATOR(pg_catalog.=) $1::pg_catalog.bytea
  AND (index_fields OPERATOR(pg_catalog.->) '737461747573'::pg_catalog.text
      OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text)
      OPERATOR(pg_catalog.=) $2::pg_catalog.text
  AND ((index_fields OPERATOR(pg_catalog.->) '66696e69736865645f6174'::pg_catalog.text
      OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text))::pg_catalog.int8
      OPERATOR(pg_catalog.<) (pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.statement_timestamp())
          OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8 OPERATOR(pg_catalog.-) $3::pg_catalog.int8)
  AND body IS NOT NULL
ORDER BY ((index_fields OPERATOR(pg_catalog.->) '66696e69736865645f6174'::pg_catalog.text
      OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text))::pg_catalog.int8,
  key USING OPERATOR(pg_catalog.<)
LIMIT $4::pg_catalog.int8
"#,
		table = table
	)
}

fn expired_dead_letters_query(table: &PgQualifiedTableName) -> AuditedSql {
	crate::audited_sql!(
		r#"
SELECT key
FROM ONLY {table}
WHERE namespace OPERATOR(pg_catalog.=) $1::pg_catalog.bytea
  AND ((index_fields OPERATOR(pg_catalog.->) '646561645f6c657474657265645f6174'::pg_catalog.text
      OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text))::pg_catalog.int8
      OPERATOR(pg_catalog.<) (pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.statement_timestamp())
          OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8 OPERATOR(pg_catalog.-) $2::pg_catalog.int8)
  AND body IS NOT NULL
ORDER BY ((index_fields OPERATOR(pg_catalog.->) '646561645f6c657474657265645f6174'::pg_catalog.text
      OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text))::pg_catalog.int8,
  key USING OPERATOR(pg_catalog.<)
LIMIT $3::pg_catalog.int8
"#,
		table = table
	)
}

// Listing selects metadata in key order before joining payloads within the same SQL snapshot.
fn list_query(table: &PgQualifiedTableName) -> AuditedSql {
	crate::audited_sql!(
		r#"
WITH selected AS MATERIALIZED (
SELECT key, body, expires_at, updated_at, index_fields
FROM ONLY {table}
WHERE namespace OPERATOR(pg_catalog.=) $1::pg_catalog.bytea
  AND body IS NOT NULL
  AND ($2::pg_catalog.bytea IS NULL OR key OPERATOR(pg_catalog.>) $2::pg_catalog.bytea)
  AND (index_fields OPERATOR(pg_catalog.->) '737461747573'::pg_catalog.text
      OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text)
      OPERATOR(pg_catalog.=) ANY ($3::pg_catalog.text[])
  AND ($4::pg_catalog.text IS NULL
      OR (index_fields OPERATOR(pg_catalog.->) '7461736b5f6e616d65'::pg_catalog.text
          OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text)
          COLLATE pg_catalog."C" OPERATOR(pg_catalog.=) $4::pg_catalog.text)
ORDER BY key USING OPERATOR(pg_catalog.<)
LIMIT $5::pg_catalog.int8
)
SELECT selected.key, selected.body, selected.expires_at, selected.updated_at,
       selected.index_fields::pg_catalog.text,
       payload.key, payload.body, payload.expires_at, payload.updated_at,
       payload.index_fields::pg_catalog.text
FROM selected
LEFT JOIN ONLY {table} payload
  ON payload.namespace OPERATOR(pg_catalog.=) $1::pg_catalog.bytea
 AND payload.key OPERATOR(pg_catalog.=) ($6::pg_catalog.bytea OPERATOR(pg_catalog.||) selected.key)
ORDER BY selected.key USING OPERATOR(pg_catalog.<)
"#,
		table = table
	)
}

fn due_index_statement(table: &PgQualifiedTableName, index: &PgIdentifier) -> AuditedSql {
	crate::audited_sql!(
		r#"
CREATE INDEX {index} ON ONLY {table} USING btree (
    namespace,
    (index_fields OPERATOR(pg_catalog.->) '737461747573'::pg_catalog.text
        OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text),
    ((index_fields OPERATOR(pg_catalog.->) '72756e5f61745f6f725f6166746572'::pg_catalog.text
        OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text)::pg_catalog.int8),
    key
)
"#,
		index = index,
		table = table
	)
}

/// The task index is additive: older runtimes can use it without changing stored formats.
pub(crate) fn task_due_index_name() -> PgIdentifier {
	PgIdentifier::new("paranoid_queue_due_task").expect("the Queue task index name is valid")
}

pub(crate) fn task_due_index_statement(
	table: &PgQualifiedTableName,
	index: &PgIdentifier,
	allow_existing: bool,
) -> AuditedSql {
	let mut statement = crate::db::AuditedSqlBuilder::new();
	statement.push_text("CREATE INDEX ");
	if allow_existing {
		statement.push_text("IF NOT EXISTS ");
	}
	statement.push_argument(index);
	statement.push_text(" ON ONLY ");
	statement.push_argument(table);
	statement.push_text(
		r#"
 USING btree (
    namespace,
    (index_fields OPERATOR(pg_catalog.->) '7461736b5f6e616d65'::pg_catalog.text OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text),
    ((index_fields OPERATOR(pg_catalog.->) '72756e5f61745f6f725f6166746572'::pg_catalog.text OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text)::pg_catalog.int8),
    key
) WHERE body IS NOT NULL
    AND (index_fields OPERATOR(pg_catalog.->) '737461747573'::pg_catalog.text
        OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text)
        OPERATOR(pg_catalog.=) '70656e64696e67'::pg_catalog.text
"#,
	);
	statement.finish()
}

pub(crate) fn task_due_index_validation_check(
	table: &PgQualifiedTableName,
	index: &PgIdentifier,
) -> AuditedSql {
	let qualified_index = PgQualifiedTableName::new(table.schema().clone(), index.clone());
	let index_name = qualified_index.quoted().to_string();
	let table_name = table.quoted().to_string();
	crate::audited_sql!(
		r#"EXISTS (
            SELECT 1 FROM pg_catalog.pg_index idx
            JOIN pg_catalog.pg_class index_class ON index_class.oid OPERATOR(pg_catalog.=) idx.indexrelid
            JOIN pg_catalog.pg_am am ON am.oid OPERATOR(pg_catalog.=) index_class.relam
            WHERE idx.indexrelid OPERATOR(pg_catalog.=) pg_catalog.to_regclass(pg_catalog.convert_from({index_name}, 'UTF8'))
              AND idx.indrelid OPERATOR(pg_catalog.=) pg_catalog.to_regclass(pg_catalog.convert_from({table_name}, 'UTF8'))
              AND am.amname OPERATOR(pg_catalog.=) 'btree'
              AND index_class.relam OPERATOR(pg_catalog.=) 403
              AND idx.indisvalid AND idx.indisready AND idx.indislive AND NOT idx.indisunique
              AND NOT EXISTS (
                  SELECT 1
                  FROM pg_catalog.pg_depend dep
                  WHERE dep.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_class'::pg_catalog.regclass
                    AND dep.objid OPERATOR(pg_catalog.=) idx.indexrelid
                    AND NOT (
                        (dep.refclassid OPERATOR(pg_catalog.=) 'pg_catalog.pg_class'::pg_catalog.regclass
                         AND dep.refobjid OPERATOR(pg_catalog.=) idx.indrelid)
                        OR (dep.refclassid OPERATOR(pg_catalog.=) 'pg_catalog.pg_collation'::pg_catalog.regclass
                         AND dep.refobjid OPERATOR(pg_catalog.=) ANY (idx.indcollation))
                        OR (dep.refclassid OPERATOR(pg_catalog.=) 'pg_catalog.pg_constraint'::pg_catalog.regclass
                         AND EXISTS (
                             SELECT 1
                             FROM pg_catalog.pg_constraint con
                             WHERE con.oid OPERATOR(pg_catalog.=) dep.refobjid
                               AND con.conindid OPERATOR(pg_catalog.=) idx.indexrelid
                               AND con.conrelid OPERATOR(pg_catalog.=) idx.indrelid
                               AND con.contype OPERATOR(pg_catalog.=) ANY (ARRAY['p', 'u']::pg_catalog."char"[])
                         ))
                    )
              )
              AND idx.indnatts OPERATOR(pg_catalog.=) 4 AND idx.indnkeyatts OPERATOR(pg_catalog.=) 4
              AND idx.indoption OPERATOR(pg_catalog.=) '0 0 0 0'::pg_catalog.int2vector
              AND idx.indcollation[0] OPERATOR(pg_catalog.=) 0::pg_catalog.oid
              AND idx.indcollation[1] OPERATOR(pg_catalog.=) 'pg_catalog."default"'::pg_catalog.regcollation::pg_catalog.oid
              AND idx.indcollation[2] OPERATOR(pg_catalog.=) 0::pg_catalog.oid
              AND idx.indcollation[3] OPERATOR(pg_catalog.=) 0::pg_catalog.oid
              AND pg_catalog.regexp_replace(pg_catalog.pg_get_expr(idx.indpred, idx.indrelid, false), '[()]', '', 'g')
                  OPERATOR(pg_catalog.=) 'body IS NOT NULL AND index_fields -> ''737461747573''::text ->> ''value''::text = ''70656e64696e67''::text'
              AND pg_catalog.pg_get_indexdef(idx.indexrelid, 1, false) OPERATOR(pg_catalog.=) 'namespace'
              AND pg_catalog.pg_get_indexdef(idx.indexrelid, 4, false) OPERATOR(pg_catalog.=) 'key'
              AND pg_catalog.regexp_replace(pg_catalog.pg_get_indexdef(idx.indexrelid, 2, false), '[()]', '', 'g')
                  OPERATOR(pg_catalog.=) 'index_fields -> ''7461736b5f6e616d65''::text ->> ''value''::text'
              AND pg_catalog.regexp_replace(pg_catalog.pg_get_indexdef(idx.indexrelid, 3, false), '[()]', '', 'g')
                  OPERATOR(pg_catalog.=) 'index_fields -> ''72756e5f61745f6f725f6166746572''::text ->> ''value''::text::bigint'
              AND NOT EXISTS (
                  SELECT 1 FROM pg_catalog.unnest(idx.indclass::pg_catalog.oid[]) WITH ORDINALITY AS classes(oid, position)
                  JOIN pg_catalog.pg_opclass op ON op.oid OPERATOR(pg_catalog.=) classes.oid
                  WHERE op.opcnamespace OPERATOR(pg_catalog.<>) 'pg_catalog'::pg_catalog.regnamespace
                     OR op.opcname OPERATOR(pg_catalog.<>) CASE classes.position
                         WHEN 2 THEN 'text_ops'
                         WHEN 3 THEN 'int8_ops'
                         ELSE 'bytea_ops'
                     END
              )
        )"#,
		index_name = index_name.as_bytes(),
		table_name = table_name.as_bytes(),
	)
}

fn finished_index_statement(table: &PgQualifiedTableName, index: &PgIdentifier) -> AuditedSql {
	crate::audited_sql!(
		r#"
CREATE INDEX {index} ON ONLY {table} USING btree (
    namespace,
    (index_fields OPERATOR(pg_catalog.->) '737461747573'::pg_catalog.text
        OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text),
    ((index_fields OPERATOR(pg_catalog.->) '66696e69736865645f6174'::pg_catalog.text
        OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text)::pg_catalog.int8),
    key
)
"#,
		index = index,
		table = table
	)
}

fn dead_letters_index_statement(table: &PgQualifiedTableName, index: &PgIdentifier) -> AuditedSql {
	crate::audited_sql!(
		r#"
CREATE INDEX {index} ON ONLY {table} USING btree (
    namespace,
    ((index_fields OPERATOR(pg_catalog.->) '646561645f6c657474657265645f6174'::pg_catalog.text
        OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text)::pg_catalog.int8),
    key
)
"#,
		index = index,
		table = table
	)
}

fn running_index_statement(table: &PgQualifiedTableName, index: &PgIdentifier) -> AuditedSql {
	crate::audited_sql!(
		r#"
CREATE INDEX {index} ON ONLY {table} USING btree (
    namespace,
    (index_fields OPERATOR(pg_catalog.->) '737461747573'::pg_catalog.text
        OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text),
    ((index_fields OPERATOR(pg_catalog.->) '657865637574696f6e5f6865617274626561745f6174'::pg_catalog.text
        OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text)::pg_catalog.int8),
    key
)
"#,
		index = index,
		table = table
	)
}

const INDEX_OF_TABLE_SQL: &str = "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_index \
	WHERE indexrelid OPERATOR(pg_catalog.=) pg_catalog.to_regclass($1) \
	AND indrelid OPERATOR(pg_catalog.=) pg_catalog.to_regclass($2))";

/// The indexes that Queue job selection uses.
pub(crate) struct QueueSelectionIndexes<'a> {
	/// Pending jobs by due time.
	pub(crate) due: &'a PgIdentifier,
	/// Running jobs by heartbeat time.
	pub(crate) running: &'a PgIdentifier,
	/// Completed and failed jobs by finish time.
	pub(crate) finished: &'a PgIdentifier,
	/// Dead letters by dead-letter time.
	pub(crate) dead_letters: &'a PgIdentifier,
}

impl<'a> QueueSelectionIndexes<'a> {
	pub(crate) fn all(&self) -> [&'a PgIdentifier; 4] {
		[self.due, self.running, self.finished, self.dead_letters]
	}
}

impl PgPointStorage {
	/// Creates the Queue selection indexes in the table's schema.
	pub(crate) async fn create_queue_selection_indexes(
		&self,
		tx: &mut WriteTx<'_>,
		indexes: QueueSelectionIndexes<'_>,
	) -> Result<(), SchemaError> {
		for statement in [
			due_index_statement(self.table(), indexes.due),
			task_due_index_statement(self.table(), &task_due_index_name(), false),
			super::list_index::create_index_statement(self.table(), false),
			running_index_statement(self.table(), indexes.running),
			finished_index_statement(self.table(), indexes.finished),
			dead_letters_index_statement(self.table(), indexes.dead_letters),
		] {
			tx.record_database_operation(
				DatabaseOperationKind::Execute,
				CREATE_QUEUE_SELECTION_INDEX_LABEL,
				Some(statement.as_str()),
			);
			owned_portable_query(statement)
				.execute(tx.inner.as_mut())
				.await
				.map_err(|error| error.with_operation_label(CREATE_QUEUE_SELECTION_INDEX_LABEL))?;
		}
		Ok(())
	}

	/// Checks that every Queue selection index exists on this table.
	pub(crate) async fn validate_queue_selection_indexes(
		&self,
		tx: &mut Tx<'_>,
		indexes: QueueSelectionIndexes<'_>,
	) -> Result<(), SchemaError> {
		for index in indexes.all() {
			let qualified = PgQualifiedTableName::new(self.table().schema().clone(), index.clone());
			tx.record_database_operation(
				DatabaseOperationKind::FetchOne,
				VALIDATE_QUEUE_SELECTION_INDEX_LABEL,
				Some(INDEX_OF_TABLE_SQL),
			);
			let present = owned_portable_query_as::<(bool,)>(INDEX_OF_TABLE_SQL)
				.bind_validated(qualified.quoted().to_string())
				.bind_validated(self.table().quoted().to_string())
				.fetch_one(tx.inner.as_mut())
				.await
				.map_err(|error| {
					error.with_operation_label(VALIDATE_QUEUE_SELECTION_INDEX_LABEL)
				})?;
			if !present.0 {
				return Err(SchemaError::mismatch(
					"a configured Queue selection index is absent from the Queue table",
				));
			}
		}
		Ok(())
	}

	/// Returns at most `limit` keys of pending jobs of `task_names` that are due now, by the
	/// database clock, earliest first.
	pub(in crate::queue) async fn select_due_queue_job_keys(
		&self,
		tx: &mut Tx<'_>,
		jobs: &RecordNamespace,
		pauses: &RecordNamespace,
		task_names: &[&str],
		limit: u32,
	) -> Result<Vec<RecordKey>, DatabaseError> {
		let query = due_query(self.table());
		tx.record_database_operation(
			DatabaseOperationKind::FetchAll,
			SELECT_DUE_QUEUE_JOBS_LABEL,
			Some(query.as_str()),
		);
		let tasks: Vec<String> = task_names
			.iter()
			.map(|task| ByteEncoding::HexLower.encode(task.as_bytes()))
			.collect();
		let rows = owned_portable_query_as::<(Bytea,)>(query)
			.bind_validated(jobs.as_bytes())
			.bind_validated(ByteEncoding::HexLower.encode(b"pending"))
			.bind_validated(tasks)
			.bind_validated(i64::from(limit))
			.bind_validated(pauses.as_bytes())
			.fetch_all(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(SELECT_DUE_QUEUE_JOBS_LABEL))?;
		Ok(keys(rows))
	}

	/// Returns at most `limit` keys of stale running jobs, the oldest heartbeat first.
	pub(in crate::queue) async fn select_running_queue_job_keys(
		&self,
		tx: &mut Tx<'_>,
		jobs: &RecordNamespace,
		limit: u32,
		stale_threshold_micros: i64,
	) -> Result<Vec<RecordKey>, DatabaseError> {
		let query = running_query(self.table());
		tx.record_database_operation(
			DatabaseOperationKind::FetchAll,
			SELECT_RUNNING_QUEUE_JOBS_LABEL,
			Some(query.as_str()),
		);
		let rows = owned_portable_query_as::<(Bytea,)>(query)
			.bind_validated(jobs.as_bytes())
			.bind_validated(ByteEncoding::HexLower.encode(b"running"))
			.bind_validated(i64::from(limit))
			.bind_validated(stale_threshold_micros)
			.fetch_all(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(SELECT_RUNNING_QUEUE_JOBS_LABEL))?;
		Ok(keys(rows))
	}

	/// Returns at most `limit` keys of jobs with `status` that finished at least `retention_micros`
	/// before now, by the database clock, the earliest finish first.
	pub(in crate::queue) async fn select_finished_queue_job_keys(
		&self,
		tx: &mut Tx<'_>,
		jobs: &RecordNamespace,
		status: &str,
		retention_micros: i64,
		limit: u32,
	) -> Result<Vec<RecordKey>, DatabaseError> {
		let query = finished_query(self.table());
		tx.record_database_operation(
			DatabaseOperationKind::FetchAll,
			SELECT_FINISHED_QUEUE_JOBS_LABEL,
			Some(query.as_str()),
		);
		let rows = owned_portable_query_as::<(Bytea,)>(query)
			.bind_validated(jobs.as_bytes())
			.bind_validated(ByteEncoding::HexLower.encode(status.as_bytes()))
			.bind_validated(retention_micros)
			.bind_validated(i64::from(limit))
			.fetch_all(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(SELECT_FINISHED_QUEUE_JOBS_LABEL))?;
		Ok(keys(rows))
	}

	/// Returns at most `limit` keys of dead letters created at least `retention_micros` before
	/// now, by the database clock, the oldest first.
	pub(in crate::queue) async fn select_expired_queue_dead_letter_keys(
		&self,
		tx: &mut Tx<'_>,
		dead_letters: &RecordNamespace,
		retention_micros: i64,
		limit: u32,
	) -> Result<Vec<RecordKey>, DatabaseError> {
		let query = expired_dead_letters_query(self.table());
		tx.record_database_operation(
			DatabaseOperationKind::FetchAll,
			SELECT_EXPIRED_QUEUE_DEAD_LETTERS_LABEL,
			Some(query.as_str()),
		);
		let rows = owned_portable_query_as::<(Bytea,)>(query)
			.bind_validated(dead_letters.as_bytes())
			.bind_validated(retention_micros)
			.bind_validated(i64::from(limit))
			.fetch_all(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(SELECT_EXPIRED_QUEUE_DEAD_LETTERS_LABEL))?;
		Ok(keys(rows))
	}

	/// Returns the keys of running jobs with a claim that carries the owner label `owner`.
	pub(in crate::queue) async fn select_running_queue_job_keys_by_owner(
		&self,
		tx: &mut Tx<'_>,
		jobs: &RecordNamespace,
		owner: &str,
	) -> Result<Vec<RecordKey>, DatabaseError> {
		let query = running_by_owner_query(self.table());
		tx.record_database_operation(
			DatabaseOperationKind::FetchAll,
			SELECT_RUNNING_QUEUE_JOBS_BY_OWNER_LABEL,
			Some(query.as_str()),
		);
		let rows = owned_portable_query_as::<(Bytea,)>(query)
			.bind_validated(jobs.as_bytes())
			.bind_validated(ByteEncoding::HexLower.encode(b"running"))
			.bind_validated(ByteEncoding::HexLower.encode(owner.as_bytes()))
			.fetch_all(tx.inner.as_mut())
			.await
			.map_err(|error| {
				error.with_operation_label(SELECT_RUNNING_QUEUE_JOBS_BY_OWNER_LABEL)
			})?;
		Ok(keys(rows))
	}
}

/// One listing request over the jobs namespace.
pub(in crate::queue) struct QueueJobListRequest<'a> {
	pub(in crate::queue) jobs: &'a RecordNamespace,
	pub(in crate::queue) statuses: &'a [&'a str],
	pub(in crate::queue) task_name: Option<&'a str>,
	pub(in crate::queue) after: Option<&'a RecordKey>,
	pub(in crate::queue) limit: u32,
	pub(in crate::queue) separate_payload_key_prefix: Option<&'a [u8]>,
}

pub(in crate::queue) struct QueueJobListRecord {
	pub(in crate::queue) key: RecordKey,
	pub(in crate::queue) job: StoredRecord,
	pub(in crate::queue) payload: Option<StoredRecord>,
}

type QueueJobListRow = (
	Bytea,
	Option<Bytea>,
	Option<i64>,
	Option<i64>,
	String,
	Option<Bytea>,
	Option<Bytea>,
	Option<i64>,
	Option<i64>,
	Option<String>,
);

impl PgPointStorage {
	/// Returns at most `limit` job records with one of `statuses`, and of `task_name` when it is
	/// given, in key order after `after`.
	pub(in crate::queue) async fn list_queue_job_records(
		&self,
		tx: &mut Tx<'_>,
		request: QueueJobListRequest<'_>,
	) -> Result<Vec<QueueJobListRecord>, StateStorageError> {
		let query = list_query(self.table());
		tx.record_database_operation(
			DatabaseOperationKind::FetchAll,
			LIST_QUEUE_JOBS_LABEL,
			Some(query.as_str()),
		);
		let statuses: Vec<String> = request
			.statuses
			.iter()
			.map(|status| ByteEncoding::HexLower.encode(status.as_bytes()))
			.collect();
		let rows = owned_portable_query_as::<QueueJobListRow>(query)
			.bind_validated(request.jobs.as_bytes())
			.bind_validated(request.after.map(RecordKey::as_bytes))
			.bind_validated(statuses)
			.bind_validated(
				request
					.task_name
					.map(|task| ByteEncoding::HexLower.encode(task.as_bytes())),
			)
			.bind_validated(i64::from(request.limit))
			.bind_validated(request.separate_payload_key_prefix)
			.fetch_all(tx.inner.as_mut())
			.await
			.map_err(|error| {
				StateStorageError::from(error.with_operation_label(LIST_QUEUE_JOBS_LABEL))
			})?;
		let mut records = Vec::with_capacity(rows.len());
		for (
			key,
			body,
			expires_at,
			updated_at,
			index_fields,
			payload_key,
			payload_body,
			payload_expires_at,
			payload_updated_at,
			payload_index_fields,
		) in rows
		{
			let key = RecordKey::new(key.0);
			let identity = RecordIdentity::new(request.jobs.clone(), key.clone());
			let payload = match (payload_key, payload_index_fields) {
				(Some(payload_key), Some(index_fields)) => {
					let expected = request
						.separate_payload_key_prefix
						.into_iter()
						.flatten()
						.chain(key.as_bytes())
						.copied()
						.collect::<Vec<_>>();
					if payload_key.0 != expected {
						return Err(StateStorageError::InvalidRecord {
							identity,
							reason: "listed payload key does not belong to its job",
						});
					}
					decode_physical_record(
						&RecordIdentity::new(request.jobs.clone(), RecordKey::new(payload_key.0)),
						(
							payload_body,
							payload_expires_at,
							payload_updated_at,
							index_fields,
						),
					)?
				}
				(None, None)
					if payload_body.is_none()
						&& payload_expires_at.is_none()
						&& payload_updated_at.is_none() =>
				{
					None
				}
				_ => {
					return Err(StateStorageError::InvalidRecord {
						identity,
						reason: "listed payload has an incomplete physical row",
					});
				}
			};
			if let Some(record) =
				decode_physical_record(&identity, (body, expires_at, updated_at, index_fields))?
			{
				records.push(QueueJobListRecord {
					key,
					job: record,
					payload,
				});
			}
		}
		Ok(records)
	}
}

fn keys(rows: Vec<(Bytea,)>) -> Vec<RecordKey> {
	rows.into_iter()
		.map(|(key,)| RecordKey::new(key.0))
		.collect()
}
