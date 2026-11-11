use super::{
	CoordinationKeyError, FLEET_MUTEX_KEY_PART, FLEET_ONCE_COMPLETION_KEY_PART,
	FLEET_ONCE_KEY_PART, FLEET_SEMAPHORE_KEY_PART, FLEET_THROTTLER_KEY_PART,
	KEY_VALIDATION_ROOT_KEY, KeyError, KvKeyError, KvKeyPrefix, LeaseKey,
	build_coalescing_cache_entry_prefix, build_coalescing_cache_epoch_prefix,
	build_coalescing_cache_mutex_lease_key, build_counter_prefix, build_cron_mutex_lease_key,
	build_once_completion_prefix, build_once_mutex_lease_key, build_semaphore_slots_prefix,
	build_subscription_cursor_prefix, build_subscription_polling_mutex_lease_key,
	build_throttler_state_prefix, build_topic_events_prefix, build_topic_sequence_prefix,
};

/// Validated Fleet root key.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RootKey(pub(super) String);

/// Validated Fleet mutex key.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MutexKey(pub(super) String);

/// Validated Fleet counter key.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CounterKey(pub(super) String);

/// Validated Fleet coalescing cache key.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CoalescingCacheKey(pub(super) String);

/// Validated Fleet topic key.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TopicKey(pub(super) String);

/// Validated Fleet subscription key.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SubscriptionKey(pub(super) String);

/// Validated Fleet cron key.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CronKey(pub(super) String);

/// Validated Fleet semaphore key.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SemaphoreKey(pub(super) String);

/// Validated Fleet throttler key.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ThrottlerKey(pub(super) String);

/// Validated Fleet run-once key.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OnceKey(pub(super) String);

impl RootKey {
	/// Validates and copies a Fleet root key.
	pub fn new(input: impl AsRef<str>) -> Result<Self, KeyError> {
		let input = input.as_ref();
		let root_key = Self(input.to_owned());
		let counter_key = CounterKey("validation".to_owned());
		let cache_key = CoalescingCacheKey("validation".to_owned());
		let topic_key = TopicKey("validation".to_owned());
		let subscription_key = SubscriptionKey("validation".to_owned());
		let cron_key = CronKey("validation".to_owned());
		LeaseKey::from_parts([input, FLEET_MUTEX_KEY_PART, "validation"])
			.map_err(KeyError::Coordination)?;
		build_counter_prefix(&root_key, &counter_key).map_err(KeyError::Stored)?;
		build_coalescing_cache_entry_prefix(&root_key, &cache_key).map_err(KeyError::Stored)?;
		build_coalescing_cache_epoch_prefix(&root_key, &cache_key).map_err(KeyError::Stored)?;
		build_coalescing_cache_mutex_lease_key(&root_key, &cache_key, std::iter::empty::<&str>())
			.map_err(KeyError::Coordination)?;
		build_topic_sequence_prefix(&root_key, &topic_key).map_err(KeyError::Stored)?;
		build_topic_events_prefix(&root_key, &topic_key).map_err(KeyError::Stored)?;
		build_subscription_cursor_prefix(&root_key, &topic_key, &subscription_key)
			.map_err(KeyError::Stored)?;
		build_cron_mutex_lease_key(&root_key, &cron_key).map_err(KeyError::Coordination)?;
		KvKeyPrefix::from_parts([input, FLEET_SEMAPHORE_KEY_PART, "validation"])
			.map_err(KeyError::Stored)?;
		KvKeyPrefix::from_parts([input, FLEET_THROTTLER_KEY_PART, "validation"])
			.map_err(KeyError::Stored)?;
		KvKeyPrefix::from_parts([
			input,
			FLEET_ONCE_KEY_PART,
			FLEET_ONCE_COMPLETION_KEY_PART,
			"validation",
		])
		.map_err(KeyError::Stored)?;
		Ok(root_key)
	}

