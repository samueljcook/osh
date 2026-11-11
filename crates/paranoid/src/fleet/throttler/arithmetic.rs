use super::model::{RateLimit, ResolvedThrottlerRateLimit};
use crate::fleet::{
	FLEET_THROTTLER_ACTIVITY_TTL_MULTIPLIER, MAX_SLOT_COUNT, ThrottlerCircuitSettingsError,
	ThrottlerConcurrencySettingsError, ThrottlerRateSettingsError,
};
use crate::kv::{MIN_TTL, Ttl as KvTtl};
use std::time::Duration;

pub(crate) mod semantic_settings;

impl ResolvedThrottlerRateLimit {
	/// The time that an empty bucket takes to gain `capacity` tokens, rounded up to a
	/// nanosecond and limited to the longest duration of a time to live.
	pub(super) fn full_refill_duration(self) -> Duration {
		semantic_settings::full_refill_duration(self)
	}
}

pub(super) fn scale_duration_for_throttler_activity_ttl(duration: Duration) -> Duration {
	if duration.is_zero() {
		return Duration::ZERO;
	}
	let max_duration = max_kv_ttl_duration();
	duration
		.checked_mul(FLEET_THROTTLER_ACTIVITY_TTL_MULTIPLIER)
		.unwrap_or(max_duration)
		.min(max_duration)
}

#[doc(hidden)]
pub fn max_kv_ttl_duration() -> Duration {
	Duration::from_micros(i64::MAX as u64)
}

/// Adds a duration to a timestamp in Unix microseconds. Returns `None` when the sum does not
/// fit in `i64`.
pub(super) fn add_duration_to_timestamp(timestamp: i64, duration: Duration) -> Option<i64> {
	timestamp.checked_add(duration_to_rounded_micros(duration)?)
}

/// Whether a duration is positive and fits in `i64` microseconds.
pub(in crate::fleet) fn is_positive_duration_in_micros(duration: Duration) -> bool {
	!duration.is_zero() && duration_to_rounded_micros(duration).is_some()
}

#[doc(hidden)]
pub fn duration_to_rounded_micros(duration: Duration) -> Option<i64> {
	primitives_rs::time::duration_to_ceil_micros_i64(duration).ok()
}

pub(super) fn duration_to_micros_lossy(duration: Duration) -> i64 {
	duration_to_rounded_micros(duration).unwrap_or(i64::MAX)
}

#[doc(hidden)]
pub struct RateLimitRetryInput {
	#[doc(hidden)]
	pub current_tokens: f64,
	#[doc(hidden)]
	pub refill_rate_per_sec: f64,
	#[doc(hidden)]
	pub required_tokens: std::num::NonZeroU32,
}

#[doc(hidden)]
pub fn compute_rate_limit_retry_after_duration(input: RateLimitRetryInput) -> Duration {
	if input.refill_rate_per_sec <= 0.0 {
		return max_kv_ttl_duration();
	}

	let retry_after_secs =
		(f64::from(input.required_tokens.get()) - input.current_tokens) / input.refill_rate_per_sec;
	if retry_after_secs <= 0.0 {
		return Duration::from_millis(1);
	}
	if !retry_after_secs.is_finite() {
		return max_kv_ttl_duration();
	}

	let retry_after = primitives_rs::time::duration_from_clamped_secs_f64(
		retry_after_secs,
		max_kv_ttl_duration(),
	);
	retry_after.max(Duration::from_millis(1))
}
