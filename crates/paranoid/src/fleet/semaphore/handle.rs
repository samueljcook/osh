//! Same-runtime native Semaphore facade; private claims and execution machinery remain internal.

mod error;
mod task;

pub use error::{PgSemaphoreError, PgSemaphoreErrorKind};
pub use task::{
	PgSemaphoreHold, PgSemaphoreRelease, PgSemaphoreTaskCtx, PgSemaphoreTaskResult,
	PgSemaphoreTryRunTaskResult,
};

use crate::db::{Database, WriteDatabase};
use crate::fleet::semaphore::engine::NativeSemaphoreExecutionBindings;
use crate::fleet::{SemaphoreKey, SemaphoreSettings, SemaphoreStatus};
use crate::runtime::semaphore::NativeRuntimeSemaphoreHandleError;
use crate::scope::ScopeIdentity;
use std::sync::Arc;

/// The inputs of a Semaphore handle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PgSemaphoreFields {
	/// The scope that declares the Semaphore's code.
	pub scope: ScopeIdentity,
	/// The Semaphore's key.
	pub key: SemaphoreKey,
	/// The Semaphore's capacity and hold window, which every operation checks against the
	/// stored record.
	pub settings: SemaphoreSettings,
	/// The positive delay between a waiting run's acquisition attempts.
	pub acquire_retry_interval: std::time::Duration,
}
pub(crate) struct PgSemaphoreNativeFields {
	/// The execution bindings, which also hold the Semaphore that the handle reads and resets.
	pub(crate) execution: Arc<NativeSemaphoreExecutionBindings>,
	pub(crate) waiting: crate::fleet::semaphore::engine::NativeSemaphoreWaitingPolicy,
	pub(crate) holder_prefix: crate::fleet::HolderPrefix,
}
/// One Semaphore in a declared Semaphore scope of the runtime's installation.
///
/// Each operation checks the scope's installed policy and fails when the runtime does not use
/// the role that the operation needs: reading for `fetch_status`, and writing for runs and
/// `reset`. Holding a handle does not keep the runtime running.
///
/// A run does not stop its task when its slot's hold window ends, so another run can take the
/// slot while the task still runs. Make the task safe to overlap with another run.
#[derive(Clone)]
pub struct PgSemaphore {
	execution: Arc<NativeSemaphoreExecutionBindings>,
	waiting: crate::fleet::semaphore::engine::NativeSemaphoreWaitingPolicy,
	holder_prefix: crate::fleet::HolderPrefix,
}
impl PgSemaphore {
	pub(crate) fn from_native(fields: PgSemaphoreNativeFields) -> Self {
		Self {
			execution: fields.execution,
			waiting: fields.waiting,
			holder_prefix: fields.holder_prefix,
		}
	}
	pub(crate) fn construction_error(
		source: NativeRuntimeSemaphoreHandleError,
	) -> PgSemaphoreError {
		PgSemaphoreError::construction(source)
	}
	/// The Semaphore's scope.
	pub fn scope(&self) -> &ScopeIdentity {
		self.execution.handle().scope()
	}
	/// The Semaphore's key.
	pub fn key(&self) -> &SemaphoreKey {
		self.execution.handle().key()
	}
	/// The settings that every operation checks against the stored Semaphore.
	pub fn settings(&self) -> &SemaphoreSettings {
		self.execution.handle().settings()
	}
	/// Reads the number of held slots and the capacity. An absent Semaphore has no held slots;
	/// the read does not create it.
	pub async fn fetch_status(
		&self,
		database: impl Database,
	) -> Result<SemaphoreStatus, PgSemaphoreError> {
		self.execution
			.handle()
			.fetch_status(database)
			.await
			.map_err(PgSemaphoreError::operation)
	}
	/// Releases every stored slot, expired or live, and returns the number of slots that it
	/// released. The stored settings stay. Running tasks keep running, so more tasks than the
	/// capacity can run at the same time until they return. In a caller's transaction, the
	/// change takes effect when the caller commits.
	pub async fn reset(&self, database: impl WriteDatabase) -> Result<u64, PgSemaphoreError> {
		self.execution
			.handle()
			.reset(database)
			.await
			.map_err(PgSemaphoreError::operation)
	}
}
impl std::fmt::Debug for PgSemaphore {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("PgSemaphore").finish_non_exhaustive()
	}
}
