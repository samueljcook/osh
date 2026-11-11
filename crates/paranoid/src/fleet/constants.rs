use super::{ComponentSchemaMigrationVersion, Duration};

/// Root key that the key validators use to check a key's length against the persisted key
/// limits. It is not a root key that any store uses.
pub(crate) const KEY_VALIDATION_ROOT_KEY: &str = "__paranoid_fleet";

pub(crate) const FLEET_SCHEMA_COMPONENT: &str = "fleet";
pub(crate) const FLEET_SCHEMA_V1_TARGET: ComponentSchemaMigrationVersion<'static> =
	ComponentSchemaMigrationVersion::new(1, "paranoid.fleet.v1");

pub(crate) const FLEET_MUTEX_KEY_PART: &str = "mutex";

pub(crate) const FLEET_COUNTER_KEY_PART: &str = "counter";

pub(crate) const FLEET_SEMAPHORE_KEY_PART: &str = "semaphore";

pub(crate) const FLEET_THROTTLER_KEY_PART: &str = "throttler";

pub(crate) const FLEET_ONCE_KEY_PART: &str = "once";

pub(crate) const FLEET_CACHE_KEY_PART: &str = "cache";

pub(crate) const FLEET_TOPIC_KEY_PART: &str = "topic";

pub(crate) const FLEET_CRON_KEY_PART: &str = "cron";

/// The largest number of slots of a Semaphore, and of the concurrency limit of a Throttler.
pub const MAX_SLOT_COUNT: u16 = 100;

/// The multiple of a Throttler's longest window for which its stored activity stays alive.
pub(in crate::fleet) const FLEET_THROTTLER_ACTIVITY_TTL_MULTIPLIER: u32 = 10;

/// The latest database time, as Unix microseconds, at which every stored Semaphore and Throttler
/// deadline fits in a signed 64-bit count of microseconds: `9999-12-31T23:59:59.999999Z`.
const LATEST_SUPPORTED_DATABASE_TIME_MICROS: i64 = 253_402_300_799_999_999;

/// [`MAX_SETTINGS_DURATION`] in whole microseconds.
pub(in crate::fleet) const MAX_SETTINGS_DURATION_MICROS: i64 = (i64::MAX
	- LATEST_SUPPORTED_DATABASE_TIME_MICROS)
	/ FLEET_THROTTLER_ACTIVITY_TTL_MULTIPLIER as i64;

/// The longest duration that the settings of a Semaphore or a Throttler accept, which is
/// 896,996,973,605,477,580 microseconds (about 28,400 years). It applies to a Semaphore's maximum
/// hold, to a Throttler's maximum hold, recovery timeout, and probe window, and to the time that
/// an empty Throttler rate-limit bucket takes to refill to its capacity.
///
/// A Throttler keeps its stored activity for ten times the longest of these durations. With this
/// limit, every stored Semaphore and Throttler deadline fits in a signed 64-bit count of
/// microseconds while the database time is `9999-12-31T23:59:59.999999Z` or earlier.
pub const MAX_SETTINGS_DURATION: Duration =
	Duration::from_micros(MAX_SETTINGS_DURATION_MICROS as u64);

/// The duration in whole microseconds, rounded up, when it is positive and at most
/// [`MAX_SETTINGS_DURATION`], and `None` otherwise.
pub(in crate::fleet) fn settings_duration_micros(duration: Duration) -> Option<i64> {
	if duration.is_zero() {
		return None;
	}
	primitives_rs::time::duration_to_ceil_micros_i64(duration)
		.ok()
		.filter(|micros| *micros <= MAX_SETTINGS_DURATION_MICROS)
}

pub(crate) const FLEET_ACQUIRE_RETRY_JITTER_FRACTION: f64 = 0.25;

pub(crate) const FLEET_ONCE_COMPLETION_KEY_PART: &str = "completion";
pub(crate) const FLEET_ONCE_MUTEX_KEY_PART: &str = "mutex";
pub(crate) const FLEET_ONCE_DONE_KEY_PART: &str = "done";
pub(crate) const FLEET_COUNTER_VALUE_KEY_PART: &str = "value";
pub(crate) const FLEET_SEMAPHORE_SLOTS_KEY_PART: &str = "slots";
pub(crate) const FLEET_THROTTLER_STATE_KEY_PART: &str = "state";
pub(crate) const COALESCING_CACHE_VALUE_KEY_PART: &str = "value";
pub(crate) const COALESCING_CACHE_EPOCH_KEY_PART: &str = "epoch";
pub(crate) const COALESCING_CACHE_MUTEX_KEY_PART: &str = "mutex";
pub(crate) const TOPIC_SEQUENCE_KEY_PART: &str = "sequence";
pub(crate) const TOPIC_EVENTS_KEY_PART: &str = "events";
pub(crate) const TOPIC_SUBSCRIPTIONS_KEY_PART: &str = "subscriptions";
pub(crate) const TOPIC_CURSOR_KEY_PART: &str = "cursor";
pub(crate) const TOPIC_POLLING_MUTEX_KEY_PART: &str = "polling_mutex";
