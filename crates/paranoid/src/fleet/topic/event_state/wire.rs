//! Strict library TopicEvent body; application payload interpretation remains explicit.

use super::{
	NativeTopicEventState, NativeTopicEventStateFields, StateTimestampMicros, TopicEventCodecError,
	TopicEventIdentity, TopicEventIdentityFields, TopicKey,
};
use crate::fleet::payload::state::FleetPayloadWire;
use crate::fleet::scalar_codec::parse_integer;
use primitives_rs::serialization::json::{JsonDecodeFailureSummary, exceeds_max_nesting_depth};
use serde::{Deserialize, Serialize};

const MAX_TOPIC_EVENT_JSON_NESTING_DEPTH: usize = 128;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields, bound(deserialize = "'de: 'a"))]
struct StateWire<'a> {
	topic: String,
	sequence: String,
	published_at: String,
	#[serde(borrow)]
	payload: FleetPayloadWire<'a>,
}
pub(super) fn decode(bytes: &[u8]) -> Result<NativeTopicEventState, TopicEventCodecError> {
	if exceeds_max_nesting_depth(bytes, MAX_TOPIC_EVENT_JSON_NESTING_DEPTH) {
		return Err(TopicEventCodecError::MaxNestingDepth);
	}
	let wire: StateWire<'_> = serde_json::from_slice(bytes).map_err(json_error)?;
	let topic = TopicKey::new(&wire.topic).map_err(TopicEventCodecError::TopicKey)?;
	let identity = TopicEventIdentity::new(TopicEventIdentityFields {
		topic,
		sequence: parse_integer(&wire.sequence).map_err(TopicEventCodecError::Integer)?,
	})
	.map_err(TopicEventCodecError::Sequence)?;
	Ok(NativeTopicEventState::new(NativeTopicEventStateFields {
		identity,
		published_at: StateTimestampMicros::new(
			parse_integer(&wire.published_at).map_err(TopicEventCodecError::Integer)?,
		),
		payload: wire.payload.into_payload()?,
	}))
}
pub(super) fn encode(state: &NativeTopicEventState) -> Result<Vec<u8>, TopicEventCodecError> {
	serde_json::to_vec(&StateWire {
		topic: state.identity.topic().as_str().to_owned(),
		sequence: state.identity.sequence().to_string(),
		published_at: state.published_at.as_i64().to_string(),
		payload: FleetPayloadWire::from_payload(&state.payload),
	})
	.map_err(json_error)
}
fn json_error(error: serde_json::Error) -> TopicEventCodecError {
	TopicEventCodecError::Json {
		summary: JsonDecodeFailureSummary::from(&error),
	}
}
