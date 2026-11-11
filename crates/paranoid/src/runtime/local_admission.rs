//! Local admission and owned work drainage for one scoped native runtime.
//!
//! A transaction permit must remain with the owned transaction through background COMMIT or
//! ROLLBACK completion. Lease, upgrade, Throttler and Semaphore work remain registered until
//! explicit completion; abandoned ownership remains pending. This registry neither executes
//! cleanup nor proves physical drainage.

pub(crate) mod diagnostic;
pub(crate) mod lease_work;
pub(in crate::runtime) mod lifecycle;
pub(crate) mod membership;
pub(crate) mod record_turns;
pub(crate) mod semaphore_work;
pub(crate) mod task;
pub(crate) mod throttler_work;
pub(crate) mod upgrade_work;

use diagnostic::RuntimeWorkTerminalError;
use lease_work::{LeaseWork, RuntimeLeaseWorkId, RuntimeLeaseWorkSnapshot};
use semaphore_work::{RuntimeSemaphoreWorkId, RuntimeSemaphoreWorkSnapshot, SemaphoreWork};
use throttler_work::{RuntimeThrottlerWorkId, RuntimeThrottlerWorkSnapshot, ThrottlerWork};
use upgrade_work::{RuntimeUpgradeWorkId, RuntimeUpgradeWorkSnapshot, UpgradeWork};

use lifecycle::AdmissionLifecycle;
use membership::{LocalWorkRecord, WorkRegistry};
use std::fmt;
use std::sync::{Arc, Mutex};

use tokio::sync::{Notify, watch};
use tokio::time::Instant;

