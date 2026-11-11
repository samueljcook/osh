//! Strict private per-acquisition anchor payload, without grant reconstruction.

use primitives_rs::serialization::json::JsonDecodeFailureSummary;
use serde::{Deserialize, Serialize};

use super::{NativeThrottlerOutcomeAnchor, ThrottlerOutcomeCodecError, ThrottlerOutcomeId};
use crate::fleet::private_capability_wire::{SecretText, decode_capability, encode_capability};
use crate::fleet::{HolderId, ThrottlerKey};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AnchorWire {
	key: String,
	outcome_id: [u8; 32],
	holder: String,
	capability: SecretText,
	epoch: SecretText,
}

pub(super) fn decode(
	bytes: &[u8],
) -> Result<NativeThrottlerOutcomeAnchor, ThrottlerOutcomeCodecError> {
	let wire: AnchorWire = serde_json::from_slice(bytes).map_err(json_error)?;
	Ok(NativeThrottlerOutcomeAnchor {
		key: ThrottlerKey::new(&wire.key)
			.map_err(|_| ThrottlerOutcomeCodecError::InvalidResource)?,
		outcome_id: ThrottlerOutcomeId(wire.outcome_id),
		holder: HolderId::new(&wire.holder)
			.map_err(|_| ThrottlerOutcomeCodecError::InvalidHolder)?,
		capability: decode_capability(&wire.capability.0)?,
		epoch: decode_capability(&wire.epoch.0)?,
	})
}

pub(super) fn encode(
	anchor: &NativeThrottlerOutcomeAnchor,
) -> Result<Vec<u8>, ThrottlerOutcomeCodecError> {
	serde_json::to_vec(&AnchorWire {
		key: anchor.key.as_str().to_owned(),
		outcome_id: anchor.outcome_id.0,
		holder: anchor.holder.as_str().to_owned(),
		capability: encode_capability(&anchor.capability),
		epoch: encode_capability(&anchor.epoch),
	})
	.map_err(json_error)
}

fn json_error(error: serde_json::Error) -> ThrottlerOutcomeCodecError {
	ThrottlerOutcomeCodecError::Json {
		summary: JsonDecodeFailureSummary::from(&error),
	}
}
