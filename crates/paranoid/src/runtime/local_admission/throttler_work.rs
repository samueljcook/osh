//! Exact local Throttler ownership in the shared runtime registry, without database authority.

use crate::fleet::ThrottlerKey;
use crate::fleet::resource::NativeFleetResource;
use crate::scope::ScopeIdentity;

use super::diagnostic::{RuntimeThrottlerOwnerAbandonedError, RuntimeWorkTerminalError};
use super::{membership::*, *};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct RuntimeThrottlerWorkId(pub(super) u64);

pub(crate) struct RuntimeThrottlerWorkIdentityFields<'a> {
	pub(crate) scope: &'a ScopeIdentity,
	pub(crate) resource: &'a ThrottlerKey,
}
/// Scope/resource diagnostics do not authenticate any stored capability or outcome.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct RuntimeThrottlerWorkIdentity {
	scope: ScopeIdentity,
	resource: ThrottlerKey,
	record: RecordIdentity,
}
impl RuntimeThrottlerWorkIdentity {
	pub(crate) fn new(fields: RuntimeThrottlerWorkIdentityFields<'_>) -> Self {
		Self {
			scope: fields.scope.clone(),
			resource: fields.resource.clone(),
			record: RecordIdentity::new(
				fields.scope.namespace().clone(),
				NativeFleetResource::Throttler(fields.resource).record_key(),
			),
		}
	}
	pub(crate) fn scope(&self) -> &ScopeIdentity {
		&self.scope
	}
	pub(crate) fn resource(&self) -> &ThrottlerKey {
		&self.resource
	}
	pub(crate) fn record(&self) -> &RecordIdentity {
		&self.record
	}
}
impl fmt::Debug for RuntimeThrottlerWorkIdentity {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("RuntimeThrottlerWorkIdentity { scope_and_resource_retained: true }")
	}
}

pub(crate) type RuntimeThrottlerWorkSnapshot = RuntimeWorkSnapshot<ThrottlerWork>;

pub(crate) struct ThrottlerWork;
impl RuntimeWorkKind for ThrottlerWork {
	type Id = RuntimeThrottlerWorkId;
	type Identity = RuntimeThrottlerWorkIdentity;
	type State = RuntimeResourceWorkState;
	type Callers = ExclusiveCallerWork;
	const ABSENT: RuntimeAdmissionError = RuntimeAdmissionError::ThrottlerWorkAbsent;
	const STATE_MISMATCH: RuntimeAdmissionError = RuntimeAdmissionError::ThrottlerWorkStateMismatch;
	const STILL_ACTIVE: RuntimeAdmissionError = RuntimeAdmissionError::ThrottlerWorkStillActive;
	const OWNER_ABANDONED: RuntimeAdmissionFenceReason =
		RuntimeAdmissionFenceReason::ThrottlerWorkAbandoned;
	fn registry(state: &AdmissionState) -> &WorkRegistry<Self> {
		&state.throttler_work
	}
	fn registry_mut(state: &mut AdmissionState) -> &mut WorkRegistry<Self> {
		&mut state.throttler_work
	}
	fn id(value: u64) -> Self::Id {
		RuntimeThrottlerWorkId(value)
	}
	fn owned_id(id: Self::Id) -> RuntimeOwnedWorkId {
		RuntimeOwnedWorkId::Throttler(id)
	}
	fn abandoned_error(
		work_id: Self::Id,
		source: Option<RuntimeWorkTerminalError>,
	) -> RuntimeAdmissionError {
		RuntimeAdmissionError::ThrottlerWorkAbandoned { work_id, source }
	}
	fn owner_abandonment_diagnostic() -> Option<RuntimeWorkTerminalError> {
		Some(RuntimeWorkTerminalError(Arc::new(
			RuntimeThrottlerOwnerAbandonedError,
		)))
	}
}

pub(crate) type RuntimeThrottlerWorkPermit = WorkPermit<ThrottlerWork>;
impl RuntimeThrottlerWorkPermit {
	pub(crate) fn complete(&mut self) -> Result<(), RuntimeAdmissionError> {
		self.complete_local(WorkCompletion::Finished)
	}
}

pub(crate) type RuntimeThrottlerCallerWorkPermit = CallerWorkPermit<ThrottlerWork>;