use crate::records::RecordIdentity;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeAdmissionStop {
	Shutdown,
	Fenced,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeAdmissionFenceReason {
	ConfirmedLifetimeElapsed,
	ExplicitFence,
	LeaseWorkAbandoned,
	LeaseTaskAbandoned,
	UpgradeWorkAbandoned,
	UpgradeTaskAbandoned,
	ThrottlerWorkAbandoned,
	ThrottlerTaskAbandoned,
	SemaphoreWorkAbandoned,
	SemaphoreTaskAbandoned,
}

pub(crate) struct AdmissionState {
	lifecycle: AdmissionLifecycle,
	pending_transaction_count: usize,
	confirmed_until: Instant,
	/// The work for which a task future has ended but the registry has not yet observed the task
	/// handle as finished. Reaping visits only this work so that its cost does not grow with
	/// the number of registered work entries.
	ended_tasks: Vec<RuntimeOwnedWorkId>,
	lease_work: WorkRegistry<LeaseWork>,
	upgrade_work: WorkRegistry<UpgradeWork>,
	throttler_work: WorkRegistry<ThrottlerWork>,
	semaphore_work: WorkRegistry<SemaphoreWork>,
}

struct SharedAdmission {
	state: Mutex<AdmissionState>,
	/// Wakes the waiters that observe the whole registry, such as local drainage. Every change
	/// notifies it. A work entry's own waiters wait on the entry's notification instead, so
	/// that a change wakes only the waiters that it concerns.
	changed: Notify,
	/// The runtime-wide status, which every work waiter also observes.
	status: watch::Sender<RuntimeAdmissionStatus>,
	/// The turns of the acquisitions of each record; see [`record_turns`].
	record_turns: Mutex<record_turns::RecordTurnQueues>,
}

impl AdmissionState {
	fn status(&self) -> RuntimeAdmissionStatus {
		RuntimeAdmissionStatus {
			lifecycle: self.lifecycle,
			confirmed_until: self.confirmed_until,
		}
	}

	fn require_open(&self) -> Result<(), RuntimeAdmissionError> {
		self.lifecycle.require_open()
	}
	fn stop(&self) -> Option<RuntimeAdmissionStop> {
		self.lifecycle.stop()
	}
	fn lifetime_finished(&self) -> bool {
		self.lifecycle.is_finished()
	}

	fn has_pending_work(&self) -> bool {
		self.lease_work.has_pending_work()
			|| self.upgrade_work.has_pending_work()
			|| self.throttler_work.has_pending_work()
			|| self.semaphore_work.has_pending_work()
	}

	fn local_work(&self, id: RuntimeOwnedWorkId) -> Option<&dyn LocalWorkRecord> {
		match id {
			RuntimeOwnedWorkId::Lease(id) => self
				.lease_work
				.entries
				.get(&id)
				.map(|entry| entry as &dyn LocalWorkRecord),
			RuntimeOwnedWorkId::Upgrade(id) => self
				.upgrade_work
				.entries
				.get(&id)
				.map(|entry| entry as &dyn LocalWorkRecord),
			RuntimeOwnedWorkId::Throttler(id) => self
				.throttler_work
				.entries
				.get(&id)
				.map(|entry| entry as &dyn LocalWorkRecord),
			RuntimeOwnedWorkId::Semaphore(id) => self
				.semaphore_work
				.entries
				.get(&id)
				.map(|entry| entry as &dyn LocalWorkRecord),
		}
	}
	fn local_work_mut(&mut self, id: RuntimeOwnedWorkId) -> Option<&mut dyn LocalWorkRecord> {
		match id {
			RuntimeOwnedWorkId::Lease(id) => self
				.lease_work
				.entries
				.get_mut(&id)
				.map(|entry| entry as &mut dyn LocalWorkRecord),
			RuntimeOwnedWorkId::Upgrade(id) => self
				.upgrade_work
				.entries
				.get_mut(&id)
				.map(|entry| entry as &mut dyn LocalWorkRecord),
			RuntimeOwnedWorkId::Throttler(id) => self
				.throttler_work
				.entries
				.get_mut(&id)
				.map(|entry| entry as &mut dyn LocalWorkRecord),
			RuntimeOwnedWorkId::Semaphore(id) => self
				.semaphore_work
				.entries
				.get_mut(&id)
				.map(|entry| entry as &mut dyn LocalWorkRecord),
		}
	}
	fn drainage_action(&self) -> LocalDrainageAction {
		if self.stop().is_none() {
			return LocalDrainageAction::Reject(RuntimeAdmissionError::StillAccepting);
		}
		if let Some(error) = self
			.upgrade_work
			.abandoned_error()
			.or_else(|| self.lease_work.abandoned_error())
			.or_else(|| self.throttler_work.abandoned_error())
			.or_else(|| self.semaphore_work.abandoned_error())
		{
			return LocalDrainageAction::Reject(error);
		}
		if let Some(id) = self
			.lease_work
			.joinable_task()
			.or_else(|| self.upgrade_work.joinable_task())
			.or_else(|| self.throttler_work.joinable_task())
			.or_else(|| self.semaphore_work.joinable_task())
		{
			return LocalDrainageAction::Join(id);
		}
		if self.pending_transaction_count == 0
			&& self.lease_work.is_drained()
			&& self.upgrade_work.is_drained()
			&& self.throttler_work.is_drained()
			&& self.semaphore_work.is_drained()
		{
			return LocalDrainageAction::Drained;
		}
		LocalDrainageAction::Wait
	}
}

impl SharedAdmission {
	/// Wakes this work's waiters, when retained, and the whole registry's waiters.
	fn notify_owned_work_changed(&self, changed: Option<&Notify>) {
		match changed {
			Some(changed) => self.notify_work_changed(changed),
			None => self.changed.notify_waiters(),
		}
	}

	/// Domain admission checks the exact identity, phase, and operation under the same mutex.
	/// Retention then commits both local ownership facts before a transaction can start.
	fn retain_work_transaction(
		self: &Arc<Self>,
		state: &mut AdmissionState,
		work: RuntimeOwnedWorkId,
	) -> Result<RuntimeTransactionPermit, RuntimeAdmissionError> {
		let pending = state
			.pending_transaction_count
			.checked_add(1)
			.ok_or(RuntimeAdmissionError::CountExhausted)?;
		let changed = state
			.local_work_mut(work)
			.expect("admitted work remains under the same mutex")
			.retain_continuation();
		state.pending_transaction_count = pending;
		self.notify_work_changed(&changed);
		Ok(RuntimeTransactionPermit {
			shared: Arc::clone(self),
			work: Some(work),
		})
	}

	fn lock(&self) -> std::sync::MutexGuard<'_, AdmissionState> {
		self.state
			.lock()
			.expect("runtime admission state is not poisoned")
	}

	fn fence(&self, state: &mut AdmissionState, reason: RuntimeAdmissionFenceReason) {
		if state.lifecycle.fence(reason) {
			self.publish_status(state);
			self.changed.notify_waiters();
		}
	}

	/// Publishes the runtime-wide status after a change of `state`'s status fields.
	fn publish_status(&self, state: &AdmissionState) {
		let status = state.status();
		self.status.send_if_modified(|published| {
			let modified = *published != status;
			*published = status;
			modified
		});
	}

	/// Uses `work_changed` to wake one work entry's waiters and also wakes the waiters that
	/// observe the whole registry.
	fn notify_work_changed(&self, work_changed: &Notify) {
		work_changed.notify_waiters();
		self.changed.notify_waiters();
	}

	fn fence_elapsed(&self, state: &mut AdmissionState) {
		self.reap_ready_tasks(state);
		if !state.lifetime_finished() && Instant::now() >= state.confirmed_until {
			self.fence(state, RuntimeAdmissionFenceReason::ConfirmedLifetimeElapsed);
		}
	}
}

