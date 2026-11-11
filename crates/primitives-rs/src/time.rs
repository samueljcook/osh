//! Ceiling duration conversions, clamped floating-point conversions, and monotonic deadlines.

use std::error::Error as StdError;
use std::fmt;
use std::time::{Duration, Instant};

/// A duration conversion failure when a duration is zero or overflows the target representation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurationConversionError {
	/// The input duration was zero when a strictly positive duration was required.
	Zero,
	/// The converted duration exceeded the target integer or `Duration` range.
	Overflow,
}

impl fmt::Display for DurationConversionError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Zero => f.write_str("duration must be greater than zero"),
			Self::Overflow => f.write_str("rounded duration overflows target representation"),
		}
	}
}

impl StdError for DurationConversionError {}

/// Rounds a positive `duration` up to whole microseconds as a `u64`.
///
/// Returns [`DurationConversionError::Zero`] when `duration` is zero and
/// [`DurationConversionError::Overflow`] when the rounded microsecond count exceeds `u64::MAX`.
pub fn duration_to_ceil_micros_u64(duration: Duration) -> Result<u64, DurationConversionError> {
	if duration.is_zero() {
		return Err(DurationConversionError::Zero);
	}
	let truncated = duration.as_micros();
	let ceiled = if !duration.subsec_nanos().is_multiple_of(1_000) {
		truncated + 1
	} else {
		truncated
	};
	u64::try_from(ceiled).map_err(|_| DurationConversionError::Overflow)
}

/// Rounds a positive `duration` up to whole microseconds as an `i64`.
///
/// Returns [`DurationConversionError::Zero`] when `duration` is zero and
/// [`DurationConversionError::Overflow`] when the rounded microsecond count exceeds `i64::MAX`.
pub fn duration_to_ceil_micros_i64(duration: Duration) -> Result<i64, DurationConversionError> {
	if duration.is_zero() {
		return Err(DurationConversionError::Zero);
	}
	let truncated = duration.as_micros();
	let ceiled = if !duration.subsec_nanos().is_multiple_of(1_000) {
		truncated + 1
	} else {
		truncated
	};
	i64::try_from(ceiled).map_err(|_| DurationConversionError::Overflow)
}

/// Rounds a positive `duration` up to whole seconds.
///
/// Returns [`DurationConversionError::Zero`] when `duration` is zero and
/// [`DurationConversionError::Overflow`] when rounding up exceeds `u64::MAX` seconds.
pub fn duration_to_ceil_secs(duration: Duration) -> Result<Duration, DurationConversionError> {
	if duration.is_zero() {
		return Err(DurationConversionError::Zero);
	}
	if duration.subsec_nanos() == 0 {
		return Ok(duration);
	}
	duration
		.as_secs()
		.checked_add(1)
		.map(Duration::from_secs)
		.ok_or(DurationConversionError::Overflow)
}

/// Converts a `u128` nanosecond count into a [`Duration`].
///
/// Returns [`DurationConversionError::Overflow`] when `nanos / 1_000_000_000` exceeds `u64::MAX`.
pub fn duration_from_nanos_u128(nanos: u128) -> Result<Duration, DurationConversionError> {
	let secs =
		u64::try_from(nanos / 1_000_000_000).map_err(|_| DurationConversionError::Overflow)?;
	let subsec_nanos = (nanos % 1_000_000_000) as u32;
	Ok(Duration::new(secs, subsec_nanos))
}

/// Converts a floating-point second count into a [`Duration`] clamped to `[Duration::ZERO, max]`.
///
/// Returns [`Duration::ZERO`] when `secs` is `NaN`, zero, or negative, or when `max` is zero.
/// Returns `max` when `secs` is positive infinity or exceeds `max`.
pub fn duration_from_clamped_secs_f64(secs: f64, max: Duration) -> Duration {
	if secs.is_nan() || secs <= 0.0 || max.is_zero() {
		return Duration::ZERO;
	}
	Duration::try_from_secs_f64(secs).unwrap_or(max).min(max)
}

/// Adding a timeout duration to a monotonic [`Instant`] overflowed the platform clock range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeadlineOverflowError;

impl fmt::Display for DeadlineOverflowError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("timeout duration overflows monotonic Instant")
	}
}

impl StdError for DeadlineOverflowError {}

/// A monotonic [`Deadline`] has already passed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeadlineElapsedError;

impl fmt::Display for DeadlineElapsedError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("operation deadline has elapsed")
	}
}

impl StdError for DeadlineElapsedError {}

/// A monotonic point in time by which an operation must finish.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Deadline {
	instant: Instant,
}

impl Deadline {
	/// Wraps an explicit monotonic [`Instant`] as a deadline.
	pub const fn new(instant: Instant) -> Self {
		Self { instant }
	}

