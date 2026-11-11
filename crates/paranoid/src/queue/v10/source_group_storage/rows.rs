//! Exact physical source fields, with no JSON row normalization or private grant extraction.

use super::{
	DedupeKey, JobId, PreparedQueueSourceJob, PublishedV10QueueLayout, QueueSourceDeadLetter,
	QueueSourceGroupStorageError, QueueSourceJob, QueueSourcePause, QueueSourceTimestamp, TaskName,
};
use crate::db::bytea::Bytea;
use crate::queue::v10::rows::RowCtx;
use crate::queue::v10::schema_model::QueueProjectionField;
use crate::queue::{QueueColumn, WorkerClaimToken};
use crate::records::postgres::v10::source_timestamp::{PgSourceTimestamp, parse_source_timestamp};
use sqlx::postgres::PgRow;
use zeroize::Zeroize;

fn invalid(field: &'static str) -> QueueSourceGroupStorageError {
	QueueSourceGroupStorageError::InvalidPhysical { field }
}
fn text(
	row: &PgRow,
	ctx: RowCtx<'_>,
	field: &'static str,
) -> Result<String, QueueSourceGroupStorageError> {
	let bytes: Bytea = ctx.column(row, field)?;
	String::from_utf8(bytes.0).map_err(|_| invalid(field))
}
fn optional_text(
	row: &PgRow,
	ctx: RowCtx<'_>,
	field: &'static str,
) -> Result<Option<String>, QueueSourceGroupStorageError> {
	ctx.column::<Option<Bytea>>(row, field)?
		.map(|bytes| String::from_utf8(bytes.0).map_err(|_| invalid(field)))
		.transpose()
}
fn timestamp(
	value: &str,
	field: &'static str,
) -> Result<QueueSourceTimestamp, QueueSourceGroupStorageError> {
	parse_source_timestamp(value)
		.map(|value| match value {
			PgSourceTimestamp::NegativeInfinity => QueueSourceTimestamp::NegativeInfinity,
			PgSourceTimestamp::FiniteUnixMicros(value) => {
				QueueSourceTimestamp::FiniteUnixMicros(value)
			}
			PgSourceTimestamp::PositiveInfinity => QueueSourceTimestamp::PositiveInfinity,
		})
		.map_err(|_| invalid(field))
}
fn time(
	row: &PgRow,
	ctx: RowCtx<'_>,
	field: &'static str,
) -> Result<QueueSourceTimestamp, QueueSourceGroupStorageError> {
	timestamp(&ctx.column::<String>(row, field)?, field)
}
fn optional_time(
	row: &PgRow,
	ctx: RowCtx<'_>,
	field: &'static str,
) -> Result<Option<QueueSourceTimestamp>, QueueSourceGroupStorageError> {
	ctx.column::<Option<String>>(row, field)?
		.map(|value| timestamp(&value, field))
		.transpose()
}
pub(super) fn id(
	row: &PgRow,
	ctx: RowCtx<'_>,
	field: &'static str,
) -> Result<JobId, QueueSourceGroupStorageError> {
	JobId::from_bytes(&ctx.column::<Bytea>(row, field)?.0).map_err(|_| invalid(field))
}
fn task(row: &PgRow, ctx: RowCtx<'_>) -> Result<TaskName, QueueSourceGroupStorageError> {
	TaskName::new(text(row, ctx, QueueColumn::TaskName.name())?)
		.map_err(|_| invalid(QueueColumn::TaskName.name()))
}
fn dedupe(row: &PgRow, ctx: RowCtx<'_>) -> Result<Option<DedupeKey>, QueueSourceGroupStorageError> {
	optional_text(row, ctx, QueueColumn::DedupeKey.name())?
		.map(|value| DedupeKey::new(value).map_err(|_| invalid(QueueColumn::DedupeKey.name())))
		.transpose()
}

