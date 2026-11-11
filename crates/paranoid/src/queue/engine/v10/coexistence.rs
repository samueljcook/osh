//! Native Queue operations on the v10 Queue tables while v10 writers remain.
//!
//! Until v10 writers retire, an imported installation keeps its jobs, dead letters, and pauses
//! in the tables that published v10 created, and v10 deployments keep using them. Each
//! statement here has the effect that v10 gives the same operation, so v10 workers and
//! native workers claim, run, and recover each other's jobs. V10 identifies a claim by its
//! owner label alone, so on these tables a native claim is checked against its owner label and
//! its private token is not stored.

use super::source::NativeQueueSourceBindings;
use crate::db::bytea::Bytea;
use crate::db::{
	AuditedSql, AuditedSqlBuilder, DatabaseError, DatabaseOperationKind, PgQualifiedTableName, Tx,
	WriteTx, owned_portable_query, owned_portable_query_as, owned_portable_query_scalar,
};
use crate::installation::v10::PgV10SourceRole;
use crate::queue::engine::{
	GroupOperation, NativeClaimCheckRequest, NativeClaimRequest, NativeDeadLetterPage,
	NativeEnqueueRejection, NativeJobListRequest, NativeJobPage, NativeQueueGroupError,
	NativeQueueGroupInner, NativeQueueGroupOutcome, NativeQueueStaleClaimRecovery,
	OwnedJobMutation, RequiredExecutionState, RetryConsumption, SelectionKind,
};
use crate::queue::v10::rows::{RowCtx, queue_dead_letter_from_row, queue_job_from_row};
use crate::queue::{
	DeadLetterJob, DeadLetterReason, GLOBAL_PAUSE_KEY, Job, JobId, JobStatus, PersistedRowError,
	PublishedV10QueueLayout, QueueDeadLetterPolicy, QueueRecoveryPolicy,
	STALE_EXECUTION_ERROR_MESSAGE, StoredJobTimeout, TASK_PAUSE_KEY_PREFIX, paused_task_key,
};
use crate::runtime::admission::{
	NativeAdmissionError, NativeAdmittedContracts, NativeOperationError,
};
use sqlx::postgres::PgRow;

const ENQUEUE_LABEL: &str = "queue.coexistence.enqueue";
const CLAIM_LABEL: &str = "queue.coexistence.claim";
const OWNED_MUTATION_LABEL: &str = "queue.coexistence.owned_mutation";
const RECOVER_STALE_CLAIM_LABEL: &str = "queue.coexistence.recover_stale_claim";
const MOVE_TO_DEAD_LETTER_LABEL: &str = "queue.coexistence.move_to_dead_letter";
const DELETE_FINISHED_JOB_LABEL: &str = "queue.coexistence.delete_finished_job";
const DELETE_EXPIRED_DEAD_LETTER_LABEL: &str = "queue.coexistence.delete_expired_dead_letter";
const SELECT_JOBS_LABEL: &str = "queue.coexistence.select_jobs";
const LIST_JOBS_LABEL: &str = "queue.coexistence.list_jobs";
const FETCH_JOB_LABEL: &str = "queue.coexistence.fetch_job";
const LIST_DEAD_LETTERS_LABEL: &str = "queue.coexistence.list_dead_letters";

/// The columns of a job in the form that [`queue_job_from_row`] reads.
const JOB_PROJECTION_SQL: &str = "id, task_name, payload::pg_catalog.text AS payload_json, status, \
	(EXTRACT(EPOCH FROM run_at_or_after) OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8 AS run_at_or_after_unix_micros, \
	last_error AS last_error_message, retry_count, max_retries, timeout_nanos, dedupe_key, worker_id, \
	(EXTRACT(EPOCH FROM claimed_by_worker_at) OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8 AS claimed_by_worker_at_unix_micros, \
	(EXTRACT(EPOCH FROM execution_started_at) OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8 AS execution_started_at_unix_micros, \
	(EXTRACT(EPOCH FROM execution_heartbeat_at) OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8 AS execution_heartbeat_at_unix_micros, \
	(EXTRACT(EPOCH FROM finished_at) OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8 AS finished_at_unix_micros, \
	(EXTRACT(EPOCH FROM created_at) OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8 AS created_at_unix_micros, \
	(EXTRACT(EPOCH FROM updated_at) OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8 AS updated_at_unix_micros";

/// The columns of a dead letter in the form that [`queue_dead_letter_from_row`] reads.
const DEAD_LETTER_PROJECTION_SQL: &str = "id, original_job_id, task_name, \
	payload::pg_catalog.text AS payload_json, last_error AS last_error_message, retry_count, \
	max_retries, timeout_nanos, dedupe_key, reason, \
	(EXTRACT(EPOCH FROM dead_lettered_at) OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8 AS dead_lettered_at_unix_micros, \
	(EXTRACT(EPOCH FROM created_at) OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8 AS created_at_unix_micros, \
	(EXTRACT(EPOCH FROM updated_at) OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8 AS updated_at_unix_micros";

/// The assignments that end a claim; `finished_at` is set separately.
const CLEAR_CLAIM_ASSIGNMENTS_SQL: &str = "worker_id = NULL, claimed_by_worker_at = NULL, \
	execution_started_at = NULL, execution_heartbeat_at = NULL";

const STATEMENT_TIMESTAMP_SQL: &str = "pg_catalog.statement_timestamp()";

/// The three v10 Queue tables of an imported installation.
pub(in crate::queue::engine) struct CoexistenceTables<'a> {
	jobs: &'a PgQualifiedTableName,
	dead_letters: &'a PgQualifiedTableName,
	pauses: &'a PgQualifiedTableName,
	due_selection: DueSelection<'a>,
}

/// Whether a due-job selection on the v10 tables locks the jobs that it returns.
#[derive(Clone, Copy)]
enum DueSelection<'a> {
	/// The selection locks the jobs that it returns and skips jobs that another transaction
	/// holds because a claim during coexistence changes the selected v10 row in place.
	LockSelected,
	/// The selection takes no lock. While data retirement moves the v10 records, a claim takes
	/// its own locks on Paranoid's storage first and on the v10 row after them, as the move
	/// does, so a lock that the selection took first could deadlock with the move.
	DuringRelocation {
		native_table: &'a PgQualifiedTableName,
		pauses: &'a crate::records::RecordNamespace,
	},
}

