//! The application scope table, without scoped admission or source assignment.

use crate::scope::ScopeIdentity;
use primitives_rs::crypto::algorithms::blake3;

use super::super::binding_fingerprint::frame_fingerprint_input;
use super::{NativeInstallationError, PgNativeStorageBindings};
use crate::records::postgres::PgPointStorage;

/// The one table that holds every application scope. Scopes are separated by namespace,
/// which the installation requires to be unique across its scopes.
#[derive(Clone)]
pub(super) struct PgAppStorageBindings {
	storage: PgPointStorage,
}

impl PgAppStorageBindings {
	pub(super) fn new(storage: PgPointStorage) -> Self {
		Self { storage }
	}

	pub(super) fn storage(&self) -> &PgPointStorage {
		&self.storage
	}

	pub(super) fn resolve(&self, _scope: &ScopeIdentity) -> &PgPointStorage {
		&self.storage
	}

	pub(super) fn frame_fingerprint(&self, hasher: &mut blake3::Hasher) {
		frame_fingerprint_input(hasher, b"application-storage.v2");
		frame_fingerprint_input(hasher, self.storage.table().schema().as_str().as_bytes());
		frame_fingerprint_input(hasher, self.storage.table().table().as_str().as_bytes());
	}
}

impl PgNativeStorageBindings {
	/// Resolves a scope's table; callers still require scoped admission.
	pub(crate) fn app_storage(
		&self,
		scope: &ScopeIdentity,
	) -> Result<&PgPointStorage, NativeInstallationError> {
		Ok(self.app_storage.resolve(scope))
	}

	/// Every scope shares the application table, so scope namespaces must be unique across
	/// the installation; core namespaces stay reserved when a table is shared.
	pub(crate) fn check_scope_policies(
		&self,
		policies: &crate::scope::ScopePolicySet,
	) -> Result<(), NativeInstallationError> {
		let scopes = policies.policies();
		for (index, policy) in scopes.iter().enumerate() {
			if scopes[..index]
				.iter()
				.any(|prior| prior.identity().namespace() == policy.identity().namespace())
			{
				return Err(NativeInstallationError::ScopeNamespaceRepeated { index });
			}
		}
		for (index, policy) in scopes.iter().enumerate() {
			let storage = self.app_storage(policy.identity())?;
			for role in self.roles() {
				if storage.shares_container(role.storage)
					&& policy.identity().namespace() == role.namespace
				{
					return Err(NativeInstallationError::ScopeOverlapsCoreNamespace {
						index,
						role: role.name,
					});
				}
			}
		}
		Ok(())
	}
}
