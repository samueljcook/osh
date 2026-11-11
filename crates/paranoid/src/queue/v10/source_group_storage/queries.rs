//! Fixed published/prepared physical projections and exact source association probes.

use super::{PgV10SourceDescriptor, PgV10SourceRole, PublishedV10QueueLayout};
use crate::db::{AuditedSql, AuditedSqlBuilder, PgQualifiedTableName};
use crate::queue::v10::schema_model::QueueProjectionField;
use crate::queue::{QueueColumn, QueueSchemaLayout};
use crate::records::postgres::v10::source_timestamp::append_timestamp_projection;

pub(super) const CLOCK_AND_ISOLATION_SQL: &str = "SELECT pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.clock_timestamp()) OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8 AS observed_at, pg_catalog.current_setting('transaction_isolation') AS isolation";

pub(super) struct SourceQueries {
	pub(super) jobs: AuditedSql,
	pub(super) dead_letters: AuditedSql,
	pub(super) pauses: AuditedSql,
	pub(super) active_dedupe: AuditedSql,
	pub(super) original: AuditedSql,
}

fn push_text_projection(sql: &mut AuditedSqlBuilder, column: QueueColumn, alias: &'static str) {
	sql.push_text("pg_catalog.convert_to(");
	sql.push_text(column.name_in(QueueSchemaLayout::SourceEpoch));
	sql.push_text("::pg_catalog.text, 'UTF8') AS ");
	sql.push_text(alias);
}
fn push_timestamp_projection(
	sql: &mut AuditedSqlBuilder,
	column: QueueColumn,
	alias: &'static str,
) {
	append_timestamp_projection(sql, column.name_in(QueueSchemaLayout::SourceEpoch));
	sql.push_text(" AS ");
	sql.push_text(alias);
}
/// The materialized source set locks in bytewise source-key order before coverage is joined.
fn push_locked_rows_by_id(sql: &mut AuditedSqlBuilder, table: &PgQualifiedTableName) {
	sql.push_text(" FROM ONLY ");
	sql.push_argument(table);
	sql.push_text(" AS physical WHERE physical.id OPERATOR(pg_catalog.=) ANY ($1::pg_catalog.bytea[]) ORDER BY physical.id FOR UPDATE OF physical) SELECT requested.marker, requested.key AS request_key, locked.* FROM requested LEFT JOIN locked ON locked.id OPERATOR(pg_catalog.=) requested.key ORDER BY requested.marker");
}

