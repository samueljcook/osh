use super::engine::{OwnedJobMutation, RequiredExecutionState};
use super::{
	ClaimedJob, DatabaseOperationKind, DeadLetterReason, Duration, HeartbeatError, JobId,
	JobTimeout, MAX_RETRY_BACKOFF, MAX_WORKER_CONCURRENCY, MIN_RETRY_BACKOFF, OwnedJobUpdate,
	PersistedRowError, QUEUE_SET_LOCAL_STATEMENT_TIMEOUT_LABEL, RetryClassification, RetryPolicy,
	RetryPolicyError, StoredJobTimeout, TaskError, TaskName, UndeliveredClaimRelease,
	WorkerClaimError, WorkerClaimToken, WorkerConfigError, WorkerOwnerId, WorkerPersistenceError,
	WorkerQueue, WorkerRequeueError, WorkerStepError, WorkerStepFailure, WritePool, WriteTx,
	finish_pool_owned_write_transaction_and_preserve_rollback_error, is_error_msg_storable,
	owned_portable_query, validate_retry_schedule, worker,
};
use primitives_rs::random::jitter::{SymmetricJitter, generate_random_unit_f64};
use primitives_rs::random::secure_bytes as random;
use primitives_rs::time::{Deadline, duration_from_clamped_secs_f64};

#[doc(hidden)]
pub const QUEUE_SET_LOCAL_STATEMENT_TIMEOUT_SQL: &str =
	"SELECT pg_catalog.set_config('statement_timeout', $1, true)";

pub(super) struct OwnedRunningJobRetryPersistence<'a> {
	pub(super) claimed_job: &'a ClaimedJob,
	pub(super) new_retry_count: u32,
	pub(super) retry_after: Duration,
	pub(super) error_msg: &'a str,
}

pub(super) struct OwnedRunningJobDeadLetterPersistence<'a> {
	pub(super) claimed_job: &'a ClaimedJob,
	pub(super) error_msg: &'a str,
	pub(super) increment_retry_count: bool,
	pub(super) reason: DeadLetterReason,
}

pub(super) fn parse_timeout_from_persisted_nanos(
	timeout_nanos: i64,
) -> Result<StoredJobTimeout, PersistedRowError> {
	match timeout_nanos {
		-1 => Ok(StoredJobTimeout::NoTimeout),
		0 => Ok(StoredJobTimeout::RegisteredDef),
		value if value > 0 => Ok(StoredJobTimeout::ExpiresAfter(Duration::from_nanos(
			value as u64,
		))),
		value => Err(PersistedRowError::InvalidTimeout {
			timeout_nanos: value,
		}),
	}
}

pub(super) fn parse_retry_count_from_persisted_i32(
	retry_count: i32,
) -> Result<u32, PersistedRowError> {
	retry_count
		.try_into()
		.map_err(|_| PersistedRowError::InvalidRetryCount { retry_count })
}

pub(super) fn parse_max_retries_from_persisted_i32(
	max_retries: i32,
) -> Result<u32, PersistedRowError> {
	max_retries
		.try_into()
		.map_err(|_| PersistedRowError::InvalidMaxRetries { max_retries })
}

/// The timeout that a worker applies to a job, given the timeout that the job's row stores and
/// the timeout of the definition that is registered for the job's task.
pub fn resolve_queue_job_timeout(
	stored_timeout: StoredJobTimeout,
	registered_timeout: JobTimeout,
) -> Option<Duration> {
	match stored_timeout {
		StoredJobTimeout::NoTimeout => None,
		StoredJobTimeout::RegisteredDef => match registered_timeout {
			JobTimeout::NoTimeout => None,
			JobTimeout::ExpiresAfter(timeout) => Some(timeout),
		},
		StoredJobTimeout::ExpiresAfter(timeout) => Some(timeout),
	}
}

