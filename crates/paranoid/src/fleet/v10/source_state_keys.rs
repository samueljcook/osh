//! Exact whole-role inverse Fleet key classification, without value or data authority.

use std::fmt;

use crate::fleet::constants::*;
use crate::fleet::namespace_keys::*;
use crate::fleet::topic::parse_topic_sequence_key_suffix;
use crate::fleet::{
	CoalescingCacheKey, CounterKey, InvalidTopicEventSequenceSuffixError, KeyError, MAX_SLOT_COUNT,
	OnceKey, RootKey, SemaphoreKey, SubscriptionKey, ThrottlerKey, TopicKey,
};
use crate::kv;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FleetSourceFamily {
	Counter,
	SemaphoreSlot,
	Throttler,
	OnceCompletion,
	CacheEpoch,
	CacheEntry,
	TopicSequence,
	TopicEvent,
	SubscriptionCursor,
}

pub(crate) enum FleetSourceKeyKind {
	Counter(CounterKey),
	SemaphoreSlot,
	Throttler,
	OnceCompletion(OnceKey),
	CacheEpoch(CoalescingCacheKey),
	CacheEntry,
	TopicSequence(TopicKey),
	TopicEvent,
	SubscriptionCursor {
		topic: TopicKey,
		subscription: SubscriptionKey,
	},
}
impl FleetSourceKeyKind {
	pub(crate) fn family(&self) -> FleetSourceFamily {
		match self {
			Self::Counter(_) => FleetSourceFamily::Counter,
			Self::SemaphoreSlot { .. } => FleetSourceFamily::SemaphoreSlot,
			Self::Throttler => FleetSourceFamily::Throttler,
			Self::OnceCompletion(_) => FleetSourceFamily::OnceCompletion,
			Self::CacheEpoch(_) => FleetSourceFamily::CacheEpoch,
			Self::CacheEntry { .. } => FleetSourceFamily::CacheEntry,
			Self::TopicSequence(_) => FleetSourceFamily::TopicSequence,
			Self::TopicEvent { .. } => FleetSourceFamily::TopicEvent,
			Self::SubscriptionCursor { .. } => FleetSourceFamily::SubscriptionCursor,
		}
	}
	/// Whether data retirement moves this v10 row into v11. Counters, Once completions, and
	/// cache epochs move; v11 starts every Semaphore, Throttler, cache value, and Topic empty.
	pub(crate) fn is_carried_into_v11(&self) -> bool {
		matches!(
			self,
			Self::Counter(_) | Self::OnceCompletion(_) | Self::CacheEpoch(_)
		)
	}
}
impl fmt::Debug for FleetSourceKeyKind {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("FleetSourceKeyKind")
			.field("family", &self.family())
			.finish_non_exhaustive()
	}
}

pub(crate) struct FleetSourceKey {
	key: kv::Key,
	kind: FleetSourceKeyKind,
}
impl FleetSourceKey {
	pub(crate) fn parse(root: &RootKey, key: kv::Key) -> Result<Self, FleetSourceKeyError> {
		match classify(root, &key) {
			Ok(kind) => Ok(Self { key, kind }),
			Err(source) => Err(FleetSourceKeyError { source }),
		}
	}
	pub(crate) fn key(&self) -> &kv::Key {
		&self.key
	}
	pub(crate) fn kind(&self) -> &FleetSourceKeyKind {
		&self.kind
	}
}
impl fmt::Debug for FleetSourceKey {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("FleetSourceKey")
			.field("family", &self.kind.family())
			.field("key_byte_len", &self.key.as_str().len())
			.finish_non_exhaustive()
	}
}

