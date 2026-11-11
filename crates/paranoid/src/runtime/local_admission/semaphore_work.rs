//! Exact local Semaphore ownership in the shared runtime registry, without database authority.

use crate::fleet::SemaphoreKey;
use crate::scope::ScopeIdentity;

use super::diagnostic::{RuntimeSemaphoreOwnerAbandonedError, RuntimeWorkTerminalError};
use super::{membership::*, *};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct RuntimeSemaphoreWorkId(pub(super) u64);

pub(crate) struct RuntimeSemaphoreWorkIdentityFields<'a> {
	pub(crate) scope: &'a ScopeIdentity,
	pub(crate) resource: &'a SemaphoreKey,
}
/// Scope/resource diagnostics do not authenticate any stored capability or release.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct RuntimeSemaphoreWorkIdentity {
	scope: ScopeIdentity,
	resource: SemaphoreKey,
}
impl RuntimeSemaphoreWorkIdentity {
	pub(crate) fn new(fields: RuntimeSemaphoreWorkIdentityFields<'_>) -> Self {
		Self {
			scope: fields.scope.clone(),
			resource: fields.resource.clone(),
		}
	}
	pub(crate) fn scope(&self) -> &ScopeIdentity {
		&self.scope
	}
	pub(crate) fn resource(&self) -> &SemaphoreKey {
		&self.resource
	}
}
impl fmt::Debug for RuntimeSemaphoreWorkIdentity {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("RuntimeSemaphoreWorkIdentity { scope_and_resource_retained: true }")
	}
}

pub(crate) type RuntimeSemaphoreWorkSnapshot = RuntimeWorkSnapshot<SemaphoreWork>;

pub(crate) struct SemaphoreWork;
impl RuntimeWorkKind for SemaphoreWork {
	type Id = RuntimeSemaphoreWorkId;
	type Identity = RuntimeSemaphoreWorkIdentity;
	type State = RuntimeResourceWorkState;
	type Callers = ExclusiveCallerWork;
	const ABSENT: RuntimeAdmissionError = RuntimeAdmissionError::SemaphoreWorkAbsent;
	const STATE_MISMATCH: RuntimeAdmissionError = RuntimeAdmissionError::SemaphoreWorkStateMismatch;
	const STILL_ACTIVE: RuntimeAdmissionError = RuntimeAdmissionError::SemaphoreWorkStillActive;
	const OWNER_ABANDONED: RuntimeAdmissionFenceReason =
		RuntimeAdmissionFenceReason::SemaphoreWorkAbandoned;
	fn registry(state: &AdmissionState) -> &WorkRegistry<Self> {
		&state.semaphore_work
	}
	fn registry_mut(state: &mut AdmissionState) -> &mut WorkRegistry<Self> {
		&mut state.semaphore_work
	}
	fn id(value: u64) -> Self::Id {
		RuntimeSemaphoreWorkId(value)
	}
	fn owned_id(id: Self::Id) -> RuntimeOwnedWorkId {
		RuntimeOwnedWorkId::Semaphore(id)
	}
	fn abandoned_error(
		work_id: Self::Id,
		source: Option<RuntimeWorkTerminalError>,
	) -> RuntimeAdmissionError {
		RuntimeAdmissionError::SemaphoreWorkAbandoned { work_id, source }
	}
	fn owner_abandonment_diagnostic() -> Option<RuntimeWorkTerminalError> {
		Some(RuntimeWorkTerminalError(Arc::new(
			RuntimeSemaphoreOwnerAbandonedError,
		)))
	}
}

pub(crate) type RuntimeSemaphoreWorkPermit = WorkPermit<SemaphoreWork>;
impl RuntimeSemaphoreWorkPermit {
	pub(crate) fn complete(&mut self) -> Result<(), RuntimeAdmissionError> {
		self.complete_local(WorkCompletion::Finished)
	}
}

pub(crate) type RuntimeSemaphoreCallerWorkPermit = CallerWorkPermit<SemaphoreWork>;

