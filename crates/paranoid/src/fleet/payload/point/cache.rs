//! Cache entry point completions; freshness and computation authority belong to owning callers.

use super::{
	PayloadPointAction, PayloadPointCause, PayloadPointContext, PayloadPointError,
	PayloadPointFamily, RecordIdentity,
};
use crate::fleet::cache::entry_state::{CacheEntryIdentity, NativeCacheEntryState};
use crate::fleet::payload::scope::cache::{BoundCacheEntryScopeReader, CacheEntryScopeReadFields};
use crate::records::PointRecordRead;
use crate::scope::payload::BoundPayloadReader;
use std::convert::Infallible;

pub(crate) struct CachePointReadFields<'a> {
	pub(crate) identity: &'a RecordIdentity,
	pub(crate) cache_entry_identity: &'a CacheEntryIdentity,
	pub(crate) snapshot: &'a PointRecordRead,
}
pub(crate) struct CacheEntryRead {
	pub(crate) state: NativeCacheEntryState,
}
pub(crate) struct CacheValueRead<T> {
	pub(crate) value: T,
}
fn context(action: PayloadPointAction) -> PayloadPointContext {
	PayloadPointContext {
		family: PayloadPointFamily::CacheEntry,
		action,
	}
}
pub(crate) fn observe_cache_entry(
	fields: CachePointReadFields<'_>,
	reader: &BoundCacheEntryScopeReader<'_, '_>,
) -> Result<Option<CacheEntryRead>, PayloadPointError<Infallible>> {
	let Some(record) = fields.snapshot.live_record() else {
		return Ok(None);
	};
	let state = reader
		.decode_record(CacheEntryScopeReadFields {
			identity: fields.identity,
			cache_entry_identity: fields.cache_entry_identity,
			record,
		})
		.map_err(|error| {
			PayloadPointError::new(
				context(PayloadPointAction::ObserveEnvelope),
				PayloadPointCause::Envelope(error),
			)
		})?;
	Ok(Some(CacheEntryRead { state }))
}
pub(super) struct CacheEntryDecodeFields {
	pub(super) entry: CacheEntryRead,
}
pub(super) fn decode_cache_entry<T, C>(
	fields: CacheEntryDecodeFields,
	payload: &BoundPayloadReader<'_, T, C>,
) -> Result<CacheValueRead<T>, PayloadPointError<C>> {
	let value = payload
		.decode(fields.entry.state.payload())
		.map_err(|error| {
			PayloadPointError::new(
				context(PayloadPointAction::ObserveValue),
				PayloadPointCause::Payload(error),
			)
		})?;
	Ok(CacheValueRead { value })
}
