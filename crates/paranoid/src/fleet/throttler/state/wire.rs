//! Strict complete native Throttler payload with exact float, duration, and secret encoding.

#[cfg(test)]
mod tests;

use std::time::Duration;

use primitives_rs::encoding::decimal::parse_canonical_decimal;
use primitives_rs::serialization::json::{JsonDecodeFailureSummary, deserialize_required_option};
use primitives_rs::time::duration_from_nanos_u128;
use serde::{Deserialize, Serialize};

use super::{
	CircuitActivity, CircuitOwnershipView, CircuitState, ConcurrencySlot, HolderId, MAX_SLOT_COUNT,
	NativeThrottlerState, PrivateProbe, StateTimestampMicros, ThrottlerActivity,
	ThrottlerCircuitSettingsFields, ThrottlerCodecError, ThrottlerConcurrencySettingsFields,
	ThrottlerKey, ThrottlerSettings, ThrottlerSettingsFields,
};
use crate::fleet::RateLimit;
use crate::fleet::private_capability_wire::{SecretText, decode_capability, encode_capability};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StateWire {
	key: String,
	settings: SettingsWire,
	activity: ActivityWire,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::fleet) struct SettingsWire {
	#[serde(deserialize_with = "deserialize_required_option")]
	rate: Option<RateWire>,
	#[serde(deserialize_with = "deserialize_required_option")]
	concurrency: Option<ConcurrencyWire>,
	#[serde(deserialize_with = "deserialize_required_option")]
	circuit: Option<CircuitSettingsWire>,
}
impl SettingsWire {
	pub(in crate::fleet) fn from_settings(settings: ThrottlerSettings) -> Self {
		SettingsWire {
			rate: settings.rate().map(|rate| RateWire {
				capacity: rate.capacity,
				tokens_per_interval: rate.tokens_per_interval,
				interval_nanos: rate.interval.as_nanos().to_string(),
			}),
			concurrency: settings.concurrency().map(|control| ConcurrencyWire {
				capacity: control.capacity(),
				max_hold_nanos: control.max_hold_duration().as_nanos().to_string(),
			}),
			circuit: settings.circuit().map(|control| CircuitSettingsWire {
				failure_threshold: control.failure_threshold(),
				recovery_timeout_nanos: control.recovery_timeout().as_nanos().to_string(),
				probe_window_nanos: control.probe_window().as_nanos().to_string(),
			}),
		}
	}
	pub(in crate::fleet) fn into_settings(self) -> Result<ThrottlerSettings, ThrottlerCodecError> {
		ThrottlerSettings::from_stored(ThrottlerSettingsFields {
			rate: self
				.rate
				.map(|rate| {
					Ok::<_, ThrottlerCodecError>(RateLimit {
						capacity: rate.capacity,
						tokens_per_interval: rate.tokens_per_interval,
						interval: duration(&rate.interval_nanos)?,
					})
				})
				.transpose()?,
			concurrency: self
				.concurrency
				.map(|control| {
					Ok::<_, ThrottlerCodecError>(ThrottlerConcurrencySettingsFields {
						capacity: control.capacity,
						max_hold_duration: duration(&control.max_hold_nanos)?,
					})
				})
				.transpose()?,
			circuit: self
				.circuit
				.map(|control| {
					Ok::<_, ThrottlerCodecError>(ThrottlerCircuitSettingsFields {
						failure_threshold: control.failure_threshold,
						recovery_timeout: duration(&control.recovery_timeout_nanos)?,
						probe_window: duration(&control.probe_window_nanos)?,
					})
				})
				.transpose()?,
		})
		.map_err(Into::into)
	}
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RateWire {
	capacity: u32,
	tokens_per_interval: u32,
	interval_nanos: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ConcurrencyWire {
	capacity: u16,
	max_hold_nanos: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CircuitSettingsWire {
	failure_threshold: u32,
	recovery_timeout_nanos: String,
	probe_window_nanos: String,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ActivityWire {
	Absent {},
	Present(Box<PresentActivityWire>),
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PresentActivityWire {
	expires_at: String,
	updated_at: String,
	tokens_bits: String,
	last_refill: String,
	slots: Vec<SlotWire>,
	failures: u32,
	circuit: CircuitWire,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SlotWire {
	number: u16,
	expires_at: String,
	ownership: SlotOwnershipWire,
}
#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum SlotOwnershipWire {
	NativePrivate {
		holder: String,
		capability: SecretText,
	},
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CircuitWire {
	state: CircuitState,
	#[serde(deserialize_with = "deserialize_required_option")]
	opened_at: Option<String>,
	ownership: CircuitOwnershipWire,
}
#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum CircuitOwnershipWire {
	Uninitialized {},
	Native {
		epoch: SecretText,
		#[serde(deserialize_with = "deserialize_required_option")]
		probe: Option<ProbeWire>,
	},
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProbeWire {
	holder: String,
	capability: SecretText,
	expires_at: String,
}

pub(super) fn decode(bytes: &[u8]) -> Result<NativeThrottlerState, ThrottlerCodecError> {
	let wire: StateWire = serde_json::from_slice(bytes).map_err(json_error)?;
	let key = ThrottlerKey::new(&wire.key).map_err(|_| invalid("resource key is invalid"))?;
	let settings = wire.settings.into_settings()?;
	let activity = match wire.activity {
		ActivityWire::Absent {} => None,
		ActivityWire::Present(activity) => {
			let PresentActivityWire {
				expires_at,
				updated_at,
				tokens_bits,
				last_refill,
				slots,
				failures,
				circuit,
			} = *activity;
			if slots.len() > usize::from(MAX_SLOT_COUNT) {
				return Err(invalid("slot count exceeds its code-owned domain"));
			}
			let mut parsed_slots = std::collections::BTreeMap::new();
			for slot in slots {
				if !(1..=MAX_SLOT_COUNT).contains(&slot.number) {
					return Err(invalid("slot number is outside its code-owned domain"));
				}
				let expires_at = timestamp(&slot.expires_at)?;
				let SlotOwnershipWire::NativePrivate { holder, capability } = slot.ownership;
				let value = ConcurrencySlot {
					holder: HolderId::new(&holder)
						.map_err(|_| invalid("private slot holder is invalid"))?,
					capability: decode_capability(&capability.0)?,
					expires_at,
				};
				if parsed_slots.insert(slot.number, value).is_some() {
					return Err(invalid("slot collection contains a duplicate number"));
				}
			}
			let opening = circuit.opened_at.as_deref().map(timestamp).transpose()?;
			let circuit = match circuit.ownership {
				CircuitOwnershipWire::Uninitialized {} => {
					CircuitActivity::uninitialized(circuit.state, opening)?
				}
				CircuitOwnershipWire::Native { epoch, probe } => CircuitActivity::native(
					circuit.state,
					opening,
					decode_capability(&epoch.0)?,
					probe
						.map(|probe| {
							Ok::<_, ThrottlerCodecError>(PrivateProbe {
								holder: HolderId::new(&probe.holder)
									.map_err(|_| invalid("private probe holder is invalid"))?,
								capability: decode_capability(&probe.capability.0)?,
								expires_at: timestamp(&probe.expires_at)?,
							})
						})
						.transpose()?,
				)?,
			};
			Some(ThrottlerActivity {
				expires_at: timestamp(&expires_at)?,
				updated_at: timestamp(&updated_at)?,
				tokens: balance(&tokens_bits)?,
				last_refill: timestamp(&last_refill)?,
				slots: parsed_slots,
				failures,
				circuit,
			})
		}
	};
	Ok(NativeThrottlerState {
		key,
		settings,
		activity,
	})
}

pub(super) fn encode(state: &NativeThrottlerState) -> Result<Vec<u8>, ThrottlerCodecError> {
	let settings = SettingsWire::from_settings(state.settings);
	let activity = match &state.activity {
		None => ActivityWire::Absent {},
		Some(activity) => {
			if !activity.tokens.is_finite() || activity.tokens < 0.0 {
				return Err(invalid("balance must be finite and nonnegative"));
			}
			let slots = activity
				.slots
				.iter()
				.map(|(number, slot)| SlotWire {
					number: *number,
					expires_at: slot.expiry().as_i64().to_string(),
					ownership: SlotOwnershipWire::NativePrivate {
						holder: slot.holder.as_str().to_owned(),
						capability: encode_capability(&slot.capability),
					},
				})
				.collect();
			let ownership = match activity.circuit.ownership() {
				CircuitOwnershipView::Uninitialized => CircuitOwnershipWire::Uninitialized {},
				CircuitOwnershipView::NativeIdle(epoch) => CircuitOwnershipWire::Native {
					epoch: encode_capability(epoch),
					probe: None,
				},
				CircuitOwnershipView::NativeProbe { epoch, probe } => {
					CircuitOwnershipWire::Native {
						epoch: encode_capability(epoch),
						probe: Some(ProbeWire {
							holder: probe.holder.as_str().to_owned(),
							capability: encode_capability(&probe.capability),
							expires_at: probe.expires_at.as_i64().to_string(),
						}),
					}
				}
			};
			ActivityWire::Present(Box::new(PresentActivityWire {
				expires_at: activity.expires_at.as_i64().to_string(),
				updated_at: activity.updated_at.as_i64().to_string(),
				tokens_bits: format!("{:016x}", activity.tokens.to_bits()),
				last_refill: activity.last_refill.as_i64().to_string(),
				slots,
				failures: activity.failures,
				circuit: CircuitWire {
					state: activity.circuit.state(),
					opened_at: activity
						.circuit
						.opening()
						.map(|time| time.as_i64().to_string()),
					ownership,
				},
			}))
		}
	};
	serde_json::to_vec(&StateWire {
		key: state.key.as_str().to_owned(),
		settings,
		activity,
	})
	.map_err(json_error)
}

fn balance(text: &str) -> Result<f64, ThrottlerCodecError> {
	if text.len() != 16
		|| !text
			.bytes()
			.all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
	{
		return Err(invalid(
			"balance bits must use exactly 16 lowercase hexadecimal characters",
		));
	}
	let bits =
		u64::from_str_radix(text, 16).map_err(|_| invalid("balance bit pattern is invalid"))?;
	let value = f64::from_bits(bits);
	if !value.is_finite() || value < 0.0 {
		return Err(invalid("balance must be finite and nonnegative"));
	}
	Ok(value)
}
fn duration(text: &str) -> Result<Duration, ThrottlerCodecError> {
	let nanos: u128 = parse_canonical_decimal(text)
		.map_err(|_| invalid("duration must use canonical decimal nanoseconds"))?;
	duration_from_nanos_u128(nanos)
		.map_err(|_| invalid("duration exceeds the standard duration domain"))
}
fn timestamp(text: &str) -> Result<StateTimestampMicros, ThrottlerCodecError> {
	let value: i64 = parse_canonical_decimal(text)
		.map_err(|_| invalid("timestamp must use canonical signed 64-bit decimal text"))?;
	Ok(StateTimestampMicros::new(value))
}
fn invalid(reason: &'static str) -> ThrottlerCodecError {
	ThrottlerCodecError::Invalid { reason }
}
fn json_error(error: serde_json::Error) -> ThrottlerCodecError {
	ThrottlerCodecError::Json {
		summary: JsonDecodeFailureSummary::from(&error),
	}
}
