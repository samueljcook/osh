//! Actual retained native Semaphore construction from the runtime's shared admission and placement.

use super::PgRuntime;
use crate::fleet::SemaphoreKey;
use crate::fleet::primitive_scope::ManagedPrimitiveRegistration;
use crate::fleet::semaphore::engine::{
	NativeSemaphoreBindings, NativeSemaphoreBindingsFields, NativeSemaphoreError,
	NativeSemaphoreExecutionBindings, NativeSemaphoreExecutionBindingsFields,
	NativeSemaphoreFields, PgNativeSemaphore, SemaphoreDriverTiming, SemaphoreExecutionTiming,
};
use crate::fleet::semaphore::handle::{
	PgSemaphore, PgSemaphoreError, PgSemaphoreFields, PgSemaphoreNativeFields,
};
use crate::fleet::semaphore::state::SemaphoreSettings;
use crate::scope::ScopeIdentity;
use crate::scope::managed::{ManagedScopeRegistration, ManagedScopeRegistryError};
use std::sync::Arc;

/// Paranoid's Semaphore timing for one runtime.
pub(crate) struct SemaphoreRuntimeConfig {
	/// Parsed budgets for fixed registered operations and owned ending observations.
	pub(crate) execution_timing: SemaphoreExecutionTiming,
	/// Parsed retry policy for the same managed runtime owner.
	pub(crate) driver_timing: SemaphoreDriverTiming,
}

pub(crate) struct NativeRuntimeSemaphoreHandleFields {
	pub(crate) scope: ScopeIdentity,
	pub(crate) key: SemaphoreKey,
	pub(crate) settings: SemaphoreSettings,
}
struct SemaphoreExecutionForHandleFields<'a> {
	handle: PgNativeSemaphore,
	config: &'a SemaphoreRuntimeConfig,
}
impl PgRuntime {
	fn semaphore_execution_for_handle(
		&self,
		fields: SemaphoreExecutionForHandleFields<'_>,
	) -> Arc<NativeSemaphoreExecutionBindings> {
		Arc::new(NativeSemaphoreExecutionBindings::new(
			NativeSemaphoreExecutionBindingsFields {
				handle: fields.handle,
				execution_pool: self.leases.control_pool.clone(),
				timing: fields.config.execution_timing,
				driver_timing: fields.config.driver_timing,
				originating_runtime: self.leases.originating_runtime.clone(),
			},
		))
	}

	/// Builds the handle of one Semaphore without database work. It fails when the scope is not
	/// a declared Semaphore scope or when the acquire retry interval is zero or exceeds the local
	/// monotonic clock domain.
	pub fn semaphore(&self, fields: PgSemaphoreFields) -> Result<PgSemaphore, PgSemaphoreError> {
		let waiting = match crate::fleet::semaphore::engine::NativeSemaphoreWaitingPolicy::new(
			crate::fleet::semaphore::engine::NativeSemaphoreWaitingPolicyFields {
				acquire_retry_interval: fields.acquire_retry_interval,
			},
		) {
			Ok(waiting) => waiting,
			Err(source) => {
				return Err(PgSemaphore::construction_error(
					NativeRuntimeSemaphoreHandleError::Semaphore(Box::new(source)),
				));
			}
		};
		let handle = self
			.native_semaphore(NativeRuntimeSemaphoreHandleFields {
				scope: fields.scope.clone(),
				key: fields.key.clone(),
				settings: fields.settings,
			})
			.map_err(PgSemaphore::construction_error)?;
		let execution = self.semaphore_execution_for_handle(SemaphoreExecutionForHandleFields {
			handle,
			config: &self.semaphore_config,
		});
		Ok(PgSemaphore::from_native(PgSemaphoreNativeFields {
			execution,
			waiting,
			holder_prefix: self.holder_prefix.clone(),
		}))
	}

	pub(crate) fn native_semaphore(
		&self,
		fields: NativeRuntimeSemaphoreHandleFields,
	) -> Result<PgNativeSemaphore, NativeRuntimeSemaphoreHandleError> {
		let owner = self
			.managed_scopes
			.lookup(&fields.scope)
			.map_err(|error| NativeRuntimeSemaphoreHandleError::ScopeCode(Box::new(error)))?;
		let ManagedScopeRegistration::Primitive(ManagedPrimitiveRegistration::Semaphore(code)) =
			owner
		else {
			return Err(NativeRuntimeSemaphoreHandleError::WrongFamily {
				scope: fields.scope,
			});
		};
		let bindings = Arc::new(
			NativeSemaphoreBindings::new(NativeSemaphoreBindingsFields {
				code: Arc::clone(code),
				admission: Arc::clone(&self.admission),
				conflict_attempts: self.conflict_attempts,
			})
			.map_err(|error| NativeRuntimeSemaphoreHandleError::Semaphore(Box::new(error)))?,
		);
		Ok(PgNativeSemaphore::new(NativeSemaphoreFields {
			bindings,
			key: fields.key,
			settings: fields.settings,
		}))
	}
}
#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeRuntimeSemaphoreHandleError {
	#[error("native runtime has no code for this Semaphore scope")]
	ScopeCode(#[source] Box<ManagedScopeRegistryError>),
	#[error("native runtime scope does not retain the Semaphore family")]
	WrongFamily { scope: ScopeIdentity },
	#[error("native runtime cannot bind this Semaphore to its admission and placement")]
	Semaphore(#[source] Box<NativeSemaphoreError>),
}