enum SemaphoreTransactionOperation {
	Acquisition,
	Release,
	Cleanup,
}
impl RuntimeAdmissionTracker {
	/// Pre-stop registration precedes randomness, any library spawn and transaction BEGIN.
	pub(crate) fn try_register_semaphore_work(
		&self,
		identity: RuntimeSemaphoreWorkIdentity,
	) -> Result<RuntimeSemaphoreWorkPermit, RuntimeAdmissionError> {
		self.try_register_work::<SemaphoreWork>(identity)
	}
	/// The fixed backend transfers this exact membership into registration before awaiting BEGIN.
	pub(crate) fn try_admit_semaphore_acquisition(
		&self,
		owner: &RuntimeSemaphoreWorkPermit,
		identity: &RuntimeSemaphoreWorkIdentity,
	) -> Result<RuntimeTransactionPermit, RuntimeAdmissionError> {
		self.admit_semaphore_transaction(
			owner,
			identity,
			SemaphoreTransactionOperation::Acquisition,
		)
	}
	/// Consuming release closes existing ownership after stop and grants no new caller permission.
	pub(crate) fn try_admit_semaphore_release(
		&self,
		owner: &RuntimeSemaphoreWorkPermit,
		identity: &RuntimeSemaphoreWorkIdentity,
	) -> Result<RuntimeTransactionPermit, RuntimeAdmissionError> {
		self.admit_semaphore_transaction(owner, identity, SemaphoreTransactionOperation::Release)
	}
	/// Exact preexisting cleanup requires the executor's sealed purpose and current database
	/// authority.
	pub(crate) fn try_admit_semaphore_cleanup(
		&self,
		owner: &RuntimeSemaphoreWorkPermit,
		identity: &RuntimeSemaphoreWorkIdentity,
	) -> Result<RuntimeTransactionPermit, RuntimeAdmissionError> {
		self.admit_semaphore_transaction(owner, identity, SemaphoreTransactionOperation::Cleanup)
	}
	fn admit_semaphore_transaction(
		&self,
		owner: &RuntimeSemaphoreWorkPermit,
		identity: &RuntimeSemaphoreWorkIdentity,
		operation: SemaphoreTransactionOperation,
	) -> Result<RuntimeTransactionPermit, RuntimeAdmissionError> {
		if !Arc::ptr_eq(&self.0, &owner.shared) || !owner.is_active() {
			return Err(RuntimeAdmissionError::SemaphoreWorkRuntimeMismatch);
		}
		let mut state = self.0.lock();
		self.0.fence_elapsed(&mut state);
		if state.lifetime_finished() {
			return Err(RuntimeAdmissionError::LifetimeFinished);
		}
		let entry = state
			.semaphore_work
			.entries
			.get(&owner.id)
			.ok_or(RuntimeAdmissionError::SemaphoreWorkAbsent)?;
		if &entry.identity != identity {
			return Err(RuntimeAdmissionError::SemaphoreWorkIdentityMismatch);
		}
		if entry.continuation.is_retained() || entry.callers.pending_count() != 0 {
			return Err(RuntimeAdmissionError::SemaphoreWorkStillActive);
		}
		match operation {
			SemaphoreTransactionOperation::Acquisition => {
				state.require_open()?;
				if entry.state != RuntimeResourceWorkState::Acquiring {
					return Err(RuntimeAdmissionError::SemaphoreWorkStateMismatch);
				}
			}
			SemaphoreTransactionOperation::Release => {
				if entry.state != RuntimeResourceWorkState::Held {
					return Err(RuntimeAdmissionError::SemaphoreWorkStateMismatch);
				}
			}
			SemaphoreTransactionOperation::Cleanup => {
				if entry.state != RuntimeResourceWorkState::Closing {
					return Err(RuntimeAdmissionError::SemaphoreWorkStateMismatch);
				}
			}
		}
		let permit = self
			.0
			.retain_work_transaction(&mut state, RuntimeOwnedWorkId::Semaphore(owner.id))?;
		if matches!(operation, SemaphoreTransactionOperation::Release) {
			state
				.semaphore_work
				.entries
				.get_mut(&owner.id)
				.expect("admitted work remains under the same mutex")
				.state = RuntimeResourceWorkState::Closing;
		}
		Ok(permit)
	}
}