enum ThrottlerTransactionOperation {
	Acquisition,
	ProbeExtension,
	Outcome,
	Cleanup,
}
impl RuntimeAdmissionTracker {
	/// Pre-stop registration precedes randomness, any library spawn and transaction BEGIN.
	pub(crate) fn try_register_throttler_work(
		&self,
		identity: RuntimeThrottlerWorkIdentity,
	) -> Result<RuntimeThrottlerWorkPermit, RuntimeAdmissionError> {
		self.try_register_work::<ThrottlerWork>(identity)
	}
	/// The fixed backend transfers this exact membership into registration before awaiting BEGIN.
	pub(crate) fn try_admit_throttler_acquisition(
		&self,
		owner: &RuntimeThrottlerWorkPermit,
		identity: &RuntimeThrottlerWorkIdentity,
	) -> Result<RuntimeTransactionPermit, RuntimeAdmissionError> {
		self.admit_throttler_transaction(
			owner,
			identity,
			ThrottlerTransactionOperation::Acquisition,
		)
	}
	/// Only an existing Held owner can extend its probe; Shutdown requires actual caller work.
	pub(crate) fn try_admit_throttler_probe_extension(
		&self,
		owner: &RuntimeThrottlerWorkPermit,
		identity: &RuntimeThrottlerWorkIdentity,
	) -> Result<RuntimeTransactionPermit, RuntimeAdmissionError> {
		self.admit_throttler_transaction(
			owner,
			identity,
			ThrottlerTransactionOperation::ProbeExtension,
		)
	}
	/// Consuming held outcome is closing/accounting after stop, never new caller task permission.
	pub(crate) fn try_admit_throttler_outcome(
		&self,
		owner: &RuntimeThrottlerWorkPermit,
		identity: &RuntimeThrottlerWorkIdentity,
	) -> Result<RuntimeTransactionPermit, RuntimeAdmissionError> {
		self.admit_throttler_transaction(owner, identity, ThrottlerTransactionOperation::Outcome)
	}
	/// Exact preexisting cleanup requires the executor's sealed purpose and current database
	/// authority.
	pub(crate) fn try_admit_throttler_cleanup(
		&self,
		owner: &RuntimeThrottlerWorkPermit,
		identity: &RuntimeThrottlerWorkIdentity,
	) -> Result<RuntimeTransactionPermit, RuntimeAdmissionError> {
		self.admit_throttler_transaction(owner, identity, ThrottlerTransactionOperation::Cleanup)
	}
	fn admit_throttler_transaction(
		&self,
		owner: &RuntimeThrottlerWorkPermit,
		identity: &RuntimeThrottlerWorkIdentity,
		operation: ThrottlerTransactionOperation,
	) -> Result<RuntimeTransactionPermit, RuntimeAdmissionError> {
		if !Arc::ptr_eq(&self.0, &owner.shared) || !owner.is_active() {
			return Err(RuntimeAdmissionError::ThrottlerWorkRuntimeMismatch);
		}
		let mut state = self.0.lock();
		self.0.fence_elapsed(&mut state);
		if state.lifetime_finished() {
			return Err(RuntimeAdmissionError::LifetimeFinished);
		}
		let entry = state
			.throttler_work
			.entries
			.get(&owner.id)
			.ok_or(RuntimeAdmissionError::ThrottlerWorkAbsent)?;
		if &entry.identity != identity {
			return Err(RuntimeAdmissionError::ThrottlerWorkIdentityMismatch);
		}
		if entry.continuation.is_retained()
			|| (entry.callers.pending_count() != 0
				&& !matches!(operation, ThrottlerTransactionOperation::ProbeExtension))
		{
			return Err(RuntimeAdmissionError::ThrottlerWorkStillActive);
		}
		match operation {
			ThrottlerTransactionOperation::Acquisition => {
				state.require_open()?;
				if entry.state != RuntimeResourceWorkState::Acquiring {
					return Err(RuntimeAdmissionError::ThrottlerWorkStateMismatch);
				}
			}
			ThrottlerTransactionOperation::ProbeExtension => {
				if state.stop() == Some(RuntimeAdmissionStop::Fenced)
					|| (state.stop() == Some(RuntimeAdmissionStop::Shutdown)
						&& entry.callers.pending_count() == 0)
				{
					state.require_open()?;
				}
				if entry.state != RuntimeResourceWorkState::Held {
					return Err(RuntimeAdmissionError::ThrottlerWorkStateMismatch);
				}
			}
			ThrottlerTransactionOperation::Outcome => {
				if entry.state != RuntimeResourceWorkState::Held {
					return Err(RuntimeAdmissionError::ThrottlerWorkStateMismatch);
				}
			}
			ThrottlerTransactionOperation::Cleanup => {
				if entry.state != RuntimeResourceWorkState::Closing {
					return Err(RuntimeAdmissionError::ThrottlerWorkStateMismatch);
				}
			}
		}
		let permit = self
			.0
			.retain_work_transaction(&mut state, RuntimeOwnedWorkId::Throttler(owner.id))?;
		if matches!(operation, ThrottlerTransactionOperation::Outcome) {
			state
				.throttler_work
				.entries
				.get_mut(&owner.id)
				.expect("admitted work remains under the same mutex")
				.state = RuntimeResourceWorkState::Closing;
		}
		Ok(permit)
	}
}