impl<'a> CoexistenceTables<'a> {
	/// Requires the schema version that published v10 created. A prepared version 2 schema
	/// is used only by releases that never ran beside a native runtime.
	pub(super) fn new(
		source: &'a NativeQueueSourceBindings,
	) -> Result<Self, NativeQueueGroupError> {
		let descriptor = source.storage.descriptor();
		if descriptor.layout().queue_layout() != PublishedV10QueueLayout::PublishedV1 {
			return Err(NativeQueueGroupError::CoexistenceLayoutUnsupported);
		}
		Ok(Self {
			jobs: descriptor.table(PgV10SourceRole::QueueJobs),
			dead_letters: descriptor.table(PgV10SourceRole::QueueDeadLetters),
			pauses: descriptor.table(PgV10SourceRole::QueuePauses),
			due_selection: DueSelection::LockSelected,
		})
	}

	/// The tables for the reads and the unlocked selections that run while data retirement
	/// moves the v10 records. Those statements read only columns that both v10 schema versions
	/// have, so this accepts either version.
	pub(super) fn new_for_relocation_reads(
		source: &'a NativeQueueSourceBindings,
		pauses: &'a crate::records::RecordNamespace,
	) -> Self {
		let descriptor = source.storage.descriptor();
		Self {
			jobs: descriptor.table(PgV10SourceRole::QueueJobs),
			dead_letters: descriptor.table(PgV10SourceRole::QueueDeadLetters),
			pauses: descriptor.table(PgV10SourceRole::QueuePauses),
			due_selection: DueSelection::DuringRelocation {
				native_table: source.storage.native_storage().storage().table(),
				pauses,
			},
		}
	}
}

/// Appends a predicate that is true when the whole queue or the task of the job `j` is paused.
fn push_paused(sql: &mut AuditedSqlBuilder, tables: &CoexistenceTables<'_>) {
	sql.push_text("EXISTS (SELECT 1 FROM ONLY ");
	sql.push_argument(tables.pauses);
	sql.push_text(" AS p WHERE p.key OPERATOR(pg_catalog.=) '");
	sql.push_text(GLOBAL_PAUSE_KEY);
	sql.push_text("' OR p.key OPERATOR(pg_catalog.=) ('");
	sql.push_text(TASK_PAUSE_KEY_PREFIX);
	sql.push_text("' OPERATOR(pg_catalog.||) j.task_name))");
}

/// The statement's database time as Unix microseconds.
const STATEMENT_UNIX_MICROS_SQL: &str = "pg_catalog.floor(EXTRACT(EPOCH FROM \
	pg_catalog.statement_timestamp()) OPERATOR(pg_catalog.*) 1000000)";

/// Appends a predicate that is true when the timestamp `column` lies more than `param`
/// microseconds before the statement's database time.
///
/// The second comparison decides the result. It compares Unix microseconds as numeric values
/// (matching Paranoid's storage comparison) so that no age leaves the timestamp range. The first
/// comparison lets PostgreSQL use the column's index: it subtracts at most the database time's
/// own Unix microseconds, which keeps its bound at or after the Unix epoch and in range, and the
/// second comparison implies it.
fn push_older_than(sql: &mut AuditedSqlBuilder, column: &'static str, param: &'static str) {
	sql.push_text("(");
	sql.push_text(column);
	sql.push_text(
		" OPERATOR(pg_catalog.<) (pg_catalog.statement_timestamp() OPERATOR(pg_catalog.-) (LEAST(",
	);
	sql.push_text(param);
	sql.push_text("::pg_catalog.int8, ");
	sql.push_text(STATEMENT_UNIX_MICROS_SQL);
	sql.push_text(
		"::pg_catalog.int8) OPERATOR(pg_catalog.*) '1 microsecond'::pg_catalog.interval)) \
		AND pg_catalog.floor(EXTRACT(EPOCH FROM ",
	);
	sql.push_text(column);
	sql.push_text(") OPERATOR(pg_catalog.*) 1000000) OPERATOR(pg_catalog.<) (");
	sql.push_text(STATEMENT_UNIX_MICROS_SQL);
	sql.push_text(" OPERATOR(pg_catalog.-) ");
	sql.push_text(param);
	sql.push_text("::pg_catalog.int8::pg_catalog.numeric))");
}

fn enqueue_sql(tables: &CoexistenceTables<'_>) -> AuditedSql {
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text(
		"WITH pause_state AS (SELECT \
		COALESCE(pg_catalog.bool_or(key OPERATOR(pg_catalog.=) $7::pg_catalog.text), false) AS queue_paused, \
		COALESCE(pg_catalog.bool_or(key OPERATOR(pg_catalog.=) $8::pg_catalog.text), false) AS task_paused \
		FROM ONLY ",
	);
	sql.push_argument(tables.pauses);
	sql.push_text(
		" WHERE key OPERATOR(pg_catalog.=) ANY (ARRAY[$7, $8]::pg_catalog.text[])), \
		inserted AS (INSERT INTO ",
	);
	sql.push_argument(tables.jobs);
	sql.push_text(
		" (id, task_name, payload, status, run_at_or_after, max_retries, timeout_nanos, \
		dedupe_key, created_at, updated_at) SELECT $1::pg_catalog.bytea, $2::pg_catalog.text, \
		$3::pg_catalog.jsonb, 'pending', COALESCE('epoch'::pg_catalog.timestamptz \
		OPERATOR(pg_catalog.+) ($4::pg_catalog.int8 OPERATOR(pg_catalog.*) \
		'1 microsecond'::pg_catalog.interval), pg_catalog.statement_timestamp()), \
		$5::pg_catalog.int4, $6::pg_catalog.int8, $9::pg_catalog.text, \
		pg_catalog.statement_timestamp(), pg_catalog.statement_timestamp() \
		FROM pause_state WHERE NOT pause_state.queue_paused AND NOT pause_state.task_paused \
		ON CONFLICT (task_name, dedupe_key) WHERE dedupe_key IS NOT NULL AND status \
		OPERATOR(pg_catalog.=) ANY (ARRAY['pending', 'running']::pg_catalog.text[]) \
		DO NOTHING RETURNING id) \
		SELECT (SELECT queue_paused FROM pause_state), (SELECT task_paused FROM pause_state), \
		(SELECT id FROM inserted), (SELECT id FROM ONLY ",
	);
	sql.push_argument(tables.jobs);
	sql.push_text(
		" WHERE $9::pg_catalog.text IS NOT NULL AND task_name OPERATOR(pg_catalog.=) \
		$2::pg_catalog.text AND dedupe_key OPERATOR(pg_catalog.=) $9::pg_catalog.text AND status \
		OPERATOR(pg_catalog.=) ANY (ARRAY['pending', 'running']::pg_catalog.text[]) LIMIT 1)",
	);
	sql.finish()
}

