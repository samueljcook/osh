use std::time::Duration;

use super::TtlError;

pub(super) fn duration_to_rounded_micros(duration: Duration) -> Result<i64, TtlError> {
	primitives_rs::time::duration_to_ceil_micros_i64(duration).map_err(|error| match error {
		primitives_rs::time::DurationConversionError::Zero => TtlError::Zero,
		primitives_rs::time::DurationConversionError::Overflow => TtlError::TooLarge,
	})
}
