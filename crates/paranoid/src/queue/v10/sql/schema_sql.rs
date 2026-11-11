use super::{
	ACTIVE_DEDUPE_INDEX_SUFFIX, CLEANUP_INDEX_SUFFIX, DEAD_LETTERED_AT_INDEX_SUFFIX,
	EXECUTION_HEARTBEAT_INDEX_SUFFIX, INDEX_KIND, ORIGINAL_JOB_INDEX_SUFFIX,
	PAUSE_TASK_INDEX_SUFFIX, PENDING_RUN_AT_INDEX_SUFFIX, PENDING_TASK_RUN_AT_INDEX_SUFFIX,
	QueueColumn, QueueIndexDef, QueueIndexPredicate, QueueTable,
	TASK_DEAD_LETTERED_AT_INDEX_SUFFIX, TASK_STATUS_INDEX_SUFFIX, UNIQUE_INDEX_KIND,
	WORKER_INDEX_SUFFIX,
};

const PENDING_RUN_AT_INDEX_COLUMNS: [QueueColumn; 3] = [
	QueueColumn::Status,
	QueueColumn::RunAtOrAfter,
	QueueColumn::Id,
];
const PENDING_TASK_RUN_AT_INDEX_COLUMNS: [QueueColumn; 3] = [
	QueueColumn::TaskName,
	QueueColumn::RunAtOrAfter,
	QueueColumn::Id,
];
const TASK_STATUS_INDEX_COLUMNS: [QueueColumn; 2] = [QueueColumn::TaskName, QueueColumn::Status];
const WORKER_INDEX_COLUMNS: [QueueColumn; 1] = [QueueColumn::WorkerOwnerId];
const EXECUTION_HEARTBEAT_INDEX_COLUMNS: [QueueColumn; 3] = [
	QueueColumn::Status,
	QueueColumn::ExecutionHeartbeatAt,
	QueueColumn::Id,
];
const CLEANUP_INDEX_COLUMNS: [QueueColumn; 2] = [QueueColumn::FinishedAt, QueueColumn::Id];
const ACTIVE_DEDUPE_INDEX_COLUMNS: [QueueColumn; 2] =
	[QueueColumn::TaskName, QueueColumn::DedupeKey];
const DEAD_LETTERED_AT_INDEX_COLUMNS: [QueueColumn; 2] =
	[QueueColumn::DeadLetteredAt, QueueColumn::Id];
const TASK_DEAD_LETTERED_AT_INDEX_COLUMNS: [QueueColumn; 3] = [
	QueueColumn::TaskName,
	QueueColumn::DeadLetteredAt,
	QueueColumn::Id,
];
const ORIGINAL_JOB_INDEX_COLUMNS: [QueueColumn; 1] = [QueueColumn::OriginalJobId];
const PAUSE_TASK_INDEX_COLUMNS: [QueueColumn; 1] = [QueueColumn::TaskName];

pub(in crate::queue) fn queue_schema_index_defs() -> [QueueIndexDef; 11] {
	[
		QueueIndexDef {
			table: QueueTable::Jobs,
			kind: INDEX_KIND,
			suffix: PENDING_RUN_AT_INDEX_SUFFIX,
			columns: &PENDING_RUN_AT_INDEX_COLUMNS,
			predicate: QueueIndexPredicate::PendingStatus,
			unique: false,
		},
		QueueIndexDef {
			table: QueueTable::Jobs,
			kind: INDEX_KIND,
			suffix: PENDING_TASK_RUN_AT_INDEX_SUFFIX,
			columns: &PENDING_TASK_RUN_AT_INDEX_COLUMNS,
			predicate: QueueIndexPredicate::PendingStatus,
			unique: false,
		},
		QueueIndexDef {
			table: QueueTable::Jobs,
			kind: INDEX_KIND,
			suffix: TASK_STATUS_INDEX_SUFFIX,
			columns: &TASK_STATUS_INDEX_COLUMNS,
			predicate: QueueIndexPredicate::None,
			unique: false,
		},
		QueueIndexDef {
			table: QueueTable::Jobs,
			kind: INDEX_KIND,
			suffix: WORKER_INDEX_SUFFIX,
			columns: &WORKER_INDEX_COLUMNS,
			predicate: QueueIndexPredicate::WorkerOwnerIdPresent,
			unique: false,
		},
		QueueIndexDef {
			table: QueueTable::Jobs,
			kind: INDEX_KIND,
			suffix: EXECUTION_HEARTBEAT_INDEX_SUFFIX,
			columns: &EXECUTION_HEARTBEAT_INDEX_COLUMNS,
			predicate: QueueIndexPredicate::RunningExecutionHeartbeatPresent,
			unique: false,
		},
		QueueIndexDef {
			table: QueueTable::Jobs,
			kind: INDEX_KIND,
			suffix: CLEANUP_INDEX_SUFFIX,
			columns: &CLEANUP_INDEX_COLUMNS,
			predicate: QueueIndexPredicate::TerminalFinishedAtPresent,
			unique: false,
		},
		QueueIndexDef {
			table: QueueTable::Jobs,
			kind: UNIQUE_INDEX_KIND,
			suffix: ACTIVE_DEDUPE_INDEX_SUFFIX,
			columns: &ACTIVE_DEDUPE_INDEX_COLUMNS,
			predicate: QueueIndexPredicate::ActiveDedupe,
			unique: true,
		},
		QueueIndexDef {
			table: QueueTable::DeadLetter,
			kind: INDEX_KIND,
			suffix: DEAD_LETTERED_AT_INDEX_SUFFIX,
			columns: &DEAD_LETTERED_AT_INDEX_COLUMNS,
			predicate: QueueIndexPredicate::None,
			unique: false,
		},
		QueueIndexDef {
			table: QueueTable::DeadLetter,
			kind: INDEX_KIND,
			suffix: TASK_DEAD_LETTERED_AT_INDEX_SUFFIX,
			columns: &TASK_DEAD_LETTERED_AT_INDEX_COLUMNS,
			predicate: QueueIndexPredicate::None,
			unique: false,
		},
		QueueIndexDef {
			table: QueueTable::DeadLetter,
			kind: UNIQUE_INDEX_KIND,
			suffix: ORIGINAL_JOB_INDEX_SUFFIX,
			columns: &ORIGINAL_JOB_INDEX_COLUMNS,
			predicate: QueueIndexPredicate::None,
			unique: true,
		},
		QueueIndexDef {
			table: QueueTable::Pause,
			kind: INDEX_KIND,
			suffix: PAUSE_TASK_INDEX_SUFFIX,
			columns: &PAUSE_TASK_INDEX_COLUMNS,
			predicate: QueueIndexPredicate::PauseTaskNamePresent,
			unique: false,
		},
	]
}

pub(in crate::queue) fn active_dedupe_index_def() -> QueueIndexDef {
	queue_schema_index_defs()
		.into_iter()
		.find(|def| def.predicate == QueueIndexPredicate::ActiveDedupe)
		.expect("queue schema must define active-dedupe arbiter index")
}

pub(in crate::queue) fn active_dedupe_conflict_columns_sql() -> String {
	QueueColumn::list(active_dedupe_index_def().columns)
}

pub(in crate::queue) fn active_dedupe_conflict_predicate_sql() -> String {
	active_dedupe_index_def()
		.predicate
		.sql()
		.expect("active-dedupe index must have predicate")
}
