use super::super::{ClaimDuration, HolderPrefix};
use primitives_rs::time::Deadline;
use std::{num::NonZeroU32, time::Duration};

pub(crate) enum NativeLeaseRenewalPolicyFields {
	Automatic {
		heartbeat_interval: Duration,
		max_consecutive_renewal_failures: u32,
	},
}

#[derive(Clone)]
pub(crate) struct NativeLeaseOwnerOptions {
	pub(super) holder_prefix: HolderPrefix,
	pub(super) claim_duration: ClaimDuration,
	pub(super) renewal_policy: NativeLeaseRenewalPolicy,
}

#[derive(Clone, Copy)]
pub(super) enum NativeLeaseRenewalPolicy {
	Automatic(NativeLeaseAutomaticRenewal),
}

#[derive(Clone, Copy)]
pub(super) struct NativeLeaseAutomaticRenewal {
	pub(super) heartbeat_interval: Duration,
	pub(super) max_consecutive_renewal_failures: NonZeroU32,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeLeaseOwnerOptionsError {
	#[error(
		"automatic lease renewal interval must be positive and shorter than its claim duration"
	)]
	RenewalInterval,
	#[error("automatic lease maximum of consecutive failed renewals must be positive")]
	MaxConsecutiveRenewalFailuresIsZero,
	#[error("lease owner timing exceeds the local monotonic clock domain")]
	LocalClockDomain,
}

/// Validated claim and renewal timing, without a holder prefix.
#[derive(Clone)]
pub(crate) struct NativeLeaseOwnerTiming {
	claim_duration: ClaimDuration,
	renewal_policy: NativeLeaseRenewalPolicy,
}

impl NativeLeaseOwnerTiming {
	pub(crate) fn parse(
		claim_duration: ClaimDuration,
		renewal_policy: NativeLeaseRenewalPolicyFields,
	) -> Result<Self, NativeLeaseOwnerOptionsError> {
		let renewal_policy = match renewal_policy {
			NativeLeaseRenewalPolicyFields::Automatic {
				heartbeat_interval,
				max_consecutive_renewal_failures,
			} => {
				if heartbeat_interval.is_zero()
					|| heartbeat_interval >= claim_duration.as_duration()
				{
					return Err(NativeLeaseOwnerOptionsError::RenewalInterval);
				}
				let max_consecutive_renewal_failures =
					NonZeroU32::new(max_consecutive_renewal_failures)
						.ok_or(NativeLeaseOwnerOptionsError::MaxConsecutiveRenewalFailuresIsZero)?;
				if Deadline::new_after(heartbeat_interval).is_err() {
					return Err(NativeLeaseOwnerOptionsError::LocalClockDomain);
				}
				NativeLeaseRenewalPolicy::Automatic(NativeLeaseAutomaticRenewal {
					heartbeat_interval,
					max_consecutive_renewal_failures,
				})
			}
		};
		if Deadline::new_after(claim_duration.as_duration()).is_err() {
			return Err(NativeLeaseOwnerOptionsError::LocalClockDomain);
		}
		Ok(Self {
			claim_duration,
			renewal_policy,
		})
	}

	/// Combines the timing with the holder prefix of the process that will hold the lease.
	pub(crate) fn with_holder_prefix(
		&self,
		holder_prefix: HolderPrefix,
	) -> NativeLeaseOwnerOptions {
		NativeLeaseOwnerOptions {
			holder_prefix,
			claim_duration: self.claim_duration,
			renewal_policy: self.renewal_policy,
		}
	}
}

/// The retry delays of an acquisition that waits until it acquires the lease.
pub(crate) struct NativeLeaseWaitingPolicyFields {
	pub(crate) acquire_retry_interval: Duration,
	pub(crate) max_acquire_retry_interval: Duration,
}
#[derive(Clone, Copy)]
pub(crate) struct NativeLeaseWaitingPolicy {
	pub(crate) acquire_retry_interval: Duration,
	pub(crate) max_acquire_retry_interval: Duration,
}
#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeLeaseWaitingPolicyError {
	#[error("lease acquisition retry intervals must be positive and ordered")]
	RetryInterval,
	#[error("lease acquisition timing exceeds the local monotonic clock domain")]
	LocalClockDomain,
}
impl NativeLeaseWaitingPolicy {
	pub(crate) fn new(
		fields: NativeLeaseWaitingPolicyFields,
	) -> Result<Self, NativeLeaseWaitingPolicyError> {
		if fields.acquire_retry_interval.is_zero()
			|| fields.max_acquire_retry_interval < fields.acquire_retry_interval
		{
			return Err(NativeLeaseWaitingPolicyError::RetryInterval);
		}
		if Deadline::new_after(fields.max_acquire_retry_interval).is_err() {
			return Err(NativeLeaseWaitingPolicyError::LocalClockDomain);
		}
		Ok(Self {
			acquire_retry_interval: fields.acquire_retry_interval,
			max_acquire_retry_interval: fields.max_acquire_retry_interval,
		})
	}
}
#[derive(Clone, Copy)]
pub(crate) enum NativeLeaseAcquisitionChoice {
	TryOnce,
	Waiting(NativeLeaseWaitingPolicy),
}
