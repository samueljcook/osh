//! Complete strict native Semaphore payload and lossless private material encoding.

#[cfg(test)]
mod tests;

use primitives_rs::encoding::decimal::parse_canonical_decimal;
use primitives_rs::serialization::json::JsonDecodeFailureSummary;
use serde::{Deserialize, Serialize};

use crate::fleet::private_capability_wire::{SecretText, decode_capability, encode_capability};

use super::{
	HeldSemaphoreSlot, HolderId, NativeSemaphoreState, SLOT_COUNT, SemaphoreCodecError,
	SemaphoreKey, SemaphoreSettings, SemaphoreSlotState, SlotMetadata, StateTimestampMicros,
};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SemaphoreWire {
	key: String,
	settings: SettingsWire,
	slots: Vec<SlotWire>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::fleet) struct SettingsWire {
	capacity: u16,
	max_hold_micros: String,
}

impl SettingsWire {
	pub(in crate::fleet) fn from_settings(settings: SemaphoreSettings) -> Self {
		Self {
			capacity: settings.capacity(),
			max_hold_micros: settings.max_hold_micros().to_string(),
		}
	}
	pub(in crate::fleet) fn into_settings(self) -> Result<SemaphoreSettings, SemaphoreCodecError> {
		SemaphoreSettings::from_stored_micros(self.capacity, integer(&self.max_hold_micros)?)
			.map_err(Into::into)
	}
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum SlotWire {
	Absent {},
	NativePrivate {
		holder: String,
		capability: SecretText,
		expires_at: String,
		updated_at: String,
	},
}

pub(super) fn decode(bytes: &[u8]) -> Result<NativeSemaphoreState, SemaphoreCodecError> {
	let wire: SemaphoreWire = serde_json::from_slice(bytes).map_err(json_error)?;
	let key = SemaphoreKey::new(&wire.key).map_err(|_| invalid("resource key is invalid"))?;
	let settings = wire.settings.into_settings()?;
	if wire.slots.len() != SLOT_COUNT {
		return Err(invalid(
			"resource must contain exactly its complete code-owned slot set",
		));
	}
	let slots = wire
		.slots
		.into_iter()
		.map(|slot| match slot {
			SlotWire::Absent {} => Ok(SemaphoreSlotState::Absent),
			SlotWire::NativePrivate {
				holder,
				capability,
				expires_at,
				updated_at,
			} => {
				let metadata = metadata(&expires_at, &updated_at)?;
				if metadata.expires_at <= metadata.updated_at {
					return Err(invalid(
						"private acquisition must expire after its acquisition timestamp",
					));
				}
				Ok(SemaphoreSlotState::NativePrivate(HeldSemaphoreSlot {
					holder: HolderId::new(&holder)
						.map_err(|_| invalid("private holder is invalid"))?,
					capability: decode_capability(&capability.0)?,
					metadata,
				}))
			}
		})
		.collect::<Result<Vec<_>, SemaphoreCodecError>>()?;
	let slots = slots
		.try_into()
		.map_err(|_| invalid("resource slot count is invalid"))?;
	Ok(NativeSemaphoreState {
		key,
		settings,
		slots,
	})
}

pub(super) fn encode(state: &NativeSemaphoreState) -> Result<Vec<u8>, SemaphoreCodecError> {
	let slots = state
		.slots
		.iter()
		.map(|slot| match slot {
			SemaphoreSlotState::Absent => SlotWire::Absent {},
			SemaphoreSlotState::NativePrivate(held) => SlotWire::NativePrivate {
				holder: held.holder.as_str().to_owned(),
				capability: encode_capability(&held.capability),
				expires_at: held.metadata.expires_at.as_i64().to_string(),
				updated_at: held.metadata.updated_at.as_i64().to_string(),
			},
		})
		.collect();
	serde_json::to_vec(&SemaphoreWire {
		key: state.key.as_str().to_owned(),
		settings: SettingsWire::from_settings(state.settings),
		slots,
	})
	.map_err(json_error)
}

fn metadata(expires_at: &str, updated_at: &str) -> Result<SlotMetadata, SemaphoreCodecError> {
	Ok(SlotMetadata {
		expires_at: StateTimestampMicros::new(integer(expires_at)?),
		updated_at: StateTimestampMicros::new(integer(updated_at)?),
	})
}

fn integer(text: &str) -> Result<i64, SemaphoreCodecError> {
	parse_canonical_decimal(text)
		.map_err(|_| invalid("integer must be a canonical signed 64-bit decimal string"))
}

fn invalid(reason: &'static str) -> SemaphoreCodecError {
	SemaphoreCodecError::Invalid { reason }
}

fn json_error(error: serde_json::Error) -> SemaphoreCodecError {
	SemaphoreCodecError::Json {
		summary: JsonDecodeFailureSummary::from(&error),
	}
}