/// One local runtime's admission lifetime. Construction belongs after committed registration.
#[derive(Clone)]
pub(crate) struct RuntimeAdmissionTracker(Arc<SharedAdmission>);

impl RuntimeAdmissionTracker {
	pub(in crate::runtime) fn new(confirmed_until: Instant) -> Self {
		let state = AdmissionState {
			lifecycle: AdmissionLifecycle::Open,
			pending_transaction_count: 0,
			confirmed_until,
			ended_tasks: Vec::new(),
			lease_work: WorkRegistry::new(),
			upgrade_work: WorkRegistry::new(),
			throttler_work: WorkRegistry::new(),
			semaphore_work: WorkRegistry::new(),
		};
		let status = watch::Sender::new(state.status());
		Self(Arc::new(SharedAdmission {
			state: Mutex::new(state),
			changed: Notify::new(),
			status,
			record_turns: Mutex::default(),
		}))
	}

	/// Serializes a new transaction permit with shutdown and local fencing.
	pub(crate) fn try_admit(&self) -> Result<RuntimeTransactionPermit, RuntimeAdmissionError> {
		let mut state = self.0.lock();
		self.0.fence_elapsed(&mut state);
		state.require_open()?;
		state.pending_transaction_count = state
			.pending_transaction_count
			.checked_add(1)
			.ok_or(RuntimeAdmissionError::CountExhausted)?;
		self.0.changed.notify_waiters();
		Ok(RuntimeTransactionPermit {
			shared: Arc::clone(&self.0),
			work: None,
		})
	}

	/// Stops future admission without ending any application's transaction.
	/// Fencing remains observable even when it follows an ordinary shutdown request.
	pub(crate) fn stop(&self, stop: RuntimeAdmissionStop) {
		let mut state = self.0.lock();
		self.0.fence_elapsed(&mut state);
		state.lifecycle.stop_admission(stop);
		self.0.publish_status(&state);
		self.0.changed.notify_waiters();
	}

	/// The runtime-wide status. Reading it fences the runtime when its confirmed lifetime has
	/// elapsed.
	pub(crate) fn status(&self) -> RuntimeAdmissionStatus {
		let mut state = self.0.lock();
		self.0.fence_elapsed(&mut state);
		state.status()
	}

	/// Waits until the runtime-wide status differs from `previous`, including when the
	/// confirmed lifetime elapses, and returns the new status. A cancelled waiter changes no
	/// ownership.
	pub(crate) async fn wait_for_status_change(
		&self,
		previous: RuntimeAdmissionStatus,
	) -> RuntimeAdmissionStatus {
		loop {
			let mut published = self.0.status.subscribe();
			let current = self.status();
			if current != previous {
				return current;
			}
			tokio::select! {
				_ = published.changed() => {},
				_ = tokio::time::sleep_until(current.confirmed_until), if current.awaits_lifetime_end() => {},
			}
		}
	}

	/// The runtime-wide status and the entry of the work `id`.
	pub(crate) fn work_view<Id: RuntimeWorkId>(&self, id: Id) -> RuntimeWorkView<Id::Snapshot> {
		let mut state = self.0.lock();
		self.0.fence_elapsed(&mut state);
		RuntimeWorkView::new(state.status(), id.entry_snapshot(&state))
	}