	/// Returns the validated root key text.
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl MutexKey {
	/// Validates and copies a Fleet mutex key.
	pub fn new(input: impl AsRef<str>) -> Result<Self, CoordinationKeyError> {
		let input = input.as_ref();
		LeaseKey::from_parts([KEY_VALIDATION_ROOT_KEY, FLEET_MUTEX_KEY_PART, input])?;
		Ok(Self(input.to_owned()))
	}

	/// Returns the validated mutex key text.
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl CounterKey {
	/// Validates and copies a Fleet counter key.
	pub fn new(input: impl AsRef<str>) -> Result<Self, KvKeyError> {
		let input = input.as_ref();
		let counter_key = Self(input.to_owned());
		build_counter_prefix(&RootKey(KEY_VALIDATION_ROOT_KEY.to_owned()), &counter_key)?;
		Ok(counter_key)
	}

	/// Returns the validated counter key text.
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl CoalescingCacheKey {
	/// Validates and copies a Fleet coalescing cache key.
	pub fn new(input: impl AsRef<str>) -> Result<Self, KeyError> {
		let input = input.as_ref();
		let cache_key = Self(input.to_owned());
		let root_key = RootKey(KEY_VALIDATION_ROOT_KEY.to_owned());
		build_coalescing_cache_entry_prefix(&root_key, &cache_key).map_err(KeyError::Stored)?;
		build_coalescing_cache_epoch_prefix(&root_key, &cache_key).map_err(KeyError::Stored)?;
		build_coalescing_cache_mutex_lease_key(&root_key, &cache_key, std::iter::empty::<&str>())
			.map_err(KeyError::Coordination)?;
		Ok(cache_key)
	}

	/// Returns the validated cache key text.
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl TopicKey {
	/// Validates and copies a Fleet topic key.
	pub fn new(input: impl AsRef<str>) -> Result<Self, KvKeyError> {
		let input = input.as_ref();
		let topic_key = Self(input.to_owned());
		let root_key = RootKey(KEY_VALIDATION_ROOT_KEY.to_owned());
		build_topic_sequence_prefix(&root_key, &topic_key)?;
		build_topic_events_prefix(&root_key, &topic_key)?;
		Ok(topic_key)
	}

	/// Returns the validated topic key text.
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl SubscriptionKey {
	/// Validates and copies a Fleet subscription key.
	pub fn new(input: impl AsRef<str>) -> Result<Self, KeyError> {
		let input = input.as_ref();
		let subscription_key = Self(input.to_owned());
		let root_key = RootKey(KEY_VALIDATION_ROOT_KEY.to_owned());
		let topic_key = TopicKey("validation".to_owned());
		build_subscription_cursor_prefix(&root_key, &topic_key, &subscription_key)
			.map_err(KeyError::Stored)?;
		build_subscription_polling_mutex_lease_key(&root_key, &topic_key, &subscription_key)
			.map_err(KeyError::Coordination)?;
		Ok(subscription_key)
	}

	/// Returns the validated subscription key text.
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl CronKey {
	/// Validates and copies a Fleet cron key.
	pub fn new(input: impl AsRef<str>) -> Result<Self, CoordinationKeyError> {
		let input = input.as_ref();
		let cron_key = Self(input.to_owned());
		build_cron_mutex_lease_key(&RootKey(KEY_VALIDATION_ROOT_KEY.to_owned()), &cron_key)?;
		Ok(cron_key)
	}

	/// Returns the validated cron key text.
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl SemaphoreKey {
	/// Validates and copies a Fleet semaphore key.
	pub fn new(input: impl AsRef<str>) -> Result<Self, KvKeyError> {
		let input = input.as_ref();
		let semaphore_key = Self(input.to_owned());
		build_semaphore_slots_prefix(&RootKey(KEY_VALIDATION_ROOT_KEY.to_owned()), &semaphore_key)?;
		Ok(semaphore_key)
	}

	/// Returns the validated semaphore key text.
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl ThrottlerKey {
	/// Validates and copies a Fleet throttler key.
	pub fn new(input: impl AsRef<str>) -> Result<Self, KvKeyError> {
		let input = input.as_ref();
		let throttler_key = Self(input.to_owned());
		build_throttler_state_prefix(&RootKey(KEY_VALIDATION_ROOT_KEY.to_owned()), &throttler_key)?;
		Ok(throttler_key)
	}

	/// Returns the validated throttler key text.
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl OnceKey {
	/// Validates and copies a Fleet run-once key.
	pub fn new(input: impl AsRef<str>) -> Result<Self, KeyError> {
		let input = input.as_ref();
		let once_key = Self(input.to_owned());
		let root_key = RootKey(KEY_VALIDATION_ROOT_KEY.to_owned());
		build_once_completion_prefix(&root_key, &once_key).map_err(KeyError::Stored)?;
		build_once_mutex_lease_key(&root_key, &once_key).map_err(KeyError::Coordination)?;
		Ok(once_key)
	}

	/// Returns the validated run-once key text.
	pub fn as_str(&self) -> &str {
		&self.0
	}
}
