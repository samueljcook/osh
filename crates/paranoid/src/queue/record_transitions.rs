//! Native worker transitions under already acquired job mutation authority.
//!
//! Selection, pause checks, transaction admission, and persistence belong to the operation
//! that invokes these transitions. No transition can authorize work from an owner label.

use super::state::{
	JobExecution, JobLifecycle, NativeJob, QueueStateError, RunningJob, RunningJobClaim,
};
use super::{ClaimedJob, WorkerClaimToken, WorkerOwnerId};
use crate::records::StateTimestampMicros;
use std::time::Duration;

/// Borrowed credentials identify one existing claim without carrying its payload.
pub(super) struct JobClaimCredentials<'a> {
	pub(super) job_id: super::JobId,
	pub(super) owner: &'a WorkerOwnerId,
	pub(super) token: &'a WorkerClaimToken,
}
impl<'a> JobClaimCredentials<'a> {
	pub(super) fn from_claimed_job(claim: &'a ClaimedJob) -> Self {
		Self {
			job_id: claim.job_id(),
			owner: claim.worker_owner_id(),
			token: claim.claim_token(),
		}
	}
}

/// Fresh private material generated for a native claim.
pub(super) struct JobAcquisition {
	pub(super) owner: WorkerOwnerId,
	pub(super) token: WorkerClaimToken,
}

/// Execution state required by a worker's return-to-pending operation.
#[derive(Clone, Copy)]
pub(in crate::queue) enum RequiredExecutionState {
	Unstarted,
	Started,
}

/// Whether a terminal failure consumes one more attempt.
#[derive(Clone, Copy)]
pub(in crate::queue) enum RetryConsumption {
	KeepCount,
	ConsumeAttempt,
}

/// A code-defined mutation of a running job owned by one exact private claim.
#[derive(Clone)]
pub(in crate::queue) enum OwnedJobMutation {
	StartExecution,
	Heartbeat,
	Complete,
	ReturnPending(RequiredExecutionState),
	ScheduleRetry {
		new_retry_count: u32,
		retry_after: Duration,
		error_msg: String,
	},
	Fail {
		error_msg: String,
		retry_consumption: RetryConsumption,
	},
}

/// A native transition rejected its state or cannot represent the requested arithmetic.
#[derive(Debug, thiserror::Error)]
pub(crate) enum JobTransitionError {
	#[error("native queue transition produced invalid state")]
	State(#[from] QueueStateError),
	#[error("queue retry backoff exceeds the supported microsecond domain")]
	RetryBackoffTooLarge,
	#[error("queue retry schedule exceeds the signed timestamp domain")]
	ScheduleOverflow,
	#[error("queue retry count exhausted the unsigned 32-bit domain")]
	RetryCountOverflow,
}

/// What happens to a job that fails for the last time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueDeadLetterPolicy {
	/// The job stays in the Queue as failed.
	KeepFailed,
	/// The job moves to dead-letter storage.
	MoveToDeadLetters,
}

/// The inputs of an installation's Queue recovery settings.
pub struct QueueRecoveryPolicyFields {
	/// The positive age of a running job's newest heartbeat at which the job counts as
	/// abandoned. Every worker gives up a job of its own at this age, and recovery returns the
	/// job to the Queue.
	pub stale_threshold: Duration,
	/// What happens to a job that fails for the last time, including a job with an abandoned last
	/// attempt.
	pub dead_letters: QueueDeadLetterPolicy,
}

/// An installation's Queue recovery settings, which every worker and every recovery of the
/// installation uses. Opening records them when it creates the installation.
#[derive(Clone)]
pub struct QueueRecoveryPolicy {
	stale_threshold_micros: i64,
	dead_letters: QueueDeadLetterPolicy,
}

/// A stale threshold that cannot be recorded.
#[derive(Debug, thiserror::Error)]
pub enum QueueRecoveryPolicyError {
	/// The threshold is zero.
	#[error("the Queue stale threshold must be positive")]
	ThresholdIsZero,
	/// The threshold in microseconds exceeds the signed 64-bit range.
	#[error("the Queue stale threshold exceeds the signed 64-bit microsecond range")]
	ThresholdTooLarge,
}