fn claim_sql(tables: &CoexistenceTables<'_>) -> AuditedSql {
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("UPDATE ONLY ");
	sql.push_argument(tables.jobs);
	sql.push_text(
		" AS j SET status = 'running', worker_id = $2::pg_catalog.text, \
		claimed_by_worker_at = pg_catalog.statement_timestamp(), \
		execution_heartbeat_at = pg_catalog.statement_timestamp(), \
		updated_at = pg_catalog.statement_timestamp() \
		WHERE j.id OPERATOR(pg_catalog.=) $1::pg_catalog.bytea \
		AND j.status OPERATOR(pg_catalog.=) 'pending' \
		AND j.run_at_or_after OPERATOR(pg_catalog.<=) pg_catalog.statement_timestamp() AND NOT ",
	);
	push_paused(&mut sql, tables);
	sql.push_text(" RETURNING ");
	sql.push_text(JOB_PROJECTION_SQL);
	sql.finish()
}

/// An update of the running job `$1` that the owner label `$2` holds.
fn owned_sql(
	tables: &CoexistenceTables<'_>,
	assignments: &'static str,
	extra_predicate: &'static str,
) -> AuditedSql {
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("UPDATE ONLY ");
	sql.push_argument(tables.jobs);
	sql.push_text(" AS j SET ");
	sql.push_text(assignments);
	sql.push_text(", updated_at = ");
	sql.push_text(STATEMENT_TIMESTAMP_SQL);
	sql.push_text(
		" WHERE j.id OPERATOR(pg_catalog.=) $1::pg_catalog.bytea \
		AND j.status OPERATOR(pg_catalog.=) 'running' \
		AND j.worker_id OPERATOR(pg_catalog.=) $2::pg_catalog.text",
	);
	sql.push_text(extra_predicate);
	sql.push_text(" RETURNING 1");
	sql.finish()
}

fn owned_statement(tables: &CoexistenceTables<'_>, mutation: &OwnedJobMutation) -> AuditedSql {
	match mutation {
		OwnedJobMutation::StartExecution => owned_sql(
			tables,
			"execution_started_at = pg_catalog.statement_timestamp(), \
			execution_heartbeat_at = pg_catalog.statement_timestamp()",
			"",
		),
		OwnedJobMutation::Heartbeat => owned_sql(
			tables,
			"execution_heartbeat_at = GREATEST(j.execution_heartbeat_at, \
			pg_catalog.statement_timestamp())",
			"",
		),
		OwnedJobMutation::Complete => owned_sql(
			tables,
			"status = 'completed', worker_id = NULL, claimed_by_worker_at = NULL, \
			execution_started_at = NULL, execution_heartbeat_at = NULL, \
			finished_at = pg_catalog.statement_timestamp()",
			"",
		),
		OwnedJobMutation::ReturnPending(required) => owned_sql(
			tables,
			"status = 'pending', worker_id = NULL, claimed_by_worker_at = NULL, \
			execution_started_at = NULL, execution_heartbeat_at = NULL, finished_at = NULL",
			match required {
				RequiredExecutionState::Unstarted => " AND j.execution_started_at IS NULL",
				RequiredExecutionState::Started => " AND j.execution_started_at IS NOT NULL",
			},
		),
		OwnedJobMutation::ScheduleRetry { .. } => owned_sql(
			tables,
			"status = 'pending', retry_count = $3::pg_catalog.int4, \
			run_at_or_after = pg_catalog.statement_timestamp() OPERATOR(pg_catalog.+) \
			($4::pg_catalog.int8 OPERATOR(pg_catalog.*) '1 microsecond'::pg_catalog.interval), \
			last_error = $5::pg_catalog.text, worker_id = NULL, claimed_by_worker_at = NULL, \
			execution_started_at = NULL, execution_heartbeat_at = NULL, finished_at = NULL",
			"",
		),
		OwnedJobMutation::Fail { .. } => owned_sql(
			tables,
			"status = 'failed', last_error = $3::pg_catalog.text, \
			retry_count = j.retry_count OPERATOR(pg_catalog.+) $4::pg_catalog.int4, \
			worker_id = NULL, claimed_by_worker_at = NULL, execution_started_at = NULL, \
			execution_heartbeat_at = NULL, finished_at = pg_catalog.statement_timestamp()",
			"",
		),
	}
}

/// Recovers the running job `$1` when its heartbeat is older than `$2` microseconds, as
/// v10's stale-claim recovery does, and returns its new status.
fn recover_sql(tables: &CoexistenceTables<'_>) -> AuditedSql {
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("UPDATE ONLY ");
	sql.push_argument(tables.jobs);
	sql.push_text(
		" AS j SET \
		status = CASE WHEN j.execution_started_at IS NULL \
			OR j.retry_count OPERATOR(pg_catalog.<) j.max_retries THEN 'pending' ELSE 'failed' END, \
		retry_count = CASE WHEN j.execution_started_at IS NULL THEN j.retry_count \
			ELSE j.retry_count OPERATOR(pg_catalog.+) 1 END, \
		last_error = CASE WHEN j.execution_started_at IS NULL THEN j.last_error \
			ELSE COALESCE(j.last_error OPERATOR(pg_catalog.||) ' | ', '') \
			OPERATOR(pg_catalog.||) $3::pg_catalog.text END, \
		finished_at = CASE WHEN j.execution_started_at IS NOT NULL \
			AND j.retry_count OPERATOR(pg_catalog.>=) j.max_retries \
			THEN pg_catalog.statement_timestamp() END, ",
	);
	sql.push_text(CLEAR_CLAIM_ASSIGNMENTS_SQL);
	sql.push_text(", updated_at = ");
	sql.push_text(STATEMENT_TIMESTAMP_SQL);
	sql.push_text(
		" WHERE j.id OPERATOR(pg_catalog.=) $1::pg_catalog.bytea \
		AND j.status OPERATOR(pg_catalog.=) 'running' AND ",
	);
	push_older_than(&mut sql, "j.execution_heartbeat_at", "$2");
	sql.push_text(" RETURNING j.status");
	sql.finish()
}

