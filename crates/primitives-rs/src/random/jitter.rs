//! Cryptographically seeded unit floating-point sampling and symmetric duration jitter.

use std::time::Duration;

use crate::random::secure_bytes::{RandomError, fill_random_bytes};
use crate::time::duration_from_clamped_secs_f64;

/// Configuration for symmetric multiplicative duration jitter in `[1.0 - ratio, 1.0 + ratio]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SymmetricJitter {
	/// Base duration before jitter is applied.
	pub base: Duration,
	/// Non-negative fractional spread around `1.0` (for example `0.2` for `±20%`).
	pub ratio: f64,
	/// Maximum duration after jitter is applied.
	pub max: Duration,
}

/// Samples a uniformly distributed `f64` in `[0.0, 1.0)` with 53 bits of precision from the
/// platform secure random source.
pub fn generate_random_unit_f64() -> Result<f64, RandomError> {
	let mut bytes = [0_u8; 8];
	fill_random_bytes(&mut bytes)?;
	let mantissa = u64::from_le_bytes(bytes) >> 11;
	Ok((mantissa as f64) * (1.0 / ((1_u64 << 53) as f64)))
}

/// Applies symmetric multiplicative jitter in `[1.0 - ratio, 1.0 + ratio]` using a caller-supplied
/// `unit` in `[0.0, 1.0]` and clamps the resulting duration to `[Duration::ZERO, options.max]`.
pub fn apply_symmetric_jitter_with_unit(options: SymmetricJitter, unit: f64) -> Duration {
	if options.base.is_zero() || options.max.is_zero() {
		return Duration::ZERO;
	}
	if options.ratio.is_nan() || options.ratio <= 0.0 {
		return options.base.min(options.max);
	}
	let normalized_unit = if unit.is_nan() {
		0.5
	} else {
		unit.clamp(0.0, 1.0)
	};
	if !options.ratio.is_finite() {
		return if normalized_unit < 0.5 {
			Duration::ZERO
		} else if normalized_unit > 0.5 {
			options.max
		} else {
			options.base.min(options.max)
		};
	}
	let multiplier = 1.0 + options.ratio * ((2.0 * normalized_unit) - 1.0);
	duration_from_clamped_secs_f64(options.base.as_secs_f64() * multiplier, options.max)
}

/// Samples secure unit randomness, applies symmetric multiplicative jitter in
/// `[1.0 - ratio, 1.0 + ratio]`, and clamps the resulting duration to
/// `[Duration::ZERO, options.max]`.
pub fn jitter_duration_symmetric(options: SymmetricJitter) -> Result<Duration, RandomError> {
	let unit = generate_random_unit_f64()?;
	Ok(apply_symmetric_jitter_with_unit(options, unit))
}

#[cfg(test)]
mod tests {
	use super::{SymmetricJitter, apply_symmetric_jitter_with_unit, generate_random_unit_f64};
	use std::time::Duration;

	#[test]
	fn symmetric_jitter_scales_endpoints_and_clamps_max() {
		let options = SymmetricJitter {
			base: Duration::from_millis(1_000),
			ratio: 0.2,
			max: Duration::from_millis(1_100),
		};
		assert_eq!(
			apply_symmetric_jitter_with_unit(options, 0.0),
			Duration::from_millis(800)
		);
		assert_eq!(
			apply_symmetric_jitter_with_unit(options, 0.5),
			Duration::from_millis(1_000)
		);
		assert_eq!(
			apply_symmetric_jitter_with_unit(options, 1.0),
			Duration::from_millis(1_100)
		);
		let inf_options = SymmetricJitter {
			base: Duration::from_millis(1_000),
			ratio: f64::INFINITY,
			max: Duration::from_millis(1_500),
		};
		assert_eq!(
			apply_symmetric_jitter_with_unit(inf_options, 0.5),
			Duration::from_millis(1_000)
		);
		let sample = generate_random_unit_f64().expect("platform randomness");
		assert!((0.0..1.0).contains(&sample));
	}
}