	/// Waits until the runtime-wide status or the entry of the work `id` differs from
	/// `previous`, including when the confirmed lifetime elapses, and returns the new view.
	/// Only a change of the runtime-wide status or of this entry wakes the waiter. A cancelled
	/// waiter changes no ownership.
	pub(crate) async fn wait_for_work_change<Id: RuntimeWorkId>(
		&self,
		id: Id,
		previous: &RuntimeWorkView<Id::Snapshot>,
	) -> RuntimeWorkView<Id::Snapshot> {
		loop {
			let mut published = self.0.status.subscribe();
			let work_changed = id.entry_changed(&self.0.lock());
			let work_notified = work_changed.as_deref().map(Notify::notified);
			tokio::pin!(work_notified);
			if let Some(notified) = work_notified.as_mut().as_pin_mut() {
				notified.enable();
			}
			let current = self.work_view(id);
			if current != *previous {
				return current;
			}
			let work_wait = async {
				match work_notified.as_mut().as_pin_mut() {
					Some(notified) => notified.await,
					None => std::future::pending().await,
				}
			};
			tokio::select! {
				() = work_wait => {},
				_ = published.changed() => {},
				_ = tokio::time::sleep_until(current.confirmed_until), if current.status().awaits_lifetime_end() => {},
			}
		}
	}

	pub(crate) fn snapshot(&self) -> RuntimeAdmissionSnapshot {
		let mut state = self.0.lock();
		self.0.fence_elapsed(&mut state);
		RuntimeAdmissionSnapshot {
			lifecycle: state.lifecycle,
			pending_transaction_count: state.pending_transaction_count,
			confirmed_until: state.confirmed_until,
			lease_work: state
				.lease_work
				.entries
				.iter()
				.map(|(id, entry)| entry.snapshot(*id))
				.collect(),
			upgrade_work: state
				.upgrade_work
				.entries
				.iter()
				.map(|(id, entry)| entry.snapshot(*id))
				.collect(),
			throttler_work: state
				.throttler_work
				.entries
				.iter()
				.map(|(id, entry)| entry.snapshot(*id))
				.collect(),
			semaphore_work: state
				.semaphore_work
				.entries
				.iter()
				.map(|(id, entry)| entry.snapshot(*id))
				.collect(),
		}
	}

	/// Advances confirmed lifetime only before the previous deadline and without reviving a stop.
	/// Shutdown drainage may retain participation; Fenced admission can never be renewed locally.
	pub(crate) fn confirm_until(
		&self,
		confirmed_until: Instant,
	) -> Result<(), RuntimeAdmissionError> {
		let mut state = self.0.lock();
		self.0.fence_elapsed(&mut state);
		if state.lifetime_finished() {
			return Err(RuntimeAdmissionError::LifetimeFinished);
		}
		if state.stop() == Some(RuntimeAdmissionStop::Fenced) {
			return Err(RuntimeAdmissionError::Stopped {
				stop: RuntimeAdmissionStop::Fenced,
			});
		}
		if confirmed_until <= state.confirmed_until {
			return Err(RuntimeAdmissionError::DeadlineNotAdvanced);
		}
		state.confirmed_until = confirmed_until;
		self.0.publish_status(&state);
		self.0.changed.notify_waiters();
		Ok(())
	}

	/// Ends only local admission after stopped admission, local work drainage, and renewal join.
	/// This state does not certify physical transaction ending or lease cleanup.
	pub(crate) fn finish_local_lifetime(&self) -> Result<(), RuntimeAdmissionError> {
		let mut state = self.0.lock();
		if state.stop().is_none()
			|| state.pending_transaction_count != 0
			|| state.has_pending_work()
		{
			return Err(RuntimeAdmissionError::LifetimeStillActive);
		}
		state.lifecycle.finish()?;
		self.0.publish_status(&state);
		self.0.changed.notify_waiters();
		Ok(())
	}

