//! Closed framed payload keys derived from actual CacheEntry and TopicEvent identities.

use primitives_rs::bytes::append_length_prefixed;

use crate::records::RecordKey;

use crate::fleet::cache::entry_state::CacheEntryIdentity;
use crate::fleet::topic::event_state::TopicEventIdentity;

pub(crate) enum NativeFleetPayloadIdentity<'a> {
	CacheEntry(&'a CacheEntryIdentity),
	TopicEvent(&'a TopicEventIdentity),
}
impl NativeFleetPayloadIdentity<'_> {
	pub(crate) fn record_key(&self) -> RecordKey {
		let mut bytes = Vec::new();
		append_length_prefixed(&mut bytes, b"paranoid.fleet.payload.v1");
		match self {
			Self::CacheEntry(identity) => {
				append_length_prefixed(&mut bytes, b"cache-entry");
				append_length_prefixed(&mut bytes, identity.cache().as_str().as_bytes());
				let count = u64::try_from(identity.key_parts().len())
					.expect("supported platform collection lengths fit u64");
				append_length_prefixed(&mut bytes, &count.to_be_bytes());
				for key_part in identity.key_parts() {
					append_length_prefixed(&mut bytes, key_part.as_bytes());
				}
			}
			Self::TopicEvent(identity) => {
				append_length_prefixed(&mut bytes, b"topic-event");
				append_length_prefixed(&mut bytes, identity.topic().as_str().as_bytes());
				append_length_prefixed(&mut bytes, &identity.sequence().to_be_bytes());
			}
		}
		RecordKey::new(bytes)
	}
}