/// Runs `work` in a transaction that the pool supplies, under the worker's database operation
/// timeout. Both `work` and transaction completion run under a client-side deadline so that a
/// stalled network connection cannot hold the worker step past `timeout`.
pub(super) async fn run_worker_step<T, Failure: WorkerStepFailure>(
	pool: &WritePool,
	operation: &'static str,
	timeout: Duration,
	work: impl AsyncFnOnce(&mut WriteTx<'_>) -> Result<T, Failure>,
) -> Result<T, Failure> {
	if Deadline::new_after(timeout).is_err() {
		return Err(Failure::timed_out(operation, timeout));
	}
	let Some(tokio_deadline) = tokio::time::Instant::now().checked_add(timeout) else {
		return Err(Failure::timed_out(operation, timeout));
	};
	let mut tx = begin_worker_database_operation(pool, operation, timeout, tokio_deadline).await?;
	let marker = tx.statement_interruption_marker();
	let result = match tokio::time::timeout_at(tokio_deadline, work(&mut tx)).await {
		Ok(result) => result,
		Err(_) => {
			marker.mark_interrupted();
			drop(tx);
			return Err(Failure::timed_out(operation, timeout));
		}
	};
	let committing = result.is_ok();
	match tokio::time::timeout_at(
		tokio_deadline,
		finish_worker_database_operation(tx, operation, timeout, result),
	)
	.await
	{
		Ok(finished) => finished,
		Err(_) => {
			marker.mark_interrupted();
			if committing {
				Err(Failure::commit_timed_out(operation, timeout))
			} else {
				Err(Failure::timed_out(operation, timeout))
			}
		}
	}
}

/// Claims due pending jobs in one worker step. When the step ends without a delivered result
/// and its `COMMIT` may have succeeded (the caller drops the future, or the `COMMIT` fails or
/// its answer is lost), the claim is released by its token in the background and on a
/// best-effort basis.
pub(super) async fn claim_available_jobs_for_worker_with_database_operation_timeout(
	queue: &WorkerQueue,
	pool: &WritePool,
	registered_task_names: &[TaskName],
	claim_limit: u32,
	worker_owner_id: &WorkerOwnerId,
	timeout: Duration,
) -> Result<Vec<ClaimedJob>, WorkerClaimError> {
	let worker_claim_token = WorkerClaimToken::generate()?;
	let mut undelivered_claim =
		UndeliveredClaimRelease::arm(queue, pool, worker_owner_id, &worker_claim_token, timeout);
	let result: Result<Vec<ClaimedJob>, WorkerClaimError> =
		run_worker_step(pool, "claim available jobs", timeout, async |tx| {
			queue
				.worker_claim(
					tx,
					registered_task_names,
					claim_limit,
					worker_owner_id,
					worker_claim_token.clone(),
				)
				.await
		})
		.await;
	let commit_outcome_unknown = result
		.as_ref()
		.err()
		.is_some_and(|error| error.commit_outcome_is_unknown());
	if result.is_ok() || !commit_outcome_unknown {
		undelivered_claim.disarm();
	}
	result
}

pub(super) async fn return_jobs_claimed_with_token_with_database_operation_timeout(
	queue: &WorkerQueue,
	pool: &WritePool,
	worker_owner_id: &WorkerOwnerId,
	worker_claim_token: &WorkerClaimToken,
	timeout: Duration,
) -> Result<(), WorkerStepError> {
	run_worker_step(
		pool,
		"return jobs claimed with token",
		timeout,
		async |tx| {
			queue
				.worker_return_jobs_of_token(tx, worker_owner_id, worker_claim_token)
				.await
		},
	)
	.await
}

pub(super) async fn mark_owned_running_job_started_with_database_operation_timeout(
	queue: &WorkerQueue,
	pool: &WritePool,
	claimed_job: &ClaimedJob,
	timeout: Duration,
) -> Result<OwnedJobUpdate<()>, WorkerStepError> {
	run_worker_step(
		pool,
		"mark owned running job started",
		timeout,
		async |tx| {
			queue
				.worker_owned(tx, claimed_job, OwnedJobMutation::StartExecution)
				.await
		},
	)
	.await
}

pub(super) async fn mark_owned_running_job_completed_with_database_operation_timeout(
	queue: &WorkerQueue,
	pool: &WritePool,
	claimed_job: &ClaimedJob,
	timeout: Duration,
) -> Result<OwnedJobUpdate<()>, WorkerPersistenceError> {
	run_worker_step(
		pool,
		"mark owned running job completed",
		timeout,
		async |tx| {
			queue
				.worker_owned(tx, claimed_job, OwnedJobMutation::Complete)
				.await
		},
	)
	.await
}

pub(super) async fn touch_owned_running_job_execution_heartbeat_with_database_operation_timeout(
	queue: &WorkerQueue,
	pool: &WritePool,
	claimed_job: &ClaimedJob,
	timeout: Duration,
) -> Result<(), HeartbeatError> {
	run_worker_step(
		pool,
		"touch owned running job execution heartbeat",
		timeout,
		async |tx| {
			queue
				.worker_owned::<HeartbeatError>(tx, claimed_job, OwnedJobMutation::Heartbeat)
				.await?
				.into_result()
		},
	)
	.await
}

pub(super) async fn schedule_owned_running_job_retry_with_database_operation_timeout(
	queue: &WorkerQueue,
	pool: &WritePool,
	retry_persistence: OwnedRunningJobRetryPersistence<'_>,
	timeout: Duration,
) -> Result<OwnedJobUpdate<()>, WorkerPersistenceError> {
	let OwnedRunningJobRetryPersistence {
		claimed_job,
		new_retry_count,
		retry_after,
		error_msg,
	} = retry_persistence;
	run_worker_step(
		pool,
		"schedule owned running job retry",
		timeout,
		async |tx| {
			validate_retry_schedule(retry_after, error_msg)?;
			queue
				.worker_owned(
					tx,
					claimed_job,
					OwnedJobMutation::ScheduleRetry {
						new_retry_count,
						retry_after,
						error_msg: error_msg.to_owned(),
					},
				)
				.await
		},
	)
	.await
}

pub(super) async fn move_owned_running_job_to_dead_letter_with_database_operation_timeout(
	queue: &WorkerQueue,
	pool: &WritePool,
	dead_letter_persistence: OwnedRunningJobDeadLetterPersistence<'_>,
	timeout: Duration,
) -> Result<OwnedJobUpdate<JobId>, WorkerPersistenceError> {
	let OwnedRunningJobDeadLetterPersistence {
		claimed_job,
		error_msg,
		increment_retry_count,
		reason,
	} = dead_letter_persistence;
	run_worker_step(
		pool,
		"move owned running job to dead letter",
		timeout,
		async |tx| {
			if !is_error_msg_storable(error_msg) {
				return Err(WorkerPersistenceError::ErrorMsgContainsNullByte);
			}
			{
				queue
					.worker_move_to_dead_letter(
						tx,
						claimed_job,
						error_msg,
						increment_retry_count,
						reason,
					)
					.await
			}
		},
	)
	.await
}

pub(super) async fn mark_owned_running_job_failed_with_database_operation_timeout(
	queue: &WorkerQueue,
	pool: &WritePool,
	claimed_job: &ClaimedJob,
	error_msg: &str,
	increment_retry_count: bool,
	timeout: Duration,
) -> Result<OwnedJobUpdate<()>, WorkerPersistenceError> {
	run_worker_step(pool, "mark owned running job failed", timeout, async |tx| {
		if !is_error_msg_storable(error_msg) {
			return Err(WorkerPersistenceError::ErrorMsgContainsNullByte);
		}
		{
			queue
				.worker_owned(
					tx,
					claimed_job,
					OwnedJobMutation::Fail {
						error_msg: error_msg.to_owned(),
						retry_consumption: worker::steps::retry_consumption(increment_retry_count),
					},
				)
				.await
		}
	})
	.await
}

pub(super) async fn return_owned_started_running_job_to_pending_with_database_operation_timeout(
	queue: &WorkerQueue,
	pool: &WritePool,
	claimed_job: &ClaimedJob,
	timeout: Duration,
) -> Result<OwnedJobUpdate<()>, WorkerRequeueError> {
	run_worker_step(
		pool,
		"return owned started running job to pending",
		timeout,
		async |tx| {
			queue
				.worker_owned(
					tx,
					claimed_job,
					OwnedJobMutation::ReturnPending(RequiredExecutionState::Started),
				)
				.await
		},
	)
	.await
}

pub(super) async fn return_available_owned_unstarted_running_jobs_to_pending_with_database_operation_timeout(
	queue: &WorkerQueue,
	pool: &WritePool,
	claimed_jobs: &[ClaimedJob],
	timeout: Duration,
) -> Result<u64, WorkerStepError> {
	run_worker_step(
		pool,
		"return available owned unstarted running jobs to pending",
		timeout,
		async |tx| {
			queue
				.worker_return_claims(tx, claimed_jobs, RequiredExecutionState::Unstarted)
				.await
		},
	)
	.await
}

pub(super) async fn return_available_owned_started_running_jobs_to_pending_with_database_operation_timeout(
	queue: &WorkerQueue,
	pool: &WritePool,
	claimed_jobs: &[ClaimedJob],
	timeout: Duration,
) -> Result<u64, WorkerStepError> {
	run_worker_step(
		pool,
		"return available owned started running jobs to pending",
		timeout,
		async |tx| {
			queue
				.worker_return_claims(tx, claimed_jobs, RequiredExecutionState::Started)
				.await
		},
	)
	.await
}

/// Runs `run_operation` with the operation timeout. `None` reports that the job is no longer
/// running under the claim.
pub async fn run_owned_job_worker_database_operation<T, Failure, Fut, F>(
	timeout: Duration,
	run_operation: F,
) -> Result<Option<T>, Failure>
where
	F: FnOnce(Duration) -> Fut,
	Fut: Future<Output = Result<OwnedJobUpdate<T>, Failure>>,
{
	match run_operation(timeout).await? {
		OwnedJobUpdate::Applied(value) => Ok(Some(value)),
		OwnedJobUpdate::NotRunning => Ok(None),
	}
}

async fn begin_worker_database_operation<'a, Failure: WorkerStepFailure>(
	pool: &'a WritePool,
	operation: &'static str,
	operation_timeout: Duration,
	tokio_deadline: tokio::time::Instant,
) -> Result<WriteTx<'a>, Failure> {
	let mut tx = tokio::time::timeout_at(tokio_deadline, pool.begin_labeled_transaction(operation))
		.await
		.map_err(|_| Failure::timed_out(operation, operation_timeout))?
		.map_err(Failure::from_database_error)?;

	let remaining_timeout = tokio_deadline
		.checked_duration_since(tokio::time::Instant::now())
		.filter(|remaining| !remaining.is_zero())
		.ok_or_else(|| {
			tx.statement_interruption_marker().mark_interrupted();
			Failure::timed_out(operation, operation_timeout)
		})?;
	let timeout_value = pg_statement_timeout_value(remaining_timeout);
	tx.record_database_operation(
		DatabaseOperationKind::Execute,
		QUEUE_SET_LOCAL_STATEMENT_TIMEOUT_LABEL,
		Some(QUEUE_SET_LOCAL_STATEMENT_TIMEOUT_SQL),
	);
	let marker = tx.statement_interruption_marker();
	match tokio::time::timeout_at(
		tokio_deadline,
		owned_portable_query(QUEUE_SET_LOCAL_STATEMENT_TIMEOUT_SQL)
			.bind_validated(timeout_value)
			.execute(tx.inner.as_mut()),
	)
	.await
	{
		Ok(Ok(_)) => Ok(tx),
		Ok(Err(error)) => Err(Failure::from_database_error(
			error.with_operation_label(QUEUE_SET_LOCAL_STATEMENT_TIMEOUT_LABEL),
		)),
		Err(_) => {
			marker.mark_interrupted();
			drop(tx);
			Err(Failure::timed_out(operation, operation_timeout))
		}
	}
}

