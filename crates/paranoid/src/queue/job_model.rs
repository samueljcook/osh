use super::{
	DedupeKey, Duration, MAX_RUN_AT_OR_AFTER_UNIX_MICROS, PersistedRowError,
	RunAtOrAfterMicrosError, RunAtOrAfterSystemTimeError, SystemTime, TaskName, UNIX_EPOCH,
	WorkerClaimToken, WorkerOwnerId, fmt,
};

/// Queue job identifier.
pub type JobId = primitives_rs::id::time_sortable::SortableId;

/// Persisted queue job lifecycle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobStatus {
	/// Job is waiting to be claimed.
	Pending,
	/// Job is owned by a worker.
	Running,
	/// Job finished successfully.
	Completed,
	/// Job failed terminally or is awaiting operator retry.
	Failed,
}

/// The execution timeout that a queue job stores.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoredJobTimeout {
	/// The job stores no timeout of its own. A worker runs the job with the timeout of the
	/// [`TaskDef`](crate::queue::TaskDef) that is registered for the job's task.
	RegisteredDef,
	/// Run without a queue-enforced timeout.
	NoTimeout,
	/// Use this job-specific execution timeout.
	ExpiresAfter(Duration),
}

/// Earliest time at which a queued job may be claimed.
///
/// Callers supply this as an absolute Unix-microsecond instant, directly or through
/// [`Self::from_system_time`]. Paranoid's storage keeps it as Unix microseconds, and the v10
/// Queue tables keep it as `TIMESTAMPTZ`. A claim compares it with the database clock; the client
/// process clock is never consulted when deciding whether a job is due.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JobRunAtOrAfter {
	unix_micros: i64,
}

impl JobRunAtOrAfter {
	/// Creates a schedule time from Unix microseconds.
	///
	/// The microseconds are an absolute schedule input. A claim compares the stored value with
	/// the database clock, not with the caller's process clock.
	pub fn from_unix_micros(unix_micros: i64) -> Result<Self, RunAtOrAfterMicrosError> {
		if unix_micros < 0 {
			return Err(RunAtOrAfterMicrosError::Negative {
				actual: unix_micros,
			});
		}
		if unix_micros > MAX_RUN_AT_OR_AFTER_UNIX_MICROS {
			return Err(RunAtOrAfterMicrosError::TooLarge {
				actual: unix_micros,
				max: MAX_RUN_AT_OR_AFTER_UNIX_MICROS,
			});
		}
		Ok(Self { unix_micros })
	}

	/// Creates a schedule time from a [`SystemTime`].
	///
	/// This converts a client-supplied wall-clock instant into the absolute Unix-microsecond
	/// schedule domain that enqueue and operator retry accept. A claim still compares the stored
	/// value with the database clock.
	pub fn from_system_time(system_time: SystemTime) -> Result<Self, RunAtOrAfterSystemTimeError> {
		let duration = system_time
			.duration_since(UNIX_EPOCH)
			.map_err(|_| RunAtOrAfterSystemTimeError::BeforeUnixEpoch)?;
		let unix_micros = duration.as_micros();
		if unix_micros > MAX_RUN_AT_OR_AFTER_UNIX_MICROS as u128 {
			return Err(RunAtOrAfterSystemTimeError::TooLarge {
				actual: unix_micros,
				max: MAX_RUN_AT_OR_AFTER_UNIX_MICROS,
			});
		}
		Ok(Self {
			unix_micros: unix_micros as i64,
		})
	}

	/// Returns this schedule time as Unix microseconds.
	pub fn as_unix_micros(self) -> i64 {
		self.unix_micros
	}
}

/// Options for enqueuing a job.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnqueueOptions {
	/// Earliest claim time. `None` makes the job claimable immediately.
	pub run_at_or_after: Option<JobRunAtOrAfter>,
	/// Active dedupe key. `None` enqueues the job without deduplication. See [`DedupeKey`] for
	/// what a key prevents and what it does not.
	pub dedupe_key: Option<DedupeKey>,
}

/// Result of an enqueue operation.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct EnqueueResult {
	/// Inserted or reused job identifier.
	pub job_id: JobId,
	/// Whether an existing active job was reused.
	pub deduplicated: bool,
}

#[cfg(feature = "__internal")]
impl EnqueueResult {
	/// Builds a value so that tests can state an expected result.
	#[doc(hidden)]
	pub fn for_tests(job_id: JobId, deduplicated: bool) -> Self {
		Self {
			job_id,
			deduplicated,
		}
	}
}

/// The state of a queue job, with the data that the state has.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JobLifecycle {
	/// The job waits for a worker to claim it once it is due.
	Pending,
	/// A worker's claim holds the job.
	Running {
		/// The observable label of the claim. It is not a claim capability and grants nothing.
		worker_owner_id: WorkerOwnerId,
		/// Claim timestamp as Unix microseconds.
		claimed_at_unix_micros: i64,
		/// Whether the handler started running the job.
		execution: JobExecution,
		/// Newest execution heartbeat timestamp as Unix microseconds.
		execution_heartbeat_at_unix_micros: i64,
	},
	/// The job finished successfully.
	Completed {
		/// Finish timestamp as Unix microseconds.
		finished_at_unix_micros: i64,
	},
	/// The job failed terminally or awaits an operator's retry.
	Failed {
		/// Failure timestamp as Unix microseconds.
		finished_at_unix_micros: i64,
	},
}