fn move_to_dead_letter_sql(tables: &CoexistenceTables<'_>) -> AuditedSql {
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("WITH moved AS (DELETE FROM ONLY ");
	sql.push_argument(tables.jobs);
	sql.push_text(
		" AS j WHERE j.id OPERATOR(pg_catalog.=) $1::pg_catalog.bytea \
		AND j.status OPERATOR(pg_catalog.=) 'failed' \
		RETURNING j.id, j.task_name, j.payload, COALESCE(j.last_error, '') AS last_error, \
		j.retry_count, j.max_retries, j.timeout_nanos, j.dedupe_key) INSERT INTO ",
	);
	sql.push_argument(tables.dead_letters);
	sql.push_text(
		" (id, original_job_id, task_name, payload, last_error, retry_count, max_retries, \
		timeout_nanos, dedupe_key, reason, dead_lettered_at, created_at, updated_at) \
		SELECT $2::pg_catalog.bytea, moved.id, moved.task_name, moved.payload, moved.last_error, \
		moved.retry_count, moved.max_retries, moved.timeout_nanos, moved.dedupe_key, \
		$3::pg_catalog.text, pg_catalog.statement_timestamp(), pg_catalog.statement_timestamp(), \
		pg_catalog.statement_timestamp() FROM moved RETURNING 1",
	);
	sql.finish()
}

fn delete_finished_sql(tables: &CoexistenceTables<'_>) -> AuditedSql {
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("DELETE FROM ONLY ");
	sql.push_argument(tables.jobs);
	sql.push_text(
		" AS j WHERE j.id OPERATOR(pg_catalog.=) $1::pg_catalog.bytea \
		AND j.status OPERATOR(pg_catalog.=) ANY (ARRAY['completed', 'failed']::pg_catalog.text[]) \
		AND ",
	);
	push_older_than(&mut sql, "j.finished_at", "$2");
	sql.push_text(" RETURNING 1");
	sql.finish()
}

fn delete_dead_letter_sql(tables: &CoexistenceTables<'_>) -> AuditedSql {
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("DELETE FROM ONLY ");
	sql.push_argument(tables.dead_letters);
	sql.push_text(" AS l WHERE l.id OPERATOR(pg_catalog.=) $1::pg_catalog.bytea AND ");
	push_older_than(&mut sql, "l.dead_lettered_at", "$2");
	sql.push_text(" RETURNING 1");
	sql.finish()
}

fn select_sql(tables: &CoexistenceTables<'_>, kind: SelectionKind<'_>) -> AuditedSql {
	let mut sql = AuditedSqlBuilder::new();
	match kind {
		SelectionKind::Due { .. } => {
			sql.push_text("SELECT j.id FROM ONLY ");
			sql.push_argument(tables.jobs);
			sql.push_text(
				" AS j WHERE j.status OPERATOR(pg_catalog.=) 'pending' \
				AND j.task_name OPERATOR(pg_catalog.=) ANY ($1::pg_catalog.text[]) \
				AND j.run_at_or_after OPERATOR(pg_catalog.<=) pg_catalog.statement_timestamp() \
				AND NOT ",
			);
			push_paused(&mut sql, tables);
			if let DueSelection::DuringRelocation { native_table, .. } = tables.due_selection {
				// A pause can move before its jobs, and an operator can pause a task during
				// retirement. Exclude those jobs before applying the candidate limit.
				sql.push_text(" AND NOT EXISTS (SELECT 1 FROM ONLY ");
				sql.push_argument(native_table);
				sql.push_text(
					" AS p WHERE p.namespace OPERATOR(pg_catalog.=) $3::pg_catalog.bytea \
					AND p.body IS NOT NULL \
					AND (p.key OPERATOR(pg_catalog.=) pg_catalog.decode('0100', 'hex') \
					OR p.key OPERATOR(pg_catalog.=) (pg_catalog.decode('0101', 'hex') \
					OPERATOR(pg_catalog.||) pg_catalog.convert_to(j.task_name, 'UTF8'))))",
				);
			}
			sql.push_text(
				" ORDER BY j.run_at_or_after USING OPERATOR(pg_catalog.<), \
				j.id USING OPERATOR(pg_catalog.<) LIMIT $2::pg_catalog.int8",
			);
			match tables.due_selection {
				DueSelection::LockSelected => sql.push_text(" FOR UPDATE OF j SKIP LOCKED"),
				DueSelection::DuringRelocation { .. } => {}
			}
		}
		SelectionKind::StaleRunning { .. } => {
			sql.push_text("SELECT j.id FROM ONLY ");
			sql.push_argument(tables.jobs);
			sql.push_text(" AS j WHERE j.status OPERATOR(pg_catalog.=) 'running' AND ");
			push_older_than(&mut sql, "j.execution_heartbeat_at", "$2");
			sql.push_text(
				" ORDER BY j.execution_heartbeat_at USING OPERATOR(pg_catalog.<), \
                j.id USING OPERATOR(pg_catalog.<) LIMIT $1::pg_catalog.int8",
			);
		}
		SelectionKind::RunningByOwner { .. } => {
			sql.push_text("SELECT j.id FROM ONLY ");
			sql.push_argument(tables.jobs);
			sql.push_text(
				" AS j WHERE j.status OPERATOR(pg_catalog.=) 'running' \
				AND j.worker_id OPERATOR(pg_catalog.=) $1::pg_catalog.text \
				ORDER BY j.id USING OPERATOR(pg_catalog.<)",
			);
		}
		SelectionKind::Finished { .. } => {
			sql.push_text("SELECT j.id FROM ONLY ");
			sql.push_argument(tables.jobs);
			sql.push_text(" AS j WHERE j.status OPERATOR(pg_catalog.=) $1::pg_catalog.text AND ");
			push_older_than(&mut sql, "j.finished_at", "$2");
			sql.push_text(
				" ORDER BY j.finished_at USING OPERATOR(pg_catalog.<), \
				j.id USING OPERATOR(pg_catalog.<) LIMIT $3::pg_catalog.int8",
			);
		}
		SelectionKind::ExpiredDeadLetters { .. } => {
			sql.push_text("SELECT l.id FROM ONLY ");
			sql.push_argument(tables.dead_letters);
			sql.push_text(" AS l WHERE ");
			push_older_than(&mut sql, "l.dead_lettered_at", "$1");
			sql.push_text(
				" ORDER BY l.dead_lettered_at USING OPERATOR(pg_catalog.<), \
				l.id USING OPERATOR(pg_catalog.<) LIMIT $2::pg_catalog.int8",
			);
		}
	}
	sql.finish()
}

fn fetch_sql(tables: &CoexistenceTables<'_>) -> AuditedSql {
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("SELECT ");
	sql.push_text(JOB_PROJECTION_SQL);
	sql.push_text(" FROM ONLY ");
	sql.push_argument(tables.jobs);
	sql.push_text(" WHERE id OPERATOR(pg_catalog.=) $1::pg_catalog.bytea");
	sql.finish()
}

