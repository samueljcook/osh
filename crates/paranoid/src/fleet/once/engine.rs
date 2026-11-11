//! Exact retained Once placement and admitted native completion observation.

use std::{fmt, sync::Arc};

use crate::db::{Database, run_transaction_read};
use crate::records::RecordIdentity;
use crate::records::postgres::PgPointStorage;
use crate::runtime::admission::bindings::PgInstallationAdmissionBinding;
use crate::runtime::admission::{NativeOperationError, PgNativeAdmission};
use primitives_rs::bytes::append_length_prefixed;
use primitives_rs::encoding::binary_to_text::ByteEncoding;

use super::state::{NativeOnceCompletion, OnceCompletionComponent};
use crate::fleet::OnceKey;
use crate::fleet::lease::{CoordinationKeyError, LeaseKey, NativeLeaseBindings, PgNativeLease};
use crate::fleet::scalar::{FleetScalarCode, NativeFleetScalarResource};

pub(crate) mod error;
pub(crate) mod managed;
use error::{NativeOnceCause, NativeOnceConstructionCause};
pub(crate) use error::{NativeOnceConstructionError, NativeOnceError};

pub(crate) struct NativeOnceFields {
	pub(crate) key: OnceKey,
	pub(crate) code: Arc<FleetScalarCode>,
	pub(crate) leases: Arc<NativeLeaseBindings>,
	pub(crate) admission: Arc<PgNativeAdmission>,
}

/// Retains actual code and immutable placement without owning a runtime or private claim.
pub(crate) struct PgNativeOnce {
	key: OnceKey,
	code: Arc<FleetScalarCode>,
	storage: PgPointStorage,
	identity: RecordIdentity,
	lease: PgNativeLease,
	admission: Arc<PgNativeAdmission>,
}

impl PgNativeOnce {
	pub(crate) fn new(fields: NativeOnceFields) -> Result<Self, NativeOnceConstructionError> {
		let reject = |error| NativeOnceConstructionError::new(error);
		if !Arc::ptr_eq(&fields.admission, &fields.leases.admission) {
			return Err(reject(NativeOnceConstructionCause::AdmissionAssociation));
		}
		let installation = fields.admission.installation_binding();
		let native = match installation.as_ref() {
			PgInstallationAdmissionBinding::Fresh { native }
			| PgInstallationAdmissionBinding::Imported { native, .. } => native,
		};
		let placement = native.fleet_scalar();
		if !fields
			.leases
			.storage
			.shares_container(native.leases().storage())
			|| &fields.leases.namespace != native.leases().namespace()
		{
			return Err(reject(NativeOnceConstructionCause::LeasePlacementMismatch));
		}
		if placement.storage().shares_container(&fields.leases.storage)
			&& placement.once_completion() == &fields.leases.namespace
		{
			return Err(reject(NativeOnceConstructionCause::OverlappingPlacement));
		}
		let identity = RecordIdentity::new(
			placement.once_completion().clone(),
			NativeFleetScalarResource::Once(&fields.key).record_key(),
		);
		let mutex_key = match installation.as_ref() {
			PgInstallationAdmissionBinding::Imported { source, .. } => {
				crate::fleet::namespace_keys::build_once_mutex_lease_key(
					source.fleet_root(),
					&fields.key,
				)
			}
			PgInstallationAdmissionBinding::Fresh { .. } => {
				fresh_mutex_key(placement.storage(), &identity)
			}
		}
		.map_err(|error| reject(NativeOnceConstructionCause::MutexKey(error)))?;
		Ok(Self {
			storage: placement.storage().clone(),
			identity,
			lease: PgNativeLease::new(fields.leases, mutex_key),
			key: fields.key,
			code: fields.code,
			admission: fields.admission,
		})
	}

	pub(crate) fn key(&self) -> &OnceKey {
		&self.key
	}
	pub(crate) fn completion_storage(&self) -> &PgPointStorage {
		&self.storage
	}
	pub(crate) fn completion_identity(&self) -> &RecordIdentity {
		&self.identity
	}
	pub(crate) fn lease(&self) -> &PgNativeLease {
		&self.lease
	}
	pub(crate) fn component(&self) -> &Arc<OnceCompletionComponent> {
		&self.code.once
	}
	pub(crate) fn admission(&self) -> &Arc<PgNativeAdmission> {
		&self.admission
	}