async fn finish_worker_database_operation<T, Failure: WorkerStepFailure>(
	tx: WriteTx<'_>,
	operation: &'static str,
	timeout: Duration,
	result: Result<T, Failure>,
) -> Result<T, Failure> {
	let result = result.map_err(|error| {
		if is_error_pg_statement_timeout(&error) {
			Failure::timed_out(operation, timeout)
		} else {
			error
		}
	});

	finish_pool_owned_write_transaction_and_preserve_rollback_error(
		operation,
		tx,
		result,
		Failure::from_database_error,
		|_operation, error, rollback_error| Failure::from_rollback_failure(error, rollback_error),
	)
	.await
}

/// The `statement_timeout` text for `timeout`: its whole milliseconds, and at least one, because
/// zero disables the timeout. The worker configuration keeps `timeout` at most
/// [`MAX_WORKER_DATABASE_OPERATION_TIMEOUT`](crate::queue::MAX_WORKER_DATABASE_OPERATION_TIMEOUT),
/// which PostgreSQL accepts.
pub fn pg_statement_timeout_value(timeout: Duration) -> String {
	let millis = timeout.as_millis().max(1);
	format!("{millis}ms")
}

fn is_error_pg_statement_timeout(error: &impl WorkerStepFailure) -> bool {
	error
		.database_error()
		.is_some_and(|error| error.retry() == RetryClassification::QueryCanceled)
}

