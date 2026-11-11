use serde::{Deserialize, Serialize};
use std::time::Duration;

/// A token-bucket rate limit for a Fleet throttler or rate limiter.
///
/// The bucket holds at most `capacity` tokens, starts full, and gains `tokens_per_interval`
/// tokens over each `interval`. Each permitted operation consumes the handle's token cost, which
/// is one unless
/// [`PgThrottler::with_rate_limit_cost`](crate::fleet::PgThrottler::with_rate_limit_cost) sets
/// another cost. The capacity is the largest burst, and the refill is the sustained rate.
///
/// Releasing a permit never returns its token, whatever the task outcome, so a permitted
/// operation that does not run still uses its token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RateLimit {
	/// Largest number of tokens that the bucket holds. It must be positive.
	pub capacity: u32,
	/// Number of tokens that the bucket gains over each interval. It must be positive.
	pub tokens_per_interval: u32,
	/// Interval over which the bucket gains `tokens_per_interval` tokens. It must be positive.
	pub interval: Duration,
}

/// Task outcome to apply when releasing a Fleet throttler permit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThrottlerTaskOutcome {
	/// The protected task did not execute.
	NotExecuted,
	/// The protected task executed successfully.
	Succeeded,
	/// The protected task executed and should count as a circuit-breaker failure.
	Failed,
}

/// State of a Fleet circuit, shared by the throttler and the circuit breaker.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CircuitState {
	/// The circuit is closed.
	Closed,
	/// The circuit is open.
	Open,
}

/// Current Fleet throttler status.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ThrottlerStatus {
	pub(super) available_tokens: f64,
	pub(super) rate_capacity: f64,
	pub(super) current_concurrency: u16,
	pub(super) concurrency_capacity: u16,
	pub(super) circuit_state: CircuitState,
	pub(super) consecutive_failures: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedThrottlerRateLimit {
	pub(super) capacity: u32,
	pub(super) tokens_per_interval: u32,
	pub(super) interval: Duration,
}

impl ThrottlerStatus {
	/// Returns available rate-limit tokens.
	pub fn available_tokens(&self) -> f64 {
		self.available_tokens
	}

	/// Returns maximum rate-limit tokens.
	pub fn rate_capacity(&self) -> f64 {
		self.rate_capacity
	}

	/// Returns the current number of live concurrency slots.
	pub fn current_concurrency(&self) -> u16 {
		self.current_concurrency
	}

	/// Returns the maximum configured concurrency.
	pub fn concurrency_capacity(&self) -> u16 {
		self.concurrency_capacity
	}

	/// Returns the circuit state.
	pub fn circuit_state(&self) -> CircuitState {
		self.circuit_state
	}

	/// Returns the current consecutive failure count.
	pub fn consecutive_failures(&self) -> u32 {
		self.consecutive_failures
	}
}

impl ResolvedThrottlerRateLimit {
	pub(crate) fn capacity(self) -> u32 {
		self.capacity
	}
	pub(super) fn refill_rate_per_sec(self) -> f64 {
		f64::from(self.tokens_per_interval) / self.interval.as_secs_f64()
	}
}