	/// Waits for local transaction and work ownership after stop. Abandonment rejects drainage.
	/// Cancellation leaves stop and ownership in place; completion proves no server-side ending.
	pub(crate) async fn wait_until_drained(&self) -> Result<(), RuntimeAdmissionError> {
		loop {
			let notified = self.0.changed.notified();
			tokio::pin!(notified);
			notified.as_mut().enable();
			let action = {
				let mut state = self.0.lock();
				self.0.fence_elapsed(&mut state);
				state.drainage_action()
			};
			match action {
				LocalDrainageAction::Reject(error) => return Err(error),
				LocalDrainageAction::Join(id) => self.join_owned_task(id).await?,
				LocalDrainageAction::Drained => return Ok(()),
				LocalDrainageAction::Wait => notified.await,
			}
		}
	}
}

impl fmt::Debug for RuntimeAdmissionTracker {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("RuntimeAdmissionTracker")
			.field("state", &self.snapshot())
			.finish()
	}
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeAdmissionSnapshot {
	pub(in crate::runtime) lifecycle: AdmissionLifecycle,
	pub(crate) pending_transaction_count: usize,
	pub(crate) confirmed_until: Instant,
	pub(crate) lease_work: Vec<RuntimeLeaseWorkSnapshot>,
	pub(crate) upgrade_work: Vec<RuntimeUpgradeWorkSnapshot>,
	pub(crate) throttler_work: Vec<RuntimeThrottlerWorkSnapshot>,
	pub(crate) semaphore_work: Vec<RuntimeSemaphoreWorkSnapshot>,
}

impl RuntimeAdmissionSnapshot {
	pub(crate) fn stop(&self) -> Option<RuntimeAdmissionStop> {
		self.lifecycle.stop()
	}
	pub(crate) fn fence_reason(&self) -> Option<RuntimeAdmissionFenceReason> {
		self.lifecycle.fence_reason()
	}
}

/// The runtime-wide admission state, without any work entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeAdmissionStatus {
	lifecycle: AdmissionLifecycle,
	pub(crate) confirmed_until: Instant,
}

impl RuntimeAdmissionStatus {
	/// Whether the status changes when the confirmed lifetime elapses: an elapsed lifetime
	/// fences a runtime when it is not already fenced and its lifetime has not finished.
	fn awaits_lifetime_end(&self) -> bool {
		!self.lifetime_finished() && self.stop() != Some(RuntimeAdmissionStop::Fenced)
	}
	pub(crate) fn stop(&self) -> Option<RuntimeAdmissionStop> {
		self.lifecycle.stop()
	}
	pub(crate) fn fence_reason(&self) -> Option<RuntimeAdmissionFenceReason> {
		self.lifecycle.fence_reason()
	}
	pub(crate) fn lifetime_finished(&self) -> bool {
		self.lifecycle.is_finished()
	}
}

/// The runtime-wide admission state and the entry of one work, which is absent after the work
/// completes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeWorkView<W> {
	lifecycle: AdmissionLifecycle,
	pub(crate) confirmed_until: Instant,
	pub(crate) work: Option<W>,
}

impl<W> RuntimeWorkView<W> {
	fn new(status: RuntimeAdmissionStatus, work: Option<W>) -> Self {
		Self {
			lifecycle: status.lifecycle,
			confirmed_until: status.confirmed_until,
			work,
		}
	}

	pub(crate) fn status(&self) -> RuntimeAdmissionStatus {
		RuntimeAdmissionStatus {
			lifecycle: self.lifecycle,
			confirmed_until: self.confirmed_until,
		}
	}
	pub(crate) fn stop(&self) -> Option<RuntimeAdmissionStop> {
		self.lifecycle.stop()
	}
	pub(crate) fn lifetime_finished(&self) -> bool {
		self.lifecycle.is_finished()
	}
}

/// The identifier of one registered work, which selects the work's entry in the registry.
pub(crate) trait RuntimeWorkId: Copy {
	/// The observation of the work's entry.
	type Snapshot: Clone + Eq;

	fn entry_snapshot(self, state: &AdmissionState) -> Option<Self::Snapshot>;

	/// The notification that every change of the work's entry wakes.
	fn entry_changed(self, state: &AdmissionState) -> Option<Arc<Notify>>;
}

impl RuntimeWorkId for RuntimeLeaseWorkId {
	type Snapshot = RuntimeLeaseWorkSnapshot;

