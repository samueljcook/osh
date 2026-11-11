//! An integer token bucket with caller-supplied monotonic elapsed time.
//!
//! The bucket holds no request identities and allocates no memory. Callers own
//! synchronization and choose whether distinct traffic classes share a bucket.
//! A backwards time value never creates credit or moves the recorded time back.

use std::num::NonZeroU32;
use std::time::Duration;

/// Capacity and continuous refill rate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Config {
	/// The maximum accumulated credit, in whole tokens.
	pub capacity: NonZeroU32,
	/// The number of tokens replenished over one period.
	pub refill_tokens: NonZeroU32,
	/// A nonzero refill period.
	pub refill_period: Duration,
}

/// A token bucket with exact fractional credit and a fixed memory cost.
#[derive(Debug)]
pub struct TokenBucket {
	config: Config,
	credit: u128,
	last_time: Duration,
}

/// Exact state for restoring a bucket with the same configuration and time origin.
/// Callers must preserve the configuration together with this state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Snapshot {
	/// Credit in units of one token multiplied by the refill period in nanoseconds.
	pub credit_units: u128,
	/// The latest observed elapsed time from the caller's time origin.
	pub last_time: Duration,
}

/// A snapshot with more credit than the configured bucket can hold.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestoreError {
	/// The supplied configuration is invalid.
	Config(ConfigError),
	/// Snapshot credit exceeds the configured capacity.
	CreditExceedsCapacity,
}
impl std::fmt::Display for RestoreError {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Config(error) => error.fmt(formatter),
			Self::CreditExceedsCapacity => {
				formatter.write_str("token bucket snapshot credit exceeds capacity")
			}
		}
	}
}
impl std::error::Error for RestoreError {}

impl TokenBucket {
	/// Restores exact fractional credit and the recorded time without adding credit.
	pub fn from_snapshot(config: Config, snapshot: Snapshot) -> Result<Self, RestoreError> {
		let mut bucket = Self::new(config, snapshot.last_time).map_err(RestoreError::Config)?;
		if snapshot.credit_units > bucket.credit {
			return Err(RestoreError::CreditExceedsCapacity);
		}
		bucket.credit = snapshot.credit_units;
		Ok(bucket)
	}

	/// Returns exact state without advancing the bucket's time.
	pub fn snapshot(&self) -> Snapshot {
		Snapshot {
			credit_units: self.credit,
			last_time: self.last_time,
		}
	}

	/// Returns the time needed to refill completely, measured from the recorded time.
	pub fn time_until_full(&self) -> Duration {
		let capacity =
			u128::from(self.config.capacity.get()) * self.config.refill_period.as_nanos();
		let nanos = (capacity - self.credit).div_ceil(u128::from(self.config.refill_tokens.get()));
		crate::time::duration_from_nanos_u128(nanos)
			.expect("Config::new bounds refill duration to Duration::MAX")
	}

	/// Constructs a full bucket at the supplied monotonic elapsed time.
	pub fn new(config: Config, now: Duration) -> Result<Self, ConfigError> {
		if config.refill_period.is_zero() {
			return Err(ConfigError::ZeroPeriod);
		}
		let credit = u128::from(config.capacity.get()) * config.refill_period.as_nanos();
		if credit.div_ceil(u128::from(config.refill_tokens.get())) > Duration::MAX.as_nanos() {
			return Err(ConfigError::RefillTimeOverflow);
		}
		Ok(Self {
			credit,
			config,
			last_time: now,
		})
	}

	/// Attempts to consume tokens without waiting or allocating.
	pub fn try_take(&mut self, input: TakeInput) -> Admission {
		if input.tokens.get() > self.config.capacity.get() {
			return Admission::ExceedsCapacity;
		}
		self.refill(input.now);
		let period = self.config.refill_period.as_nanos();
		let cost = u128::from(input.tokens.get()) * period;
		if self.credit >= cost {
			self.credit -= cost;
			Admission::Allowed
		} else {
			let nanos = (cost - self.credit).div_ceil(u128::from(self.config.refill_tokens.get()));
			Admission::RetryAfter(
				crate::time::duration_from_nanos_u128(nanos)
					.expect("TokenBucket::new checks that full refill fits in Duration"),
			)
		}
	}

	/// Advances the recorded time and replenishes credit without consuming tokens.
	/// A backwards time value leaves both credit and recorded time unchanged.
	pub fn refill(&mut self, now: Duration) {
		let now = now.max(self.last_time);
		let elapsed = now - self.last_time;
		self.last_time = now;
		let capacity =
			u128::from(self.config.capacity.get()) * self.config.refill_period.as_nanos();
		self.credit = self
			.credit
			.saturating_add(elapsed.as_nanos() * u128::from(self.config.refill_tokens.get()))
			.min(capacity);
	}
}

/// The requested token count and current monotonic elapsed time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TakeInput {
	/// The number of whole tokens to consume.
	pub tokens: NonZeroU32,
	/// Elapsed time from the same monotonic origin used at construction.
	pub now: Duration,
}

/// A local admission result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Admission {
	/// The bucket consumed the requested tokens.
	Allowed,
	/// This much time must elapse before this request can fit, assuming no other consumption.
	RetryAfter(Duration),
	/// The request cannot fit even when the bucket is full.
	ExceedsCapacity,
}

/// A configuration that cannot represent a valid refill rate and wait duration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigError {
	/// The refill period is zero.
	ZeroPeriod,
	/// The time to refill a whole bucket exceeds `Duration::MAX`.
	RefillTimeOverflow,
}

impl std::fmt::Display for ConfigError {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str(match self {
			Self::ZeroPeriod => "a token bucket requires a nonzero refill period",
			Self::RefillTimeOverflow => "token bucket refill time exceeds Duration::MAX",
		})
	}
}

impl std::error::Error for ConfigError {}
