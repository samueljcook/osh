//! Actual Cache construction, signed-epoch freshness and caller-owned computation.

pub(crate) mod compute;
pub(crate) mod options;
pub(crate) mod publication_identity;

use crate::db::{Database, WriteDatabase};
use crate::fleet::CoalescingCacheKey;
use crate::fleet::cache::entry_state::{CacheEntryIdentity, CacheEntryIdentityFields};
use crate::fleet::cache::epoch::{NativeCacheEpochError, PgNativeCacheEpoch};
use crate::fleet::lease::NativeLeaseBindings;
use crate::fleet::lease::{CoordinationKeyError, LeaseKey};
use crate::fleet::payload::point::cache::CacheValueRead;
use crate::fleet::payload::point::cache_handle::PgNativeCacheEntry;
use crate::fleet::payload::point::factory::NativePayloadConstructionError;
use crate::fleet::payload::point::read::NativePayloadReadError;
use crate::fleet::payload::scope::cache::CacheEntryScopeDeclaration;
use crate::scope::payload::PayloadSchemaDeclaration;
use options::NativeCacheOptions;
use primitives_rs::bytes::append_length_prefixed;
use primitives_rs::encoding::binary_to_text::ByteEncoding;
use std::{fmt, sync::Arc};

/// Runtime construction receives actual declarations and parsed local choices.
pub(crate) struct NativeCacheConstructionFields<'scope, 'code> {
	pub(crate) key: CoalescingCacheKey,
	pub(crate) declaration: CacheEntryScopeDeclaration<'scope, 'code>,
	pub(crate) options: NativeCacheOptions,
}
pub(crate) struct NativeCacheFields<'scope, 'code> {
	pub(crate) key: CoalescingCacheKey,
	pub(crate) entry: PgNativeCacheEntry<'scope, 'code>,
	pub(crate) epoch: PgNativeCacheEpoch,
	pub(crate) leases: Arc<NativeLeaseBindings>,
	pub(crate) options: NativeCacheOptions,
}
pub(crate) struct NativeCacheFreshReadFields<'a, 'scope, 'code, T, C> {
	pub(crate) cache_entry_identity: &'a CacheEntryIdentity,
	pub(crate) payload: &'a PayloadSchemaDeclaration<'scope, 'code, T, C>,
}
pub(crate) struct PgNativeCache<'scope, 'code> {
	key: CoalescingCacheKey,
	entry: PgNativeCacheEntry<'scope, 'code>,
	epoch: PgNativeCacheEpoch,
	leases: Arc<NativeLeaseBindings>,
	options: NativeCacheOptions,
}
impl<'scope, 'code> PgNativeCache<'scope, 'code> {
	pub(crate) fn new(
		fields: NativeCacheFields<'scope, 'code>,
	) -> Result<Self, NativeCacheConstructionError> {
		if fields.epoch.key() != &fields.key
			|| !fields.entry.shares_admission(&fields.leases.admission)
			|| !fields.epoch.shares_admission(&fields.leases.admission)
		{
			return Err(NativeCacheConstructionError::new(
				NativeCacheConstructionCause::Association,
			));
		}
		let cache = Self {
			key: fields.key,
			entry: fields.entry,
			epoch: fields.epoch,
			leases: fields.leases,
			options: fields.options,
		};
		let initial = CacheEntryIdentity::new(CacheEntryIdentityFields {
			cache: cache.key.clone(),
			key_parts: Vec::new(),
		})
		.expect("an empty validated Cache suffix tuple is valid");
		cache.mutex_key(&initial).map_err(|error| {
			NativeCacheConstructionError::new(NativeCacheConstructionCause::MutexKey(error))
		})?;
		Ok(cache)
	}
	pub(in crate::fleet) fn entry(&self) -> &PgNativeCacheEntry<'scope, 'code> {
		&self.entry
	}
	pub(crate) fn key(&self) -> &CoalescingCacheKey {
		&self.key
	}
	/// The wait after a computation for one complete release attempt of an entry's lease.
	pub(crate) fn close_wait(
		&self,
	) -> crate::fleet::lease::owned_driver::close::NativeLeaseCloseWait {
		crate::fleet::lease::owned_driver::close::NativeLeaseCloseWait::one_release_attempt(
			&self.leases.timing,
		)
	}

	/// The key of the lease that computes one value. v10 deployments never take it, so it is
	/// derived from the scope and the value's record key on every installation, and it cannot
	/// equal the key of a lease that data retirement moves from v10.
	pub(crate) fn mutex_key(
		&self,
		cache_entry_identity: &CacheEntryIdentity,
	) -> Result<LeaseKey, NativeCacheIdentityError> {
		if cache_entry_identity.cache() != &self.key {
			return Err(NativeCacheIdentityError::DifferentCache);
		}
		let scope = self.entry.scope();
		let identity = self.entry.record_identity(cache_entry_identity);
		cache_entry_lease_key(CacheEntryLeaseKeyFields {
			storage: scope.storage().as_bytes(),
			namespace: scope.namespace().as_bytes(),
			record_key: identity.key().as_bytes(),
		})
		.map_err(NativeCacheIdentityError::Key)
	}
	/// Increments the cache's epoch, which makes every stored value stale.
	pub(crate) async fn invalidate_all(
		&self,
		database: impl WriteDatabase,
	) -> Result<i64, NativeCacheEpochError> {
		self.epoch.invalidate(database).await
	}
	pub(crate) async fn fetch_fresh<T, C>(
		&self,
		database: impl Database,
		fields: NativeCacheFreshReadFields<'_, '_, '_, T, C>,
	) -> Result<Option<CacheValueRead<T>>, NativePayloadReadError<C>> {
		self.entry
			.fetch_fresh(
				database,
				fields.cache_entry_identity,
				fields.payload,
				&self.epoch,
			)
			.await
	}
}
const CACHE_LEASE_KEY_LABEL: &str = "paranoid.native-cache.v1";
const CACHE_DIGEST_LEASE_KEY_LABEL: &str = "paranoid.native-cache.v1.digest";

