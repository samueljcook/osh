use super::diagnostic::RuntimeWorkTerminalError;
use super::{membership::*, *};

/// Diagnostic membership identity; it provides no database or lease authority.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct RuntimeLeaseWorkId(pub(super) u64);

/// Local completion only. Neither variant certifies database release or physical drainage.
/// The owned executor must establish its ending/cleanup result before completing membership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeLeaseWorkCompletion {
	Finished,
	Unresolved,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeLeaseContinuation {
	RenewHeld,
	ReleaseCleanup,
}

enum LeaseTransactionOperation {
	Acquisition,
	CachePublication,
	OnceCompletion,
	Continuation(RuntimeLeaseContinuation),
}

pub(crate) type RuntimeLeaseWorkSnapshot = RuntimeWorkSnapshot<LeaseWork>;

pub(crate) struct LeaseWork;
impl RuntimeWorkKind for LeaseWork {
	type Id = RuntimeLeaseWorkId;
	type Identity = RecordIdentity;
	type State = RuntimeResourceWorkState;
	type Callers = SharedCallerWork;
	const ABSENT: RuntimeAdmissionError = RuntimeAdmissionError::LeaseWorkAbsent;
	const STATE_MISMATCH: RuntimeAdmissionError = RuntimeAdmissionError::LeaseWorkStateMismatch;
	const STILL_ACTIVE: RuntimeAdmissionError = RuntimeAdmissionError::LeaseWorkStillActive;
	const OWNER_ABANDONED: RuntimeAdmissionFenceReason =
		RuntimeAdmissionFenceReason::LeaseWorkAbandoned;
	fn registry(state: &AdmissionState) -> &WorkRegistry<Self> {
		&state.lease_work
	}
	fn registry_mut(state: &mut AdmissionState) -> &mut WorkRegistry<Self> {
		&mut state.lease_work
	}
	fn id(value: u64) -> Self::Id {
		RuntimeLeaseWorkId(value)
	}
	fn owned_id(id: Self::Id) -> RuntimeOwnedWorkId {
		RuntimeOwnedWorkId::Lease(id)
	}
	fn abandoned_error(
		work_id: Self::Id,
		source: Option<RuntimeWorkTerminalError>,
	) -> RuntimeAdmissionError {
		RuntimeAdmissionError::LeaseWorkAbandoned { work_id, source }
	}
	fn owner_abandonment_diagnostic() -> Option<RuntimeWorkTerminalError> {
		None
	}
}

pub(crate) type RuntimeLeaseWorkPermit = WorkPermit<LeaseWork>;
impl RuntimeLeaseWorkPermit {
	pub(crate) fn runtime_view(&self) -> RuntimeWorkView<RuntimeLeaseWorkSnapshot> {
		RuntimeAdmissionTracker(Arc::clone(&self.shared)).work_view(self.id)
	}
	pub(crate) async fn wait_for_runtime_change(
		&self,
		previous: &RuntimeWorkView<RuntimeLeaseWorkSnapshot>,
	) -> RuntimeWorkView<RuntimeLeaseWorkSnapshot> {
		RuntimeAdmissionTracker(Arc::clone(&self.shared))
			.wait_for_work_change(self.id, previous)
			.await
	}
	pub(crate) fn complete(
		&mut self,
		outcome: RuntimeLeaseWorkCompletion,
	) -> Result<(), RuntimeAdmissionError> {
		self.complete_local(match outcome {
			RuntimeLeaseWorkCompletion::Finished => WorkCompletion::Finished,
			RuntimeLeaseWorkCompletion::Unresolved => WorkCompletion::Unresolved(None),
		})
	}
}

pub(crate) type RuntimeLeaseCallerWorkPermit = CallerWorkPermit<LeaseWork>;
pub(crate) use crate::db::transaction_permits::LeaseCallerTransactionAssociationError;

impl crate::db::transaction_permits::LeaseCallerPermitHandle for RuntimeLeaseCallerWorkPermit {
	fn runtime_identity(&self) -> usize {
		Arc::as_ptr(&self.shared) as usize
	}

	fn lease_work_id(&self) -> u64 {
		self.id.0
	}
}

impl RuntimeLeaseCallerWorkPermit {
	fn require_owner(
		&self,
		owner: &RuntimeLeaseWorkPermit,
	) -> Result<(), LeaseCallerTransactionAssociationError> {
		if !Arc::ptr_eq(&self.shared, &owner.shared) {
			return Err(LeaseCallerTransactionAssociationError::RuntimeMismatch);
		}
		if self.id != owner.id {
			return Err(LeaseCallerTransactionAssociationError::WorkMismatch);
		}
		Ok(())
	}

