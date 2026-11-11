//! Once construction from the runtime's scalar code, leases, and admission.

use std::sync::Arc;

use crate::fleet::OnceKey;
use crate::fleet::once::engine::{NativeOnceConstructionError, NativeOnceFields, PgNativeOnce};

use super::PgRuntime;

pub(crate) struct NativeRuntimeOnceFields {
	pub(crate) key: OnceKey,
}

impl PgRuntime {
	/// Builds the handle for one Once; construction performs no database work. It fails when
	/// Paranoid cannot build the lease key of the Once's mutex from the key, for example because
	/// that lease key would be too long.
	pub fn once(
		&self,
		fields: crate::fleet::PgOnceFields,
	) -> Result<crate::fleet::PgOnce, crate::fleet::PgOnceError> {
		let native = self
			.native_once(NativeRuntimeOnceFields { key: fields.key })
			.map_err(crate::fleet::PgOnceError::construction)?;
		Ok(crate::fleet::PgOnce::new(
			crate::fleet::once::handle::PgOnceParts {
				native,
				pool: self.app_pool().clone(),
				holder_prefix: self.holder_prefix.clone(),
				lease_options: fields.lease_options,
			},
		))
	}

	pub(crate) fn native_once(
		&self,
		fields: NativeRuntimeOnceFields,
	) -> Result<PgNativeOnce, NativeOnceConstructionError> {
		let fleet = &self.fleet_scalar;
		PgNativeOnce::new(NativeOnceFields {
			key: fields.key,
			code: Arc::clone(fleet.code()),
			leases: Arc::clone(&self.leases),
			admission: Arc::clone(&self.admission),
		})
	}
}
