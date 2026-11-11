//! Actual scoped Throttler handle construction from one runtime's retained code and control pool.

use crate::fleet::primitive_scope::ManagedPrimitiveRegistration;
use crate::fleet::throttler::engine::{
	NativeThrottlerBindings, NativeThrottlerBindingsFields, NativeThrottlerError,
	PgNativeThrottler, ThrottlerDriverTiming, ThrottlerExecutionTiming,
};
use crate::fleet::throttler::state::ThrottlerSettings;
use crate::fleet::{PgThrottler, PgThrottlerError, PgThrottlerFields, ThrottlerKey};
use crate::scope::ScopeIdentity;
use crate::scope::managed::{ManagedScopeRegistration, ManagedScopeRegistryError};
use std::sync::Arc;

use super::PgRuntime;

/// Paranoid's Throttler timing for one runtime.
pub(crate) struct ThrottlerRuntimeConfig {
	/// Parsed budgets for fixed registered operations and owned ending observations.
	pub(crate) execution_timing: ThrottlerExecutionTiming,
	/// Parsed retry policy for the same managed runtime owner.
	pub(crate) driver_timing: ThrottlerDriverTiming,
}
pub(crate) struct NativeRuntimeThrottlerHandleFields {
	pub(crate) scope: ScopeIdentity,
	pub(crate) key: ThrottlerKey,
	pub(crate) settings: ThrottlerSettings,
}

impl PgRuntime {
	/// Builds the handle of one Throttler without database work. It fails when the scope is not
	/// a declared Throttler scope or when the probe heartbeat does not match the circuit
	/// settings.
	pub fn throttler(&self, fields: PgThrottlerFields) -> Result<PgThrottler, PgThrottlerError> {
		let handle = self
			.native_throttler(NativeRuntimeThrottlerHandleFields {
				scope: fields.scope.clone(),
				key: fields.key.clone(),
				settings: fields.settings,
			})
			.map_err(PgThrottler::construction_error)?;
		PgThrottler::from_native(
			handle,
			fields.probe_heartbeat,
			self.holder_prefix.clone(),
			fields.acquire_retry_interval,
		)
	}

	/// A retained handle authenticates current installed policy and use at each operation.
	pub(crate) fn native_throttler(
		&self,
		fields: NativeRuntimeThrottlerHandleFields,
	) -> Result<PgNativeThrottler, NativeRuntimeThrottlerHandleError> {
		let config = &self.throttler_config;
		let owner = self
			.managed_scopes
			.lookup(&fields.scope)
			.map_err(|error| NativeRuntimeThrottlerHandleError::ScopeCode(Box::new(error)))?;
		let ManagedScopeRegistration::Primitive(ManagedPrimitiveRegistration::Throttler(code)) =
			owner
		else {
			return Err(NativeRuntimeThrottlerHandleError::WrongFamily {
				scope: fields.scope,
			});
		};
		let bindings = Arc::new(
			NativeThrottlerBindings::new(NativeThrottlerBindingsFields {
				code: Arc::clone(code),
				admission: Arc::clone(&self.admission),
				execution_pool: self.leases.control_pool.clone(),
				group_budget: self.group_budget,
				timing: config.execution_timing,
				driver_timing: config.driver_timing,
				originating_runtime: self.leases.originating_runtime.clone(),
			})
			.map_err(|error| NativeRuntimeThrottlerHandleError::Throttler(Box::new(error)))?,
		);
		Ok(PgNativeThrottler::new(
			bindings,
			fields.key,
			fields.settings,
		))
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeRuntimeThrottlerHandleError {
	#[error("native runtime has no code for this Throttler scope")]
	ScopeCode(#[source] Box<ManagedScopeRegistryError>),
	#[error("native runtime scope does not retain the Throttler family")]
	WrongFamily { scope: ScopeIdentity },
	#[error("native runtime cannot bind this Throttler to its admission and placement")]
	Throttler(#[source] Box<NativeThrottlerError>),
}