pub(super) fn resolve_queue_worker_concurrency(concurrency: u32) -> Result<u32, WorkerConfigError> {
	if concurrency == 0 {
		return Err(WorkerConfigError::ConcurrencyIsZero);
	}
	if concurrency > MAX_WORKER_CONCURRENCY {
		return Err(WorkerConfigError::ConcurrencyTooLarge {
			actual: concurrency,
			max: MAX_WORKER_CONCURRENCY,
		});
	}
	Ok(concurrency)
}

pub(super) fn validate_positive_worker_duration(
	duration: Duration,
	zero_error: WorkerConfigError,
) -> Result<Duration, WorkerConfigError> {
	if duration.is_zero() {
		return Err(zero_error);
	}
	Ok(duration)
}

pub(super) fn validate_worker_timing(
	stale_threshold: Duration,
	execution_heartbeat_interval: Duration,
) -> Result<(), WorkerConfigError> {
	if stale_threshold.is_zero() {
		return Err(WorkerConfigError::StaleThresholdIsZero);
	}
	if execution_heartbeat_interval.is_zero() {
		return Err(WorkerConfigError::ExecutionHeartbeatIntervalIsZero);
	}
	if execution_heartbeat_interval >= stale_threshold {
		return Err(WorkerConfigError::ExecutionHeartbeatIntervalNotBelowStaleThreshold);
	}
	Ok(())
}