fn classify(root: &RootKey, key: &kv::Key) -> Result<FleetSourceKeyKind, FleetSourceKeyCauseError> {
	let body = key
		.as_str()
		.strip_suffix(kv::KEY_SEPARATOR)
		.ok_or(FleetSourceKeyCauseError::UnsupportedShape)?;
	let parts: Vec<&str> = body.split(kv::KEY_SEPARATOR).collect();
	if parts.first() != Some(&root.as_str()) {
		return Err(FleetSourceKeyCauseError::RootMismatch);
	}
	let suffix = &parts[1..];
	let (kind, rebuilt) = match suffix {
		[FLEET_COUNTER_KEY_PART, name, FLEET_COUNTER_VALUE_KEY_PART] => {
			let name = CounterKey::new(name).map_err(FleetSourceKeyCauseError::Kv)?;
			let prefix = build_counter_prefix(root, &name).map_err(FleetSourceKeyCauseError::Kv)?;
			(
				FleetSourceKeyKind::Counter(name),
				append_key(&prefix, [FLEET_COUNTER_VALUE_KEY_PART])?,
			)
		}
		[
			FLEET_SEMAPHORE_KEY_PART,
			FLEET_SEMAPHORE_SLOTS_KEY_PART,
			name,
			slot,
		] => {
			let name = SemaphoreKey::new(name).map_err(FleetSourceKeyCauseError::Kv)?;
			let _number = slot
				.parse::<u16>()
				.ok()
				.filter(|number| {
					(1..=MAX_SLOT_COUNT).contains(number) && number.to_string() == *slot
				})
				.ok_or(FleetSourceKeyCauseError::SlotSuffix)?;
			let prefix =
				build_semaphore_slots_prefix(root, &name).map_err(FleetSourceKeyCauseError::Kv)?;
			(
				FleetSourceKeyKind::SemaphoreSlot,
				append_key(&prefix, [*slot])?,
			)
		}
		[
			FLEET_THROTTLER_KEY_PART,
			name,
			FLEET_THROTTLER_STATE_KEY_PART,
		] => {
			let name = ThrottlerKey::new(name).map_err(FleetSourceKeyCauseError::Kv)?;
			let prefix =
				build_throttler_state_prefix(root, &name).map_err(FleetSourceKeyCauseError::Kv)?;
			(
				FleetSourceKeyKind::Throttler,
				append_key(&prefix, [FLEET_THROTTLER_STATE_KEY_PART])?,
			)
		}
		[
			FLEET_ONCE_KEY_PART,
			FLEET_ONCE_COMPLETION_KEY_PART,
			name,
			FLEET_ONCE_DONE_KEY_PART,
		] => {
			let name = OnceKey::new(name).map_err(FleetSourceKeyCauseError::Domain)?;
			let prefix =
				build_once_completion_prefix(root, &name).map_err(FleetSourceKeyCauseError::Kv)?;
			(
				FleetSourceKeyKind::OnceCompletion(name),
				append_key(&prefix, [FLEET_ONCE_DONE_KEY_PART])?,
			)
		}
		[FLEET_CACHE_KEY_PART, name, COALESCING_CACHE_EPOCH_KEY_PART] => {
			let name = CoalescingCacheKey::new(name).map_err(FleetSourceKeyCauseError::Domain)?;
			let prefix = build_coalescing_cache_epoch_prefix(root, &name)
				.map_err(FleetSourceKeyCauseError::Kv)?;
			(
				FleetSourceKeyKind::CacheEpoch(name),
				append_key(&prefix, std::iter::empty())?,
			)
		}
		[
			FLEET_CACHE_KEY_PART,
			name,
			COALESCING_CACHE_VALUE_KEY_PART,
			rest @ ..,
		] => {
			let name = CoalescingCacheKey::new(name).map_err(FleetSourceKeyCauseError::Domain)?;
			let prefix = build_coalescing_cache_entry_prefix(root, &name)
				.map_err(FleetSourceKeyCauseError::Kv)?;
			let rebuilt = append_key(&prefix, rest.iter().copied())?;
			(FleetSourceKeyKind::CacheEntry, rebuilt)
		}
		[FLEET_TOPIC_KEY_PART, name, TOPIC_SEQUENCE_KEY_PART] => {
			let name = TopicKey::new(name).map_err(FleetSourceKeyCauseError::Kv)?;
			let prefix =
				build_topic_sequence_prefix(root, &name).map_err(FleetSourceKeyCauseError::Kv)?;
			(
				FleetSourceKeyKind::TopicSequence(name),
				append_key(&prefix, std::iter::empty())?,
			)
		}
		[FLEET_TOPIC_KEY_PART, name, TOPIC_EVENTS_KEY_PART, sequence] => {
			let name = TopicKey::new(name).map_err(FleetSourceKeyCauseError::Kv)?;
			let _sequence_value = parse_topic_sequence_key_suffix(sequence)
				.map_err(FleetSourceKeyCauseError::EventSequence)?;
			let prefix =
				build_topic_events_prefix(root, &name).map_err(FleetSourceKeyCauseError::Kv)?;
			(
				FleetSourceKeyKind::TopicEvent,
				append_key(&prefix, [*sequence])?,
			)
		}
		[
			FLEET_TOPIC_KEY_PART,
			name,
			TOPIC_SUBSCRIPTIONS_KEY_PART,
			subscription,
			TOPIC_CURSOR_KEY_PART,
		] => {
			let name = TopicKey::new(name).map_err(FleetSourceKeyCauseError::Kv)?;
			let subscription =
				SubscriptionKey::new(subscription).map_err(FleetSourceKeyCauseError::Domain)?;
			let prefix = build_subscription_cursor_prefix(root, &name, &subscription)
				.map_err(FleetSourceKeyCauseError::Kv)?;
			(
				FleetSourceKeyKind::SubscriptionCursor {
					topic: name,
					subscription,
				},
				append_key(&prefix, std::iter::empty())?,
			)
		}
		_ => return Err(FleetSourceKeyCauseError::UnsupportedShape),
	};
	if rebuilt != *key {
		return Err(FleetSourceKeyCauseError::UnsupportedShape);
	}
	Ok(kind)
}
fn append_key<'a>(
	prefix: &kv::KeyPrefix,
	suffix: impl IntoIterator<Item = &'a str>,
) -> Result<kv::Key, FleetSourceKeyCauseError> {
	kv::Key::from_prefix_and_parts(prefix, suffix).map_err(FleetSourceKeyCauseError::Kv)
}

/// The exact rejected key and original typed cause are deliberate diagnostic accessors.
pub(crate) struct FleetSourceKeyError {
	source: FleetSourceKeyCauseError,
}
impl fmt::Debug for FleetSourceKeyError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("FleetSourceKeyError")
			.field("source", &self.source)
			.finish_non_exhaustive()
	}
}
impl fmt::Display for FleetSourceKeyError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("source Fleet key cannot be classified under its actual root")
	}
}
impl std::error::Error for FleetSourceKeyError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(&self.source)
	}
}
#[derive(thiserror::Error)]
pub(crate) enum FleetSourceKeyCauseError {
	#[error("source Fleet key belongs to another root")]
	RootMismatch,
	#[error("source Fleet key has an unsupported family or full shape")]
	UnsupportedShape,
	#[error("source semaphore slot suffix is not canonical or supported")]
	SlotSuffix,
	#[error("source Fleet key component is not a valid KV identity")]
	Kv(#[source] kv::KeyError),
	#[error("source Fleet key component is not a valid primitive identity")]
	Domain(#[source] KeyError),
	#[error("source topic event suffix is invalid")]
	EventSequence(#[source] InvalidTopicEventSequenceSuffixError),
}
impl fmt::Debug for FleetSourceKeyCauseError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		fmt::Display::fmt(self, f)
	}
}
