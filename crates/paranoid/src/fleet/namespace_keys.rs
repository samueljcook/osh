use super::{
	COALESCING_CACHE_EPOCH_KEY_PART, COALESCING_CACHE_MUTEX_KEY_PART,
	COALESCING_CACHE_VALUE_KEY_PART, CoalescingCacheKey, CoordinationKeyError, CounterKey, CronKey,
	FLEET_CACHE_KEY_PART, FLEET_COUNTER_KEY_PART, FLEET_CRON_KEY_PART, FLEET_MUTEX_KEY_PART,
	FLEET_ONCE_COMPLETION_KEY_PART, FLEET_ONCE_KEY_PART, FLEET_ONCE_MUTEX_KEY_PART,
	FLEET_SEMAPHORE_KEY_PART, FLEET_SEMAPHORE_SLOTS_KEY_PART, FLEET_THROTTLER_KEY_PART,
	FLEET_TOPIC_KEY_PART, KvKeyError, KvKeyPrefix, LeaseKey, MutexKey, OnceKey, RootKey,
	SemaphoreKey, SubscriptionKey, TOPIC_CURSOR_KEY_PART, TOPIC_EVENTS_KEY_PART,
	TOPIC_POLLING_MUTEX_KEY_PART, TOPIC_SEQUENCE_KEY_PART, TOPIC_SUBSCRIPTIONS_KEY_PART,
	ThrottlerKey, TopicKey,
};

pub(super) fn build_counter_prefix(
	root_key: &RootKey,
	counter_key: &CounterKey,
) -> Result<KvKeyPrefix, KvKeyError> {
	KvKeyPrefix::from_parts([
		root_key.as_str(),
		FLEET_COUNTER_KEY_PART,
		counter_key.as_str(),
	])
}

pub(super) fn build_topic_sequence_prefix(
	root_key: &RootKey,
	topic_key: &TopicKey,
) -> Result<KvKeyPrefix, KvKeyError> {
	KvKeyPrefix::from_parts([
		root_key.as_str(),
		FLEET_TOPIC_KEY_PART,
		topic_key.as_str(),
		TOPIC_SEQUENCE_KEY_PART,
	])
}

pub(super) fn build_topic_events_prefix(
	root_key: &RootKey,
	topic_key: &TopicKey,
) -> Result<KvKeyPrefix, KvKeyError> {
	KvKeyPrefix::from_parts([
		root_key.as_str(),
		FLEET_TOPIC_KEY_PART,
		topic_key.as_str(),
		TOPIC_EVENTS_KEY_PART,
	])
}

pub(super) fn build_subscription_cursor_prefix(
	root_key: &RootKey,
	topic_key: &TopicKey,
	subscription_key: &SubscriptionKey,
) -> Result<KvKeyPrefix, KvKeyError> {
	KvKeyPrefix::from_parts([
		root_key.as_str(),
		FLEET_TOPIC_KEY_PART,
		topic_key.as_str(),
		TOPIC_SUBSCRIPTIONS_KEY_PART,
		subscription_key.as_str(),
		TOPIC_CURSOR_KEY_PART,
	])
}

pub(super) fn build_subscription_polling_mutex_lease_key(
	root_key: &RootKey,
	topic_key: &TopicKey,
	subscription_key: &SubscriptionKey,
) -> Result<LeaseKey, CoordinationKeyError> {
	LeaseKey::from_parts([
		root_key.as_str(),
		FLEET_TOPIC_KEY_PART,
		topic_key.as_str(),
		TOPIC_SUBSCRIPTIONS_KEY_PART,
		subscription_key.as_str(),
		TOPIC_POLLING_MUTEX_KEY_PART,
	])
}

pub(crate) fn build_mutex_lease_key(
	root_key: &RootKey,
	mutex_key: &MutexKey,
) -> Result<LeaseKey, CoordinationKeyError> {
	LeaseKey::from_parts([root_key.as_str(), FLEET_MUTEX_KEY_PART, mutex_key.as_str()])
}