/// Whether the handler of a running job started running it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobExecution {
	/// The handler has not started.
	Unstarted,
	/// The handler started.
	Started {
		/// Handler start timestamp as Unix microseconds.
		started_at_unix_micros: i64,
	},
}

impl JobLifecycle {
	/// The status of this state.
	pub fn status(&self) -> JobStatus {
		match self {
			Self::Pending => JobStatus::Pending,
			Self::Running { .. } => JobStatus::Running,
			Self::Completed { .. } => JobStatus::Completed,
			Self::Failed { .. } => JobStatus::Failed,
		}
	}

	/// Builds the state from the stored status and the stored fields of a running or finished
	/// job. `None` means that the fields do not form a state of `status`: a pending job has none
	/// of them, a running job has an owner label, a claim time, and a heartbeat time, a started
	/// running job also has a start time, and a completed or failed job has only a finish time.
	pub(in crate::queue) fn from_stored_fields(fields: StoredJobLifecycleFields) -> Option<Self> {
		let StoredJobLifecycleFields {
			status,
			worker_owner_id,
			claimed_at,
			started_at,
			heartbeat_at,
			finished_at,
		} = fields;
		match (
			status,
			worker_owner_id,
			claimed_at,
			started_at,
			heartbeat_at,
			finished_at,
		) {
			(JobStatus::Pending, None, None, None, None, None) => Some(Self::Pending),
			(JobStatus::Completed, None, None, None, None, Some(finished_at_unix_micros)) => {
				Some(Self::Completed {
					finished_at_unix_micros,
				})
			}
			(JobStatus::Failed, None, None, None, None, Some(finished_at_unix_micros)) => {
				Some(Self::Failed {
					finished_at_unix_micros,
				})
			}
			(
				JobStatus::Running,
				Some(worker_owner_id),
				Some(claimed_at_unix_micros),
				started_at,
				Some(execution_heartbeat_at_unix_micros),
				None,
			) => Some(Self::Running {
				worker_owner_id,
				claimed_at_unix_micros,
				execution: match started_at {
					Some(started_at_unix_micros) => JobExecution::Started {
						started_at_unix_micros,
					},
					None => JobExecution::Unstarted,
				},
				execution_heartbeat_at_unix_micros,
			}),
			_ => None,
		}
	}
}

/// The stored status and lifecycle fields of a job, which
/// [`JobLifecycle::from_stored_fields`] parses together.
pub(in crate::queue) struct StoredJobLifecycleFields {
	pub(in crate::queue) status: JobStatus,
	pub(in crate::queue) worker_owner_id: Option<WorkerOwnerId>,
	pub(in crate::queue) claimed_at: Option<i64>,
	pub(in crate::queue) started_at: Option<i64>,
	pub(in crate::queue) heartbeat_at: Option<i64>,
	pub(in crate::queue) finished_at: Option<i64>,
}

/// Queue job loaded from the database.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct Job {
	/// Job identifier.
	pub id: JobId,
	/// Task name.
	pub task_name: TaskName,
	/// Raw JSON payload text.
	pub payload_json: String,
	/// Lifecycle state, with the data of that state.
	pub lifecycle: JobLifecycle,
	/// Earliest run time as Unix microseconds.
	pub run_at_or_after_unix_micros: i64,
	/// Last error text, when present.
	pub last_error_msg: Option<String>,
	/// Number of retry attempts already consumed.
	pub retry_count: u32,
	/// Maximum retry attempts.
	pub max_retries: u32,
	/// Job timeout behavior.
	pub timeout: StoredJobTimeout,
	/// Active dedupe key, when present.
	pub dedupe_key: Option<DedupeKey>,
	/// Creation timestamp as Unix microseconds.
	pub created_at_unix_micros: i64,
	/// Last update timestamp as Unix microseconds.
	pub updated_at_unix_micros: i64,
}

/// One claimed running job plus the unforgeable authority for that exact claim.
///
/// Worker owner IDs are observable labels and may be reused. Queue worker
/// transitions accept this typed handle so a delayed heartbeat or completion
/// from an older claim cannot act on a later claim of the same job.
#[derive(Clone, Eq, PartialEq)]
#[must_use = "a claimed queue job carries live worker authority; complete, retry, fail, dead-letter, or return it explicitly"]
pub(in crate::queue) struct ClaimedJob {
	job: Job,
	worker_owner_id: WorkerOwnerId,
	claim_token: WorkerClaimToken,
}

impl ClaimedJob {
	pub(crate) fn new(
		job: Job,
		worker_owner_id: WorkerOwnerId,
		claim_token: WorkerClaimToken,
	) -> Self {
		Self {
			job,
			worker_owner_id,
			claim_token,
		}
	}

	/// Returns the claimed job observation.
	pub fn job(&self) -> &Job {
		&self.job
	}