pub(super) enum PhysicalJob {
	Published(QueueSourceJob),
	Prepared(PreparedQueueSourceJob),
}
pub(super) fn job(
	row: &PgRow,
	ctx: RowCtx<'_>,
	layout: PublishedV10QueueLayout,
) -> Result<PhysicalJob, QueueSourceGroupStorageError> {
	let job = QueueSourceJob {
		id: id(row, ctx, QueueColumn::Id.name())?,
		task_name: task(row, ctx)?,
		payload_json: text(row, ctx, QueueProjectionField::PayloadJson.name())?,
		status: crate::queue::JobStatus::parse(&text(row, ctx, QueueColumn::Status.name())?)
			.map_err(|_| invalid(QueueColumn::Status.name()))?,
		run_at_or_after: time(
			row,
			ctx,
			QueueProjectionField::RunAtOrAfterUnixMicros.name(),
		)?,
		last_error_msg: optional_text(row, ctx, QueueColumn::LastErrorMsg.name())?,
		retry_count: ctx.column(row, QueueColumn::RetryCount.name())?,
		max_retries: ctx.column(row, QueueColumn::MaxRetries.name())?,
		timeout_nanos: ctx.column(row, QueueColumn::TimeoutNanos.name())?,
		dedupe_key: dedupe(row, ctx)?,
		worker_owner_id: optional_text(row, ctx, QueueColumn::WorkerOwnerId.name())?
			.map(|value| {
				crate::queue::WorkerOwnerId::from_validated_text(value)
					.map_err(|_| invalid(QueueColumn::WorkerOwnerId.name()))
			})
			.transpose()?,
		claimed_by_worker_at: optional_time(
			row,
			ctx,
			QueueProjectionField::ClaimedByWorkerAtUnixMicros.name(),
		)?,
		execution_started_at: optional_time(
			row,
			ctx,
			QueueProjectionField::ExecutionStartedAtUnixMicros.name(),
		)?,
		execution_heartbeat_at: optional_time(
			row,
			ctx,
			QueueProjectionField::ExecutionHeartbeatAtUnixMicros.name(),
		)?,
		finished_at: optional_time(row, ctx, QueueProjectionField::FinishedAtUnixMicros.name())?,
		created_at: time(row, ctx, QueueProjectionField::CreatedAtUnixMicros.name())?,
		updated_at: time(row, ctx, QueueProjectionField::UpdatedAtUnixMicros.name())?,
	};
	match layout {
		PublishedV10QueueLayout::PublishedV1 => Ok(PhysicalJob::Published(job)),
		PublishedV10QueueLayout::PreparedV2 => {
			let token = ctx
				.column::<Option<Bytea>>(row, QueueColumn::WorkerClaimToken.name())?
				.map(|bytes| {
					let mut bytes = bytes.0;
					let array =
						<[u8; crate::queue::WORKER_CLAIM_TOKEN_LEN]>::try_from(bytes.as_slice());
					bytes.zeroize();
					array
						.map(WorkerClaimToken::from_persisted_bytes)
						.map_err(|_| invalid(QueueColumn::WorkerClaimToken.name()))
				})
				.transpose()?;
			Ok(PhysicalJob::Prepared(PreparedQueueSourceJob {
				job,
				claim_token: token,
			}))
		}
	}
}
pub(super) fn dead_letter(
	row: &PgRow,
	ctx: RowCtx<'_>,
) -> Result<QueueSourceDeadLetter, QueueSourceGroupStorageError> {
	Ok(QueueSourceDeadLetter {
		id: id(row, ctx, QueueColumn::Id.name())?,
		original_job_id: id(row, ctx, QueueColumn::OriginalJobId.name())?,
		task_name: task(row, ctx)?,
		payload_json: text(row, ctx, QueueProjectionField::PayloadJson.name())?,
		last_error_msg: text(row, ctx, QueueColumn::LastErrorMsg.name())?,
		retry_count: ctx.column(row, QueueColumn::RetryCount.name())?,
		max_retries: ctx.column(row, QueueColumn::MaxRetries.name())?,
		timeout_nanos: ctx.column(row, QueueColumn::TimeoutNanos.name())?,
		dedupe_key: dedupe(row, ctx)?,
		reason: crate::queue::DeadLetterReason::parse(&text(row, ctx, QueueColumn::Reason.name())?)
			.map_err(|_| invalid(QueueColumn::Reason.name()))?,
		dead_lettered_at: time(
			row,
			ctx,
			QueueProjectionField::DeadLetteredAtUnixMicros.name(),
		)?,
		created_at: time(row, ctx, QueueProjectionField::CreatedAtUnixMicros.name())?,
		updated_at: time(row, ctx, QueueProjectionField::UpdatedAtUnixMicros.name())?,
	})
}
pub(super) fn pause(
	row: &PgRow,
	ctx: RowCtx<'_>,
) -> Result<QueueSourcePause, QueueSourceGroupStorageError> {
	Ok(QueueSourcePause {
		key: text(row, ctx, QueueColumn::Key.name())?,
		task_name: optional_text(row, ctx, QueueColumn::TaskName.name())?
			.map(|value| TaskName::new(value).map_err(|_| invalid(QueueColumn::TaskName.name())))
			.transpose()?,
		updated_at: time(row, ctx, QueueProjectionField::UpdatedAtUnixMicros.name())?,
	})
}
