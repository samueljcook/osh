//! The stored settings of one Throttler.

use super::super::arithmetic::semantic_settings::{
	self, ThrottlerCircuitSettings, ThrottlerConcurrencySettings,
};
use crate::fleet::throttler::model::ResolvedThrottlerRateLimit;
use crate::fleet::{
	RateLimit, ThrottlerCircuitSettingsError, ThrottlerConcurrencySettingsError,
	ThrottlerRateSettingsError,
};

pub use super::super::arithmetic::semantic_settings::{
	ThrottlerCircuitSettingsFields, ThrottlerConcurrencySettingsFields,
};

/// The inputs of a Throttler's settings. At least one of the three controls must be present.
pub struct ThrottlerSettingsFields {
	/// The rate limit, or `None` for no rate limit. The time that an empty bucket takes to refill
	/// to its capacity, `interval × capacity / tokens_per_interval`, must be at most
	/// [`MAX_SETTINGS_DURATION`](crate::fleet::MAX_SETTINGS_DURATION).
	pub rate: Option<RateLimit>,
	/// The concurrency limit, or `None` for no concurrency limit.
	pub concurrency: Option<ThrottlerConcurrencySettingsFields>,
	/// The circuit, or `None` for no circuit.
	pub circuit: Option<ThrottlerCircuitSettingsFields>,
}

/// The settings of one Throttler. Paranoid stores them with the Throttler, and every handle of
/// the Throttler must have the same settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ThrottlerSettings {
	rate: Option<ResolvedThrottlerRateLimit>,
	concurrency: Option<ThrottlerConcurrencySettings>,
	circuit: Option<ThrottlerCircuitSettings>,
}
impl ThrottlerSettings {
	/// Checks each present control. It fails when a control is invalid or when no control is
	/// present.
	pub fn new(fields: ThrottlerSettingsFields) -> Result<Self, ThrottlerSettingsError> {
		Self::from_stored(fields)
	}

	/// Checks stored settings against every control's bounds, including the maximum settings
	/// duration.
	pub(in crate::fleet) fn from_stored(
		fields: ThrottlerSettingsFields,
	) -> Result<Self, ThrottlerSettingsError> {
		let settings = Self {
			rate: fields
				.rate
				.map(semantic_settings::parse_rate)
				.transpose()
				.map_err(ThrottlerSettingsError::Rate)?,
			concurrency: fields
				.concurrency
				.map(ThrottlerConcurrencySettings::new)
				.transpose()
				.map_err(ThrottlerSettingsError::Concurrency)?,
			circuit: fields
				.circuit
				.map(ThrottlerCircuitSettings::new)
				.transpose()
				.map_err(ThrottlerSettingsError::Circuit)?,
		};
		if settings.rate.is_none() && settings.concurrency.is_none() && settings.circuit.is_none() {
			return Err(ThrottlerSettingsError::NoControls);
		}
		if settings.rate.is_some_and(|rate| {
			crate::fleet::settings_duration_micros(semantic_settings::full_refill_duration(rate))
				.is_none()
		}) {
			return Err(ThrottlerSettingsError::Rate(
				ThrottlerRateSettingsError::IntervalInvalid,
			));
		}
		if settings.concurrency.is_some_and(|control| {
			crate::fleet::settings_duration_micros(control.max_hold_duration()).is_none()
		}) {
			return Err(ThrottlerSettingsError::Concurrency(
				ThrottlerConcurrencySettingsError::MaxHoldDurationInvalid,
			));
		}
		if let Some(control) = settings.circuit {
			if crate::fleet::settings_duration_micros(control.recovery_timeout()).is_none() {
				return Err(ThrottlerSettingsError::Circuit(
					ThrottlerCircuitSettingsError::RecoveryTimeoutInvalid,
				));
			}
			if crate::fleet::settings_duration_micros(control.probe_window()).is_none() {
				return Err(ThrottlerSettingsError::Circuit(
					ThrottlerCircuitSettingsError::ProbeWindowInvalid,
				));
			}
		}
		Ok(settings)
	}
	pub(crate) fn rate(self) -> Option<ResolvedThrottlerRateLimit> {
		self.rate
	}
	pub(crate) fn concurrency(self) -> Option<ThrottlerConcurrencySettings> {
		self.concurrency
	}
	pub(crate) fn circuit(self) -> Option<ThrottlerCircuitSettings> {
		self.circuit
	}
	pub(crate) fn activity_ttl_micros(self) -> i64 {
		semantic_settings::activity_ttl(self.rate, self.concurrency, self.circuit)
			.positive_micros()
			.expect("Throttler activity has a positive parsed lifetime")
	}
}