fn list_sql(tables: &CoexistenceTables<'_>) -> AuditedSql {
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("SELECT ");
	sql.push_text(JOB_PROJECTION_SQL);
	sql.push_text(" FROM ONLY ");
	sql.push_argument(tables.jobs);
	sql.push_text(
		" WHERE status OPERATOR(pg_catalog.=) ANY ($1::pg_catalog.text[]) \
		AND ($2::pg_catalog.text IS NULL OR task_name OPERATOR(pg_catalog.=) $2::pg_catalog.text) \
		AND ($3::pg_catalog.bytea IS NULL OR id OPERATOR(pg_catalog.>) $3::pg_catalog.bytea) \
		ORDER BY id USING OPERATOR(pg_catalog.<) LIMIT $4::pg_catalog.int8",
	);
	sql.finish()
}

fn list_dead_letters_sql(tables: &CoexistenceTables<'_>) -> AuditedSql {
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("SELECT ");
	sql.push_text(DEAD_LETTER_PROJECTION_SQL);
	sql.push_text(" FROM ONLY ");
	sql.push_argument(tables.dead_letters);
	sql.push_text(
		" WHERE ($1::pg_catalog.bytea IS NULL OR id OPERATOR(pg_catalog.>) $1::pg_catalog.bytea) \
		ORDER BY id USING OPERATOR(pg_catalog.<) LIMIT $2::pg_catalog.int8",
	);
	sql.finish()
}

/// A failure to read a row of the v10 Queue tables.
enum RowFailure {
	Database(DatabaseError),
	Row(PersistedRowError),
}
impl From<DatabaseError> for RowFailure {
	fn from(error: DatabaseError) -> Self {
		Self::Database(error)
	}
}
impl From<PersistedRowError> for RowFailure {
	fn from(error: PersistedRowError) -> Self {
		Self::Row(error)
	}
}
impl From<RowFailure> for NativeQueueGroupError {
	fn from(failure: RowFailure) -> Self {
		match failure {
			RowFailure::Database(error) => Self::Database(error),
			RowFailure::Row(error) => Self::CoexistenceRow(error),
		}
	}
}

fn job_from_row(
	row: &PgRow,
	operation: &'static str,
	sql: &AuditedSql,
) -> Result<Job, NativeQueueGroupError> {
	queue_job_from_row::<RowFailure>(
		row,
		RowCtx {
			operation,
			statement: sql.as_str(),
		},
	)
	.map_err(NativeQueueGroupError::from)
}

fn dead_letter_from_row(
	row: &PgRow,
	operation: &'static str,
	sql: &AuditedSql,
) -> Result<DeadLetterJob, NativeQueueGroupError> {
	queue_dead_letter_from_row::<RowFailure>(
		row,
		RowCtx {
			operation,
			statement: sql.as_str(),
		},
	)
	.map_err(NativeQueueGroupError::from)
}

fn job_id(bytes: &Bytea) -> Result<JobId, NativeQueueGroupError> {
	JobId::from_bytes(&bytes.0).map_err(|_| NativeQueueGroupError::InvalidJobKey)
}

fn timeout_nanos(timeout: StoredJobTimeout) -> Result<i64, NativeQueueGroupError> {
	match timeout {
		StoredJobTimeout::RegisteredDef => Ok(0),
		StoredJobTimeout::NoTimeout => Ok(-1),
		StoredJobTimeout::ExpiresAfter(duration) => i64::try_from(duration.as_nanos())
			.map_err(|_| NativeQueueGroupError::CoexistenceValueOutOfRange),
	}
}

fn int4(value: u32) -> Result<i32, NativeQueueGroupError> {
	i32::try_from(value).map_err(|_| NativeQueueGroupError::CoexistenceValueOutOfRange)
}

impl NativeQueueGroupInner {
	/// Binds the installed v10 Queue import policies, which show that the installation imported
	/// this Queue from the v10 tables and record their schema version.
	pub(in crate::queue::engine) fn coexistence_tables(
		&self,
		contracts: &NativeAdmittedContracts,
	) -> Result<CoexistenceTables<'_>, Box<NativeOperationError<NativeQueueGroupError>>> {
		let source = self
			.bindings
			.source
			.as_deref()
			.ok_or(NativeOperationError::Operation(
				NativeQueueGroupError::SourceMappingAbsent,
			))?;
		let job_declaration = source.imports.job_declaration();
		let jobs = contracts
			.bind(&job_declaration)
			.map_err(NativeOperationError::Admission)?;
		let auxiliary_declaration = source.imports.auxiliary_declaration();
		let auxiliary = contracts
			.bind(&auxiliary_declaration)
			.map_err(NativeOperationError::Admission)?;
		source
			.imports
			.require_bound_pair(
				&jobs,
				&auxiliary,
				source.storage.descriptor().layout().queue_layout(),
			)
			.map_err(|error| {
				NativeOperationError::Operation(NativeQueueGroupError::SourceObservation(
					error.into(),
				))
			})?;
		Ok(CoexistenceTables::new(source).map_err(NativeOperationError::Operation)?)
	}

	/// Binds the installed v10 Queue import policies, as `coexistence_tables` does, and checks
	/// the recorded source storage, as a group operation does, for the reads and the unlocked
	/// selections that run while data retirement moves the v10 records.
	pub(in crate::queue::engine) fn relocation_tables(
		&self,
		contracts: &NativeAdmittedContracts,
	) -> Result<CoexistenceTables<'_>, Box<NativeOperationError<NativeQueueGroupError>>> {
		let source = self
			.bindings
			.source
			.as_deref()
			.ok_or(NativeOperationError::Operation(
				NativeQueueGroupError::SourceMappingAbsent,
			))?;
		contracts
			.require_queue_source_storage(
				source.storage.descriptor(),
				source.storage.native_storage(),
			)
			.map_err(|error| {
				NativeOperationError::Operation(NativeQueueGroupError::SourceObservation(
					error.into(),
				))
			})?;
		let job_declaration = source.imports.job_declaration();
		let jobs = contracts
			.bind(&job_declaration)
			.map_err(NativeOperationError::Admission)?;
		let auxiliary_declaration = source.imports.auxiliary_declaration();
		let auxiliary = contracts
			.bind(&auxiliary_declaration)
			.map_err(NativeOperationError::Admission)?;
		source
			.imports
			.require_bound_pair(
				&jobs,
				&auxiliary,
				source.storage.descriptor().layout().queue_layout(),
			)
			.map_err(|error| {
				NativeOperationError::Operation(NativeQueueGroupError::SourceObservation(
					error.into(),
				))
			})?;
		Ok(CoexistenceTables::new_for_relocation_reads(
			source,
			self.bindings.storage.pauses(),
		))
	}
}