#[doc(hidden)]
pub fn resolve_queue_retry_policy(
	retry_policy: RetryPolicy,
) -> Result<RetryPolicy, RetryPolicyError> {
	let jitter_fraction = match &retry_policy {
		RetryPolicy::Fixed {
			jitter_fraction, ..
		}
		| RetryPolicy::Exponential {
			jitter_fraction, ..
		}
		| RetryPolicy::Custom {
			jitter_fraction, ..
		} => *jitter_fraction,
	};
	if !jitter_fraction.is_finite() || jitter_fraction < 0.0 {
		return Err(RetryPolicyError::JitterFractionInvalid);
	}

	match &retry_policy {
		RetryPolicy::Fixed { delay, .. } => {
			if delay.is_zero() {
				return Err(RetryPolicyError::FixedDelayIsZero);
			}
			// Jitter lengthens a wait by up to `jitter_fraction` of the delay.
			if *delay > MAX_RETRY_BACKOFF
				|| delay.as_secs_f64() * (1.0 + jitter_fraction) > MAX_RETRY_BACKOFF.as_secs_f64()
			{
				return Err(RetryPolicyError::FixedDelayTooLarge {
					delay: *delay,
					jitter_fraction,
					max: MAX_RETRY_BACKOFF,
				});
			}
		}
		RetryPolicy::Exponential {
			initial,
			multiplier,
			max,
			..
		} => {
			if !multiplier.is_finite() || *multiplier <= 1.0 {
				return Err(RetryPolicyError::MultiplierInvalid);
			}
			if initial.is_zero() {
				return Err(RetryPolicyError::InitialBackoffIsZero);
			}
			validate_retry_max_backoff(*max)?;
			if initial > max {
				return Err(RetryPolicyError::InitialBackoffAboveMax);
			}
		}
		RetryPolicy::Custom { max, .. } => validate_retry_max_backoff(*max)?,
	}

	Ok(retry_policy)
}