impl SourceQueries {
	pub(super) fn new(descriptor: &PgV10SourceDescriptor) -> Self {
		let mut jobs = AuditedSqlBuilder::new();
		jobs.push_text("WITH requested AS MATERIALIZED (SELECT key, marker FROM pg_catalog.unnest($1::pg_catalog.bytea[]) WITH ORDINALITY AS r(key, marker)), locked AS MATERIALIZED (SELECT id, ");
		push_text_projection(
			&mut jobs,
			QueueColumn::TaskName,
			QueueColumn::TaskName.name(),
		);
		jobs.push_text(", ");
		push_text_projection(
			&mut jobs,
			QueueColumn::Payload,
			QueueProjectionField::PayloadJson.name(),
		);
		jobs.push_text(", ");
		push_text_projection(&mut jobs, QueueColumn::Status, QueueColumn::Status.name());
		jobs.push_text(", ");
		push_text_projection(
			&mut jobs,
			QueueColumn::LastErrorMsg,
			QueueColumn::LastErrorMsg.name(),
		);
		jobs.push_text(", retry_count, max_retries, timeout_nanos, ");
		push_text_projection(
			&mut jobs,
			QueueColumn::DedupeKey,
			QueueColumn::DedupeKey.name(),
		);
		jobs.push_text(", ");
		push_text_projection(
			&mut jobs,
			QueueColumn::WorkerOwnerId,
			QueueColumn::WorkerOwnerId.name(),
		);
		if descriptor.layout().queue_layout() == PublishedV10QueueLayout::PreparedV2 {
			jobs.push_text(", worker_claim_token");
		}
		for (column, alias) in [
			(
				QueueColumn::RunAtOrAfter,
				QueueProjectionField::RunAtOrAfterUnixMicros,
			),
			(
				QueueColumn::ClaimedByWorkerAt,
				QueueProjectionField::ClaimedByWorkerAtUnixMicros,
			),
			(
				QueueColumn::ExecutionStartedAt,
				QueueProjectionField::ExecutionStartedAtUnixMicros,
			),
			(
				QueueColumn::ExecutionHeartbeatAt,
				QueueProjectionField::ExecutionHeartbeatAtUnixMicros,
			),
			(
				QueueColumn::FinishedAt,
				QueueProjectionField::FinishedAtUnixMicros,
			),
			(
				QueueColumn::CreatedAt,
				QueueProjectionField::CreatedAtUnixMicros,
			),
			(
				QueueColumn::UpdatedAt,
				QueueProjectionField::UpdatedAtUnixMicros,
			),
		] {
			jobs.push_text(", ");
			push_timestamp_projection(&mut jobs, column, alias.name());
		}
		push_locked_rows_by_id(&mut jobs, descriptor.table(PgV10SourceRole::QueueJobs));

		let mut dead_letters = AuditedSqlBuilder::new();
		dead_letters.push_text("WITH requested AS MATERIALIZED (SELECT key, marker FROM pg_catalog.unnest($1::pg_catalog.bytea[]) WITH ORDINALITY AS r(key, marker)), locked AS MATERIALIZED (SELECT id, original_job_id, ");
		push_text_projection(
			&mut dead_letters,
			QueueColumn::TaskName,
			QueueColumn::TaskName.name(),
		);
		dead_letters.push_text(", ");
		push_text_projection(
			&mut dead_letters,
			QueueColumn::Payload,
			QueueProjectionField::PayloadJson.name(),
		);
		dead_letters.push_text(", ");
		push_text_projection(
			&mut dead_letters,
			QueueColumn::LastErrorMsg,
			QueueColumn::LastErrorMsg.name(),
		);
		dead_letters.push_text(", retry_count, max_retries, timeout_nanos, ");
		push_text_projection(
			&mut dead_letters,
			QueueColumn::DedupeKey,
			QueueColumn::DedupeKey.name(),
		);
		dead_letters.push_text(", ");
		push_text_projection(
			&mut dead_letters,
			QueueColumn::Reason,
			QueueColumn::Reason.name(),
		);
		for (column, alias) in [
			(
				QueueColumn::DeadLetteredAt,
				QueueProjectionField::DeadLetteredAtUnixMicros,
			),
			(
				QueueColumn::CreatedAt,
				QueueProjectionField::CreatedAtUnixMicros,
			),
			(
				QueueColumn::UpdatedAt,
				QueueProjectionField::UpdatedAtUnixMicros,
			),
		] {
			dead_letters.push_text(", ");
			push_timestamp_projection(&mut dead_letters, column, alias.name());
		}
		push_locked_rows_by_id(
			&mut dead_letters,
			descriptor.table(PgV10SourceRole::QueueDeadLetters),
		);

		let mut pauses = AuditedSqlBuilder::new();
		pauses.push_text("WITH requested AS MATERIALIZED (SELECT key, marker FROM pg_catalog.unnest($1::pg_catalog.bytea[]) WITH ORDINALITY AS r(key, marker)), locked AS MATERIALIZED (SELECT ");
		push_text_projection(&mut pauses, QueueColumn::Key, QueueColumn::Key.name());
		pauses.push_text(", ");
		push_text_projection(
			&mut pauses,
			QueueColumn::TaskName,
			QueueColumn::TaskName.name(),
		);
		pauses.push_text(", ");
		push_timestamp_projection(
			&mut pauses,
			QueueColumn::UpdatedAt,
			QueueProjectionField::UpdatedAtUnixMicros.name(),
		);
		pauses.push_text(" FROM ONLY ");
		pauses.push_argument(descriptor.table(PgV10SourceRole::QueuePauses));
		pauses.push_text(" AS physical WHERE physical.key COLLATE pg_catalog.\"C\" OPERATOR(pg_catalog.=) ANY (ARRAY(SELECT pg_catalog.convert_from(key, 'UTF8') COLLATE pg_catalog.\"C\" FROM requested)) ORDER BY physical.key COLLATE pg_catalog.\"C\" FOR UPDATE OF physical) SELECT requested.marker, requested.key AS request_key, locked.* FROM requested LEFT JOIN locked ON locked.key OPERATOR(pg_catalog.=) requested.key ORDER BY requested.marker");

		let active_dedupe = crate::audited_sql!(
			"WITH requested AS MATERIALIZED (SELECT task, dedupe, marker FROM ROWS FROM (pg_catalog.unnest($1::pg_catalog.bytea[]), pg_catalog.unnest($2::pg_catalog.bytea[])) WITH ORDINALITY AS r(task, dedupe, marker)) SELECT requested.marker, requested.task AS request_key, requested.dedupe AS request_dedupe, owner.id FROM requested LEFT JOIN LATERAL (SELECT id FROM ONLY {table} WHERE task_name COLLATE pg_catalog.\"C\" OPERATOR(pg_catalog.=) pg_catalog.convert_from(requested.task, 'UTF8') COLLATE pg_catalog.\"C\" AND dedupe_key COLLATE pg_catalog.\"C\" OPERATOR(pg_catalog.=) pg_catalog.convert_from(requested.dedupe, 'UTF8') COLLATE pg_catalog.\"C\" AND status OPERATOR(pg_catalog.=) ANY (ARRAY['pending','running']::pg_catalog.text[]) LIMIT 2) AS owner ON TRUE ORDER BY requested.marker",
			table = descriptor.table(PgV10SourceRole::QueueJobs)
		);
		let original = crate::audited_sql!(
			"WITH requested AS MATERIALIZED (SELECT key, marker FROM pg_catalog.unnest($1::pg_catalog.bytea[]) WITH ORDINALITY AS r(key, marker)) SELECT requested.marker, requested.key AS request_key, owner.id FROM requested LEFT JOIN LATERAL (SELECT id FROM ONLY {table} WHERE original_job_id OPERATOR(pg_catalog.=) requested.key LIMIT 2) AS owner ON TRUE ORDER BY requested.marker",
			table = descriptor.table(PgV10SourceRole::QueueDeadLetters)
		);
		Self {
			jobs: jobs.finish(),
			dead_letters: dead_letters.finish(),
			pauses: pauses.finish(),
			active_dedupe,
			original,
		}
	}
}
