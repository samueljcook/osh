//! Exact configured Cache/Lease association without current-policy or mutation authority.

use super::{
	Arc, CacheEntryIdentity, LeaseKey, NativeCacheIdentityError, NativeLeaseBindings,
	PgNativeCache, fmt,
};
use crate::fleet::lease::PgNativeLease;
use crate::records::postgres::PgPointStorage;
use crate::records::postgres::record_set::{PgRecordTarget, PgRecordTargetFields};
use crate::records::{RecordIdentity, RecordKey};
use crate::runtime::admission::owned_primitive::CacheLeaseOriginIdentity;

pub(crate) struct NativeCachePublicationIdentity {
	cache_entry_identity: CacheEntryIdentity,
	storage: PgPointStorage,
	leases: Arc<NativeLeaseBindings>,
	mutex_key: LeaseKey,
	origin: CacheLeaseOriginIdentity,
}
impl PgNativeCache<'_, '_> {
	pub(in crate::fleet) fn publication_identity(
		&self,
		cache_entry_identity: &CacheEntryIdentity,
	) -> Result<NativeCachePublicationIdentity, NativeCachePublicationIdentityError> {
		let mutex = self
			.mutex_key(cache_entry_identity)
			.map_err(NativeCachePublicationIdentityError::Key)?;
		if !self.entry.shares_admission(&self.leases.admission) {
			return Err(NativeCachePublicationIdentityError::Association);
		}
		let entry = self.entry.record_identity(cache_entry_identity);
		let lease = RecordIdentity::new(
			self.leases.namespace.clone(),
			RecordKey::new(mutex.as_str().as_bytes().to_vec()),
		);
		let entry_target = PgRecordTarget::new(PgRecordTargetFields {
			storage: self.entry.storage(),
			record: entry.clone(),
		});
		let lease_target = PgRecordTarget::new(PgRecordTargetFields {
			storage: &self.leases.storage,
			record: lease.clone(),
		});
		if entry_target.identity() == lease_target.identity() {
			return Err(NativeCachePublicationIdentityError::AliasedRecords);
		}
		let scope = self.entry.scope().clone();
		let origin = CacheLeaseOriginIdentity::new(
			crate::runtime::admission::owned_primitive::CacheLeaseOriginIdentityFields {
				scope,
				entry: entry_target.identity().clone(),
				lease: lease_target.identity().clone(),
			},
		);
		Ok(NativeCachePublicationIdentity {
			cache_entry_identity: cache_entry_identity.clone(),
			storage: self.entry.storage().clone(),
			leases: Arc::clone(&self.leases),
			mutex_key: mutex,
			origin,
		})
	}
}
impl NativeCachePublicationIdentity {
	pub(crate) fn origin_identity(&self) -> &CacheLeaseOriginIdentity {
		&self.origin
	}
	pub(crate) fn lease_identity(&self) -> &RecordIdentity {
		self.origin.lease().record()
	}
	pub(crate) fn admission(&self) -> &Arc<crate::runtime::admission::PgNativeAdmission> {
		&self.leases.admission
	}
	pub(in crate::fleet) fn lease_bindings(&self) -> &Arc<NativeLeaseBindings> {
		&self.leases
	}
	pub(in crate::fleet) fn mutex_key(&self) -> &LeaseKey {
		&self.mutex_key
	}
	/// Compares this configured purpose with the actual borrowed Cache owner and key.
	pub(in crate::fleet) fn require_cache(
		&self,
		cache: &PgNativeCache<'_, '_>,
		cache_entry_identity: &CacheEntryIdentity,
	) -> Result<(), NativeCachePublicationIdentityError> {
		let actual = cache.publication_identity(cache_entry_identity)?;
		if self.cache_entry_identity != actual.cache_entry_identity
			|| !self.storage.shares_container(&actual.storage)
			|| !Arc::ptr_eq(&self.leases, &actual.leases)
			|| self.mutex_key != actual.mutex_key
			|| self.origin != actual.origin
		{
			return Err(NativeCachePublicationIdentityError::Association);
		}
		Ok(())
	}
	pub(in crate::fleet) fn matches_lease(&self, lease: &PgNativeLease) -> bool {
		self.lease_identity() == lease.identity()
			&& self.mutex_key == *lease.key()
			&& lease.shares_admission(&self.leases.admission)
	}
}
#[derive(thiserror::Error)]
pub(crate) enum NativeCachePublicationIdentityError {
	#[error("Cache publication mutex identity is invalid")]
	Key(#[source] NativeCacheIdentityError),
	#[error("Cache publication belongs to different configured storage or Lease bindings")]
	Association,
	#[error("Cache publication entry and Lease have the same physical record identity")]
	AliasedRecords,
}
impl fmt::Debug for NativeCachePublicationIdentityError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("NativeCachePublicationIdentityError { typed_cause_retained: true }")
	}
}