pub(crate) fn build_cron_mutex_lease_key(
	root_key: &RootKey,
	cron_key: &CronKey,
) -> Result<LeaseKey, CoordinationKeyError> {
	LeaseKey::from_parts([root_key.as_str(), FLEET_CRON_KEY_PART, cron_key.as_str()])
}

pub(super) fn build_coalescing_cache_entry_prefix(
	root_key: &RootKey,
	cache_key: &CoalescingCacheKey,
) -> Result<KvKeyPrefix, KvKeyError> {
	KvKeyPrefix::from_parts([
		root_key.as_str(),
		FLEET_CACHE_KEY_PART,
		cache_key.as_str(),
		COALESCING_CACHE_VALUE_KEY_PART,
	])
}

pub(super) fn build_coalescing_cache_epoch_prefix(
	root_key: &RootKey,
	cache_key: &CoalescingCacheKey,
) -> Result<KvKeyPrefix, KvKeyError> {
	KvKeyPrefix::from_parts([
		root_key.as_str(),
		FLEET_CACHE_KEY_PART,
		cache_key.as_str(),
		COALESCING_CACHE_EPOCH_KEY_PART,
	])
}

pub(super) fn build_coalescing_cache_mutex_lease_key<S, I>(
	root_key: &RootKey,
	cache_key: &CoalescingCacheKey,
	key_parts: I,
) -> Result<LeaseKey, CoordinationKeyError>
where
	S: AsRef<str>,
	I: IntoIterator<Item = S>,
{
	let mut parts = vec![
		root_key.as_str().to_owned(),
		FLEET_CACHE_KEY_PART.to_owned(),
		cache_key.as_str().to_owned(),
		COALESCING_CACHE_MUTEX_KEY_PART.to_owned(),
	];
	parts.extend(key_parts.into_iter().map(|part| part.as_ref().to_owned()));
	LeaseKey::from_parts(parts)
}

pub(super) fn validated_cache_key_parts<S, I>(key_parts: I) -> Result<Vec<String>, KvKeyError>
where
	S: AsRef<str>,
	I: IntoIterator<Item = S>,
{
	let key_parts = key_parts
		.into_iter()
		.map(|part| part.as_ref().to_owned())
		.collect::<Vec<_>>();
	KvKeyPrefix::from_parts(
		std::iter::once("validation").chain(key_parts.iter().map(String::as_str)),
	)?;
	Ok(key_parts)
}

pub(super) fn build_semaphore_slots_prefix(
	root_key: &RootKey,
	semaphore_key: &SemaphoreKey,
) -> Result<KvKeyPrefix, KvKeyError> {
	KvKeyPrefix::from_parts([
		root_key.as_str(),
		FLEET_SEMAPHORE_KEY_PART,
		FLEET_SEMAPHORE_SLOTS_KEY_PART,
		semaphore_key.as_str(),
	])
}

pub(super) fn build_throttler_state_prefix(
	root_key: &RootKey,
	throttler_key: &ThrottlerKey,
) -> Result<KvKeyPrefix, KvKeyError> {
	KvKeyPrefix::from_parts([
		root_key.as_str(),
		FLEET_THROTTLER_KEY_PART,
		throttler_key.as_str(),
	])
}

pub(super) fn build_once_completion_prefix(
	root_key: &RootKey,
	once_key: &OnceKey,
) -> Result<KvKeyPrefix, KvKeyError> {
	KvKeyPrefix::from_parts([
		root_key.as_str(),
		FLEET_ONCE_KEY_PART,
		FLEET_ONCE_COMPLETION_KEY_PART,
		once_key.as_str(),
	])
}

pub(super) fn build_once_mutex_lease_key(
	root_key: &RootKey,
	once_key: &OnceKey,
) -> Result<LeaseKey, CoordinationKeyError> {
	LeaseKey::from_parts([
		root_key.as_str(),
		FLEET_ONCE_KEY_PART,
		FLEET_ONCE_MUTEX_KEY_PART,
		once_key.as_str(),
	])
}