	/// Checks local ownership only; it neither admits work nor proves a transaction ending.
	#[cfg(test)]
	pub(crate) fn require_transaction_association(
		&self,
		transaction: &RuntimeTransactionPermit,
	) -> Result<(), LeaseCallerTransactionAssociationError> {
		crate::db::transaction_permits::LeaseCallerPermitHandle::check_transaction_association(
			self,
			transaction,
		)
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RuntimeLeaseFinalPurposeAdmissionError {
	#[error("Lease final-purpose caller does not belong to the owner")]
	Caller(#[source] LeaseCallerTransactionAssociationError),
	#[error("Lease final-purpose local continuation was rejected")]
	Admission(#[source] RuntimeAdmissionError),
}

impl RuntimeAdmissionTracker {
	pub(crate) fn try_admit_lease_cache_publication(
		&self,
		owner: &RuntimeLeaseWorkPermit,
		identity: &RecordIdentity,
		caller: &RuntimeLeaseCallerWorkPermit,
	) -> Result<RuntimeTransactionPermit, RuntimeLeaseFinalPurposeAdmissionError> {
		self.admit_lease_final_purpose(
			owner,
			identity,
			caller,
			LeaseTransactionOperation::CachePublication,
		)
	}
	pub(crate) fn try_admit_lease_once_completion(
		&self,
		owner: &RuntimeLeaseWorkPermit,
		identity: &RecordIdentity,
		caller: &RuntimeLeaseCallerWorkPermit,
	) -> Result<RuntimeTransactionPermit, RuntimeLeaseFinalPurposeAdmissionError> {
		self.admit_lease_final_purpose(
			owner,
			identity,
			caller,
			LeaseTransactionOperation::OnceCompletion,
		)
	}
	fn admit_lease_final_purpose(
		&self,
		owner: &RuntimeLeaseWorkPermit,
		identity: &RecordIdentity,
		caller: &RuntimeLeaseCallerWorkPermit,
		operation: LeaseTransactionOperation,
	) -> Result<RuntimeTransactionPermit, RuntimeLeaseFinalPurposeAdmissionError> {
		caller
			.require_owner(owner)
			.map_err(RuntimeLeaseFinalPurposeAdmissionError::Caller)?;
		self.admit_lease_transaction(owner, identity, operation)
			.map_err(RuntimeLeaseFinalPurposeAdmissionError::Admission)
	}

	/// Registers actual library lease work before randomness, SQL, or shutdown.
	pub(crate) fn try_register_lease_work(
		&self,
		identity: RecordIdentity,
	) -> Result<RuntimeLeaseWorkPermit, RuntimeAdmissionError> {
		self.try_register_work::<LeaseWork>(identity)
	}

	/// Only an owned control transaction may consume this exact lease acquisition permit.
	pub(crate) fn try_admit_lease_acquisition(
		&self,
		owner: &RuntimeLeaseWorkPermit,
		identity: &RecordIdentity,
	) -> Result<RuntimeTransactionPermit, RuntimeAdmissionError> {
		self.admit_lease_transaction(owner, identity, LeaseTransactionOperation::Acquisition)
	}

	/// Local continuation only; actual installed-contract and committed participant checks remain
	/// mandatory. The permit must remain with one owned lease control transaction through its
	/// actual ending.
	pub(crate) fn try_admit_lease_continuation(
		&self,
		owner: &RuntimeLeaseWorkPermit,
		identity: &RecordIdentity,
		operation: RuntimeLeaseContinuation,
	) -> Result<RuntimeTransactionPermit, RuntimeAdmissionError> {
		self.admit_lease_transaction(
			owner,
			identity,
			LeaseTransactionOperation::Continuation(operation),
		)
	}

	fn admit_lease_transaction(
		&self,
		owner: &RuntimeLeaseWorkPermit,
		identity: &RecordIdentity,
		operation: LeaseTransactionOperation,
	) -> Result<RuntimeTransactionPermit, RuntimeAdmissionError> {
		if !Arc::ptr_eq(&self.0, &owner.shared) || !owner.is_active() {
			return Err(RuntimeAdmissionError::LeaseWorkRuntimeMismatch);
		}
		let mut state = self.0.lock();
		self.0.fence_elapsed(&mut state);
		if state.lifetime_finished() {
			return Err(RuntimeAdmissionError::LifetimeFinished);
		}
		let entry = state
			.lease_work
			.entries
			.get(&owner.id)
			.ok_or(RuntimeAdmissionError::LeaseWorkAbsent)?;
		if &entry.identity != identity {
			return Err(RuntimeAdmissionError::LeaseWorkIdentityMismatch);
		}
		if entry.continuation.is_retained() {
			return Err(RuntimeAdmissionError::LeaseWorkStillActive);
		}
		match operation {
			LeaseTransactionOperation::Acquisition => {
				state.require_open()?;
				if entry.state != RuntimeResourceWorkState::Acquiring {
					return Err(RuntimeAdmissionError::LeaseWorkStateMismatch);
				}
			}
			LeaseTransactionOperation::CachePublication
			| LeaseTransactionOperation::OnceCompletion => {
				if state.stop() == Some(RuntimeAdmissionStop::Fenced) {
					return Err(RuntimeAdmissionError::Stopped {
						stop: RuntimeAdmissionStop::Fenced,
					});
				}
				if entry.state != RuntimeResourceWorkState::Held
					|| entry.callers.pending_count() == 0
				{
					return Err(RuntimeAdmissionError::LeaseWorkStateMismatch);
				}
			}
			LeaseTransactionOperation::Continuation(RuntimeLeaseContinuation::RenewHeld) => {
				if state.stop() == Some(RuntimeAdmissionStop::Fenced) {
					return Err(RuntimeAdmissionError::Stopped {
						stop: RuntimeAdmissionStop::Fenced,
					});
				}
				if entry.state != RuntimeResourceWorkState::Held
					|| (state.stop() == Some(RuntimeAdmissionStop::Shutdown)
						&& entry.callers.pending_count() == 0)
				{
					return Err(RuntimeAdmissionError::LeaseWorkStateMismatch);
				}
			}
			LeaseTransactionOperation::Continuation(RuntimeLeaseContinuation::ReleaseCleanup) => {
				if entry.state != RuntimeResourceWorkState::Closing
					|| entry.callers.pending_count() != 0
				{
					return Err(RuntimeAdmissionError::LeaseWorkStillActive);
				}
			}
		}
		let permit = self
			.0
			.retain_work_transaction(&mut state, RuntimeOwnedWorkId::Lease(owner.id))?;
		Ok(permit)
	}
}