impl QueueRecoveryPolicy {
	/// Validates the settings without database work. The stale threshold is recorded in whole
	/// microseconds, and at least one microsecond.
	pub fn new(fields: QueueRecoveryPolicyFields) -> Result<Self, QueueRecoveryPolicyError> {
		if fields.stale_threshold.is_zero() {
			return Err(QueueRecoveryPolicyError::ThresholdIsZero);
		}
		let stale_threshold_micros = i64::try_from(fields.stale_threshold.as_micros().max(1))
			.map_err(|_| QueueRecoveryPolicyError::ThresholdTooLarge)?;
		Self::from_positive_micros(stale_threshold_micros, fields.dead_letters)
	}
	pub(super) fn from_positive_micros(
		stale_threshold_micros: i64,
		dead_letters: QueueDeadLetterPolicy,
	) -> Result<Self, QueueRecoveryPolicyError> {
		if stale_threshold_micros <= 0 {
			return Err(QueueRecoveryPolicyError::ThresholdIsZero);
		}
		Ok(Self {
			stale_threshold_micros,
			dead_letters,
		})
	}
	pub(in crate::queue) fn stale_threshold_micros(&self) -> i64 {
		self.stale_threshold_micros
	}
	pub(in crate::queue) fn dead_letters(&self) -> QueueDeadLetterPolicy {
		self.dead_letters
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StaleClaimRecoveryKind {
	NeverStartedPending,
	/// A started job with a claim that v10 retirement ended returns to pending without spending a
	/// retry.
	RetiredClaimPending,
	StartedRetryPending,
	StartedFailed,
	ExecutionExpiredDeadLetter,
}

/// Prepared working state has no usable acquisition authority and proves no committed recovery.
pub(super) struct StaleClaimRecoveryChange {
	pub(super) state: NativeJob,
	pub(super) kind: StaleClaimRecoveryKind,
}

/// Recovers a running job when its claim's heartbeat is older than the installed stale threshold,
/// whether a native worker or a retired published deployment holds the claim. Recovery replaces
/// the claim, so a late operation of the previous holder changes nothing. Dead-letter changes are
/// preparation for a same-transaction group move, not point writes.
pub(super) fn recover_stale_claim(
	state: NativeJob,
	policy: &QueueRecoveryPolicy,
	observed_at: StateTimestampMicros,
) -> Result<Option<StaleClaimRecoveryChange>, JobTransitionError> {
	let Some(running) = state.job().lifecycle.running() else {
		return Ok(None);
	};
	let stale_boundary =
		i128::from(observed_at.as_i64()) - i128::from(policy.stale_threshold_micros);
	if i128::from(running.heartbeat_at) >= stale_boundary {
		return Ok(None);
	}
	let mut parts = state.into_parts();
	let mut job = parts.job;
	let JobLifecycle::Running(running) = job.lifecycle else {
		unreachable!("recovery selected a running job")
	};
	let now = observed_at.as_i64();
	let (lifecycle, kind) = match (running.execution, running.claim) {
		(JobExecution::Unstarted, _) => (
			JobLifecycle::Pending,
			StaleClaimRecoveryKind::NeverStartedPending,
		),
		(JobExecution::Started { .. }, RunningJobClaim::RetiredPublished) => {
			// Retirement ends a source claim without establishing a failed execution.
			(
				JobLifecycle::Pending,
				StaleClaimRecoveryKind::RetiredClaimPending,
			)
		}
		(JobExecution::Started { .. }, RunningJobClaim::NativePrivate(_)) => {
			let exhausted = job.retry_count >= job.max_retries;
			job.retry_count = job
				.retry_count
				.checked_add(1)
				.ok_or(JobTransitionError::RetryCountOverflow)?;
			job.last_error_msg = Some(match job.last_error_msg.take() {
				Some(message) => format!("{message} | {}", super::STALE_EXECUTION_ERROR_MESSAGE),
				None => super::STALE_EXECUTION_ERROR_MESSAGE.to_owned(),
			});
			if exhausted {
				let kind = match policy.dead_letters {
					QueueDeadLetterPolicy::KeepFailed => StaleClaimRecoveryKind::StartedFailed,
					QueueDeadLetterPolicy::MoveToDeadLetters => {
						StaleClaimRecoveryKind::ExecutionExpiredDeadLetter
					}
				};
				(JobLifecycle::Failed { finished_at: now }, kind)
			} else {
				(
					JobLifecycle::Pending,
					StaleClaimRecoveryKind::StartedRetryPending,
				)
			}
		}
	};
	job.lifecycle = lifecycle;
	job.updated_at_unix_micros = now;
	parts.job = job;
	Ok(Some(StaleClaimRecoveryChange {
		state: NativeJob::new(parts)?,
		kind,
	}))
}

/// Claims a selected due pending job; native authority is never inferred from source labels.
/// The caller must check handler availability and the installation's pause state in the
/// same admitted operation. Authority must be delivered only after a known successful commit.
pub(super) fn claim_pending(
	state: NativeJob,
	acquisition: JobAcquisition,
	observed_at: StateTimestampMicros,
) -> Result<Option<NativeJob>, JobTransitionError> {
	if !matches!(state.job().lifecycle, JobLifecycle::Pending)
		|| state.job().run_at_or_after_unix_micros > observed_at.as_i64()
	{
		return Ok(None);
	}
	let mut parts = state.into_parts();
	let now = observed_at.as_i64();
	parts.job.lifecycle = JobLifecycle::Running(RunningJob {
		owner: acquisition.owner,
		claimed_at: now,
		execution: JobExecution::Unstarted,
		heartbeat_at: now,
		claim: RunningJobClaim::NativePrivate(acquisition.token),
	});
	parts.job.updated_at_unix_micros = now;
	Ok(Some(NativeJob::new(parts)?))
}

/// Applies a worker transition only to the exact running job, owner, and secret token.
/// A mismatch leaves the persisted value unchanged; the caller must finish its guard with
/// `keep`. Retry schedules and terminal failures retain the persisted per-job policy.
pub(super) fn apply_owned_mutation(
	state: NativeJob,
	authority: &JobClaimCredentials<'_>,
	mutation: OwnedJobMutation,
	observed_at: StateTimestampMicros,
) -> Result<Option<NativeJob>, JobTransitionError> {
	if !matches_authority(&state, authority) {
		return Ok(None);
	}
	let mut parts = state.into_parts();
	let mut job = parts.job;
	let JobLifecycle::Running(mut running) = job.lifecycle else {
		unreachable!("exact claim authority requires a running job")
	};
	let now = observed_at.as_i64();
	job.lifecycle = match mutation {
		OwnedJobMutation::StartExecution => {
			running.execution = JobExecution::Started { at: now };
			running.heartbeat_at = now;
			JobLifecycle::Running(running)
		}
		OwnedJobMutation::Heartbeat => {
			running.heartbeat_at = running.heartbeat_at.max(now);
			JobLifecycle::Running(running)
		}
		OwnedJobMutation::Complete => JobLifecycle::Completed { finished_at: now },
		OwnedJobMutation::ReturnPending(required) => {
			if !matches!(
				(required, running.execution),
				(RequiredExecutionState::Unstarted, JobExecution::Unstarted)
					| (
						RequiredExecutionState::Started,
						JobExecution::Started { .. }
					)
			) {
				return Ok(None);
			}
			JobLifecycle::Pending
		}
		OwnedJobMutation::ScheduleRetry {
			new_retry_count,
			retry_after,
			error_msg,
		} => {
			if new_retry_count > super::MAX_RETRIES {
				return Err(JobTransitionError::RetryCountOverflow);
			}
			let delay = super::retry_backoff_to_micros(retry_after)
				.map_err(|_| JobTransitionError::RetryBackoffTooLarge)?;
			job.run_at_or_after_unix_micros = now
				.checked_add(delay)
				.ok_or(JobTransitionError::ScheduleOverflow)?;
			job.retry_count = new_retry_count;
			job.last_error_msg = Some(error_msg);
			JobLifecycle::Pending
		}
		OwnedJobMutation::Fail {
			error_msg,
			retry_consumption,
		} => {
			if matches!(retry_consumption, RetryConsumption::ConsumeAttempt) {
				job.retry_count = job
					.retry_count
					.checked_add(1)
					.ok_or(JobTransitionError::RetryCountOverflow)?;
			}
			job.last_error_msg = Some(error_msg);
			JobLifecycle::Failed { finished_at: now }
		}
	};
	job.updated_at_unix_micros = now;
	parts.job = job;
	Ok(Some(NativeJob::new(parts)?))
}

fn matches_authority(state: &NativeJob, authority: &JobClaimCredentials<'_>) -> bool {
	let Some(running) = state.job().lifecycle.running() else {
		return false;
	};
	state.job().id == authority.job_id
		&& &running.owner == authority.owner
		&& matches!(&running.claim, RunningJobClaim::NativePrivate(token) if token == authority.token)
}
