use super::schema_model::{QueueColumn, QueueProjectionField};
use crate::db::DatabaseError;
use crate::db::bytea::Bytea;
use crate::queue::{
	DeadLetterJob, DeadLetterReason, DedupeKey, Job, JobId, JobLifecycle, JobStatus,
	PersistedRowError, StoredJobLifecycleFields, TaskName, WorkerOwnerId,
	parse_max_retries_from_persisted_i32, parse_retry_count_from_persisted_i32,
	parse_timeout_from_persisted_nanos,
};
use sqlx::Row;
use sqlx::postgres::PgRow;

/// The operation and statement that produced a row, for the failures of decoding it.
#[derive(Clone, Copy)]
pub(in crate::queue) struct RowCtx<'a> {
	pub(in crate::queue) operation: &'static str,
	pub(in crate::queue) statement: &'a str,
}

impl RowCtx<'_> {
	/// Reads one column. A column that cannot be read is a failure of the statement that
	/// produced the row.
	pub(super) fn column<'r, T>(&self, row: &'r PgRow, name: &str) -> Result<T, DatabaseError>
	where
		T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
	{
		row.try_get(name)
			.map_err(|error| DatabaseError::for_statement(self.operation, self.statement, error))
	}
}

pub(super) fn parse_job_id_from_persisted_bytes(bytes: &[u8]) -> Result<JobId, PersistedRowError> {
	JobId::from_bytes(bytes).map_err(PersistedRowError::InvalidId)
}

pub(super) fn parse_task_name_from_persisted(text: String) -> Result<TaskName, PersistedRowError> {
	TaskName::new(text).map_err(PersistedRowError::InvalidTaskName)
}

pub(super) fn parse_dedupe_key_from_persisted(
	text: Option<String>,
) -> Result<Option<DedupeKey>, PersistedRowError> {
	text.map(|text| DedupeKey::new(text).map_err(PersistedRowError::InvalidDedupeKey))
		.transpose()
}

pub(in crate::queue) fn queue_job_from_row<E>(row: &PgRow, ctx: RowCtx<'_>) -> Result<Job, E>
where
	E: From<DatabaseError> + From<PersistedRowError>,
{
	let id_bytes: Bytea = ctx.column(row, QueueColumn::Id.name())?;
	let status_text: String = ctx.column(row, QueueColumn::Status.name())?;
	let id = parse_job_id_from_persisted_bytes(&id_bytes.0)?;
	let task_name = parse_task_name_from_persisted(ctx.column(row, QueueColumn::TaskName.name())?)?;
	let payload_json = ctx.column(row, QueueProjectionField::PayloadJson.name())?;
	let status = JobStatus::parse(&status_text)?;
	Ok(Job {
		id,
		task_name,
		payload_json,
		run_at_or_after_unix_micros: ctx
			.column(row, QueueProjectionField::RunAtOrAfterUnixMicros.name())?,
		last_error_msg: ctx.column(row, QueueColumn::LastErrorMsg.name())?,
		retry_count: parse_retry_count_from_persisted_i32(
			ctx.column(row, QueueColumn::RetryCount.name())?,
		)?,
		max_retries: parse_max_retries_from_persisted_i32(
			ctx.column(row, QueueColumn::MaxRetries.name())?,
		)?,
		timeout: parse_timeout_from_persisted_nanos(
			ctx.column(row, QueueColumn::TimeoutNanos.name())?,
		)?,
		dedupe_key: parse_dedupe_key_from_persisted(
			ctx.column(row, QueueColumn::DedupeKey.name())?,
		)?,
		lifecycle: JobLifecycle::from_stored_fields(StoredJobLifecycleFields {
			status,
			worker_owner_id: ctx
				.column::<Option<String>>(row, QueueColumn::WorkerOwnerId.name())?
				.map(|text| {
					WorkerOwnerId::from_validated_text(text)
						.map_err(PersistedRowError::InvalidWorkerOwnerId)
				})
				.transpose()?,
			claimed_at: ctx.column(
				row,
				QueueProjectionField::ClaimedByWorkerAtUnixMicros.name(),
			)?,
			started_at: ctx.column(
				row,
				QueueProjectionField::ExecutionStartedAtUnixMicros.name(),
			)?,
			heartbeat_at: ctx.column(
				row,
				QueueProjectionField::ExecutionHeartbeatAtUnixMicros.name(),
			)?,
			finished_at: ctx.column(row, QueueProjectionField::FinishedAtUnixMicros.name())?,
		})
		.ok_or(PersistedRowError::InvalidLifecycle { status })?,
		created_at_unix_micros: ctx
			.column(row, QueueProjectionField::CreatedAtUnixMicros.name())?,
		updated_at_unix_micros: ctx
			.column(row, QueueProjectionField::UpdatedAtUnixMicros.name())?,
	})
}

pub(in crate::queue) fn queue_dead_letter_from_row<E>(
	row: &PgRow,
	ctx: RowCtx<'_>,
) -> Result<DeadLetterJob, E>
where
	E: From<DatabaseError> + From<PersistedRowError>,
{
	let id_bytes: Bytea = ctx.column(row, QueueColumn::Id.name())?;
	let original_job_id_bytes: Bytea = ctx.column(row, QueueColumn::OriginalJobId.name())?;
	let reason_text: String = ctx.column(row, QueueColumn::Reason.name())?;
	Ok(DeadLetterJob {
		id: parse_job_id_from_persisted_bytes(&id_bytes.0)?,
		original_job_id: parse_job_id_from_persisted_bytes(&original_job_id_bytes.0)?,
		task_name: parse_task_name_from_persisted(ctx.column(row, QueueColumn::TaskName.name())?)?,
		payload_json: ctx.column(row, QueueProjectionField::PayloadJson.name())?,
		last_error_msg: ctx.column(row, QueueColumn::LastErrorMsg.name())?,
		retry_count: parse_retry_count_from_persisted_i32(
			ctx.column(row, QueueColumn::RetryCount.name())?,
		)?,
		max_retries: parse_max_retries_from_persisted_i32(
			ctx.column(row, QueueColumn::MaxRetries.name())?,
		)?,
		timeout: parse_timeout_from_persisted_nanos(
			ctx.column(row, QueueColumn::TimeoutNanos.name())?,
		)?,
		dedupe_key: parse_dedupe_key_from_persisted(
			ctx.column(row, QueueColumn::DedupeKey.name())?,
		)?,
		reason: DeadLetterReason::parse(&reason_text)?,
		dead_lettered_at_unix_micros: ctx
			.column(row, QueueProjectionField::DeadLetteredAtUnixMicros.name())?,
		created_at_unix_micros: ctx
			.column(row, QueueProjectionField::CreatedAtUnixMicros.name())?,
		updated_at_unix_micros: ctx
			.column(row, QueueProjectionField::UpdatedAtUnixMicros.name())?,
	})
}