	/// Computes a deadline at `Instant::now() + timeout`.
	pub fn new_after(timeout: Duration) -> Result<Self, DeadlineOverflowError> {
		Self::new_from_base(Instant::now(), timeout)
	}

	/// Computes a deadline at `base + timeout`.
	pub fn new_from_base(base: Instant, timeout: Duration) -> Result<Self, DeadlineOverflowError> {
		base.checked_add(timeout)
			.map(Self::new)
			.ok_or(DeadlineOverflowError)
	}

	/// Returns the underlying monotonic [`Instant`].
	pub const fn instant(&self) -> Instant {
		self.instant
	}

	/// Returns the remaining non-zero duration after `Instant::now()`, or `None` once `Instant::now() >= self.instant()`.
	pub fn remaining(&self) -> Option<Duration> {
		self.remaining_at(Instant::now())
	}

	/// Returns the remaining non-zero duration after `now`, or `None` once `now >= self.instant()`.
	pub fn remaining_at(&self, now: Instant) -> Option<Duration> {
		self.instant
			.checked_duration_since(now)
			.filter(|duration| !duration.is_zero())
	}

	/// Returns the remaining non-zero duration after `Instant::now()`, or [`DeadlineElapsedError`] once the deadline has passed.
	pub fn check(&self) -> Result<Duration, DeadlineElapsedError> {
		self.check_at(Instant::now())
	}

	/// Returns the remaining non-zero duration after `now`, or [`DeadlineElapsedError`] once `now >= self.instant()`.
	pub fn check_at(&self, now: Instant) -> Result<Duration, DeadlineElapsedError> {
		self.remaining_at(now).ok_or(DeadlineElapsedError)
	}
}

#[cfg(test)]
mod tests {
	use super::{
		Deadline, DeadlineElapsedError, DurationConversionError, duration_from_clamped_secs_f64,
		duration_from_nanos_u128, duration_to_ceil_micros_i64, duration_to_ceil_micros_u64,
		duration_to_ceil_secs,
	};
	use std::time::{Duration, Instant};

	#[test]
	fn ceiling_conversions_round_up_sub_unit_remainders_and_reject_zero() {
		assert_eq!(
			duration_to_ceil_micros_u64(Duration::ZERO),
			Err(DurationConversionError::Zero)
		);
		assert_eq!(duration_to_ceil_micros_u64(Duration::from_nanos(1)), Ok(1));
		assert_eq!(
			duration_to_ceil_micros_u64(Duration::from_nanos(1_000)),
			Ok(1)
		);
		assert_eq!(
			duration_to_ceil_micros_u64(Duration::from_nanos(1_001)),
			Ok(2)
		);
		assert_eq!(
			duration_to_ceil_micros_i64(Duration::from_nanos(1_001)),
			Ok(2)
		);
		assert_eq!(
			duration_to_ceil_micros_u64(Duration::MAX),
			Err(DurationConversionError::Overflow)
		);
		assert_eq!(
			duration_to_ceil_secs(Duration::from_millis(1)),
			Ok(Duration::from_secs(1))
		);
		assert_eq!(
			duration_to_ceil_secs(Duration::from_secs(3)),
			Ok(Duration::from_secs(3))
		);
	}

	#[test]
	fn nanos_and_clamped_float_seconds_preserve_bounds() {
		assert_eq!(
			duration_from_nanos_u128(1_500_000_000),
			Ok(Duration::new(1, 500_000_000))
		);
		assert_eq!(
			duration_from_nanos_u128(u128::MAX),
			Err(DurationConversionError::Overflow)
		);
		assert_eq!(
			duration_from_clamped_secs_f64(f64::NAN, Duration::from_secs(10)),
			Duration::ZERO
		);
		assert_eq!(
			duration_from_clamped_secs_f64(-1.0, Duration::from_secs(10)),
			Duration::ZERO
		);
		assert_eq!(
			duration_from_clamped_secs_f64(25.0, Duration::from_secs(10)),
			Duration::from_secs(10)
		);
		assert_eq!(
			duration_from_clamped_secs_f64(f64::INFINITY, Duration::MAX),
			Duration::MAX
		);
		let precise_max = Duration::new(1_000_000_000, 50);
		assert_eq!(
			duration_from_clamped_secs_f64(1_000_000_000.0, precise_max),
			Duration::from_secs(1_000_000_000)
		);
	}

	#[test]
	fn deadline_reports_remaining_time_and_elapsed_boundary() {
		let base = Instant::now();
		let deadline =
			Deadline::new_from_base(base, Duration::from_millis(50)).expect("valid timeout");
		assert_eq!(
			deadline.check_at(base + Duration::from_millis(20)),
			Ok(Duration::from_millis(30))
		);
		assert_eq!(
			deadline.check_at(base + Duration::from_millis(50)),
			Err(DeadlineElapsedError)
		);
	}
}