	fn entry_snapshot(self, state: &AdmissionState) -> Option<Self::Snapshot> {
		state
			.lease_work
			.entries
			.get(&self)
			.map(|entry| entry.snapshot(self))
	}

	fn entry_changed(self, state: &AdmissionState) -> Option<Arc<Notify>> {
		state
			.lease_work
			.entries
			.get(&self)
			.map(|entry| Arc::clone(&entry.changed))
	}
}

impl RuntimeWorkId for RuntimeThrottlerWorkId {
	type Snapshot = RuntimeThrottlerWorkSnapshot;

	fn entry_snapshot(self, state: &AdmissionState) -> Option<Self::Snapshot> {
		state
			.throttler_work
			.entries
			.get(&self)
			.map(|entry| entry.snapshot(self))
	}

	fn entry_changed(self, state: &AdmissionState) -> Option<Arc<Notify>> {
		state
			.throttler_work
			.entries
			.get(&self)
			.map(|entry| Arc::clone(&entry.changed))
	}
}

impl RuntimeWorkId for RuntimeSemaphoreWorkId {
	type Snapshot = RuntimeSemaphoreWorkSnapshot;

	fn entry_snapshot(self, state: &AdmissionState) -> Option<Self::Snapshot> {
		state
			.semaphore_work
			.entries
			.get(&self)
			.map(|entry| entry.snapshot(self))
	}

	fn entry_changed(self, state: &AdmissionState) -> Option<Arc<Notify>> {
		state
			.semaphore_work
			.entries
			.get(&self)
			.map(|entry| Arc::clone(&entry.changed))
	}
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum RuntimeOwnedWorkId {
	Lease(RuntimeLeaseWorkId),
	Upgrade(RuntimeUpgradeWorkId),
	Throttler(RuntimeThrottlerWorkId),
	Semaphore(RuntimeSemaphoreWorkId),
}

/// Owned local transaction membership, optionally retaining exactly one registered work owner.
/// Drop decrements local counts; it proves no physical transaction ending.
pub(crate) struct RuntimeTransactionPermit {
	shared: Arc<SharedAdmission>,
	work: Option<RuntimeOwnedWorkId>,
}

impl crate::db::transaction_permits::TransactionPermitHandle for RuntimeTransactionPermit {
	fn runtime_identity(&self) -> usize {
		Arc::as_ptr(&self.shared) as usize
	}