struct CacheEntryLeaseKeyFields<'a> {
	storage: &'a [u8],
	namespace: &'a [u8],
	record_key: &'a [u8],
}

/// The lease key of one Cache entry: the label and the hex of the entry's storage, namespace,
/// and record key. When that key exceeds the lease key limit, the key holds the digest of the
/// same three values, framed after the label.
fn cache_entry_lease_key(
	fields: CacheEntryLeaseKeyFields<'_>,
) -> Result<LeaseKey, CoordinationKeyError> {
	LeaseKey::from_parts_or_digest(
		[
			CACHE_LEASE_KEY_LABEL.to_owned(),
			ByteEncoding::HexLower.encode(fields.storage),
			ByteEncoding::HexLower.encode(fields.namespace),
			ByteEncoding::HexLower.encode(fields.record_key),
		],
		CACHE_DIGEST_LEASE_KEY_LABEL,
		|| {
			let mut framed = Vec::new();
			append_length_prefixed(&mut framed, CACHE_LEASE_KEY_LABEL.as_bytes());
			append_length_prefixed(&mut framed, fields.storage);
			append_length_prefixed(&mut framed, fields.namespace);
			append_length_prefixed(&mut framed, fields.record_key);
			framed
		},
	)
}

pub(crate) enum NativeCacheIdentityError {
	DifferentCache,
	Key(CoordinationKeyError),
}
impl fmt::Debug for NativeCacheIdentityError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("NativeCacheIdentityError { .. }")
	}
}
impl fmt::Display for NativeCacheIdentityError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("Cache mutex identity is invalid")
	}
}
impl std::error::Error for NativeCacheIdentityError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Key(source) => Some(source),
			Self::DifferentCache => None,
		}
	}
}
pub(crate) enum NativeCacheConstructionCause {
	Association,
	Entry(NativePayloadConstructionError),
	Epoch(NativeCacheEpochError),
	MutexKey(NativeCacheIdentityError),
}
pub(crate) struct NativeCacheConstructionError {
	source: Box<NativeCacheConstructionCause>,
}
impl NativeCacheConstructionError {
	pub(crate) fn new(source: NativeCacheConstructionCause) -> Self {
		Self {
			source: Box::new(source),
		}
	}
}
impl fmt::Debug for NativeCacheConstructionError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("NativeCacheConstructionError { .. }")
	}
}
impl fmt::Display for NativeCacheConstructionError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("Cache code, epoch, lease or identity association is unavailable")
	}
}
impl std::error::Error for NativeCacheConstructionError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self.source.as_ref() {
			NativeCacheConstructionCause::Association => None,
			NativeCacheConstructionCause::Entry(source) => Some(source),
			NativeCacheConstructionCause::Epoch(source) => Some(source),
			NativeCacheConstructionCause::MutexKey(source) => Some(source),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::fleet::lease::MAX_LEASE_KEY_LEN;
	use primitives_rs::crypto::algorithms::blake3;

	/// The bytes of a lease key with one-byte storage and namespace values, apart from the hex
	/// of the record key: the label, two hex digits for each one-byte value, and four separators.
	const FIXED_KEY_LEN: usize = CACHE_LEASE_KEY_LABEL.len() + 2 + 2 + 4 * 2;

	fn lease_key(record_key: &[u8]) -> Result<LeaseKey, CoordinationKeyError> {
		cache_entry_lease_key(CacheEntryLeaseKeyFields {
			storage: b"s",
			namespace: b"n",
			record_key,
		})
	}

	#[test]
	fn an_entry_keeps_its_hex_lease_key_up_to_the_lease_key_limit() {
		let record_key = vec![0xab; (MAX_LEASE_KEY_LEN - FIXED_KEY_LEN) / 2];
		let key = lease_key(&record_key).expect("a lease key at the limit");
		assert_eq!(key.as_str().len(), MAX_LEASE_KEY_LEN);
		assert_eq!(
			key.as_str(),
			format!(
				"paranoid.native-cache.v1::73::6e::{}::",
				"ab".repeat(record_key.len())
			)
		);
		assert_eq!(
			lease_key(b"entry").expect("a short lease key").as_str(),
			"paranoid.native-cache.v1::73::6e::656e747279::"
		);
	}

	#[test]
	fn an_entry_whose_hex_lease_key_exceeds_the_limit_gets_the_digest_form() {
		let record_key = vec![0xab; (MAX_LEASE_KEY_LEN - FIXED_KEY_LEN) / 2 + 1];
		assert!(matches!(
			LeaseKey::from_parts([
				CACHE_LEASE_KEY_LABEL.to_owned(),
				"73".to_owned(),
				"6e".to_owned(),
				"ab".repeat(record_key.len()),
			]),
			Err(CoordinationKeyError::TooLong { .. })
		));
		let mut framed = Vec::new();
		for value in [
			CACHE_LEASE_KEY_LABEL.as_bytes(),
			b"s",
			b"n",
			record_key.as_slice(),
		] {
			append_length_prefixed(&mut framed, value);
		}
		assert_eq!(
			lease_key(&record_key).expect("a digest lease key").as_str(),
			format!(
				"paranoid.native-cache.v1.digest::{}::",
				blake3::hash(&framed).to_hex()
			)
		);
	}
}
