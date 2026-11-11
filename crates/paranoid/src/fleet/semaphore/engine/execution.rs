//! Parsed fixed execution configuration; construction grants no installed role.

use std::time::Duration;

use super::{NativeSemaphoreError, PgNativeSemaphore, SemaphoreDriverTiming};
use crate::db::WritePool;
use crate::runtime::local_admission::semaphore_work::{
	RuntimeSemaphoreWorkIdentity, RuntimeSemaphoreWorkIdentityFields, RuntimeSemaphoreWorkPermit,
};
use tokio::runtime::Handle;

/// Explicit wait budgets for registered Semaphore execution and settlement.
pub(crate) struct SemaphoreExecutionTimingFields {
	/// The positive budget for one physical-ordering proof attempt.
	pub physical_proof_attempt_timeout: Duration,
	/// The positive budget for one exact undelivered cleanup attempt.
	pub cleanup_attempt_timeout: Duration,
	/// The positive budget for observing an owned ending and local settlement.
	pub ending_wait_timeout: Duration,
}
/// Parsed positive execution waits; actual ending waits retain their existing ownership. An
/// acquisition ends with the runtime's confirmed lifetime instead of a wait of its own.
#[derive(Clone, Copy)]
pub(crate) struct SemaphoreExecutionTiming {
	pub(super) physical_proof_attempt_timeout: Duration,
	pub(super) cleanup_attempt_timeout: Duration,
	pub(super) ending_wait_timeout: Duration,
}
/// A zero wait duration supplied to Semaphore execution or driver timing.
#[derive(Debug, thiserror::Error)]
#[error("Semaphore timing waits must be positive")]
pub(crate) struct SemaphoreTimingError;

impl SemaphoreExecutionTiming {
	/// Checks that every supplied execution wait is positive.
	/// Actual operations separately check additions to their current local clock.
	pub fn new(fields: SemaphoreExecutionTimingFields) -> Result<Self, SemaphoreTimingError> {
		if [
			fields.physical_proof_attempt_timeout,
			fields.cleanup_attempt_timeout,
			fields.ending_wait_timeout,
		]
		.into_iter()
		.any(|wait| wait.is_zero())
		{
			return Err(SemaphoreTimingError);
		}
		Ok(Self {
			physical_proof_attempt_timeout: fields.physical_proof_attempt_timeout,
			cleanup_attempt_timeout: fields.cleanup_attempt_timeout,
			ending_wait_timeout: fields.ending_wait_timeout,
		})
	}
}
pub(crate) struct NativeSemaphoreExecutionBindingsFields {
	pub(crate) handle: PgNativeSemaphore,
	pub(crate) execution_pool: WritePool,
	pub(crate) timing: SemaphoreExecutionTiming,
	pub(crate) driver_timing: SemaphoreDriverTiming,
	pub(crate) originating_runtime: Handle,
}
pub(crate) struct NativeSemaphoreExecutionBindings {
	pub(super) handle: PgNativeSemaphore,
	pub(super) execution_pool: WritePool,
	pub(super) timing: SemaphoreExecutionTiming,
	pub(super) driver_timing: SemaphoreDriverTiming,
	pub(super) originating_runtime: Handle,
}
pub(super) struct NativeSemaphoreRegistration {
	pub(super) identity: RuntimeSemaphoreWorkIdentity,
	pub(super) owner: RuntimeSemaphoreWorkPermit,
}
impl NativeSemaphoreExecutionBindings {
	pub(crate) fn new(fields: NativeSemaphoreExecutionBindingsFields) -> Self {
		Self {
			handle: fields.handle,
			execution_pool: fields.execution_pool,
			timing: fields.timing,
			driver_timing: fields.driver_timing,
			originating_runtime: fields.originating_runtime,
		}
	}
	/// The Semaphore that every execution of these bindings acquires.
	pub(crate) fn handle(&self) -> &PgNativeSemaphore {
		&self.handle
	}
	pub(super) fn register_owner(
		&self,
	) -> Result<NativeSemaphoreRegistration, NativeSemaphoreError> {
		let identity = RuntimeSemaphoreWorkIdentity::new(RuntimeSemaphoreWorkIdentityFields {
			scope: self.handle.bindings.code.declaration().identity(),
			resource: &self.handle.key,
		});
		let owner = self
			.handle
			.bindings
			.admission
			.register_semaphore_work(identity.clone())?;
		Ok(NativeSemaphoreRegistration { identity, owner })
	}
}