fn validate_retry_max_backoff(max: Duration) -> Result<(), RetryPolicyError> {
	if max.is_zero() {
		return Err(RetryPolicyError::MaxBackoffIsZero);
	}
	if max < MIN_RETRY_BACKOFF {
		return Err(RetryPolicyError::MaxBackoffBelowMinimum);
	}
	if max > MAX_RETRY_BACKOFF {
		return Err(RetryPolicyError::MaxBackoffTooLarge {
			actual: max,
			max: MAX_RETRY_BACKOFF,
		});
	}
	Ok(())
}

/// The wait before the retry with the given count, which is 1 for the first retry.
pub fn compute_queue_retry_backoff(
	retry_policy: &RetryPolicy,
	retry_count: u32,
	error: &TaskError,
) -> Result<Duration, random::RandomError> {
	match retry_policy {
		RetryPolicy::Fixed {
			delay,
			jitter_fraction,
		} => {
			let jittered = apply_symmetric_duration_jitter(*delay, *jitter_fraction)?;
			Ok(normalize_queue_retry_backoff(jittered, None))
		}
		RetryPolicy::Custom {
			backoff_fn,
			max,
			jitter_fraction,
		} => {
			let normalized =
				normalize_queue_retry_backoff(backoff_fn(retry_count, error), Some(*max));
			let jittered = apply_symmetric_duration_jitter(normalized, *jitter_fraction)?;
			Ok(normalize_queue_retry_backoff(jittered, Some(*max)))
		}
		RetryPolicy::Exponential {
			initial,
			multiplier,
			max,
			jitter_fraction,
		} => {
			let max_backoff_secs = max.as_secs_f64();
			let mut backoff_secs =
				initial.as_secs_f64() * multiplier.powf(f64::from(retry_count.saturating_sub(1)));
			if !backoff_secs.is_finite() || backoff_secs > max_backoff_secs {
				backoff_secs = max_backoff_secs;
			}
			if *jitter_fraction > 0.0 {
				backoff_secs = apply_symmetric_f64_jitter(backoff_secs, *jitter_fraction)?;
			}
			Ok(normalize_queue_retry_backoff(
				duration_from_clamped_secs_f64(backoff_secs, *max),
				Some(*max),
			))
		}
	}
}

#[doc(hidden)]
pub fn normalize_queue_retry_backoff(backoff: Duration, max_backoff: Option<Duration>) -> Duration {
	let backoff = backoff.max(MIN_RETRY_BACKOFF);
	if let Some(max_backoff) = max_backoff
		&& !max_backoff.is_zero()
	{
		return backoff.min(max_backoff);
	}
	backoff
}

pub(super) fn apply_symmetric_duration_jitter(
	duration: Duration,
	jitter_fraction: f64,
) -> Result<Duration, random::RandomError> {
	if jitter_fraction == 0.0 {
		return Ok(duration);
	}
	primitives_rs::random::jitter::jitter_duration_symmetric(SymmetricJitter {
		base: duration,
		ratio: jitter_fraction,
		max: Duration::MAX,
	})
}

pub(super) fn apply_symmetric_f64_jitter(
	value: f64,
	jitter_fraction: f64,
) -> Result<f64, random::RandomError> {
	if jitter_fraction == 0.0 {
		return Ok(value);
	}
	let unit = generate_random_unit_f64()?;
	Ok(value + (value * jitter_fraction * ((2.0 * unit) - 1.0)))
}

#[cfg(test)]
mod tests {
	use super::super::*;
	use super::*;

	#[test]
	fn retry_backoff_seconds_convert_to_a_duration_within_zero_and_the_maximum() {
		let max = Duration::from_secs(2);
		assert_eq!(duration_from_clamped_secs_f64(-0.5, max), Duration::ZERO);
		assert_eq!(
			duration_from_clamped_secs_f64(f64::NAN, max),
			Duration::ZERO
		);
		assert_eq!(
			duration_from_clamped_secs_f64(f64::INFINITY, max),
			Duration::from_secs(2)
		);
	}
}
