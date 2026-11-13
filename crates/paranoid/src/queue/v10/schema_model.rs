#[derive(Clone, Copy)]
pub(in crate::queue) struct RequiredColumn {
	pub(in crate::queue) column: QueueColumn,
	pub(in crate::queue) not_null: bool,
}

#[derive(Clone, Debug)]
pub(in crate::queue) struct ActualColumn {
	pub(in crate::queue) name: String,
	pub(in crate::queue) data_type: String,
	pub(in crate::queue) not_null: bool,
	pub(in crate::queue) collation: Option<String>,
	pub(in crate::queue) writable: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::queue) enum QueueTable {
	Jobs,
	DeadLetter,
	Pause,
}

impl QueueTable {
	pub(in crate::queue) fn table_name(
		self,
		tables: &impl QueueSchemaTableBinding,
	) -> &crate::db::PgQualifiedTableName {
		tables.schema_tables().table(self)
	}
}

/// One actual table association view shared by schema construction and validation.
pub(in crate::queue) trait QueueSchemaTableBinding {
	fn schema_tables(&self) -> super::source_schema::QueueSourceSchemaTables<'_>;
}

/// A physical layout of the Queue tables.
///
/// Instances of an earlier compatibility epoch use the tables until activation, and their SQL
/// names the columns of schema version 2. Preparation under such an epoch therefore creates and
/// validates that layout, and activation changes it to the current layout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueSchemaLayout {
	/// Schema version 2, in which the text of a job's last failure is in the column `last_error`.
	SourceEpoch,
}

/// The name that schema version 2 gives the column of a job's last failure.
pub(in crate::queue) const SOURCE_EPOCH_LAST_ERROR_MESSAGE_COLUMN_NAME: &str = "last_error";

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueColumn {
	Id,
	OriginalJobId,
	TaskName,
	Payload,
	Status,
	RunAtOrAfter,
	LastErrorMsg,
	RetryCount,
	MaxRetries,
	TimeoutNanos,
	DedupeKey,
	WorkerOwnerId,
	WorkerClaimToken,
	ClaimedByWorkerAt,
	ExecutionStartedAt,
	ExecutionHeartbeatAt,
	FinishedAt,
	Reason,
	DeadLetteredAt,
	Key,
	PausedAt,
	CreatedAt,
	UpdatedAt,
}

impl QueueColumn {
	#[doc(hidden)]
	pub const fn name(self) -> &'static str {
		match self {
			Self::Id => "id",
			Self::OriginalJobId => "original_job_id",
			Self::TaskName => "task_name",
			Self::Payload => "payload",
			Self::Status => "status",
			Self::RunAtOrAfter => "run_at_or_after",
			Self::LastErrorMsg => "last_error_message",
			Self::RetryCount => "retry_count",
			Self::MaxRetries => "max_retries",
			Self::TimeoutNanos => "timeout_nanos",
			Self::DedupeKey => "dedupe_key",
			Self::WorkerOwnerId => "worker_id",
			Self::WorkerClaimToken => "worker_claim_token",
			Self::ClaimedByWorkerAt => "claimed_by_worker_at",
			Self::ExecutionStartedAt => "execution_started_at",
			Self::ExecutionHeartbeatAt => "execution_heartbeat_at",
			Self::FinishedAt => "finished_at",
			Self::Reason => "reason",
			Self::DeadLetteredAt => "dead_lettered_at",
			Self::Key => "key",
			Self::PausedAt => "paused_at",
			Self::CreatedAt => "created_at",
			Self::UpdatedAt => "updated_at",
		}
	}

	/// Returns the column's name in the given layout.
	pub const fn name_in(self, layout: QueueSchemaLayout) -> &'static str {
		match (self, layout) {
			(Self::LastErrorMsg, QueueSchemaLayout::SourceEpoch) => {
				SOURCE_EPOCH_LAST_ERROR_MESSAGE_COLUMN_NAME
			}
			_ => self.name(),
		}
	}

	pub(in crate::queue) const fn validation_type(self) -> &'static str {
		match self {
			Self::Id | Self::OriginalJobId | Self::WorkerClaimToken => "bytea",
			Self::TaskName
			| Self::Status
			| Self::LastErrorMsg
			| Self::DedupeKey
			| Self::WorkerOwnerId
			| Self::Reason
			| Self::Key => "text",
			Self::Payload => "jsonb",
			Self::RetryCount | Self::MaxRetries => "integer",
			Self::TimeoutNanos => "bigint",
			Self::RunAtOrAfter
			| Self::ClaimedByWorkerAt
			| Self::ExecutionStartedAt
			| Self::ExecutionHeartbeatAt
			| Self::FinishedAt
			| Self::DeadLetteredAt
			| Self::PausedAt
			| Self::CreatedAt
			| Self::UpdatedAt => "timestamp with time zone",
		}
	}

	pub(in crate::queue) const fn requires_bytewise_collation(self) -> bool {
		matches!(
			self,
			Self::TaskName
				| Self::Status
				| Self::LastErrorMsg
				| Self::DedupeKey
				| Self::WorkerOwnerId
				| Self::Reason
				| Self::Key
		)
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::queue) enum QueueProjectionField {
	PayloadJson,
	RunAtOrAfterUnixMicros,
	ClaimedByWorkerAtUnixMicros,
	ExecutionStartedAtUnixMicros,
	ExecutionHeartbeatAtUnixMicros,
	FinishedAtUnixMicros,
	DeadLetteredAtUnixMicros,
	CreatedAtUnixMicros,
	UpdatedAtUnixMicros,
}

