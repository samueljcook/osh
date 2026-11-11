use primitives_rs::random::jitter::{SymmetricJitter, jitter_duration_symmetric};

use super::{Duration, FLEET_ACQUIRE_RETRY_JITTER_FRACTION, RandomnessError};

fn acquire_retry_jitter(base_delay: Duration) -> SymmetricJitter {
	SymmetricJitter {
		base: base_delay,
		ratio: FLEET_ACQUIRE_RETRY_JITTER_FRACTION,
		max: Duration::MAX,
	}
}

pub(crate) fn generate_jittered_acquire_retry_delay(
	base_delay: Duration,
) -> Result<Duration, RandomnessError> {
	jitter_duration_symmetric(acquire_retry_jitter(base_delay))
		.map_err(RandomnessError::RetryJitter)
}

#[cfg(feature = "__internal")]
#[doc(hidden)]
pub fn apply_acquire_retry_jitter_with_unit(base_delay: Duration, unit: f64) -> Duration {
	primitives_rs::random::jitter::apply_symmetric_jitter_with_unit(
		acquire_retry_jitter(base_delay),
		unit,
	)
}
