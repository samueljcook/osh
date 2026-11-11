//! Framed native resource keys without source, scoped role, or mutation authority.

use std::fmt;

use primitives_rs::bytes::append_length_prefixed;

use crate::records::RecordKey;

use super::{SemaphoreKey, ThrottlerKey};

const RESOURCE_DOMAIN: &[u8] = b"paranoid.fleet.resource.v1";
const OUTCOME_DOMAIN: &[u8] = b"paranoid.fleet.throttler.outcome.v1";
const SEMAPHORE_FAMILY: &[u8] = b"semaphore";
const THROTTLER_FAMILY: &[u8] = b"throttler";

/// Parsed names retain their actual primitive family and complete byte identity.
#[derive(Clone, Copy)]
pub(crate) enum NativeFleetResource<'a> {
	Semaphore(&'a SemaphoreKey),
	Throttler(&'a ThrottlerKey),
}
impl<'a> NativeFleetResource<'a> {
	pub(crate) fn record_key(&self) -> RecordKey {
		let mut bytes = Vec::new();
		append_length_prefixed(&mut bytes, RESOURCE_DOMAIN);
		append_length_prefixed(&mut bytes, self.family_tag());
		append_length_prefixed(&mut bytes, self.name_bytes());
		RecordKey::new(bytes)
	}
	/// Derives a private accounting identity distinct from every resource family key.
	pub(crate) fn throttler_outcome_key(
		key: &ThrottlerKey,
		outcome_id: &super::throttler::state::outcome::ThrottlerOutcomeId,
	) -> RecordKey {
		throttler_outcome_key_parts(key, outcome_id.as_bytes())
	}
	pub(super) fn family_tag(&self) -> &'static [u8] {
		match self {
			Self::Semaphore(_) => SEMAPHORE_FAMILY,
			Self::Throttler(_) => THROTTLER_FAMILY,
		}
	}
	pub(super) fn name_bytes(&self) -> &'a [u8] {
		match self {
			Self::Semaphore(key) => key.as_str().as_bytes(),
			Self::Throttler(key) => key.as_str().as_bytes(),
		}
	}
}
fn throttler_outcome_key_parts(key: &ThrottlerKey, outcome_id: &[u8; 32]) -> RecordKey {
	let mut bytes = Vec::new();
	append_length_prefixed(&mut bytes, OUTCOME_DOMAIN);
	append_length_prefixed(&mut bytes, key.as_str().as_bytes());
	append_length_prefixed(&mut bytes, outcome_id);
	RecordKey::new(bytes)
}
impl fmt::Debug for NativeFleetResource<'_> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("NativeFleetResource")
			.field(
				"family",
				&match self {
					Self::Semaphore(_) => "Semaphore",
					Self::Throttler(_) => "Throttler",
				},
			)
			.finish_non_exhaustive()
	}
}
