//! Explicit retry policy for exact accounting attempts; pending physical proof is not a failure count.

use super::{Duration, HolderId, ThrottlerSettings, ThrottlerTimingError};
use std::num::NonZeroUsize;

/// Explicit retry cadence and failure budgets for managed Throttler accounting.
pub(crate) struct ThrottlerDriverTimingFields {
	/// The positive cadence between pending physical proof attempts.
	pub physical_proof_retry_interval: Duration,
	/// The positive cadence between exact cleanup attempts.
	pub cleanup_retry_interval: Duration,
	/// The consecutive exact-cleanup failure budget.
	pub max_consecutive_cleanup_failures: NonZeroUsize,
	/// The positive cadence between exact task-outcome reconciliation attempts.
	pub outcome_retry_interval: Duration,
	/// The consecutive task-outcome reconciliation failure budget.
	pub max_consecutive_outcome_failures: NonZeroUsize,
}
/// Parsed accounting retry policy; pending physical proof does not consume a failure budget.
#[derive(Clone, Copy)]
pub(crate) struct ThrottlerDriverTiming {
	pub(super) physical_proof_retry_interval: Duration,
	pub(super) cleanup_retry_interval: Duration,
	pub(super) max_consecutive_cleanup_failures: NonZeroUsize,
	pub(super) outcome_retry_interval: Duration,
	pub(super) max_consecutive_outcome_failures: NonZeroUsize,
}
impl ThrottlerDriverTiming {
	/// Checks positive retry intervals and retains the explicit nonzero budgets.
	pub fn new(fields: ThrottlerDriverTimingFields) -> Result<Self, ThrottlerTimingError> {
		if fields.physical_proof_retry_interval.is_zero()
			|| fields.cleanup_retry_interval.is_zero()
			|| fields.outcome_retry_interval.is_zero()
		{
			return Err(ThrottlerTimingError);
		}
		Ok(Self {
			physical_proof_retry_interval: fields.physical_proof_retry_interval,
			cleanup_retry_interval: fields.cleanup_retry_interval,
			max_consecutive_cleanup_failures: fields.max_consecutive_cleanup_failures,
			outcome_retry_interval: fields.outcome_retry_interval,
			max_consecutive_outcome_failures: fields.max_consecutive_outcome_failures,
		})
	}
}

/// Business cadence is explicit and checked against the actual handle profile.
#[derive(Clone, Copy)]
pub(crate) struct NativeThrottlerProbeHeartbeatPolicyFields {
	pub(crate) interval: Duration,
	pub(crate) max_consecutive_failures: NonZeroUsize,
}
pub(crate) struct NativeThrottlerStartOptionsFields {
	pub(crate) rate_limit_cost: std::num::NonZeroU32,
	pub(crate) holder: Option<HolderId>,
	pub(crate) probe_heartbeat: Option<NativeThrottlerProbeHeartbeatPolicyFields>,
}
#[derive(Clone, Copy)]
pub(super) struct NativeThrottlerProbeHeartbeatPolicy {
	pub(super) interval: Duration,
	pub(super) max_consecutive_failures: NonZeroUsize,
}
/// Parsed profile association is not acquisition or extension authority.
pub(crate) struct NativeThrottlerStartOptions {
	pub(super) rate_limit_cost: std::num::NonZeroU32,
	pub(super) holder: Option<HolderId>,
	pub(super) settings: ThrottlerSettings,
	pub(super) probe_heartbeat: Option<NativeThrottlerProbeHeartbeatPolicy>,
}
