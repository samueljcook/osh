use super::{ComponentSchemaMigrationVersion, Duration, QueueColumn, id};

/// Size, in bytes, of queue job identifiers.
pub const JOB_ID_LEN: usize = id::SORTABLE_ID_LEN;

pub(in crate::queue) const QUEUE_SCHEMA_COMPONENT: &str = "queue";
pub(in crate::queue) const QUEUE_SCHEMA_V1_TARGET: ComponentSchemaMigrationVersion<'static> =
	ComponentSchemaMigrationVersion::new(1, "paranoid.queue.v1");
/// The version that instances of an earlier compatibility epoch run on. Preparation under such
/// an epoch stops at this version, and activation continues from it.
pub(in crate::queue) const QUEUE_SCHEMA_SOURCE_EPOCH_TARGET: ComponentSchemaMigrationVersion<
	'static,
> = ComponentSchemaMigrationVersion::new(2, "paranoid.queue.v2");

/// Maximum task name length in bytes.
pub const MAX_TASK_NAME_LEN: usize = 128;

/// Maximum active dedupe key length in bytes.
pub const MAX_DEDUPE_KEY_LEN: usize = 512;

/// Maximum worker owner ID length in bytes.
pub const MAX_WORKER_OWNER_ID_LEN: usize = 512;

/// Maximum logical worker name length in bytes.
pub const MAX_QUEUE_WORKER_NAME_LEN: usize =
	MAX_WORKER_OWNER_ID_LEN - QUEUE_WORKER_OWNER_ID_SUFFIX_LEN;

pub(in crate::queue) const QUEUE_WORKER_OWNER_ID_SEPARATOR: &str = ".";
pub(in crate::queue) const QUEUE_WORKER_OWNER_ID_SUFFIX_LEN: usize = 1 + id::SORTABLE_ID_TEXT_LEN;

/// Maximum configurable serialized JSON payload size per queued job.
pub const MAX_PAYLOAD_JSON_SIZE_LIMIT: usize = 16 << 20;

/// Maximum accepted queued-job schedule time, as Unix microseconds.
///
/// This is `9999-12-31T23:59:59.999999Z`, which gives applications an
/// effectively-future scheduling bound without accepting nonsensical or
/// database-hostile timestamp values.
pub const MAX_RUN_AT_OR_AFTER_UNIX_MICROS: i64 = 253_402_300_799_999_999;

/// Maximum number of retries after the first attempt that a stored job can record.
pub const MAX_RETRIES: u32 = i32::MAX as u32;

/// Maximum number of jobs that can be claimed in one call.
pub(in crate::queue) const MAX_CLAIM_LIMIT: u32 = 1000;

/// Maximum number of jobs processed by one worker pass.
pub const MAX_WORKER_CONCURRENCY: u32 = MAX_CLAIM_LIMIT;

/// Minimum retry backoff returned by queue retry policies.
pub const MIN_RETRY_BACKOFF: Duration = Duration::from_millis(1);

/// The longest wait that a retry policy can produce: `i64::MAX` microseconds minus
/// [`MAX_RUN_AT_OR_AFTER_UNIX_MICROS`], which is 8,969,969,736,054,775,808 microseconds, about
/// 284,000 years.
///
/// A worker schedules a retry at the database time plus the wait. Paranoid's storage keeps that
/// time as signed 64-bit Unix microseconds, and the v10 Queue tables keep it as `TIMESTAMPTZ`,
/// which holds every such time. With a wait of at most this length and a database time of at
/// most [`MAX_RUN_AT_OR_AFTER_UNIX_MICROS`], the retry time is at most `i64::MAX`
/// microseconds, so both stores can hold it. The v10 statement multiplies the wait in
/// microseconds as a double-precision number, which represents this value exactly.
pub const MAX_RETRY_BACKOFF: Duration =
	Duration::from_micros((i64::MAX - MAX_RUN_AT_OR_AFTER_UNIX_MICROS) as u64);

/// The longest database operation timeout of a worker: 2,147,483,647 milliseconds, about 24.8
/// days.
///
/// A worker sets PostgreSQL's `statement_timeout` for each of its transactions to the part of
/// this timeout that remains, and `statement_timeout` accepts at most 2,147,483,647
/// milliseconds.
pub const MAX_WORKER_DATABASE_OPERATION_TIMEOUT: Duration = Duration::from_millis(2_147_483_647);

pub(in crate::queue) const GLOBAL_PAUSE_KEY: &str = "__global__";
pub(in crate::queue) const TASK_PAUSE_KEY_PREFIX: &str = "task:";
pub(in crate::queue) const STALE_EXECUTION_ERROR_MESSAGE: &str =
	"execution expired (stale threshold exceeded)";

pub(in crate::queue) const INDEX_KIND: &str = "idx";
pub(in crate::queue) const UNIQUE_INDEX_KIND: &str = "uidx";
pub(in crate::queue) const CHECK_KIND: &str = "chk";

pub(in crate::queue) const PENDING_RUN_AT_INDEX_SUFFIX: &str = "pending_run_at";
pub(in crate::queue) const PENDING_TASK_RUN_AT_INDEX_SUFFIX: &str = "pending_task_run_at";
pub(in crate::queue) const TASK_STATUS_INDEX_SUFFIX: &str = "task_status";
pub(in crate::queue) const WORKER_INDEX_SUFFIX: &str = "worker";
pub(in crate::queue) const EXECUTION_HEARTBEAT_INDEX_SUFFIX: &str = "execution_heartbeat";
pub(in crate::queue) const CLEANUP_INDEX_SUFFIX: &str = "cleanup";
pub(in crate::queue) const ACTIVE_DEDUPE_INDEX_SUFFIX: &str = "dedupe_active";
pub(in crate::queue) const DEAD_LETTERED_AT_INDEX_SUFFIX: &str = QueueColumn::DeadLetteredAt.name();
pub(in crate::queue) const TASK_DEAD_LETTERED_AT_INDEX_SUFFIX: &str = "task_dead_lettered_at";
pub(in crate::queue) const ORIGINAL_JOB_INDEX_SUFFIX: &str = "original_job";
pub(in crate::queue) const PAUSE_TASK_INDEX_SUFFIX: &str = QueueColumn::TaskName.name();
pub(in crate::queue) const JOB_STATUS_CONSTRAINT_SUFFIX: &str = "status_allowed";
pub(in crate::queue) const JOB_LIFECYCLE_CONSTRAINT_SUFFIX: &str = "status_lifecycle_shape";
pub(in crate::queue) const JOB_NUMERIC_CONSTRAINT_SUFFIX: &str = "numeric_domains";
pub(in crate::queue) const JOB_TEXT_CONSTRAINT_SUFFIX: &str = "text_domains";
pub(in crate::queue) const JOB_CLAIM_TOKEN_CONSTRAINT_SUFFIX: &str = "claim_token_shape";
pub(in crate::queue) const DEAD_LETTER_REASON_CONSTRAINT_SUFFIX: &str = "reason_allowed";
pub(in crate::queue) const DEAD_LETTER_NUMERIC_CONSTRAINT_SUFFIX: &str = "numeric_domains";
pub(in crate::queue) const DEAD_LETTER_TEXT_CONSTRAINT_SUFFIX: &str = "text_domains";
pub(in crate::queue) const PAUSE_KEY_TASK_CONSTRAINT_SUFFIX: &str = "key_task_match";
pub(in crate::queue) const PAUSE_TEXT_CONSTRAINT_SUFFIX: &str = "text_domains";
