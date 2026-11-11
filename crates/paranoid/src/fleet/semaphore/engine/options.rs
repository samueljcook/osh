//! Explicit managed acquisition choices and runtime-wide accounting retry policy.

use std::{num::NonZeroUsize, time::Duration};

use super::{NativeSemaphoreError, SemaphoreTimingError};
use crate::fleet::HolderId;

/// Explicit retry cadence and failure budgets for managed Semaphore accounting.
pub(crate) struct SemaphoreDriverTimingFields {
	/// The positive cadence between pending physical proof attempts.
	pub physical_proof_retry_interval: Duration,
	/// The positive cadence between exact cleanup attempts.
	pub cleanup_retry_interval: Duration,
	/// The consecutive exact-cleanup failure budget.
	pub max_consecutive_cleanup_failures: NonZeroUsize,
	/// The positive cadence between exact claimed-release reconciliation attempts.
	pub release_retry_interval: Duration,
	/// The consecutive claimed-release reconciliation failure budget.
	pub max_consecutive_release_failures: NonZeroUsize,
}
/// Parsed accounting retry policy; pending physical proof does not consume a failure budget.
#[derive(Clone, Copy)]
pub(crate) struct SemaphoreDriverTiming {
	pub(super) physical_proof_retry_interval: Duration,
	pub(super) cleanup_retry_interval: Duration,
	pub(super) max_consecutive_cleanup_failures: NonZeroUsize,
	pub(super) release_retry_interval: Duration,
	pub(super) max_consecutive_release_failures: NonZeroUsize,
}
impl SemaphoreDriverTiming {
	/// Checks positive retry intervals and retains the explicit nonzero budgets.
	pub fn new(fields: SemaphoreDriverTimingFields) -> Result<Self, SemaphoreTimingError> {
		if fields.physical_proof_retry_interval.is_zero()
			|| fields.cleanup_retry_interval.is_zero()
			|| fields.release_retry_interval.is_zero()
		{
			return Err(SemaphoreTimingError);
		}
		Ok(Self {
			physical_proof_retry_interval: fields.physical_proof_retry_interval,
			cleanup_retry_interval: fields.cleanup_retry_interval,
			max_consecutive_cleanup_failures: fields.max_consecutive_cleanup_failures,
			release_retry_interval: fields.release_retry_interval,
			max_consecutive_release_failures: fields.max_consecutive_release_failures,
		})
	}
}

pub(crate) struct NativeSemaphoreWaitingPolicyFields {
	pub(crate) acquire_retry_interval: Duration,
}
/// A waiting acquisition retries until its slot is acquired or the caller cancels it.
#[derive(Clone, Copy)]
pub(crate) struct NativeSemaphoreWaitingPolicy {
	pub(super) acquire_retry_interval: Duration,
}
impl NativeSemaphoreWaitingPolicy {
	pub(crate) fn new(
		fields: NativeSemaphoreWaitingPolicyFields,
	) -> Result<Self, NativeSemaphoreError> {
		if fields.acquire_retry_interval.is_zero()
			|| primitives_rs::time::Deadline::new_after(fields.acquire_retry_interval).is_err()
		{
			return Err(NativeSemaphoreError::InvalidTiming);
		}
		Ok(Self {
			acquire_retry_interval: fields.acquire_retry_interval,
		})
	}
}
#[derive(Clone, Copy)]
pub(crate) enum NativeSemaphoreAcquisitionChoice {
	TryOnce,
	Waiting(NativeSemaphoreWaitingPolicy),
}
pub(crate) struct NativeSemaphoreStartOptionsFields {
	pub(crate) holder: HolderId,
	pub(crate) acquisition: NativeSemaphoreAcquisitionChoice,
}
/// Parsed business choices grant no claim, installed role or caller permission.
pub(crate) struct NativeSemaphoreStartOptions {
	pub(super) holder: HolderId,
	pub(super) acquisition: NativeSemaphoreAcquisitionChoice,
}
impl NativeSemaphoreStartOptions {
	pub(crate) fn new(fields: NativeSemaphoreStartOptionsFields) -> Self {
		Self {
			holder: fields.holder,
			acquisition: fields.acquisition,
		}
	}
}
