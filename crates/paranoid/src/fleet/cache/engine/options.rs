//! Parsed local Cache timing and value lifetime, without shared-state authority.

use crate::fleet::lease::owned_driver::options::{
	NativeLeaseOwnerOptions, NativeLeaseWaitingPolicy,
};
use crate::fleet::throttler::is_positive_duration_in_micros;
use crate::kv::Ttl;
use primitives_rs::time::Deadline;
use std::time::Duration;

/// The validated value lifetime and waits of a Cache, without its lease options.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NativeCacheTiming {
	value_ttl: Ttl,
	lease_wait_timeout: Duration,
	compute_timeout: Option<Duration>,
}
#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeCacheOptionsError {
	#[error("Cache lease wait must be positive in its microsecond domain")]
	LeaseWait,
	#[error("Cache computation timeout must be positive in its microsecond domain")]
	ComputeTimeout,
	#[error("Cache timing exceeds the local monotonic clock domain")]
	LocalClockDomain,
}
impl NativeCacheTiming {
	pub(crate) fn parse(
		value_ttl: Ttl,
		lease_wait_timeout: Duration,
		compute_timeout: Option<Duration>,
	) -> Result<Self, NativeCacheOptionsError> {
		if !is_positive_duration_in_micros(lease_wait_timeout) {
			return Err(NativeCacheOptionsError::LeaseWait);
		}
		if compute_timeout.is_some_and(|value| !is_positive_duration_in_micros(value)) {
			return Err(NativeCacheOptionsError::ComputeTimeout);
		}
		for duration in [Some(lease_wait_timeout), compute_timeout]
			.into_iter()
			.flatten()
		{
			if Deadline::new_after(duration).is_err() {
				return Err(NativeCacheOptionsError::LocalClockDomain);
			}
		}
		Ok(Self {
			value_ttl,
			lease_wait_timeout,
			compute_timeout,
		})
	}
}

pub(crate) struct NativeCacheOptions {
	timing: NativeCacheTiming,
	waiting: NativeLeaseWaitingPolicy,
	lease: NativeLeaseOwnerOptions,
}
impl NativeCacheOptions {
	pub(crate) fn new(
		timing: NativeCacheTiming,
		waiting: NativeLeaseWaitingPolicy,
		lease: NativeLeaseOwnerOptions,
	) -> Self {
		Self {
			timing,
			waiting,
			lease,
		}
	}
	pub(crate) fn value_ttl(&self) -> Ttl {
		self.timing.value_ttl
	}
	pub(crate) fn lease_wait_timeout(&self) -> Duration {
		self.timing.lease_wait_timeout
	}
	pub(crate) fn waiting(&self) -> NativeLeaseWaitingPolicy {
		self.waiting
	}
	pub(crate) fn compute_timeout(&self) -> Option<Duration> {
		self.timing.compute_timeout
	}
	pub(crate) fn lease(&self) -> &NativeLeaseOwnerOptions {
		&self.lease
	}
}
