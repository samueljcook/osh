//! Shared actual Throttler semantic domains and activity lifetime arithmetic.

use super::{
	Duration, KvTtl, MAX_SLOT_COUNT, MIN_TTL, RateLimit, ResolvedThrottlerRateLimit,
	ThrottlerCircuitSettingsError, ThrottlerConcurrencySettingsError, ThrottlerRateSettingsError,
	is_positive_duration_in_micros, max_kv_ttl_duration, scale_duration_for_throttler_activity_ttl,
};

/// The concurrency limit of a Throttler.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThrottlerConcurrencySettingsFields {
	/// The number of concurrency slots, from 1 to
	/// [`MAX_SLOT_COUNT`](crate::fleet::MAX_SLOT_COUNT).
	pub capacity: u16,
	/// The maximum time that one run holds its slot. It must be positive and at most
	/// [`MAX_SETTINGS_DURATION`](crate::fleet::MAX_SETTINGS_DURATION). A hold shorter than an
	/// acquisition's database round trip makes every run fail with
	/// [`PgThrottlerErrorKind::Internal`] because the slot's window ends before the run receives
	/// its permit.
	///
	/// [`PgThrottlerErrorKind::Internal`]: crate::fleet::PgThrottlerErrorKind::Internal
	pub max_hold_duration: Duration,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ThrottlerConcurrencySettings {
	capacity: u16,
	max_hold_duration: Duration,
}
impl ThrottlerConcurrencySettings {
	pub(crate) fn new(
		fields: ThrottlerConcurrencySettingsFields,
	) -> Result<Self, ThrottlerConcurrencySettingsError> {
		if fields.capacity == 0 || fields.capacity > MAX_SLOT_COUNT {
			return Err(ThrottlerConcurrencySettingsError::CapacityInvalid {
				value: fields.capacity,
				max: MAX_SLOT_COUNT,
			});
		}
		if !is_positive_duration_in_micros(fields.max_hold_duration) {
			return Err(ThrottlerConcurrencySettingsError::MaxHoldDurationInvalid);
		}
		Ok(Self {
			capacity: fields.capacity,
			max_hold_duration: fields.max_hold_duration,
		})
	}
	pub(crate) fn capacity(self) -> u16 {
		self.capacity
	}
	pub(crate) fn max_hold_duration(self) -> Duration {
		self.max_hold_duration
	}
}

/// The circuit of a Throttler.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThrottlerCircuitSettingsFields {
	/// The positive number of consecutive failed runs that opens the circuit.
	pub failure_threshold: u32,
	/// The time that the circuit stays open before it lets one probe run. It must be positive
	/// and at most [`MAX_SETTINGS_DURATION`](crate::fleet::MAX_SETTINGS_DURATION).
	pub recovery_timeout: Duration,
	/// The maximum time that a probe run holds the probe reservation without renewing
	/// it. It must be positive and at most
	/// [`MAX_SETTINGS_DURATION`](crate::fleet::MAX_SETTINGS_DURATION).
	pub probe_window: Duration,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ThrottlerCircuitSettings {
	failure_threshold: u32,
	recovery_timeout: Duration,
	probe_window: Duration,
}
impl ThrottlerCircuitSettings {
	pub(crate) fn new(
		fields: ThrottlerCircuitSettingsFields,
	) -> Result<Self, ThrottlerCircuitSettingsError> {
		if fields.failure_threshold == 0 {
			return Err(ThrottlerCircuitSettingsError::FailureThresholdIsZero);
		}
		if !is_positive_duration_in_micros(fields.recovery_timeout) {
			return Err(ThrottlerCircuitSettingsError::RecoveryTimeoutInvalid);
		}
		if !is_positive_duration_in_micros(fields.probe_window) {
			return Err(ThrottlerCircuitSettingsError::ProbeWindowInvalid);
		}
		Ok(Self {
			failure_threshold: fields.failure_threshold,
			recovery_timeout: fields.recovery_timeout,
			probe_window: fields.probe_window,
		})
	}
	pub(crate) fn failure_threshold(self) -> u32 {
		self.failure_threshold
	}
	pub(crate) fn recovery_timeout(self) -> Duration {
		self.recovery_timeout
	}
	pub(crate) fn probe_window(self) -> Duration {
		self.probe_window
	}
}

pub(crate) fn parse_rate(
	rate: RateLimit,
) -> Result<ResolvedThrottlerRateLimit, ThrottlerRateSettingsError> {
	if rate.capacity == 0 {
		return Err(ThrottlerRateSettingsError::CapacityIsZero);
	}
	if rate.tokens_per_interval == 0 {
		return Err(ThrottlerRateSettingsError::TokensPerIntervalIsZero);
	}
	if !is_positive_duration_in_micros(rate.interval) {
		return Err(ThrottlerRateSettingsError::IntervalInvalid);
	}
	let resolved = ResolvedThrottlerRateLimit {
		capacity: rate.capacity,
		tokens_per_interval: rate.tokens_per_interval,
		interval: rate.interval,
	};
	Ok(resolved)
}

pub(crate) fn full_refill_duration(rate: ResolvedThrottlerRateLimit) -> Duration {
	let nanos = rate.interval.as_nanos() * u128::from(rate.capacity);
	let refill_nanos = nanos.div_ceil(u128::from(rate.tokens_per_interval));
	primitives_rs::time::duration_from_nanos_u128(refill_nanos)
		.unwrap_or_else(|_| max_kv_ttl_duration())
		.min(max_kv_ttl_duration())
}

pub(crate) fn activity_ttl(
	rate: Option<ResolvedThrottlerRateLimit>,
	concurrency: Option<ThrottlerConcurrencySettings>,
	circuit: Option<ThrottlerCircuitSettings>,
) -> KvTtl {
	let mut ttl = Duration::ZERO;
	if let Some(rate) = rate {
		ttl = ttl.max(scale_duration_for_throttler_activity_ttl(
			rate.full_refill_duration(),
		));
	}
	if let Some(concurrency) = concurrency {
		ttl = ttl.max(scale_duration_for_throttler_activity_ttl(
			concurrency.max_hold_duration(),
		));
	}
	if let Some(circuit) = circuit {
		ttl = ttl.max(scale_duration_for_throttler_activity_ttl(
			circuit.recovery_timeout(),
		));
		ttl = ttl.max(scale_duration_for_throttler_activity_ttl(
			circuit.probe_window(),
		));
	}
	KvTtl::expires_after(ttl.max(MIN_TTL))
		.expect("actual Throttler activity lifetime stays within its parsed TTL domain")
}
