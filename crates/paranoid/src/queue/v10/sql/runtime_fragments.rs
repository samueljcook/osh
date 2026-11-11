use super::QueueColumn;

const ENQUEUE_WITH_DEDUPE_INSERT_COLUMNS: [QueueColumn; 10] = [
	QueueColumn::Id,
	QueueColumn::TaskName,
	QueueColumn::Payload,
	QueueColumn::Status,
	QueueColumn::RunAtOrAfter,
	QueueColumn::MaxRetries,
	QueueColumn::TimeoutNanos,
	QueueColumn::CreatedAt,
	QueueColumn::UpdatedAt,
	QueueColumn::DedupeKey,
];

pub(in crate::queue) fn enqueue_with_dedupe_insert_columns_sql() -> String {
	QueueColumn::list(&ENQUEUE_WITH_DEDUPE_INSERT_COLUMNS)
}