	fn work_owner(&self) -> Option<crate::db::transaction_permits::TransactionWorkOwner> {
		self.work.map(|id| match id {
			RuntimeOwnedWorkId::Lease(lease_id) => {
				crate::db::transaction_permits::TransactionWorkOwner::Lease(lease_id.0)
			}
			RuntimeOwnedWorkId::Upgrade(_)
			| RuntimeOwnedWorkId::Throttler(_)
			| RuntimeOwnedWorkId::Semaphore(_) => {
				crate::db::transaction_permits::TransactionWorkOwner::Other
			}
		})
	}
}

impl Drop for RuntimeTransactionPermit {
	fn drop(&mut self) {
		let mut state = self.shared.lock();
		state.pending_transaction_count -= 1;
		let changed = self.work.map(|id| {
			state
				.local_work_mut(id)
				.expect("a retained continuation keeps its runtime membership")
				.finish_continuation()
		});
		self.shared.notify_owned_work_changed(changed.as_deref());
	}
}

/// Local drainage chooses work under the mutex and awaits it after releasing the mutex.
enum LocalDrainageAction {
	Reject(RuntimeAdmissionError),
	Join(RuntimeOwnedWorkId),
	Drained,
	Wait,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RuntimeAdmissionError {
	#[error("runtime admission stopped: {stop:?}")]
	Stopped { stop: RuntimeAdmissionStop },
	#[error("runtime admission ownership count exhausted")]
	CountExhausted,
	#[error("runtime must stop admission before waiting for local ownership drainage")]
	StillAccepting,
	#[error("runtime confirmed deadline must advance the preceding confirmed deadline")]
	DeadlineNotAdvanced,
	#[error("runtime local admission lifetime has finished")]
	LifetimeFinished,
	#[error("runtime local admission lifetime still accepts work or retains local ownership")]
	LifetimeStillActive,
	#[error("runtime work identifier exhausted")]
	WorkIdentifierExhausted,
	#[error("runtime lease work membership is absent")]
	LeaseWorkAbsent,
	#[error("runtime lease work belongs to a different runtime or has completed")]
	LeaseWorkRuntimeMismatch,
	#[error("runtime lease work belongs to a different record identity")]
	LeaseWorkIdentityMismatch,
	#[error("runtime lease work state does not authorize this local operation")]
	LeaseWorkStateMismatch,
	#[error("runtime lease work retains caller scopes or continuations")]
	LeaseWorkStillActive,
	#[error("runtime lease work ownership was abandoned: {work_id:?}")]
	LeaseWorkAbandoned {
		work_id: RuntimeLeaseWorkId,
		#[source]
		source: Option<RuntimeWorkTerminalError>,
	},
	#[error("runtime owned task state does not authorize this operation")]
	OwnedTaskStateMismatch,
	#[error("runtime lease task did not complete normally: {work_id:?}")]
	LeaseTaskAbandoned {
		work_id: RuntimeLeaseWorkId,
		#[source]
		source: Option<RuntimeWorkTerminalError>,
	},
	#[error("runtime upgrade work membership is absent")]
	UpgradeWorkAbsent,
	#[error("runtime upgrade work belongs to another runtime or has completed")]
	UpgradeWorkRuntimeMismatch,
	#[error("runtime upgrade work belongs to another installation or request")]
	UpgradeWorkIdentityMismatch,
	#[error("runtime upgrade work state does not authorize this local operation")]
	UpgradeWorkStateMismatch,
	#[error("runtime upgrade work retains a pending continuation")]
	UpgradeWorkStillActive,
	#[error("runtime upgrade work ownership was abandoned: {work_id:?}")]
	UpgradeWorkAbandoned {
		work_id: RuntimeUpgradeWorkId,
		#[source]
		source: Option<RuntimeWorkTerminalError>,
	},
	#[error("runtime upgrade task did not complete normally: {work_id:?}")]
	UpgradeTaskAbandoned {
		work_id: RuntimeUpgradeWorkId,
		#[source]
		source: Option<RuntimeWorkTerminalError>,
	},
	#[error("runtime Throttler work membership is absent")]
	ThrottlerWorkAbsent,
	#[error("runtime Throttler work belongs to another runtime or has completed")]
	ThrottlerWorkRuntimeMismatch,
	#[error("runtime Throttler work belongs to another scope or resource")]
	ThrottlerWorkIdentityMismatch,
	#[error("runtime Throttler work phase does not authorize this local operation")]
	ThrottlerWorkStateMismatch,
	#[error("runtime Throttler work retains caller work or a pending continuation")]
	ThrottlerWorkStillActive,
	#[error("runtime Throttler ownership was abandoned: {work_id:?}")]
	ThrottlerWorkAbandoned {
		work_id: RuntimeThrottlerWorkId,
		#[source]
		source: Option<RuntimeWorkTerminalError>,
	},
	#[error("runtime Throttler task did not complete normally: {work_id:?}")]
	ThrottlerTaskAbandoned {
		work_id: RuntimeThrottlerWorkId,
		#[source]
		source: Option<RuntimeWorkTerminalError>,
	},
	#[error("runtime Semaphore work membership is absent")]
	SemaphoreWorkAbsent,
	#[error("runtime Semaphore work belongs to another runtime or has completed")]
	SemaphoreWorkRuntimeMismatch,
	#[error("runtime Semaphore work belongs to another scope or resource")]
	SemaphoreWorkIdentityMismatch,
	#[error("runtime Semaphore work phase does not authorize this local operation")]
	SemaphoreWorkStateMismatch,
	#[error("runtime Semaphore work retains caller work or a pending continuation")]
	SemaphoreWorkStillActive,
	#[error("runtime Semaphore ownership was abandoned: {work_id:?}")]
	SemaphoreWorkAbandoned {
		work_id: RuntimeSemaphoreWorkId,
		#[source]
		source: Option<RuntimeWorkTerminalError>,
	},
	#[error("runtime Semaphore task did not complete normally: {work_id:?}")]
	SemaphoreTaskAbandoned {
		work_id: RuntimeSemaphoreWorkId,
		#[source]
		source: Option<RuntimeWorkTerminalError>,
	},
}
