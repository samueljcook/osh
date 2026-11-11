use super::{
	AtomicLiveMutationCurrent, AtomicMutationCurrent, Duration, MIN_TTL, StateTimestampMicros, Ttl,
	TtlError, duration_to_rounded_micros,
};

#[derive(Debug, thiserror::Error)]
#[error("TTL expiration exceeds the signed timestamp domain")]
pub(crate) struct ExpirationOverflowError;

impl<'a> AtomicMutationCurrent<'a> {
	/// Returns the live value observed while the key lock is held.
	pub fn live_value(&self) -> Option<&'a [u8]> {
		self.live_value
	}

	/// Reports whether a live value exists while the key lock is held.
	pub fn has_live_value(&self) -> bool {
		self.live_value.is_some()
	}

	/// Returns the database timestamp observed after the key lock was acquired.
	pub fn observed_at(&self) -> StateTimestampMicros {
		self.observed_at
	}
}

impl<'a> AtomicLiveMutationCurrent<'a> {
	/// Returns the live value observed while the key lock is held.
	pub fn live_value(&self) -> &'a [u8] {
		self.live_value
	}

	/// Returns the database timestamp observed after the key lock was acquired.
	pub fn observed_at(&self) -> StateTimestampMicros {
		self.observed_at
	}
}

impl Ttl {
	pub(crate) fn expiration_at(
		self,
		observed_at: crate::records::StateTimestampMicros,
	) -> Result<Option<crate::records::StateTimestampMicros>, ExpirationOverflowError> {
		self.positive_micros()
			.map(|duration| {
				observed_at
					.as_i64()
					.checked_add(duration)
					.map(crate::records::StateTimestampMicros::new)
					.ok_or(ExpirationOverflowError)
			})
			.transpose()
	}
	/// TTL value for non-expiring rows.
	pub const NO_EXPIRATION: Self = Self {
		positive_duration: None,
	};

	/// Validates a positive TTL.
	pub fn expires_after(duration: Duration) -> Result<Self, TtlError> {
		if duration.is_zero() {
			return Err(TtlError::Zero);
		}
		if duration < MIN_TTL {
			return Err(TtlError::BelowMinimum { minimum: MIN_TTL });
		}
		duration_to_rounded_micros(duration)?;
		Ok(Self {
			positive_duration: Some(duration),
		})
	}

	/// Returns this TTL as positive microseconds, or `None` when it does not expire.
	#[cfg(not(feature = "__internal"))]
	pub(crate) fn positive_micros(self) -> Option<i64> {
		self.positive_micros_inner()
	}

	/// Returns this TTL as positive microseconds, or `None` when it does not expire.
	#[cfg(feature = "__internal")]
	#[doc(hidden)]
	pub fn positive_micros(self) -> Option<i64> {
		self.positive_micros_inner()
	}

	fn positive_micros_inner(self) -> Option<i64> {
		let duration = self.positive_duration?;
		Some(
			primitives_rs::time::duration_to_ceil_micros_i64(duration)
				.expect("Ttl::expires_after admits only durations that fit in microseconds"),
		)
	}
}