fn database_error_with_operation(
	operation: &'static str,
) -> impl Fn(DatabaseError) -> NativeQueueGroupError {
	move |error| NativeQueueGroupError::Database(error.with_operation_label(operation))
}

impl NativeQueueGroupInner {
	/// Runs one group operation on the v10 Queue tables. `recovery` is the installation's
	/// recorded Queue recovery settings.
	pub(in crate::queue::engine) async fn execute_coexistence(
		&self,
		tx: &mut WriteTx<'_>,
		tables: &CoexistenceTables<'_>,
		recovery: &QueueRecoveryPolicy,
		operation: &GroupOperation<'_>,
	) -> Result<NativeQueueGroupOutcome, NativeQueueGroupError> {
		match operation {
			GroupOperation::Enqueue { request, .. } => self.coexistence_enqueue(tx, tables, request).await,
			GroupOperation::Claim { request, .. } => coexistence_claim(tx, tables, request).await,
			GroupOperation::Owned {
				authority,
				mutation,
			} => {
				let applied = coexistence_owned(
					tx,
					tables,
					authority.job_id(),
					authority.worker_owner_id(),
					mutation,
				)
				.await?;
				Ok(if applied {
					NativeQueueGroupOutcome::Reconciled
				} else {
					NativeQueueGroupOutcome::Unchanged
				})
			}
			GroupOperation::ClaimCheck(request) => {
				coexistence_claim_check(tx, tables, request).await
			}
			GroupOperation::RecoverStaleClaim(request) => {
				coexistence_recover(tx, tables, recovery, request).await
			}
			GroupOperation::MoveToDeadLetter(request) => {
				coexistence_move_to_dead_letter(
					tx,
					tables,
					request.job_id,
					request.dead_letter_id,
					request.reason,
				)
				.await?;
				Ok(NativeQueueGroupOutcome::DeadLetterMoved {
					dead_letter_id: request.dead_letter_id,
				})
			}
			GroupOperation::DeleteFinished {
				job_id,
				retention_micros,
			} => {
				let sql = delete_finished_sql(tables);
				let deleted =
					delete_one(tx, DELETE_FINISHED_JOB_LABEL, sql, *job_id, *retention_micros).await?;
				Ok(if deleted {
					NativeQueueGroupOutcome::JobDeleted
				} else {
					NativeQueueGroupOutcome::Unchanged
				})
			}
			GroupOperation::DeleteExpiredDeadLetter {
				dead_letter_id,
				retention_micros,
			} => {
				let sql = delete_dead_letter_sql(tables);
				let deleted =
					delete_one(tx, DELETE_EXPIRED_DEAD_LETTER_LABEL, sql, *dead_letter_id, *retention_micros).await?;
				Ok(if deleted {
					NativeQueueGroupOutcome::DeadLetterDeleted
				} else {
					NativeQueueGroupOutcome::Unchanged
				})
			}
			GroupOperation::OperatorAction(_)
			| GroupOperation::Relocate(_)
			| GroupOperation::DeleteDeadLetter(_)
			| GroupOperation::RequeueDeadLetter(_)
			| GroupOperation::Pause(_) => Err(NativeQueueGroupError::Admission(
				NativeAdmissionError::StorageRouteUnsupported {
					route: crate::runtime::admission::bindings::InstallationStorageRoute::SourceCoexistence,
				},
			)),
		}
	}

	/// Inserts one pending job, or returns the active job of the same task that holds the
	/// request's dedupe key. A conflict with a concurrent enqueue of the same key that this
	/// statement cannot see runs the statement again, at most the conflict budget's times.
	async fn coexistence_enqueue(
		&self,
		tx: &mut WriteTx<'_>,
		tables: &CoexistenceTables<'_>,
		request: &crate::queue::group_state::NativeEnqueueRequest,
	) -> Result<NativeQueueGroupOutcome, NativeQueueGroupError> {
		let sql = enqueue_sql(tables);
		let timeout = timeout_nanos(request.timeout)?;
		let max_retries = int4(request.max_retries)?;
		let dedupe = request
			.dedupe_key
			.as_ref()
			.map(|key| key.as_str().to_owned());
		for _ in 0..self.bindings.attempt_budget.conflict_attempts.get() {
			tx.record_database_operation(
				DatabaseOperationKind::FetchOne,
				ENQUEUE_LABEL,
				Some(sql.as_str()),
			);
			let (queue_paused, task_paused, inserted, existing) =
				owned_portable_query_as::<(bool, bool, Option<Bytea>, Option<Bytea>)>(sql.clone())
					.bind_validated(request.job_id.as_bytes().to_vec())
					.bind_validated(request.task_name.as_str())
					.bind_validated(request.payload_json.as_str())
					.bind_validated(request.run_at_or_after.map(|time| time.as_unix_micros()))
					.bind_validated(max_retries)
					.bind_validated(timeout)
					.bind_validated(GLOBAL_PAUSE_KEY)
					.bind_validated(paused_task_key(&request.task_name))
					.bind_validated(dedupe.clone())
					.fetch_one(tx.inner.as_mut())
					.await
					.map_err(database_error_with_operation(ENQUEUE_LABEL))?;
			if queue_paused {
				return Ok(NativeQueueGroupOutcome::EnqueueRejected(
					NativeEnqueueRejection::QueuePaused,
				));
			}
			if task_paused {
				return Ok(NativeQueueGroupOutcome::EnqueueRejected(
					NativeEnqueueRejection::TaskPaused,
				));
			}
			if let Some(id) = inserted {
				return Ok(NativeQueueGroupOutcome::Enqueued {
					job_id: job_id(&id)?,
					deduplicated: false,
				});
			}
			if let Some(id) = existing {
				return Ok(NativeQueueGroupOutcome::Enqueued {
					job_id: job_id(&id)?,
					deduplicated: true,
				});
			}
		}
		Err(NativeQueueGroupError::CoexistenceDedupeContention)
	}
}