	/// Returns the claimed job ID.
	pub fn job_id(&self) -> JobId {
		self.job.id
	}

	/// Returns the public worker owner label attached to this claim.
	pub fn worker_owner_id(&self) -> &WorkerOwnerId {
		&self.worker_owner_id
	}

	/// Returns the token proving this worker's claim on the job.
	#[cfg(not(feature = "__internal"))]
	pub(in crate::queue) fn claim_token(&self) -> &WorkerClaimToken {
		self.claim_token_inner()
	}

	/// Returns the token proving this worker's claim on the job.
	#[cfg(feature = "__internal")]
	#[doc(hidden)]
	pub fn claim_token(&self) -> &WorkerClaimToken {
		self.claim_token_inner()
	}

	fn claim_token_inner(&self) -> &WorkerClaimToken {
		&self.claim_token
	}
}

impl fmt::Debug for ClaimedJob {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("ClaimedJob")
			.field("job", &self.job)
			.field("worker_owner_id", &self.worker_owner_id)
			.field("claim_token", &self.claim_token)
			.finish()
	}
}

/// Reason a job was moved into dead-letter storage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeadLetterReason {
	/// The job exhausted its retry budget.
	MaxRetriesExceeded,
	/// The handler declared the error permanent.
	PermanentError,
	/// A v10 worker claimed the job without a handler for its task, or a v10 operator moved the
	/// failed job to dead-letter storage with this reason. Paranoid reads this reason from the
	/// dead letters that v10 wrote and does not write it.
	OperatorAction,
	/// Execution exceeded reclaim safety bounds.
	ExecutionExpired,
}

/// Queue job loaded from dead-letter storage.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct DeadLetterJob {
	/// Dead-letter row identifier.
	pub id: JobId,
	/// Source job identifier from the main jobs table.
	pub original_job_id: JobId,
	/// Task name.
	pub task_name: TaskName,
	/// Raw JSON payload text.
	pub payload_json: String,
	/// Error text captured at dead-letter time.
	pub last_error_msg: String,
	/// Number of retry attempts consumed before dead-lettering.
	pub retry_count: u32,
	/// Maximum retry attempts configured on the original job.
	pub max_retries: u32,
	/// Job timeout behavior carried by the original job.
	pub timeout: StoredJobTimeout,
	/// Active dedupe key carried by the original job, when present.
	pub dedupe_key: Option<DedupeKey>,
	/// Why the job was dead-lettered.
	pub reason: DeadLetterReason,
	/// Dead-letter timestamp as Unix microseconds.
	pub dead_lettered_at_unix_micros: i64,
	/// Creation timestamp of the dead letter, which is the dead-letter time, as Unix
	/// microseconds.
	pub created_at_unix_micros: i64,
	/// Last update timestamp of the dead letter, which is the dead-letter time, as Unix
	/// microseconds.
	pub updated_at_unix_micros: i64,
}

impl JobStatus {
	/// Returns the persisted status text.
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Pending => "pending",
			Self::Running => "running",
			Self::Completed => "completed",
			Self::Failed => "failed",
		}
	}

	#[cfg(not(feature = "__internal"))]
	pub(in crate::queue) fn parse(input: &str) -> Result<Self, PersistedRowError> {
		Self::parse_inner(input)
	}

	/// Parses this value from its stored text form.
	#[cfg(feature = "__internal")]
	#[doc(hidden)]
	pub fn parse(input: &str) -> Result<Self, PersistedRowError> {
		Self::parse_inner(input)
	}

	fn parse_inner(input: &str) -> Result<Self, PersistedRowError> {
		match input {
			"pending" => Ok(Self::Pending),
			"running" => Ok(Self::Running),
			"completed" => Ok(Self::Completed),
			"failed" => Ok(Self::Failed),
			other => Err(PersistedRowError::InvalidStatus {
				status: other.into(),
			}),
		}
	}
}

impl DeadLetterReason {
	/// Returns the persisted reason text.
	pub fn as_str(self) -> &'static str {
		match self {
			Self::MaxRetriesExceeded => "max_retries_exceeded",
			Self::PermanentError => "permanent_error",
			Self::OperatorAction => "operator_action",
			Self::ExecutionExpired => "execution_expired",
		}
	}

	#[cfg(not(feature = "__internal"))]
	pub(in crate::queue) fn parse(input: &str) -> Result<Self, PersistedRowError> {
		Self::parse_inner(input)
	}

	/// Parses this value from its stored text form.
	#[cfg(feature = "__internal")]
	#[doc(hidden)]
	pub fn parse(input: &str) -> Result<Self, PersistedRowError> {
		Self::parse_inner(input)
	}

	fn parse_inner(input: &str) -> Result<Self, PersistedRowError> {
		match input {
			"max_retries_exceeded" => Ok(Self::MaxRetriesExceeded),
			"permanent_error" => Ok(Self::PermanentError),
			"operator_action" => Ok(Self::OperatorAction),
			"execution_expired" => Ok(Self::ExecutionExpired),
			other => Err(PersistedRowError::InvalidDeadLetterReason {
				reason: other.into(),
			}),
		}
	}
}
