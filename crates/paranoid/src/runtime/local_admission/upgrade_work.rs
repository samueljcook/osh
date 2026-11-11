//! Actual upgrade membership in the same local runtime as transactions and held leases.

use primitives_rs::id::time_sortable::SortableId;

use super::diagnostic::RuntimeWorkTerminalError;
use super::{membership::*, *};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct RuntimeUpgradeWorkId(pub(super) u64);

/// The identity of one recorded installation upgrade.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct RuntimeUpgradeWorkIdentity {
	pub(crate) installation_id: SortableId,
	pub(crate) request_id: SortableId,
}

impl RuntimeUpgradeWorkIdentity {
	pub(crate) fn from_parts(installation_id: SortableId, request_id: SortableId) -> Self {
		Self {
			installation_id,
			request_id,
		}
	}
}
impl fmt::Debug for RuntimeUpgradeWorkIdentity {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("RuntimeUpgradeWorkIdentity { installation_and_request_retained: true }")
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeUpgradeWorkState {
	Running,
	Stopping,
	Finished,
	Abandoned,
	Unresolved,
}
impl WorkPhase for RuntimeUpgradeWorkState {
	const INITIAL: Self = Self::Running;
	const FINISHED: Self = Self::Finished;
	const ABANDONED: Self = Self::Abandoned;
	const UNRESOLVED: Self = Self::Unresolved;
	fn permits_completion(self) -> bool {
		matches!(self, Self::Running | Self::Stopping)
	}
}

pub(crate) type RuntimeUpgradeWorkSnapshot = RuntimeWorkSnapshot<UpgradeWork>;

pub(crate) struct UpgradeWork;
impl RuntimeWorkKind for UpgradeWork {
	type Id = RuntimeUpgradeWorkId;
	type Identity = RuntimeUpgradeWorkIdentity;
	type State = RuntimeUpgradeWorkState;
	type Callers = NoCallerWork;
	const ABSENT: RuntimeAdmissionError = RuntimeAdmissionError::UpgradeWorkAbsent;
	const STATE_MISMATCH: RuntimeAdmissionError = RuntimeAdmissionError::UpgradeWorkStateMismatch;
	const STILL_ACTIVE: RuntimeAdmissionError = RuntimeAdmissionError::UpgradeWorkStillActive;
	const OWNER_ABANDONED: RuntimeAdmissionFenceReason =
		RuntimeAdmissionFenceReason::UpgradeWorkAbandoned;
	fn registry(state: &AdmissionState) -> &WorkRegistry<Self> {
		&state.upgrade_work
	}
	fn registry_mut(state: &mut AdmissionState) -> &mut WorkRegistry<Self> {
		&mut state.upgrade_work
	}
	fn id(value: u64) -> Self::Id {
		RuntimeUpgradeWorkId(value)
	}
	fn owned_id(id: Self::Id) -> RuntimeOwnedWorkId {
		RuntimeOwnedWorkId::Upgrade(id)
	}
	fn abandoned_error(
		work_id: Self::Id,
		source: Option<RuntimeWorkTerminalError>,
	) -> RuntimeAdmissionError {
		RuntimeAdmissionError::UpgradeWorkAbandoned { work_id, source }
	}
	fn owner_abandonment_diagnostic() -> Option<RuntimeWorkTerminalError> {
		None
	}
}

pub(crate) type RuntimeUpgradeWorkPermit = WorkPermit<UpgradeWork>;
impl RuntimeUpgradeWorkPermit {
	pub(crate) fn begin_stopping(&self) -> Result<(), RuntimeAdmissionError> {
		let mut state = self.shared.lock();
		let entry = state
			.upgrade_work
			.entries
			.get_mut(&self.id)
			.ok_or(RuntimeAdmissionError::UpgradeWorkAbsent)?;
		match entry.state {
			RuntimeUpgradeWorkState::Running | RuntimeUpgradeWorkState::Stopping => {
				entry.state = RuntimeUpgradeWorkState::Stopping
			}
			_ => return Err(RuntimeAdmissionError::UpgradeWorkStateMismatch),
		}
		self.shared.notify_work_changed(&entry.changed);
		Ok(())
	}
	pub(crate) fn complete(&mut self) -> Result<(), RuntimeAdmissionError> {
		self.complete_local(WorkCompletion::Finished)
	}
}
impl RuntimeAdmissionTracker {
	/// Registers actual requested upgrade ownership before spawning or executing SQL.
	pub(crate) fn try_register_upgrade_work(
		&self,
		identity: RuntimeUpgradeWorkIdentity,
	) -> Result<RuntimeUpgradeWorkPermit, RuntimeAdmissionError> {
		self.try_register_work::<UpgradeWork>(identity)
	}
	/// Only this exact pre-stop membership can retain one actual owned continuation transaction.
	pub(crate) fn try_admit_upgrade_continuation(
		&self,
		owner: &RuntimeUpgradeWorkPermit,
		identity: RuntimeUpgradeWorkIdentity,
	) -> Result<RuntimeTransactionPermit, RuntimeAdmissionError> {
		if !Arc::ptr_eq(&self.0, &owner.shared) || !owner.is_active() {
			return Err(RuntimeAdmissionError::UpgradeWorkRuntimeMismatch);
		}
		let mut state = self.0.lock();
		self.0.fence_elapsed(&mut state);
		state.require_open()?;
		let entry = state
			.upgrade_work
			.entries
			.get(&owner.id)
			.ok_or(RuntimeAdmissionError::UpgradeWorkAbsent)?;
		if entry.identity != identity {
			return Err(RuntimeAdmissionError::UpgradeWorkIdentityMismatch);
		}
		if entry.state != RuntimeUpgradeWorkState::Running {
			return Err(RuntimeAdmissionError::UpgradeWorkStateMismatch);
		}
		if entry.continuation.is_retained() {
			return Err(RuntimeAdmissionError::UpgradeWorkStillActive);
		}
		let permit = self
			.0
			.retain_work_transaction(&mut state, RuntimeOwnedWorkId::Upgrade(owner.id))?;
		Ok(permit)
	}
}