	/// Observes permanent completion; it does not acquire exclusion or authorize execution.
	pub(crate) async fn fetch_completion(
		&self,
		database: impl Database,
	) -> Result<Option<NativeOnceCompletion>, NativeOnceError> {
		run_transaction_read(database, "fleet.native_once.fetch_completion", async |tx| {
			self.admission
				.execute_read_with_contracts(tx, async |contracts, tx| {
					contracts
						.require_native_storage()
						.map_err(NativeOperationError::Admission)?;
					let declaration = self.code.once.declaration();
					let component = contracts
						.bind(&declaration)
						.map_err(NativeOperationError::Admission)?;
					let schema = self.code.once.bind_schema(&component).map_err(|error| {
						NativeOperationError::Operation(NativeOnceError::new(
							NativeOnceCause::State(error),
						))
					})?;
					let snapshot =
						self.storage
							.read(tx, &self.identity)
							.await
							.map_err(|error| {
								NativeOperationError::Operation(NativeOnceError::new(
									NativeOnceCause::Storage(error),
								))
							})?;
					snapshot
						.record()
						.map(|record| {
							if !record.index_fields().is_empty() {
								return Err(NativeOnceError::new(
									NativeOnceCause::UnexpectedIndexes,
								));
							}
							schema.decode_record(record).map_err(|error| {
								NativeOnceError::new(NativeOnceCause::State(error))
							})
						})
						.transpose()
						.map_err(NativeOperationError::Operation)
				})
				.await
				.map_err(NativeOnceError::from_operation)
		})
		.await
		.map_err(|error| error.with_context_if_absent(&self.key, &self.identity))
	}
}

fn fresh_mutex_key(
	storage: &PgPointStorage,
	identity: &RecordIdentity,
) -> Result<LeaseKey, CoordinationKeyError> {
	let mut bytes = Vec::new();
	append_length_prefixed(&mut bytes, b"paranoid.fleet.once-mutex.v1");
	append_length_prefixed(&mut bytes, storage.table().schema().as_str().as_bytes());
	append_length_prefixed(&mut bytes, storage.table().table().as_str().as_bytes());
	append_length_prefixed(&mut bytes, identity.namespace().as_bytes());
	append_length_prefixed(&mut bytes, identity.key().as_bytes());
	once_lease_key(&bytes)
}

/// The lease key of a Once on a fresh installation: the label and the hex of the Once's framed
/// identity. When that key exceeds the lease key limit, the key holds the digest of the framed
/// identity.
fn once_lease_key(framed_identity: &[u8]) -> Result<LeaseKey, CoordinationKeyError> {
	let encoded = ByteEncoding::HexLower.encode(framed_identity);
	LeaseKey::from_parts_or_digest(
		["paranoid.native-once.v1", encoded.as_str()],
		"paranoid.native-once.v1.digest",
		|| framed_identity,
	)
}

impl fmt::Debug for PgNativeOnce {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("PgNativeOnce { .. }")
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::fleet::lease::MAX_LEASE_KEY_LEN;
	use primitives_rs::crypto::algorithms::blake3;

	/// The bytes of a lease key apart from the hex of the framed identity: the label and two
	/// separators.
	const FIXED_KEY_LEN: usize = "paranoid.native-once.v1".len() + 2 * 2;

	/// A Once lease key has an odd length, so the longest key that fits is one byte shorter than
	/// the limit.
	#[test]
	fn a_once_keeps_its_hex_lease_key_up_to_the_lease_key_limit() {
		let framed = vec![7; (MAX_LEASE_KEY_LEN - FIXED_KEY_LEN) / 2];
		let key = once_lease_key(&framed).expect("a lease key at the limit");
		assert_eq!(key.as_str().len(), MAX_LEASE_KEY_LEN - 1);
		assert_eq!(
			key.as_str(),
			format!("paranoid.native-once.v1::{}::", "07".repeat(framed.len()))
		);
	}

	#[test]
	fn a_once_whose_hex_lease_key_exceeds_the_limit_gets_the_digest_form() {
		let framed = vec![7; (MAX_LEASE_KEY_LEN - FIXED_KEY_LEN) / 2 + 1];
		assert!(matches!(
			LeaseKey::from_parts([
				"paranoid.native-once.v1",
				"07".repeat(framed.len()).as_str()
			]),
			Err(CoordinationKeyError::TooLong { .. })
		));
		assert_eq!(
			once_lease_key(&framed)
				.expect("a digest lease key")
				.as_str(),
			format!(
				"paranoid.native-once.v1.digest::{}::",
				blake3::hash(&framed).to_hex()
			)
		);
	}
}