async fn coexistence_claim(
	tx: &mut WriteTx<'_>,
	tables: &CoexistenceTables<'_>,
	request: &NativeClaimRequest,
) -> Result<NativeQueueGroupOutcome, NativeQueueGroupError> {
	let sql = claim_sql(tables);
	tx.record_database_operation(
		DatabaseOperationKind::FetchOptional,
		CLAIM_LABEL,
		Some(sql.as_str()),
	);
	let row = owned_portable_query(sql.clone())
		.bind_validated(request.job_id.as_bytes().to_vec())
		.bind_validated(request.owner.as_str())
		.fetch_optional(tx.inner.as_mut())
		.await
		.map_err(database_error_with_operation(CLAIM_LABEL))?;
	Ok(match row {
		Some(row) => NativeQueueGroupOutcome::Claimed {
			job: Box::new(job_from_row(&row, CLAIM_LABEL, &sql)?),
		},
		None => NativeQueueGroupOutcome::ClaimSkipped,
	})
}

/// Applies `mutation` to the running job that `owner` holds and reports whether it did.
async fn coexistence_owned(
	tx: &mut WriteTx<'_>,
	tables: &CoexistenceTables<'_>,
	job_id: JobId,
	owner: &crate::queue::WorkerOwnerId,
	mutation: &OwnedJobMutation,
) -> Result<bool, NativeQueueGroupError> {
	let sql = owned_statement(tables, mutation);
	tx.record_database_operation(
		DatabaseOperationKind::FetchOptional,
		OWNED_MUTATION_LABEL,
		Some(sql.as_str()),
	);
	let query = owned_portable_query_scalar::<i32>(sql)
		.bind_validated(job_id.as_bytes().to_vec())
		.bind_validated(owner.as_str());
	let query = match mutation {
		OwnedJobMutation::ScheduleRetry {
			new_retry_count,
			retry_after,
			error_msg,
		} => query
			.bind_validated(int4(*new_retry_count)?)
			.bind_validated(
				crate::queue::retry_backoff_to_micros(*retry_after)
					.map_err(|_| NativeQueueGroupError::CoexistenceValueOutOfRange)?,
			)
			.bind_validated(error_msg.as_str()),
		OwnedJobMutation::Fail {
			error_msg,
			retry_consumption,
		} => query
			.bind_validated(error_msg.as_str())
			.bind_validated(match retry_consumption {
				RetryConsumption::KeepCount => 0_i32,
				RetryConsumption::ConsumeAttempt => 1_i32,
			}),
		OwnedJobMutation::StartExecution
		| OwnedJobMutation::Heartbeat
		| OwnedJobMutation::Complete
		| OwnedJobMutation::ReturnPending(_) => query,
	};
	Ok(query
		.fetch_optional(tx.inner.as_mut())
		.await
		.map_err(database_error_with_operation(OWNED_MUTATION_LABEL))?
		.is_some())
}

/// Returns the job to pending when the claim's owner label holds it and its execution has not
/// started.
async fn coexistence_claim_check(
	tx: &mut WriteTx<'_>,
	tables: &CoexistenceTables<'_>,
	request: &NativeClaimCheckRequest,
) -> Result<NativeQueueGroupOutcome, NativeQueueGroupError> {
	let returned = coexistence_owned(
		tx,
		tables,
		request.job_id,
		&request.owner,
		&OwnedJobMutation::ReturnPending(RequiredExecutionState::Unstarted),
	)
	.await?;
	Ok(if returned {
		NativeQueueGroupOutcome::ClaimReturned
	} else {
		NativeQueueGroupOutcome::ClaimNotReturned
	})
}

/// Recovers one running job with a heartbeat older than the installation's stale threshold.
/// If this abandons the job's last attempt, the job fails and moves to dead-letter storage when the
/// installation's recovery settings say so.
async fn coexistence_recover(
	tx: &mut WriteTx<'_>,
	tables: &CoexistenceTables<'_>,
	recovery: &QueueRecoveryPolicy,
	request: &NativeQueueStaleClaimRecovery,
) -> Result<NativeQueueGroupOutcome, NativeQueueGroupError> {
	let sql = recover_sql(tables);
	tx.record_database_operation(
		DatabaseOperationKind::FetchOptional,
		RECOVER_STALE_CLAIM_LABEL,
		Some(sql.as_str()),
	);
	let status = owned_portable_query_scalar::<String>(sql)
		.bind_validated(request.job_id.as_bytes().to_vec())
		.bind_validated(recovery.stale_threshold_micros())
		.bind_validated(STALE_EXECUTION_ERROR_MESSAGE)
		.fetch_optional(tx.inner.as_mut())
		.await
		.map_err(database_error_with_operation(RECOVER_STALE_CLAIM_LABEL))?;
	let Some(status) = status else {
		return Ok(NativeQueueGroupOutcome::Unchanged);
	};
	let failed = JobStatus::parse(&status).map_err(NativeQueueGroupError::CoexistenceRow)?
		== JobStatus::Failed;
	if failed && recovery.dead_letters() == QueueDeadLetterPolicy::MoveToDeadLetters {
		coexistence_move_to_dead_letter(
			tx,
			tables,
			request.job_id,
			request.dead_letter_id,
			DeadLetterReason::ExecutionExpired,
		)
		.await?;
		return Ok(NativeQueueGroupOutcome::DeadLetterMoved {
			dead_letter_id: request.dead_letter_id,
		});
	}
	Ok(NativeQueueGroupOutcome::Reconciled)
}

/// Moves the job `job_id`, which this transaction failed, to the v10 dead letters. A failed job
/// that the statement does not delete, which a trigger or a row security policy on the v10 Queue
/// tables can cause, fails the operation with
/// [`NativeQueueGroupError::CoexistenceDeadLetterMoveRejected`].
async fn coexistence_move_to_dead_letter(
	tx: &mut WriteTx<'_>,
	tables: &CoexistenceTables<'_>,
	job_id: JobId,
	dead_letter_id: JobId,
	reason: DeadLetterReason,
) -> Result<(), NativeQueueGroupError> {
	let sql = move_to_dead_letter_sql(tables);
	tx.record_database_operation(
		DatabaseOperationKind::FetchOptional,
		MOVE_TO_DEAD_LETTER_LABEL,
		Some(sql.as_str()),
	);
	owned_portable_query_scalar::<i32>(sql)
		.bind_validated(job_id.as_bytes().to_vec())
		.bind_validated(dead_letter_id.as_bytes().to_vec())
		.bind_validated(reason.as_str())
		.fetch_optional(tx.inner.as_mut())
		.await
		.map_err(database_error_with_operation(MOVE_TO_DEAD_LETTER_LABEL))?
		.map(|_moved| ())
		.ok_or(NativeQueueGroupError::CoexistenceDeadLetterMoveRejected { job_id })
}

