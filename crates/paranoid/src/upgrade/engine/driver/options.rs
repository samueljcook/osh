//! Explicit scheduling and settlement limits for one owned upgrade request.

use crate::upgrade::wait::require_valid_wait;
use std::{num::NonZeroU32, time::Duration};

/// Explicit local scheduling and settlement choices for one upgrade owner.
pub(crate) struct NativeUpgradeDriverOptionsFields {
	/// Maximum wait for a recorded transaction ending and its local continuation.
	pub ending_wait_timeout: Duration,
	/// Delay before retrying eligible transient failures or deferred work.
	pub retry_interval: Duration,
	/// Wait budget for a physical recovery observation.
	pub recovery_wait_timeout: Duration,
	/// Maximum consecutive eligible transient failures before local progress stops.
	pub max_consecutive_transient_failures: NonZeroU32,
	/// Maximum fixed steps before the owner yields a scheduling pass.
	pub max_steps_per_pass: NonZeroU32,
}

/// Parsed local upgrade scheduling choices, without transaction or upgrade authority.
pub(crate) struct NativeUpgradeDriverOptions {
	fields: NativeUpgradeDriverOptionsFields,
}

/// A rejected local scheduling wait.
#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeUpgradeDriverOptionsError {
	/// The ending wait is zero or exceeds the local clock's range.
	#[error("upgrade ending wait must be positive and fit the local clock")]
	EndingWait,
	/// The retry interval is zero or exceeds the local clock's range.
	#[error("upgrade retry interval must be positive and fit the local clock")]
	RetryInterval,
	/// The recovery wait is zero or exceeds the local clock's range.
	#[error("upgrade recovery wait must be positive and fit the local clock")]
	RecoveryWait,
}

impl NativeUpgradeDriverOptions {
	/// Parses explicit scheduling choices and checks each local wait.
	pub fn new(
		fields: NativeUpgradeDriverOptionsFields,
	) -> Result<Self, NativeUpgradeDriverOptionsError> {
		require_valid_wait(
			fields.ending_wait_timeout,
			NativeUpgradeDriverOptionsError::EndingWait,
		)?;
		require_valid_wait(
			fields.retry_interval,
			NativeUpgradeDriverOptionsError::RetryInterval,
		)?;
		require_valid_wait(
			fields.recovery_wait_timeout,
			NativeUpgradeDriverOptionsError::RecoveryWait,
		)?;
		Ok(Self { fields })
	}
	pub(super) fn ending_wait_timeout(&self) -> Duration {
		self.fields.ending_wait_timeout
	}
	pub(super) fn retry_interval(&self) -> Duration {
		self.fields.retry_interval
	}
	pub(super) fn recovery_wait_timeout(&self) -> Duration {
		self.fields.recovery_wait_timeout
	}
	pub(super) fn max_consecutive_transient_failures(&self) -> NonZeroU32 {
		self.fields.max_consecutive_transient_failures
	}
	pub(super) fn max_steps_per_pass(&self) -> NonZeroU32 {
		self.fields.max_steps_per_pass
	}
}