#[cfg(test)]
mod tests {
	use super::super::*;
	use super::*;

	fn prefix_text(prefix: Result<KvKeyPrefix, KvKeyError>) -> String {
		prefix.expect("prefix").as_str().to_owned()
	}

	fn lease_key_text(key: Result<LeaseKey, CoordinationKeyError>) -> String {
		key.expect("lease key").as_str().to_owned()
	}

	fn root() -> RootKey {
		RootKey::new("cook").expect("root key")
	}

	#[test]
	fn every_key_prefix_lies_below_the_caller_root_in_its_primitive_namespace() {
		let counter = CounterKey::new("visits").expect("counter key");
		let cache = CoalescingCacheKey::new("pages").expect("cache key");
		let topic = TopicKey::new("events").expect("topic key");
		let subscription = SubscriptionKey::new("indexer").expect("subscription key");
		let semaphore = SemaphoreKey::new("uploads").expect("semaphore key");
		let throttler = ThrottlerKey::new("api").expect("throttler key");
		let once = OnceKey::new("seed").expect("once key");
		assert_eq!(
			prefix_text(build_counter_prefix(&root(), &counter)),
			"cook::counter::visits::"
		);
		assert_eq!(
			prefix_text(build_coalescing_cache_entry_prefix(&root(), &cache)),
			"cook::cache::pages::value::"
		);
		assert_eq!(
			prefix_text(build_coalescing_cache_epoch_prefix(&root(), &cache)),
			"cook::cache::pages::epoch::"
		);
		assert_eq!(
			prefix_text(build_topic_sequence_prefix(&root(), &topic)),
			"cook::topic::events::sequence::"
		);
		assert_eq!(
			prefix_text(build_topic_events_prefix(&root(), &topic)),
			"cook::topic::events::events::"
		);
		assert_eq!(
			prefix_text(build_subscription_cursor_prefix(
				&root(),
				&topic,
				&subscription
			)),
			"cook::topic::events::subscriptions::indexer::cursor::"
		);
		assert_eq!(
			prefix_text(build_semaphore_slots_prefix(&root(), &semaphore)),
			"cook::semaphore::slots::uploads::"
		);
		assert_eq!(
			prefix_text(build_throttler_state_prefix(&root(), &throttler)),
			"cook::throttler::api::"
		);
		assert_eq!(
			prefix_text(build_once_completion_prefix(&root(), &once)),
			"cook::once::completion::seed::"
		);
	}

	#[test]
	fn every_lease_key_lies_below_the_caller_root_in_its_primitive_namespace() {
		let cron = CronKey::new("sweep").expect("cron key");
		let once = OnceKey::new("seed").expect("once key");
		let cache = CoalescingCacheKey::new("pages").expect("cache key");
		let topic = TopicKey::new("events").expect("topic key");
		let subscription = SubscriptionKey::new("indexer").expect("subscription key");
		assert_eq!(
			lease_key_text(build_cron_mutex_lease_key(&root(), &cron)),
			"cook::cron::sweep::"
		);
		assert_eq!(
			lease_key_text(build_once_mutex_lease_key(&root(), &once)),
			"cook::once::mutex::seed::"
		);
		assert_eq!(
			lease_key_text(build_coalescing_cache_mutex_lease_key(
				&root(),
				&cache,
				["tenant", "42"]
			)),
			"cook::cache::pages::mutex::tenant::42::"
		);
		assert_eq!(
			lease_key_text(build_subscription_polling_mutex_lease_key(
				&root(),
				&topic,
				&subscription
			)),
			"cook::topic::events::subscriptions::indexer::polling_mutex::"
		);
	}

	#[test]
	fn cache_key_parts_are_validated_like_every_other_key_part() {
		assert_eq!(
			validated_cache_key_parts(["tenant", "42"]).expect("parts"),
			["tenant", "42"]
		);
		for invalid in [vec![""], vec!["a:b"], vec!["a\0b"]] {
			assert!(validated_cache_key_parts(invalid).is_err());
		}
	}
}