async fn delete_one(
	tx: &mut WriteTx<'_>,
	operation: &'static str,
	sql: AuditedSql,
	id: JobId,
	retention_micros: i64,
) -> Result<bool, NativeQueueGroupError> {
	tx.record_database_operation(
		DatabaseOperationKind::FetchOptional,
		operation,
		Some(sql.as_str()),
	);
	Ok(owned_portable_query_scalar::<i32>(sql)
		.bind_validated(id.as_bytes().to_vec())
		.bind_validated(retention_micros)
		.fetch_optional(tx.inner.as_mut())
		.await
		.map_err(database_error_with_operation(operation))?
		.is_some())
}

/// Selects job or dead-letter identifiers from the v10 Queue tables. During coexistence, the
/// due-job selection locks the jobs that it returns and skips jobs that another transaction
/// holds; every other selection, and every selection while data retirement moves the v10
/// records, holds no lock.
pub(in crate::queue::engine) async fn coexistence_select(
	tx: &mut WriteTx<'_>,
	tables: &CoexistenceTables<'_>,
	kind: SelectionKind<'_>,
) -> Result<Vec<JobId>, NativeQueueGroupError> {
	let sql = select_sql(tables, kind);
	tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		SELECT_JOBS_LABEL,
		Some(sql.as_str()),
	);
	let query = owned_portable_query_as::<(Bytea,)>(sql);
	let query = match kind {
		SelectionKind::Due { task_names, limit } => {
			let query = query
				.bind_validated(
					task_names
						.iter()
						.map(|name| (*name).to_owned())
						.collect::<Vec<String>>(),
				)
				.bind_validated(i64::from(limit));
			match tables.due_selection {
				DueSelection::LockSelected => query,
				DueSelection::DuringRelocation { pauses, .. } => {
					query.bind_validated(pauses.as_bytes())
				}
			}
		}
		SelectionKind::StaleRunning {
			limit,
			stale_threshold_micros,
		} => query
			.bind_validated(i64::from(limit))
			.bind_validated(stale_threshold_micros),
		SelectionKind::RunningByOwner { owner } => query.bind_validated(owner),
		SelectionKind::Finished {
			status,
			retention_micros,
			limit,
		} => query
			.bind_validated(status)
			.bind_validated(retention_micros)
			.bind_validated(i64::from(limit)),
		SelectionKind::ExpiredDeadLetters {
			retention_micros,
			limit,
		} => query
			.bind_validated(retention_micros)
			.bind_validated(i64::from(limit)),
	};
	query
		.fetch_all(tx.inner.as_mut())
		.await
		.map_err(database_error_with_operation(SELECT_JOBS_LABEL))?
		.iter()
		.map(|(id,)| job_id(id))
		.collect()
}

/// Reads one job from the v10 Queue tables.
pub(in crate::queue::engine) async fn coexistence_fetch(
	tx: &mut Tx<'_>,
	tables: &CoexistenceTables<'_>,
	job_id: JobId,
) -> Result<Option<Job>, NativeQueueGroupError> {
	let sql = fetch_sql(tables);
	tx.record_database_operation(
		DatabaseOperationKind::FetchOptional,
		FETCH_JOB_LABEL,
		Some(sql.as_str()),
	);
	let row = owned_portable_query(sql.clone())
		.bind_validated(job_id.as_bytes().to_vec())
		.fetch_optional(tx.inner.as_mut())
		.await
		.map_err(database_error_with_operation(FETCH_JOB_LABEL))?;
	row.map(|row| job_from_row(&row, FETCH_JOB_LABEL, &sql))
		.transpose()
}

/// Lists jobs of the v10 Queue tables in job-identifier order. Listing holds no lock.
pub(in crate::queue::engine) async fn coexistence_list(
	tx: &mut Tx<'_>,
	tables: &CoexistenceTables<'_>,
	request: &NativeJobListRequest<'_>,
) -> Result<NativeJobPage, NativeQueueGroupError> {
	let sql = list_sql(tables);
	tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		LIST_JOBS_LABEL,
		Some(sql.as_str()),
	);
	let rows = owned_portable_query(sql.clone())
		.bind_validated(
			request
				.statuses
				.iter()
				.map(|status| status.as_str().to_owned())
				.collect::<Vec<String>>(),
		)
		.bind_validated(request.task_name.map(|task| task.as_str().to_owned()))
		.bind_validated(request.after.map(|id| id.as_bytes().to_vec()))
		.bind_validated(i64::from(request.limit))
		.fetch_all(tx.inner.as_mut())
		.await
		.map_err(database_error_with_operation(LIST_JOBS_LABEL))?;
	let jobs = rows
		.iter()
		.map(|row| job_from_row(row, LIST_JOBS_LABEL, &sql))
		.collect::<Result<Vec<_>, _>>()?;
	let next_after =
		if jobs.len() == usize::try_from(request.limit).expect("a u32 limit fits usize") {
			jobs.last().map(|job| job.id)
		} else {
			None
		};
	Ok(NativeJobPage { jobs, next_after })
}

/// Lists the dead letters of the v10 dead-letter table in identifier order after `after`.
/// Listing holds no lock.
pub(in crate::queue::engine) async fn coexistence_list_dead_letters(
	tx: &mut Tx<'_>,
	tables: &CoexistenceTables<'_>,
	after: Option<JobId>,
	limit: std::num::NonZeroU32,
) -> Result<NativeDeadLetterPage, NativeQueueGroupError> {
	let sql = list_dead_letters_sql(tables);
	tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		LIST_DEAD_LETTERS_LABEL,
		Some(sql.as_str()),
	);
	let rows = owned_portable_query(sql.clone())
		.bind_validated(after.map(|id| id.as_bytes().to_vec()))
		.bind_validated(i64::from(limit.get()))
		.fetch_all(tx.inner.as_mut())
		.await
		.map_err(database_error_with_operation(LIST_DEAD_LETTERS_LABEL))?;
	let dead_letters = rows
		.iter()
		.map(|row| dead_letter_from_row(row, LIST_DEAD_LETTERS_LABEL, &sql))
		.collect::<Result<Vec<_>, _>>()?;
	let next_after =
		if dead_letters.len() == usize::try_from(limit.get()).expect("a u32 limit fits usize") {
			dead_letters.last().map(|dead_letter| dead_letter.id)
		} else {
			None
		};
	Ok(NativeDeadLetterPage {
		dead_letters,
		next_after,
	})
}
