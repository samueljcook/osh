use primitives_rs::time::Deadline;
use std::{num::NonZeroU32, time::Duration};

/// Runtime-wide control work budgets. Claim duration and holder identity are owner inputs.
pub(crate) struct LeaseControlTimingFields {
	/// The positive wait budget for ordinary Lease control work.
	pub operation_timeout: Duration,
	/// The positive wait budget for one physical-ordering proof attempt.
	pub physical_proof_attempt_timeout: Duration,
	/// The positive cadence between pending physical proof attempts.
	pub physical_proof_retry_interval: Duration,
	/// The positive wait budget for one exact cleanup attempt.
	pub cleanup_attempt_timeout: Duration,
	/// The positive cadence between exact cleanup attempts.
	pub cleanup_retry_interval: Duration,
	/// The positive consecutive cleanup-failure budget.
	pub max_consecutive_cleanup_failures: u32,
}

/// Parsed explicit timing; no operation, proof, cleanup, or retry value is inferred.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LeaseControlTiming {
	pub(super) operation_timeout: Duration,
	pub(super) physical_proof_attempt_timeout: Duration,
	pub(super) physical_proof_retry_interval: Duration,
	pub(super) cleanup_attempt_timeout: Duration,
	pub(super) cleanup_retry_interval: Duration,
	pub(super) max_consecutive_cleanup_failures: NonZeroU32,
	/// The longest time that one release attempt can take: its ordering proof and then its
	/// cleanup.
	pub(super) release_attempt_timeout: Duration,
}

/// A Lease control timing input outside its positive or local-clock domain.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LeaseControlTimingError {
	/// The ordinary operation wait budget is zero.
	#[error("native lease operation timeout must be positive")]
	OperationTimeout,
	/// The physical proof attempt budget is zero.
	#[error("native lease physical proof attempt timeout must be positive")]
	PhysicalProofAttemptTimeout,
	/// The physical proof retry cadence is zero.
	#[error("native lease physical proof retry interval must be positive")]
	PhysicalProofRetryInterval,
	/// The cleanup attempt budget is zero.
	#[error("native lease cleanup attempt timeout must be positive")]
	CleanupAttemptTimeout,
	/// The cleanup retry cadence is zero.
	#[error("native lease cleanup retry interval must be positive")]
	CleanupRetryInterval,
	/// The consecutive cleanup-failure budget is zero.
	#[error("native lease cleanup failure budget must be positive")]
	MaxConsecutiveCleanupFailuresIsZero,
	/// A parsed duration exceeds the local monotonic clock domain.
	#[error("native lease timing exceeds the local monotonic clock domain")]
	LocalClockDomain,
}

impl LeaseControlTiming {
	/// Parses every explicit control budget and checks its current local-clock addition.
	pub fn new(fields: LeaseControlTimingFields) -> Result<Self, LeaseControlTimingError> {
		if fields.operation_timeout.is_zero() {
			return Err(LeaseControlTimingError::OperationTimeout);
		}
		if fields.physical_proof_attempt_timeout.is_zero() {
			return Err(LeaseControlTimingError::PhysicalProofAttemptTimeout);
		}
		if fields.physical_proof_retry_interval.is_zero() {
			return Err(LeaseControlTimingError::PhysicalProofRetryInterval);
		}
		if fields.cleanup_attempt_timeout.is_zero() {
			return Err(LeaseControlTimingError::CleanupAttemptTimeout);
		}
		if fields.cleanup_retry_interval.is_zero() {
			return Err(LeaseControlTimingError::CleanupRetryInterval);
		}
		let max_consecutive_cleanup_failures =
			NonZeroU32::new(fields.max_consecutive_cleanup_failures)
				.ok_or(LeaseControlTimingError::MaxConsecutiveCleanupFailuresIsZero)?;
		let release_attempt_timeout = fields
			.physical_proof_attempt_timeout
			.checked_add(fields.cleanup_attempt_timeout)
			.ok_or(LeaseControlTimingError::LocalClockDomain)?;
		if [
			release_attempt_timeout,
			fields.operation_timeout,
			fields.physical_proof_attempt_timeout,
			fields.physical_proof_retry_interval,
			fields.cleanup_attempt_timeout,
			fields.cleanup_retry_interval,
		]
		.into_iter()
		.any(|duration| Deadline::new_after(duration).is_err())
		{
			return Err(LeaseControlTimingError::LocalClockDomain);
		}
		Ok(Self {
			operation_timeout: fields.operation_timeout,
			physical_proof_attempt_timeout: fields.physical_proof_attempt_timeout,
			physical_proof_retry_interval: fields.physical_proof_retry_interval,
			cleanup_attempt_timeout: fields.cleanup_attempt_timeout,
			cleanup_retry_interval: fields.cleanup_retry_interval,
			max_consecutive_cleanup_failures,
			release_attempt_timeout,
		})
	}
}
