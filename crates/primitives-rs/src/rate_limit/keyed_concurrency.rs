//! Nonwaiting concurrency admission with global and per-key bounds.
//!
//! A permit retains one key until it is dropped. Keys with no active permits are
//! removed immediately, so retained entries cannot exceed the global limit.
//! The limiter has no queue, clock, eviction policy, or background task.

use std::{
	collections::HashMap,
	hash::Hash,
	num::NonZeroUsize,
	sync::{Arc, Mutex},
};

/// Explicit limits for all active work and for each key.
#[derive(Clone, Copy, Debug)]
pub struct Config {
	/// The maximum number of active permits across all keys.
	pub max_concurrent: NonZeroUsize,
	/// The maximum number of active permits for one key; it cannot exceed the global limit.
	pub max_concurrent_per_key: NonZeroUsize,
}

/// Shared bounded admission for work associated with caller-supplied keys.
#[derive(Clone)]
pub struct KeyedConcurrency<K: Eq + Hash> {
	inner: Arc<Inner<K>>,
}
struct Inner<K> {
	config: Config,
	state: Mutex<State<K>>,
}
struct State<K> {
	total: usize,
	counts: HashMap<K, NonZeroUsize>,
}

/// One admitted operation. Dropping this value releases both capacity limits.
#[must_use = "The permit must remain alive while its operation is active."]
pub struct Permit<K: Eq + Hash> {
	inner: Arc<Inner<K>>,
	key: K,
}

/// A per-key limit that exceeds the global limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidConfig;
impl std::fmt::Display for InvalidConfig {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str("per-key concurrency exceeds global concurrency")
	}
}
impl std::error::Error for InvalidConfig {}

/// The reason that an operation could not acquire a permit immediately.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AcquireError {
	/// All global permits are in use.
	GlobalCapacity,
	/// This key already has its maximum number of active permits.
	KeyCapacity,
	/// A panic occurred while the limiter's state was locked.
	Poisoned,
}
impl std::fmt::Display for AcquireError {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str(match self {
			Self::GlobalCapacity => "global concurrency capacity unavailable",
			Self::KeyCapacity => "per-key concurrency capacity unavailable",
			Self::Poisoned => "concurrency limiter state poisoned",
		})
	}
}
impl std::error::Error for AcquireError {}

impl<K: Eq + Hash + Clone> KeyedConcurrency<K> {
	/// Creates an empty limiter after checking that the limits are consistent.
	pub fn new(config: Config) -> Result<Self, InvalidConfig> {
		if config.max_concurrent_per_key > config.max_concurrent {
			return Err(InvalidConfig);
		}
		Ok(Self {
			inner: Arc::new(Inner {
				config,
				state: Mutex::new(State {
					total: 0,
					counts: HashMap::new(),
				}),
			}),
		})
	}

	/// Acquires one permit without waiting, retaining a clone of the key in shared state.
	pub fn try_acquire(&self, key: K) -> Result<Permit<K>, AcquireError> {
		let mut state = self
			.inner
			.state
			.lock()
			.map_err(|_| AcquireError::Poisoned)?;
		if state.counts.get(&key).map_or(0, |count| count.get())
			>= self.inner.config.max_concurrent_per_key.get()
		{
			return Err(AcquireError::KeyCapacity);
		}
		if state.total >= self.inner.config.max_concurrent.get() {
			return Err(AcquireError::GlobalCapacity);
		}
		state
			.counts
			.entry(key.clone())
			.and_modify(|count| {
				*count = count
					.checked_add(1)
					.expect("admitted key counts fit the configured bound")
			})
			.or_insert(NonZeroUsize::new(1).expect("one is nonzero"));
		state.total += 1;
		Ok(Permit {
			inner: self.inner.clone(),
			key,
		})
	}
}

impl<K: Eq + Hash> Drop for Permit<K> {
	fn drop(&mut self) {
		let Ok(mut state) = self.inner.state.lock() else {
			return;
		};
		let count = state
			.counts
			.get_mut(&self.key)
			.expect("an active permit has a retained key");
		match NonZeroUsize::new(count.get() - 1) {
			Some(remaining) => *count = remaining,
			None => {
				state.counts.remove(&self.key);
			}
		}
		state.total -= 1;
	}
}