impl QueueProjectionField {
	pub(in crate::queue) const fn name(self) -> &'static str {
		match self {
			Self::PayloadJson => "payload_json",
			Self::RunAtOrAfterUnixMicros => "run_at_or_after_unix_micros",
			Self::ClaimedByWorkerAtUnixMicros => "claimed_by_worker_at_unix_micros",
			Self::ExecutionStartedAtUnixMicros => "execution_started_at_unix_micros",
			Self::ExecutionHeartbeatAtUnixMicros => "execution_heartbeat_at_unix_micros",
			Self::FinishedAtUnixMicros => "finished_at_unix_micros",
			Self::DeadLetteredAtUnixMicros => "dead_lettered_at_unix_micros",
			Self::CreatedAtUnixMicros => "created_at_unix_micros",
			Self::UpdatedAtUnixMicros => "updated_at_unix_micros",
		}
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::queue) struct QueueIndexDef {
	pub(in crate::queue) table: QueueTable,
	pub(in crate::queue) kind: &'static str,
	pub(in crate::queue) suffix: &'static str,
	pub(in crate::queue) columns: &'static [QueueColumn],
	pub(in crate::queue) predicate: QueueIndexPredicate,
	pub(in crate::queue) unique: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::queue) enum QueueIndexPredicate {
	None,
	PendingStatus,
	WorkerOwnerIdPresent,
	RunningExecutionHeartbeatPresent,
	TerminalFinishedAtPresent,
	ActiveDedupe,
	PauseTaskNamePresent,
}

impl QueueIndexPredicate {
	/// The predicate as PostgreSQL prints it when no other object shadows a built-in name.
	pub(in crate::queue) fn catalog_expression(self) -> Option<&'static str> {
		match self {
			Self::None => None,
			Self::PendingStatus => Some("(status = 'pending'::text)"),
			Self::WorkerOwnerIdPresent => Some("(worker_id IS NOT NULL)"),
			Self::RunningExecutionHeartbeatPresent => {
				Some("((status = 'running'::text) AND (execution_heartbeat_at IS NOT NULL))")
			}
			Self::TerminalFinishedAtPresent => Some(
				"((status = ANY (ARRAY['completed'::text, 'failed'::text])) AND (finished_at IS NOT NULL))",
			),
			Self::ActiveDedupe => Some(
				"((dedupe_key IS NOT NULL) AND (status = ANY (ARRAY['pending'::text, 'running'::text])))",
			),
			Self::PauseTaskNamePresent => Some("(task_name IS NOT NULL)"),
		}
	}
}
