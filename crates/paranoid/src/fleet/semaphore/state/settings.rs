//! The stored settings of one Semaphore.

use std::time::Duration;

use crate::fleet::constants::MAX_SETTINGS_DURATION_MICROS;
use crate::fleet::{MAX_SLOT_COUNT, settings_duration_micros};

/// The inputs of a Semaphore's settings.
pub struct SemaphoreSettingsFields {
	/// The number of slots, from 1 to `MAX_SLOT_COUNT`.
	pub capacity: u16,
	/// The maximum time that one acquisition holds its slot. It must be positive and at most
	/// [`MAX_SETTINGS_DURATION`](crate::fleet::MAX_SETTINGS_DURATION). Paranoid stores it in
	/// whole microseconds, rounded up. A hold shorter than an acquisition's database round trip
	/// makes every run fail with [`PgSemaphoreErrorKind::Timeout`] because the hold ends before
	/// the run confirms its slot.
	///
	/// [`PgSemaphoreErrorKind::Timeout`]: crate::fleet::PgSemaphoreErrorKind::Timeout
	pub max_hold_duration: Duration,
}

/// The settings of one Semaphore. Paranoid stores them with the Semaphore, and every handle of
/// the Semaphore must have the same settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemaphoreSettings {
	capacity: u16,
	max_hold_micros: i64,
}

impl SemaphoreSettings {
	/// Checks the settings. It fails when the capacity is outside 1 to
	/// [`MAX_SLOT_COUNT`](crate::fleet::MAX_SLOT_COUNT) or when the hold duration is zero or
	/// longer than [`MAX_SETTINGS_DURATION`](crate::fleet::MAX_SETTINGS_DURATION).
	pub fn new(fields: SemaphoreSettingsFields) -> Result<Self, SemaphoreSettingsError> {
		let max_hold_micros = settings_duration_micros(fields.max_hold_duration)
			.ok_or(SemaphoreSettingsError::MaxHoldDurationInvalid)?;
		Self::from_stored_micros(fields.capacity, max_hold_micros)
	}

	/// Checks stored settings against the capacity and maximum hold duration bounds.
	pub(in crate::fleet) fn from_stored_micros(
		capacity: u16,
		max_hold_micros: i64,
	) -> Result<Self, SemaphoreSettingsError> {
		if !(1..=MAX_SLOT_COUNT).contains(&capacity) {
			return Err(SemaphoreSettingsError::Capacity);
		}
		if !(1..=MAX_SETTINGS_DURATION_MICROS).contains(&max_hold_micros) {
			return Err(SemaphoreSettingsError::MaxHoldDurationInvalid);
		}
		Ok(Self {
			capacity,
			max_hold_micros,
		})
	}

	/// The number of slots.
	pub fn capacity(self) -> u16 {
		self.capacity
	}

	pub(crate) fn max_hold_micros(self) -> i64 {
		self.max_hold_micros
	}
}

/// Invalid Semaphore settings.
#[derive(Debug, thiserror::Error)]
pub enum SemaphoreSettingsError {
	#[error("Semaphore capacity must be between one and its code-owned slot limit")]
	/// The capacity is outside 1 to `MAX_SLOT_COUNT`.
	Capacity,
	#[error("Semaphore maximum hold duration must be positive and at most the settings maximum")]
	/// The hold duration is zero or longer than
	/// [`MAX_SETTINGS_DURATION`](crate::fleet::MAX_SETTINGS_DURATION).
	MaxHoldDurationInvalid,
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::fleet::MAX_SETTINGS_DURATION;

	#[test]
	fn stored_micros_reject_durations_above_the_settings_maximum() {
		assert!(SemaphoreSettings::from_stored_micros(1, MAX_SETTINGS_DURATION_MICROS).is_ok());
		assert!(matches!(
			SemaphoreSettings::from_stored_micros(1, MAX_SETTINGS_DURATION_MICROS + 1),
			Err(SemaphoreSettingsError::MaxHoldDurationInvalid)
		));
		assert!(matches!(
			SemaphoreSettings::from_stored_micros(1, i64::MAX),
			Err(SemaphoreSettingsError::MaxHoldDurationInvalid)
		));
		assert!(matches!(
			SemaphoreSettings::from_stored_micros(1, 0),
			Err(SemaphoreSettingsError::MaxHoldDurationInvalid)
		));
		assert!(
			SemaphoreSettings::new(SemaphoreSettingsFields {
				capacity: 1,
				max_hold_duration: MAX_SETTINGS_DURATION,
			})
			.is_ok()
		);
	}
}