/// Invalid Throttler settings.
#[derive(Debug, thiserror::Error)]
pub enum ThrottlerSettingsError {
	#[error("Throttler settings must have at least one control")]
	/// The settings have no rate limit, no concurrency limit, and no circuit.
	NoControls,
	#[error("Throttler rate settings are invalid")]
	/// The rate limit is invalid.
	Rate(#[source] ThrottlerRateSettingsError),
	#[error("Throttler concurrency settings are invalid")]
	/// The concurrency limit is invalid.
	Concurrency(#[source] ThrottlerConcurrencySettingsError),
	#[error("Throttler circuit settings are invalid")]
	/// The circuit settings are invalid.
	Circuit(#[source] ThrottlerCircuitSettingsError),
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use super::*;
	use crate::fleet::MAX_SETTINGS_DURATION;

	#[test]
	fn from_stored_rejects_durations_above_the_settings_maximum() {
		let above_max = MAX_SETTINGS_DURATION + Duration::from_micros(1);

		assert!(
			ThrottlerSettings::from_stored(ThrottlerSettingsFields {
				rate: Some(RateLimit {
					tokens_per_interval: 1,
					interval: MAX_SETTINGS_DURATION,
					capacity: 1,
				}),
				concurrency: Some(ThrottlerConcurrencySettingsFields {
					capacity: 1,
					max_hold_duration: MAX_SETTINGS_DURATION,
				}),
				circuit: Some(ThrottlerCircuitSettingsFields {
					failure_threshold: 1,
					recovery_timeout: MAX_SETTINGS_DURATION,
					probe_window: MAX_SETTINGS_DURATION,
				}),
			})
			.is_ok()
		);

		assert!(matches!(
			ThrottlerSettings::from_stored(ThrottlerSettingsFields {
				rate: Some(RateLimit {
					tokens_per_interval: 1,
					interval: above_max,
					capacity: 1,
				}),
				concurrency: None,
				circuit: None,
			}),
			Err(ThrottlerSettingsError::Rate(
				ThrottlerRateSettingsError::IntervalInvalid
			))
		));

		assert!(matches!(
			ThrottlerSettings::from_stored(ThrottlerSettingsFields {
				rate: None,
				concurrency: Some(ThrottlerConcurrencySettingsFields {
					capacity: 1,
					max_hold_duration: above_max,
				}),
				circuit: None,
			}),
			Err(ThrottlerSettingsError::Concurrency(
				ThrottlerConcurrencySettingsError::MaxHoldDurationInvalid
			))
		));

		assert!(matches!(
			ThrottlerSettings::from_stored(ThrottlerSettingsFields {
				rate: None,
				concurrency: None,
				circuit: Some(ThrottlerCircuitSettingsFields {
					failure_threshold: 1,
					recovery_timeout: above_max,
					probe_window: Duration::from_secs(1),
				}),
			}),
			Err(ThrottlerSettingsError::Circuit(
				ThrottlerCircuitSettingsError::RecoveryTimeoutInvalid
			))
		));

		assert!(matches!(
			ThrottlerSettings::from_stored(ThrottlerSettingsFields {
				rate: None,
				concurrency: None,
				circuit: Some(ThrottlerCircuitSettingsFields {
					failure_threshold: 1,
					recovery_timeout: Duration::from_secs(1),
					probe_window: above_max,
				}),
			}),
			Err(ThrottlerSettingsError::Circuit(
				ThrottlerCircuitSettingsError::ProbeWindowInvalid
			))
		));
	}
}
