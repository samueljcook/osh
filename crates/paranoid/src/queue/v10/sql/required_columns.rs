use super::{QueueColumn, RequiredColumn};

pub(in crate::queue) fn required_job_columns() -> [RequiredColumn; 18] {
	[
		required_column(QueueColumn::Id, true),
		required_column(QueueColumn::TaskName, true),
		required_column(QueueColumn::Payload, true),
		required_column(QueueColumn::Status, true),
		required_column(QueueColumn::RunAtOrAfter, true),
		required_column(QueueColumn::LastErrorMsg, false),
		required_column(QueueColumn::RetryCount, true),
		required_column(QueueColumn::MaxRetries, true),
		required_column(QueueColumn::TimeoutNanos, true),
		required_column(QueueColumn::DedupeKey, false),
		required_column(QueueColumn::WorkerOwnerId, false),
		required_column(QueueColumn::WorkerClaimToken, false),
		required_column(QueueColumn::ClaimedByWorkerAt, false),
		required_column(QueueColumn::ExecutionStartedAt, false),
		required_column(QueueColumn::ExecutionHeartbeatAt, false),
		required_column(QueueColumn::FinishedAt, false),
		required_column(QueueColumn::CreatedAt, true),
		required_column(QueueColumn::UpdatedAt, true),
	]
}

pub(in crate::queue) fn required_dead_letter_columns() -> [RequiredColumn; 13] {
	[
		required_column(QueueColumn::Id, true),
		required_column(QueueColumn::OriginalJobId, true),
		required_column(QueueColumn::TaskName, true),
		required_column(QueueColumn::Payload, true),
		required_column(QueueColumn::LastErrorMsg, true),
		required_column(QueueColumn::RetryCount, true),
		required_column(QueueColumn::MaxRetries, true),
		required_column(QueueColumn::TimeoutNanos, true),
		required_column(QueueColumn::DedupeKey, false),
		required_column(QueueColumn::Reason, true),
		required_column(QueueColumn::DeadLetteredAt, true),
		required_column(QueueColumn::CreatedAt, true),
		required_column(QueueColumn::UpdatedAt, true),
	]
}

pub(in crate::queue) fn required_pause_columns() -> [RequiredColumn; 4] {
	[
		required_column(QueueColumn::Key, true),
		required_column(QueueColumn::TaskName, false),
		required_column(QueueColumn::PausedAt, true),
		required_column(QueueColumn::UpdatedAt, true),
	]
}

fn required_column(column: QueueColumn, not_null: bool) -> RequiredColumn {
	RequiredColumn { not_null, column }
}
