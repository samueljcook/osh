//! The error types of Queue.
//!
//! Each operation returns an error type that lists exactly the failures that it can produce,
//! and operations with the same failures share one type. A database failure is the `Database`
//! variant. A protocol state that this release cannot run under is the `Protocol` variant, so a
//! caller can tell it apart from a database failure without inspecting one. An operation that
//! owns its transaction keeps both an operation failure and a rollback failure in its
//! `RollbackFailed` variant.
//!
//! A task handler's own error ([`TaskError`](super::TaskError)) stays separate from these
//! types.

use super::engine;
use crate::db::database_work_error;
use primitives_rs::{id::time_sortable as id, random::secure_bytes as random};
use std::time::Duration;

/// A task name is invalid.
#[derive(Debug, thiserror::Error)]
pub enum TaskNameError {
	/// The task name was empty.
	#[error("queue task name is required")]
	Empty,
	/// The task name contained a byte outside ASCII letters, digits, underscores, hyphens, and
	/// dots, or started with a hyphen or a dot.
	#[error(
		"queue task name must contain only ASCII letters, digits, underscores, hyphens, or dots, and must start with an ASCII letter, digit, or underscore"
	)]
	InvalidByte,
	/// The task name exceeded [`crate::queue::MAX_TASK_NAME_LEN`].
	#[error("queue task name is {actual} bytes, maximum is {max}")]
	TooLong {
		/// The supplied length in bytes.
		actual: usize,
		/// Maximum accepted byte length, [`crate::queue::MAX_TASK_NAME_LEN`].
		max: usize,
	},
}

/// A dedupe key is invalid.
#[derive(Debug, thiserror::Error)]
pub enum DedupeKeyError {
	/// The dedupe key was empty.
	#[error("queue dedupe key is required")]
	Empty,
	/// The dedupe key exceeded [`crate::queue::MAX_DEDUPE_KEY_LEN`].
	#[error("queue dedupe key is {actual} bytes, maximum is {max}")]
	TooLong {
		/// The supplied length in bytes.
		actual: usize,
		/// Maximum accepted byte length, [`crate::queue::MAX_DEDUPE_KEY_LEN`].
		max: usize,
	},
	/// The dedupe key contained a null byte.
	#[error("queue dedupe key must not contain null bytes")]
	ContainsNullByte,
}

/// A worker owner ID is invalid.
#[derive(Debug, thiserror::Error)]
pub enum WorkerOwnerIdError {
	/// The worker owner ID was empty.
	#[error("queue worker owner id is required")]
	Empty,
	/// The worker owner ID exceeded [`crate::queue::MAX_WORKER_OWNER_ID_LEN`].
	#[error("queue worker owner id is {actual} bytes, maximum is {max}")]
	TooLong {
		/// The supplied length in bytes.
		actual: usize,
		/// Maximum accepted byte length, [`crate::queue::MAX_WORKER_OWNER_ID_LEN`].
		max: usize,
	},
	/// The worker owner ID contained a null byte.
	#[error("queue worker owner id must not contain null bytes")]
	ContainsNullByte,
}

/// A logical worker name is invalid.
#[derive(Debug, thiserror::Error)]
pub enum WorkerNameError {
	/// The worker name was empty.
	#[error("queue worker name is required")]
	Empty,
	/// The worker name exceeded [`crate::queue::MAX_QUEUE_WORKER_NAME_LEN`].
	#[error("queue worker name is {actual} bytes, maximum is {max}")]
	TooLong {
		/// The supplied length in bytes.
		actual: usize,
		/// Maximum accepted byte length, [`crate::queue::MAX_QUEUE_WORKER_NAME_LEN`].
		max: usize,
	},
	/// The worker name contained a null byte.
	#[error("queue worker name must not contain null bytes")]
	ContainsNullByte,
}

