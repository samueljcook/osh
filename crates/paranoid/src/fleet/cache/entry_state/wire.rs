//! Strict library CacheEntry body; application payload interpretation remains explicit.

use super::{
	CacheEntryCodecError, CacheEntryIdentity, CacheEntryIdentityFields, CoalescingCacheKey,
	NativeCacheEntryState, NativeCacheEntryStateFields,
};
use crate::fleet::payload::state::FleetPayloadWire;
use crate::fleet::scalar_codec::parse_integer;
use primitives_rs::serialization::json::{JsonDecodeFailureSummary, exceeds_max_nesting_depth};
use serde::{Deserialize, Serialize};

const MAX_CACHE_ENTRY_JSON_NESTING_DEPTH: usize = 128;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields, bound(deserialize = "'de: 'a"))]
struct StateWire<'a> {
	cache: String,
	key_parts: Vec<String>,
	epoch: String,
	#[serde(borrow)]
	payload: FleetPayloadWire<'a>,
}
pub(super) fn decode(bytes: &[u8]) -> Result<NativeCacheEntryState, CacheEntryCodecError> {
	if exceeds_max_nesting_depth(bytes, MAX_CACHE_ENTRY_JSON_NESTING_DEPTH) {
		return Err(CacheEntryCodecError::MaxNestingDepth);
	}
	let wire: StateWire<'_> = serde_json::from_slice(bytes).map_err(json_error)?;
	let cache = CoalescingCacheKey::new(&wire.cache).map_err(CacheEntryCodecError::CacheKey)?;
	let identity = CacheEntryIdentity::new(CacheEntryIdentityFields {
		cache,
		key_parts: wire.key_parts,
	})
	.map_err(CacheEntryCodecError::KeyParts)?;
	Ok(NativeCacheEntryState::new(NativeCacheEntryStateFields {
		identity,
		epoch: parse_integer(&wire.epoch).map_err(CacheEntryCodecError::Integer)?,
		payload: wire.payload.into_payload()?,
	}))
}
pub(super) fn encode(state: &NativeCacheEntryState) -> Result<Vec<u8>, CacheEntryCodecError> {
	serde_json::to_vec(&StateWire {
		cache: state.identity.cache().as_str().to_owned(),
		key_parts: state.identity.key_parts().to_vec(),
		epoch: state.epoch.to_string(),
		payload: FleetPayloadWire::from_payload(&state.payload),
	})
	.map_err(json_error)
}
fn json_error(error: serde_json::Error) -> CacheEntryCodecError {
	CacheEntryCodecError::Json {
		summary: JsonDecodeFailureSummary::from(&error),
	}
}