/// A fresh worker owner ID could not be created from a worker name.
#[derive(Debug, thiserror::Error)]
pub enum GenerateWorkerOwnerIdError {
	/// Generating the unique suffix failed.
	#[error("queue worker owner id generation failed")]
	IdGeneration(#[source] random::RandomError),
}

/// A schedule time in Unix microseconds is outside the accepted range.
#[derive(Debug, thiserror::Error)]
pub enum RunAtOrAfterMicrosError {
	/// The value was negative.
	#[error("queue scheduled run time is {actual} Unix microseconds, minimum is 0")]
	Negative {
		/// The requested Unix microsecond timestamp.
		actual: i64,
	},
	/// The value exceeded [`crate::queue::MAX_RUN_AT_OR_AFTER_UNIX_MICROS`].
	#[error("queue scheduled run time is {actual} Unix microseconds, maximum is {max}")]
	TooLarge {
		/// The requested Unix microsecond timestamp.
		actual: i64,
		/// Maximum accepted timestamp, [`crate::queue::MAX_RUN_AT_OR_AFTER_UNIX_MICROS`].
		max: i64,
	},
}

/// A schedule time as a system time is outside the accepted range.
#[derive(Debug, thiserror::Error)]
pub enum RunAtOrAfterSystemTimeError {
	/// The system time was before the Unix epoch.
	#[error("queue scheduled run time cannot be before the Unix epoch")]
	BeforeUnixEpoch,
	/// The system time exceeded [`crate::queue::MAX_RUN_AT_OR_AFTER_UNIX_MICROS`].
	#[error("queue scheduled run time is {actual} Unix microseconds, maximum is {max}")]
	TooLarge {
		/// The requested Unix microsecond timestamp.
		actual: u128,
		/// Maximum accepted timestamp, [`crate::queue::MAX_RUN_AT_OR_AFTER_UNIX_MICROS`].
		max: i64,
	},
}

/// A job policy cannot be stored.
#[derive(Debug, thiserror::Error)]
pub enum JobPolicyError {
	/// The maximum retry count did not fit into a non-negative Postgres `INT`.
	#[error("queue max retries must fit into a non-negative Postgres INT")]
	MaxRetriesTooLarge,
	/// The job timeout was zero or too large to store as nanoseconds.
	#[error("queue job timeout must be positive and fit in nanoseconds")]
	InvalidTimeout,
}

/// A task handler could not be registered.
#[derive(Debug, thiserror::Error)]
pub enum RegisterTaskError {
	/// The job policy of the task definition cannot be stored.
	#[error("invalid queue job policy")]
	JobPolicy(#[from] JobPolicyError),
	/// The retry policy is invalid.
	#[error("invalid queue retry policy")]
	RetryPolicy(#[from] RetryPolicyError),
	/// A handler is already registered under the task name.
	#[error("queue task is already registered")]
	AlreadyRegistered,
}

/// A row that the database returned does not satisfy the queue contract.
///
/// The schema's constraints keep these states out of the tables, so they appear only when
/// the tables were changed outside Paranoid.
#[derive(Debug, thiserror::Error)]
pub enum PersistedRowError {
	/// A job's status text was not a queue status.
	#[error("queue persisted invalid job status {status:?}")]
	InvalidStatus {
		/// The persisted text.
		status: Box<str>,
	},
	/// A job's owner label and times did not form a state of its status.
	#[error("queue persisted lifecycle fields that do not form a {status:?} job")]
	InvalidLifecycle {
		/// The persisted status.
		status: crate::queue::JobStatus,
	},
	/// A dead-letter reason text was not a queue reason.
	#[error("queue persisted invalid dead-letter reason {reason:?}")]
	InvalidDeadLetterReason {
		/// The persisted text.
		reason: Box<str>,
	},
	/// A job's timeout value was outside the queue contract.
	#[error("queue persisted invalid timeout_nanos {timeout_nanos}")]
	InvalidTimeout {
		/// The persisted value.
		timeout_nanos: i64,
	},
	/// A job's retry count was outside the queue contract.
	#[error("queue persisted invalid retry_count {retry_count}")]
	InvalidRetryCount {
		/// The persisted value.
		retry_count: i32,
	},
	/// A job's maximum retry count was outside the queue contract.
	#[error("queue persisted invalid max_retries {max_retries}")]
	InvalidMaxRetries {
		/// The persisted value.
		max_retries: i32,
	},
	/// A persisted identifier was not a job identifier.
	#[error("queue persisted an invalid job identifier")]
	InvalidId(#[source] id::SortableIdParseError),
	/// A persisted worker owner ID was invalid.
	#[error("queue persisted an invalid worker owner id")]
	InvalidWorkerOwnerId(#[source] WorkerOwnerIdError),
	/// A persisted task name was invalid.
	#[error("queue persisted an invalid task name")]
	InvalidTaskName(#[source] TaskNameError),
	/// A persisted dedupe key was invalid.
	#[error("queue persisted an invalid dedupe key")]
	InvalidDedupeKey(#[source] DedupeKeyError),
}

/// A request to enqueue one job is invalid.
#[derive(Debug, thiserror::Error)]
pub enum EnqueueRequestError {
	/// The job policy of the task definition cannot be stored.
	#[error("invalid queue job policy")]
	JobPolicy(#[from] JobPolicyError),
	/// The payload could not be serialized as JSON, or nests more than 100 arrays or objects
	/// along a path.
	#[error("queue payload could not be encoded as JSON")]
	PayloadJson(#[source] serde_json::Error),
	/// The serialized payload exceeded the store's payload limit.
	#[error("queue payload JSON is at least {actual_minimum} bytes, maximum is {max}")]
	PayloadJsonTooLarge {
		/// Lower bound on the serialized payload size.
		actual_minimum: usize,
		/// The store's payload limit.
		max: usize,
	},
	/// Generating the job identifier failed.
	#[error("queue job id generation failed")]
	JobIdGeneration(#[source] random::RandomError),
}

/// A retry policy is invalid.
#[derive(Debug, thiserror::Error)]
pub enum RetryPolicyError {
	/// The jitter fraction was negative, infinite, or not a number.
	#[error("queue retry jitter fraction must be finite and non-negative")]
	JitterFractionInvalid,
	/// The exponential multiplier was not finite and greater than one.
	#[error("queue retry exponential multiplier must be finite and greater than one")]
	MultiplierInvalid,
	/// The initial backoff of an exponential policy was zero.
	#[error("queue retry initial backoff must be positive")]
	InitialBackoffIsZero,
	/// The maximum backoff was zero.
	#[error("queue retry max backoff must be positive")]
	MaxBackoffIsZero,
	/// The maximum backoff was below [`crate::queue::MIN_RETRY_BACKOFF`].
	#[error("queue retry max backoff must be at least the minimum retry backoff")]
	MaxBackoffBelowMinimum,
	/// The initial backoff of an exponential policy was above its maximum backoff.
	#[error("queue retry initial backoff must not exceed the max backoff")]
	InitialBackoffAboveMax,
	/// The fixed delay was zero.
	#[error("queue retry fixed delay must be positive")]
	FixedDelayIsZero,
	/// The maximum backoff exceeded [`crate::queue::MAX_RETRY_BACKOFF`].
	#[error("queue retry max backoff is {actual:?}, maximum is {max:?}")]
	MaxBackoffTooLarge {
		/// The requested maximum backoff.
		actual: Duration,
		/// The longest accepted backoff, [`crate::queue::MAX_RETRY_BACKOFF`].
		max: Duration,
	},
	/// The fixed delay, lengthened by its jitter fraction, exceeded
	/// [`crate::queue::MAX_RETRY_BACKOFF`].
	#[error(
		"queue retry fixed delay {delay:?} with jitter fraction {jitter_fraction} can exceed the maximum backoff {max:?}"
	)]
	FixedDelayTooLarge {
		/// The requested delay.
		delay: Duration,
		/// The requested jitter fraction.
		jitter_fraction: f64,
		/// The longest accepted backoff, [`crate::queue::MAX_RETRY_BACKOFF`].
		max: Duration,
	},
}

/// A worker configuration is invalid.
#[derive(Debug, thiserror::Error)]
pub enum WorkerConfigError {
	/// The poll interval was zero.
	#[error("queue worker poll interval must be positive")]
	PollIntervalIsZero,
	/// The concurrency was zero.
	#[error("queue worker concurrency must be positive")]
	ConcurrencyIsZero,
	/// The concurrency exceeded [`crate::queue::MAX_WORKER_CONCURRENCY`].
	#[error("queue worker concurrency is {actual}, maximum is {max}")]
	ConcurrencyTooLarge {
		/// The requested concurrency.
		actual: u32,
		/// Maximum accepted concurrency, [`crate::queue::MAX_WORKER_CONCURRENCY`].
		max: u32,
	},
	/// The stale threshold was zero.
	#[error("queue worker stale threshold must be positive")]
	StaleThresholdIsZero,
	/// The execution heartbeat interval was zero.
	#[error("queue worker execution heartbeat interval must be positive")]
	ExecutionHeartbeatIntervalIsZero,
	/// The execution heartbeat interval was not below the stale threshold.
	#[error("queue worker execution heartbeat interval must be less than the stale threshold")]
	ExecutionHeartbeatIntervalNotBelowStaleThreshold,
	/// The database operation timeout was zero.
	#[error("queue worker database operation timeout must be positive")]
	DatabaseOperationTimeoutIsZero,
	/// The database operation timeout exceeded
	/// [`crate::queue::MAX_WORKER_DATABASE_OPERATION_TIMEOUT`].
	#[error("queue worker database operation timeout is {actual:?}, maximum is {max:?}")]
	DatabaseOperationTimeoutTooLarge {
		/// The requested timeout.
		actual: Duration,
		/// The longest accepted timeout, [`crate::queue::MAX_WORKER_DATABASE_OPERATION_TIMEOUT`].
		max: Duration,
	},
	/// The minimum claim-error backoff was zero.
	#[error("queue worker claim error minimum backoff must be positive")]
	ClaimErrorMinBackoffIsZero,
	/// The maximum claim-error backoff was below the minimum.
	#[error(
		"queue worker claim error maximum backoff must be at least the claim error minimum backoff"
	)]
	ClaimErrorMaxBackoffBelowMin,
}

/// Defines an error enum for an operation that can fail with the listed causes and the
/// database work that the operation owns. Each cause is one variant. The macro appends the
/// variants `Protocol`, `Database`, and `RollbackFailed` after the causes, and implements the
/// conversions that the transaction runners need.
macro_rules! queue_error {
	(
		$(#[$meta:meta])*
		pub enum $name:ident {
			$($source:ident),* $(,)?
		}
	) => {
		queue_error!(@collect [$(#[$meta])*] $name [] [] $($source)*);
	};

	(@collect [$($meta:tt)*] $name:ident [$($variants:tt)*] [$($impls:tt)*]) => {
		database_work_error! {
			$($meta)*
			pub enum $name {
				$($variants)*
			}
		}
		$($impls)*
	};

	(@collect [$($meta:tt)*] $name:ident [$($variants:tt)*] [$($impls:tt)*] ErrorMsgContainsNullByte $($rest:ident)*) => {
		queue_error!(@collect [$($meta)*] $name [$($variants)*
			/// The error message contained a null byte, which Postgres text cannot store.
			#[error("queue error message must not contain null bytes")]
			ErrorMsgContainsNullByte,
		] [$($impls)*

		] $($rest)*);
	};

	(@collect [$($meta:tt)*] $name:ident [$($variants:tt)*] [$($impls:tt)*] RetryBackoffTooLarge $($rest:ident)*) => {
		queue_error!(@collect [$($meta)*] $name [$($variants)*
			/// The retry backoff was too large to bind as microseconds.
			#[error("queue retry backoff is too large")]
			RetryBackoffTooLarge,
		] [$($impls)*

		] $($rest)*);
	};

	(@collect [$($meta:tt)*] $name:ident [$($variants:tt)*] [$($impls:tt)*] JobIdGeneration $($rest:ident)*) => {
		queue_error!(@collect [$($meta)*] $name [$($variants)*
			/// Generating a job identifier failed.
			#[error("queue job id generation failed")]
			JobIdGeneration(#[from] primitives_rs::random::secure_bytes::RandomError),
		] [$($impls)*

		] $($rest)*);
	};

	(@collect [$($meta:tt)*] $name:ident [$($variants:tt)*] [$($impls:tt)*] ClaimTokenGeneration $($rest:ident)*) => {
		queue_error!(@collect [$($meta)*] $name [$($variants)*
			/// Generating a claim token failed.
			#[error("queue claim token generation failed")]
			ClaimTokenGeneration(#[from] primitives_rs::random::secure_bytes::RandomError),
		] [$($impls)*

		] $($rest)*);
	};

	(@collect [$($meta:tt)*] $name:ident [$($variants:tt)*] [$($impls:tt)*] OwnerIdGeneration $($rest:ident)*) => {
		queue_error!(@collect [$($meta)*] $name [$($variants)*
			/// Generating the owner label of a claim failed.
			#[error("queue worker owner id generation failed")]
			OwnerIdGeneration(#[from] $crate::queue::GenerateWorkerOwnerIdError),
		] [$($impls)*

		] $($rest)*);
	};

	(@collect [$($meta:tt)*] $name:ident [$($variants:tt)*] [$($impls:tt)*] JobNotRunning $($rest:ident)*) => {
		queue_error!(@collect [$($meta)*] $name [$($variants)*
			/// The job does not exist, is not running, or is no longer claimed by this claim.
			#[error("queue job is not running")]
			JobNotRunning,
		] [$($impls)*
			impl $crate::queue::JobNotRunningCause for $name {
				fn job_not_running() -> Self {
					Self::JobNotRunning
				}
			}
		] $($rest)*);
	};

	(@collect [$($meta:tt)*] $name:ident [$($variants:tt)*] [$($impls:tt)*] Runtime $($rest:ident)*) => {
		queue_error!(@collect [$($meta)*] $name [$($variants)*
			/// A Queue operation on a v11 installation failed for a reason other than a database or
			/// protocol failure. The cause's `kind` classifies it.
			#[error("the Queue operation of the runtime failed")]
			Runtime(#[source] $crate::queue::QueueRuntimeError),
		] [$($impls)*
			impl $crate::queue::FromNativeQueueError for $name {
				fn from_native_queue_error(
					error: $crate::queue::engine::NativeQueueGroupError,
				) -> Self {
					use $crate::queue::engine::NativeQueueGroupError as Native;
					match error {
						Native::Database(error) => Self::Database(error),
						Native::Protocol(error) => Self::Protocol(error),
						Native::RollbackFailed {
							operation_error,
							rollback_error,
						} => Self::RollbackFailed {
							operation_error: Box::new(Self::from_native_queue_error(*operation_error)),
							rollback_error,
						},
						error => Self::Runtime($crate::queue::QueueRuntimeError::new(error)),
					}
				}
			}
		] $($rest)*);
	};

	(@collect [$($meta:tt)*] $name:ident [$($variants:tt)*] [$($impls:tt)*] TimedOut $($rest:ident)*) => {
		queue_error!(@collect [$($meta)*] $name [$($variants)*
			/// A worker-owned database operation did not finish within the worker's database
			/// operation timeout.
			#[error("queue worker database operation {operation} timed out after {timeout:?}")]
			TimedOut {
				/// The label of the operation that timed out.
				operation: &'static str,
				/// The configured timeout.
				timeout: Duration,
			},
			/// The transaction commit of a worker-owned database operation did not finish within
			/// the worker's database operation timeout, so the change may or may not have
			/// committed.
			#[error(
				"queue worker database operation {operation} timed out during commit after {timeout:?}"
			)]
			CommitTimedOut {
				/// The label of the operation that timed out during commit.
				operation: &'static str,
				/// The configured timeout.
				timeout: Duration,
			},
		] [$($impls)*
			impl $crate::queue::WorkerStepFailure for $name {
				fn timed_out(operation: &'static str, timeout: Duration) -> Self {
					Self::TimedOut { operation, timeout }
				}

				fn commit_timed_out(operation: &'static str, timeout: Duration) -> Self {
					Self::CommitTimedOut { operation, timeout }
				}

				fn database_error(&self) -> Option<&$crate::db::DatabaseError> {
					match self {
						Self::Database(error) => Some(error),
						_ => None,
					}
				}
			}
		] $($rest)*);
	};
}

queue_error! {
	/// Recording a heartbeat for the current job failed.
	pub enum HeartbeatError { JobNotRunning, Runtime, TimedOut }
}

queue_error! {
	/// Claiming jobs in a worker failed.
	pub enum WorkerClaimError {
		ClaimTokenGeneration, OwnerIdGeneration, Runtime, TimedOut
	}
}

macro_rules! impl_is_protocol_failure {
	($($name:ident),*) => {$(
		impl $name {
			/// Whether the failure is a protocol failure, directly or as the failure that
			/// required a rollback.
			pub(in crate::queue) fn is_protocol_failure(&self) -> bool {
				match self {
					Self::Protocol(_) => true,
					Self::RollbackFailed {
						operation_error, ..
					} => operation_error.is_protocol_failure(),
					_ => false,
				}
			}
		}
	)*};
}

impl_is_protocol_failure!(HeartbeatError, WorkerClaimError);

queue_error! {
	/// A database step of a worker failed: marking a job started or returning claimed jobs to
	/// pending.
	pub enum WorkerStepError { Runtime, TimedOut }
}

queue_error! {
	/// Recovering running jobs with stale claims failed.
	pub enum RecoverStaleClaimsError { JobIdGeneration, Runtime, TimedOut }
}

queue_error! {
	/// Deleting finished jobs or expired dead letters failed.
	pub enum DeleteFinishedJobsError { Runtime, TimedOut }
}

queue_error! {
	/// A database step of a worker that returns a started job to pending failed.
	pub enum WorkerRequeueError { Runtime, TimedOut }
}

queue_error! {
	/// Persisting the end of a job's execution failed: completing the job, scheduling its
	/// retry, marking it failed, or moving it to dead-letter storage.
	pub enum WorkerPersistenceError {
		RetryBackoffTooLarge, ErrorMsgContainsNullByte, JobIdGeneration, Runtime, TimedOut
	}
}

macro_rules! impl_worker_database_outcome_queries {
	($($name:ident),*) => {$(
		impl $name {
			/// Whether a `COMMIT` in the failure's cause failed or timed out, which means that the
			/// change may or may not have committed.
			pub fn commit_outcome_is_unknown(&self) -> bool {
				match self {
					Self::Runtime(error) => error.commit_outcome_is_unknown(),
					Self::CommitTimedOut { .. } => true,
					Self::Database(error) => error.commit_outcome_is_unknown(),
					Self::RollbackFailed {
						operation_error,
						rollback_error,
					} => {
						operation_error.commit_outcome_is_unknown()
							|| rollback_error.commit_outcome_is_unknown()
					}
					_ => false,
				}
			}
		}
	)*};
}

macro_rules! impl_maintenance_database_failure_queries {
	($($name:ident),*) => {$(
		impl $name {
			pub(in crate::queue) fn involves_database_failure(&self) -> bool {
				match self {
					Self::Runtime(error) => {
						$crate::db::TransactionFailure::involves_database_failure(error.native())
					}
					Self::TimedOut { .. }
					| Self::CommitTimedOut { .. }
					| Self::Database(_)
					| Self::RollbackFailed { .. } => true,
					_ => false,
				}
			}
		}
	)*};
}

impl_worker_database_outcome_queries!(
	HeartbeatError,
	WorkerClaimError,
	WorkerStepError,
	RecoverStaleClaimsError,
	DeleteFinishedJobsError,
	WorkerRequeueError,
	WorkerPersistenceError
);

impl_maintenance_database_failure_queries!(RecoverStaleClaimsError, DeleteFinishedJobsError);

/// An error type with the cause that a claim no longer owns a running job.
pub(crate) trait JobNotRunningCause {
	/// The job does not exist, is not running, or is no longer claimed by this claim.
	fn job_not_running() -> Self;
}

/// What the worker internals need from an error type for an operation that runs under a worker's
/// database operation timeout.
pub trait WorkerStepFailure: crate::db::TransactionFailure {
	/// The operation did not finish within the timeout.
	fn timed_out(operation: &'static str, timeout: Duration) -> Self;

	/// The transaction commit of the operation did not finish within the timeout, so the commit
	/// outcome is unknown.
	fn commit_timed_out(operation: &'static str, timeout: Duration) -> Self;

	/// The database failure that this error is, when it is exactly one.
	fn database_error(&self) -> Option<&crate::db::DatabaseError>;
}

/// Processing one claimed job failed.
#[derive(Debug, thiserror::Error)]
pub enum WorkerJobError {
	/// Marking the job started failed.
	#[error("queue worker failed to mark a job started")]
	Started(#[from] WorkerStepError),
	/// Persisting the end of the job's execution failed, and the job went back to pending.
	#[error("queue worker failed to persist job state")]
	Persistence(#[source] WorkerPersistenceError),
	/// Persisting the end of the job's execution failed, and returning the job to pending also
	/// failed.
	#[error("queue worker failed to persist job state, then failed to requeue the job")]
	PersistenceAndRequeue {
		/// The persistence failure.
		#[source]
		persistence_error: WorkerPersistenceError,
		/// The failure to return the job to pending.
		requeue_error: WorkerRequeueError,
	},
	/// Random generation for the retry jitter failed.
	#[error("queue retry jitter random generation failed")]
	RetryJitterRandom(#[source] random::RandomError),
	/// The execution heartbeat failed because of the protocol.
	#[error("queue worker execution heartbeat failed")]
	Heartbeat(#[source] HeartbeatError),
	/// The heartbeat task panicked or was cancelled at the Tokio task boundary.
	#[error("queue worker heartbeat task failed to join")]
	HeartbeatTaskJoinFailed(#[source] tokio::task::JoinError),
	/// The heartbeat failed, and persisting the end of the job's execution also failed.
	#[error("queue worker heartbeat failed, then job finalization also failed")]
	HeartbeatAndFinalization {
		/// The heartbeat failure.
		heartbeat_error: Box<WorkerJobError>,
		/// The failure to persist the end of the job's execution.
		finalization_error: Box<WorkerJobError>,
	},
}

/// A long-running worker stopped with failures.
#[derive(Debug, thiserror::Error)]
pub enum WorkerRunError {
	/// Claiming jobs failed because of the protocol.
	#[error("queue worker failed to claim jobs")]
	Claim(#[source] WorkerClaimError),
	/// Processing a job failed.
	#[error("queue worker failed to process a job")]
	Job(#[source] WorkerJobError),
	/// A job task panicked or was cancelled at the Tokio task boundary.
	#[error("queue worker job task failed to join")]
	JobTaskJoinFailed(#[source] tokio::task::JoinError),
	/// The worker task panicked or was cancelled at the Tokio task boundary.
	#[error("queue worker task failed to join")]
	WorkerTaskJoinFailed(#[source] tokio::task::JoinError),
	/// Random generation for the startup jitter failed.
	#[error("queue worker startup jitter random generation failed")]
	StartupJitterRandom(#[source] random::RandomError),
	/// Recovering running jobs with stale claims failed, so the worker stopped.
	#[error("queue worker stale-claim recovery failed")]
	StaleClaimRecovery(#[source] RecoverStaleClaimsError),
	/// The worker's cleanup of finished jobs and dead letters failed, so the worker stopped.
	#[error("queue worker cleanup failed")]
	Cleanup(#[source] Box<crate::fleet::PgCronRunError<DeleteFinishedJobsError>>),
	/// A summary counter overflowed.
	#[error("queue worker summary counter overflowed")]
	SummaryCounterOverflow,
	/// Returning the worker's claimed jobs to pending failed.
	#[error("queue worker failed to return claimed jobs to pending")]
	ClaimedJobCleanup(#[source] WorkerStepError),
	/// A worker component failed, and returning the worker's claimed jobs to pending also
	/// failed.
	#[error("queue worker task failed, then failed to return worker-owned jobs to pending")]
	RuntimeFailureAndClaimedJobCleanupFailed {
		/// The failure of the worker component.
		worker_error: Box<WorkerRunError>,
		/// The failure to return the claimed jobs to pending.
		cleanup_error: WorkerStepError,
	},
	/// More than one worker component failed during the same shutdown.
	#[error("queue worker runtime had multiple failures")]
	MultipleFailures {
		/// The collected failures.
		failures: Vec<WorkerRunError>,
	},
}

/// Starting a long-running worker failed.
#[derive(Debug, thiserror::Error)]
pub enum StartWorkerError {
	/// The worker configuration is invalid.
	#[error("invalid queue worker configuration")]
	Config(#[from] WorkerConfigError),
}

/// A failure of a Queue operation of a `PgRuntime` that is neither a database failure nor a
/// protocol failure. The source chain holds the complete cause.
pub struct QueueRuntimeError(Box<engine::NativeQueueGroupError>);

/// The class of a failed Queue operation of a `PgRuntime`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum QueueRuntimeErrorKind {
	/// The runtime's code does not match the installation's recorded Queue, so the runtime
	/// cannot operate on the Queue.
	Admission,
	/// Concurrent work kept changing the records that the operation needed, and Paranoid's
	/// attempts ran out.
	Contention,
	/// The v10 Queue tables refused or failed the operation, while v10 deployments still write
	/// or while data retirement moves the v10 records.
	V10,
	/// Stored Queue state could not be interpreted or is inconsistent.
	State,
	/// A database statement or the transaction's own control failed. The error's source chain
	/// holds the database error.
	Database,
}

impl QueueRuntimeError {
	pub(crate) fn new(error: engine::NativeQueueGroupError) -> Self {
		Self(Box::new(error))
	}

	/// The Queue failure that this error holds.
	pub(in crate::queue) fn native(&self) -> &engine::NativeQueueGroupError {
		&self.0
	}

	/// Whether a `COMMIT` in the failure's cause failed, which means that the change may or may
	/// not have committed.
	pub fn commit_outcome_is_unknown(&self) -> bool {
		crate::db::TransactionFailure::commit_outcome_is_unknown(self.0.as_ref())
	}

	/// The class of the failure.
	pub fn kind(&self) -> QueueRuntimeErrorKind {
		native_kind(&self.0)
	}
}

fn native_kind(error: &engine::NativeQueueGroupError) -> QueueRuntimeErrorKind {
	use QueueRuntimeErrorKind as Kind;
	use engine::NativeQueueErrorClass as Class;
	match error.classify() {
		Class::CommitOutcomeUnknown | Class::Database => Kind::Database,
		Class::Admission => Kind::Admission,
		Class::Contention => Kind::Contention,
		Class::V10DedupeConflict | Class::V10 => Kind::V10,
		Class::NativeDedupeConflict | Class::State => Kind::State,
	}
}

impl std::fmt::Display for QueueRuntimeError {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		self.0.fmt(formatter)
	}
}

impl std::fmt::Debug for QueueRuntimeError {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(formatter, "QueueRuntimeError: {}", self.0)?;
		let mut source = std::error::Error::source(self.0.as_ref());
		while let Some(error) = source {
			write!(formatter, ": {error}")?;
			source = error.source();
		}
		Ok(())
	}
}

impl std::error::Error for QueueRuntimeError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(self.0.as_ref())
	}
}

/// Converts a native Queue failure into a worker error, keeping database and protocol failures
/// in the worker error's own variants.
pub(crate) trait FromNativeQueueError {
	fn from_native_queue_error(error: engine::NativeQueueGroupError) -> Self;
}
