//! Typed local work ownership, completion, and retention within one runtime.

#[cfg(test)]
mod tests;

use std::{collections::BTreeMap, fmt, future::Future, sync::Arc};
use tokio::{runtime::Handle, sync::Notify};

use super::{
	AdmissionState, RuntimeAdmissionError, RuntimeAdmissionFenceReason, RuntimeAdmissionTracker,
	RuntimeOwnedWorkId, SharedAdmission,
	diagnostic::RuntimeWorkTerminalError,
	task::{RuntimeTaskOwnership, RuntimeTaskStatus},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeResourceWorkState {
	Acquiring,
	Held,
	Closing,
	Finished,
	Abandoned,
	Unresolved,
}

pub(crate) trait WorkPhase: Copy + Eq + fmt::Debug + Send + 'static {
	const INITIAL: Self;
	const FINISHED: Self;
	const ABANDONED: Self;
	const UNRESOLVED: Self;

	fn permits_completion(self) -> bool;

	fn is_resolved(self) -> bool {
		self == Self::FINISHED || self == Self::UNRESOLVED
	}
}

impl WorkPhase for RuntimeResourceWorkState {
	const INITIAL: Self = Self::Acquiring;
	const FINISHED: Self = Self::Finished;
	const ABANDONED: Self = Self::Abandoned;
	const UNRESOLVED: Self = Self::Unresolved;

	fn permits_completion(self) -> bool {
		matches!(self, Self::Acquiring | Self::Closing)
	}
}

pub(crate) trait CallerWork: Send + 'static {
	fn empty() -> Self;
	fn pending_count(&self) -> usize;
}

pub(crate) struct NoCallerWork;
impl CallerWork for NoCallerWork {
	fn empty() -> Self {
		Self
	}
	fn pending_count(&self) -> usize {
		0
	}
}

pub(crate) struct SharedCallerWork {
	count: usize,
}
pub(crate) enum ExclusiveCallerWork {
	Idle,
	Running,
}

pub(crate) enum CallerWorkError {
	StillActive,
	CountExhausted,
}

/// Shared callers can overlap control work; an exclusive caller cannot start alongside it.
pub(crate) trait ResourceCallerWork: CallerWork {
	fn start(&mut self, continuation: ContinuationOwnership) -> Result<(), CallerWorkError>;
	fn finish(&mut self);
	fn require_closing(&self) -> Result<(), CallerWorkError>;
}

impl CallerWork for SharedCallerWork {
	fn empty() -> Self {
		Self { count: 0 }
	}
	fn pending_count(&self) -> usize {
		self.count
	}
}
impl ResourceCallerWork for SharedCallerWork {
	fn start(&mut self, _continuation: ContinuationOwnership) -> Result<(), CallerWorkError> {
		self.count = self
			.count
			.checked_add(1)
			.ok_or(CallerWorkError::CountExhausted)?;
		Ok(())
	}
	fn finish(&mut self) {
		self.count -= 1;
	}
	fn require_closing(&self) -> Result<(), CallerWorkError> {
		Ok(())
	}
}
impl CallerWork for ExclusiveCallerWork {
	fn empty() -> Self {
		Self::Idle
	}
	fn pending_count(&self) -> usize {
		usize::from(matches!(self, Self::Running))
	}
}
impl ResourceCallerWork for ExclusiveCallerWork {
	fn start(&mut self, continuation: ContinuationOwnership) -> Result<(), CallerWorkError> {
		if matches!(self, Self::Running) || continuation.is_retained() {
			return Err(CallerWorkError::StillActive);
		}
		*self = Self::Running;
		Ok(())
	}
	fn finish(&mut self) {
		assert!(
			matches!(self, Self::Running),
			"an exclusive caller retains ownership until Drop"
		);
		*self = Self::Idle;
	}
	fn require_closing(&self) -> Result<(), CallerWorkError> {
		if matches!(self, Self::Running) {
			Err(CallerWorkError::StillActive)
		} else {
			Ok(())
		}
	}
}

/// Each work kind selects a typed registry and its domain's identities and errors.
pub(crate) trait RuntimeWorkKind: Sized + Send + 'static {
	type Id: Copy + Ord + fmt::Debug + Send + 'static;
	type Identity: Clone + Eq + fmt::Debug + Send + 'static;
	type State: WorkPhase;
	type Callers: CallerWork;

	const ABSENT: RuntimeAdmissionError;
	const STATE_MISMATCH: RuntimeAdmissionError;
	const STILL_ACTIVE: RuntimeAdmissionError;
	const OWNER_ABANDONED: RuntimeAdmissionFenceReason;

	fn registry(state: &AdmissionState) -> &WorkRegistry<Self>;
	fn registry_mut(state: &mut AdmissionState) -> &mut WorkRegistry<Self>;
	fn id(value: u64) -> Self::Id;
	fn owned_id(id: Self::Id) -> RuntimeOwnedWorkId;
	fn abandoned_error(
		id: Self::Id,
		source: Option<RuntimeWorkTerminalError>,
	) -> RuntimeAdmissionError;
	fn owner_abandonment_diagnostic() -> Option<RuntimeWorkTerminalError>;
}

pub(crate) type RuntimeWorkSnapshot<K> = WorkSnapshot<
	<K as RuntimeWorkKind>::Id,
	<K as RuntimeWorkKind>::Identity,
	<K as RuntimeWorkKind>::State,
>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkSnapshot<Id, Identity, State> {
	pub(crate) id: Id,
	pub(crate) identity: Identity,
	pub(crate) state: State,
	pub(crate) pending_caller_work_count: usize,
	pub(crate) continuation: ContinuationOwnership,
	pub(crate) task: RuntimeTaskStatus,
	pub(crate) terminal_diagnostic: Option<RuntimeWorkTerminalError>,
}
impl<Id, Identity, State: WorkPhase> WorkSnapshot<Id, Identity, State> {
	pub(crate) fn is_pending(&self) -> bool {
		has_pending_local_work(self.state, self.pending_caller_work_count, self.task)
	}
}

fn has_pending_local_work<State: WorkPhase>(
	state: State,
	callers: usize,
	task: RuntimeTaskStatus,
) -> bool {
	!state.is_resolved() || callers != 0 || task.is_pending()
}

pub(crate) struct WorkEntry<K: RuntimeWorkKind> {
	pub(super) identity: K::Identity,
	pub(super) state: K::State,
	pub(super) callers: K::Callers,
	pub(super) continuation: ContinuationOwnership,
	pub(super) task: RuntimeTaskOwnership,
	pub(super) terminal_diagnostic: Option<RuntimeWorkTerminalError>,
	pub(super) changed: Arc<Notify>,
}

/// Whether a transaction of the work holds the work's one continuation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ContinuationOwnership {
	Vacant,
	Retained,
}

impl ContinuationOwnership {
	pub(crate) fn is_retained(self) -> bool {
		matches!(self, Self::Retained)
	}
}
impl<K: RuntimeWorkKind> WorkEntry<K> {
	pub(super) fn snapshot(&self, id: K::Id) -> RuntimeWorkSnapshot<K> {
		WorkSnapshot {
			id,
			identity: self.identity.clone(),
			state: self.state,
			pending_caller_work_count: self.callers.pending_count(),
			continuation: self.continuation,
			task: self.task.status(),
			terminal_diagnostic: self.terminal_diagnostic.clone(),
		}
	}

	fn is_pending(&self) -> bool {
		has_pending_local_work(self.state, self.callers.pending_count(), self.task.status())
	}
}

pub(crate) struct WorkRegistry<K: RuntimeWorkKind> {
	next_id: u64,
	pub(super) entries: BTreeMap<K::Id, WorkEntry<K>>,
}
impl<K: RuntimeWorkKind> WorkRegistry<K> {
	pub(super) fn new() -> Self {
		Self {
			next_id: 0,
			entries: BTreeMap::new(),
		}
	}

	fn register(&mut self, identity: K::Identity) -> Result<K::Id, RuntimeAdmissionError> {
		let next = self
			.next_id
			.checked_add(1)
			.ok_or(RuntimeAdmissionError::WorkIdentifierExhausted)?;
		let id = K::id(self.next_id);
		self.entries.insert(
			id,
			WorkEntry {
				identity,
				state: K::State::INITIAL,
				callers: K::Callers::empty(),
				continuation: ContinuationOwnership::Vacant,
				task: RuntimeTaskOwnership::Unregistered,
				terminal_diagnostic: None,
				changed: Arc::new(Notify::new()),
			},
		);
		self.next_id = next;
		Ok(id)
	}

	pub(super) fn has_pending_work(&self) -> bool {
		self.entries.values().any(WorkEntry::is_pending)
	}

	pub(super) fn abandoned_error(&self) -> Option<RuntimeAdmissionError> {
		self.entries
			.iter()
			.find(|(_, entry)| entry.state == K::State::ABANDONED)
			.map(|(id, entry)| K::abandoned_error(*id, entry.terminal_diagnostic.clone()))
	}

	pub(super) fn joinable_task(&self) -> Option<RuntimeOwnedWorkId> {
		self.entries
			.iter()
			.find(|(_, entry)| {
				entry.state.is_resolved() && matches!(entry.task, RuntimeTaskOwnership::Running(_))
			})
			.map(|(id, _)| K::owned_id(*id))
	}

	pub(super) fn is_drained(&self) -> bool {
		self.entries.values().all(|entry| {
			entry.state == K::State::UNRESOLVED
				&& entry.callers.pending_count() == 0
				&& !entry.continuation.is_retained()
				&& !entry.task.is_pending()
		})
	}

	pub(super) fn remove_finished(&mut self, id: K::Id) {
		if self
			.entries
			.get(&id)
			.is_some_and(|entry| entry.state == K::State::FINISHED)
		{
			self.entries.remove(&id);
		}
	}
}

pub(super) enum LocalWorkState {
	Active,
	Finished,
	Unresolved,
	Abandoned,
}

/// Task and continuation operations need no access to a family's diagnostic identity.
pub(super) trait LocalWorkRecord {
	fn local_state(&self) -> LocalWorkState;
	fn task(&self) -> &RuntimeTaskOwnership;
	fn task_mut(&mut self) -> &mut RuntimeTaskOwnership;
	fn changed(&self) -> Arc<Notify>;
	fn diagnostic(&self) -> Option<RuntimeWorkTerminalError>;
	fn mark_abandoned(&mut self) -> &mut Option<RuntimeWorkTerminalError>;
	fn retain_continuation(&mut self) -> Arc<Notify>;
	fn finish_continuation(&mut self) -> Arc<Notify>;
}
impl<K: RuntimeWorkKind> LocalWorkRecord for WorkEntry<K> {
	fn local_state(&self) -> LocalWorkState {
		if self.state == K::State::FINISHED {
			LocalWorkState::Finished
		} else if self.state == K::State::UNRESOLVED {
			LocalWorkState::Unresolved
		} else if self.state == K::State::ABANDONED {
			LocalWorkState::Abandoned
		} else {
			LocalWorkState::Active
		}
	}
	fn task(&self) -> &RuntimeTaskOwnership {
		&self.task
	}
	fn task_mut(&mut self) -> &mut RuntimeTaskOwnership {
		&mut self.task
	}
	fn changed(&self) -> Arc<Notify> {
		Arc::clone(&self.changed)
	}
	fn diagnostic(&self) -> Option<RuntimeWorkTerminalError> {
		self.terminal_diagnostic.clone()
	}
	fn mark_abandoned(&mut self) -> &mut Option<RuntimeWorkTerminalError> {
		self.state = K::State::ABANDONED;
		&mut self.terminal_diagnostic
	}
	fn retain_continuation(&mut self) -> Arc<Notify> {
		assert!(
			!self.continuation.is_retained(),
			"domain admission permits one owned continuation at a time"
		);
		self.continuation = ContinuationOwnership::Retained;
		Arc::clone(&self.changed)
	}
	fn finish_continuation(&mut self) -> Arc<Notify> {
		assert!(
			self.continuation.is_retained(),
			"a transaction retains its work continuation until Drop"
		);
		self.continuation = ContinuationOwnership::Vacant;
		Arc::clone(&self.changed)
	}
}

pub(super) enum WorkCompletion {
	Finished,
	Unresolved(Option<RuntimeWorkTerminalError>),
}

/// Local membership retains no database capability and proves no physical ending.
pub(crate) struct WorkPermit<K: RuntimeWorkKind> {
	pub(super) shared: Arc<SharedAdmission>,
	pub(super) id: K::Id,
	ownership: WorkPermitOwnership,
}

#[derive(Debug)]
enum WorkPermitOwnership {
	Retained,
	Released,
}
impl<K: RuntimeWorkKind> WorkPermit<K> {
	pub(super) fn is_active(&self) -> bool {
		matches!(self.ownership, WorkPermitOwnership::Retained)
	}
	pub(crate) fn id(&self) -> K::Id {
		self.id
	}
	pub(crate) fn admission_tracker(&self) -> RuntimeAdmissionTracker {
		RuntimeAdmissionTracker(Arc::clone(&self.shared))
	}
	pub(crate) fn snapshot(&self) -> Result<RuntimeWorkSnapshot<K>, RuntimeAdmissionError> {
		let state = self.shared.lock();
		Ok(K::registry(&state)
			.entries
			.get(&self.id)
			.ok_or(K::ABSENT)?
			.snapshot(self.id))
	}
	pub(crate) async fn wait_until_continuations_end(&self) -> Result<(), RuntimeAdmissionError> {
		let changed = K::registry(&self.shared.lock())
			.entries
			.get(&self.id)
			.map(|entry| Arc::clone(&entry.changed))
			.ok_or(K::ABSENT)?;
		loop {
			let notified = changed.notified();
			tokio::pin!(notified);
			notified.as_mut().enable();
			{
				let state = self.shared.lock();
				let entry = K::registry(&state).entries.get(&self.id).ok_or(K::ABSENT)?;
				if entry.state == K::State::ABANDONED {
					return Err(K::abandoned_error(
						self.id,
						entry.terminal_diagnostic.clone(),
					));
				}
				if !entry.continuation.is_retained() {
					return Ok(());
				}
			}
			notified.await;
		}
	}
	pub(crate) fn complete_unresolved(
		&mut self,
		source: Arc<dyn std::error::Error + Send + Sync>,
	) -> Result<(), RuntimeAdmissionError> {
		self.complete_local(WorkCompletion::Unresolved(Some(RuntimeWorkTerminalError(
			source,
		))))
	}
	pub(super) fn complete_local(
		&mut self,
		completion: WorkCompletion,
	) -> Result<(), RuntimeAdmissionError> {
		let mut state = self.shared.lock();
		let registry = K::registry_mut(&mut state);
		let entry = registry.entries.get_mut(&self.id).ok_or(K::ABSENT)?;
		if !entry.state.permits_completion() {
			return Err(K::STATE_MISMATCH);
		}
		if entry.continuation.is_retained() || entry.callers.pending_count() != 0 {
			return Err(K::STILL_ACTIVE);
		}
		let changed = Arc::clone(&entry.changed);
		match completion {
			WorkCompletion::Unresolved(diagnostic) => {
				entry.state = K::State::UNRESOLVED;
				entry.terminal_diagnostic = diagnostic;
			}
			WorkCompletion::Finished if entry.task.is_pending() => entry.state = K::State::FINISHED,
			WorkCompletion::Finished => {
				registry.entries.remove(&self.id);
			}
		}
		self.ownership = WorkPermitOwnership::Released;
		self.shared.notify_work_changed(&changed);
		Ok(())
	}
	pub(crate) fn spawn_owned_task<F, Work>(
		self,
		runtime: &Handle,
		work_fn: Work,
	) -> Result<(), RuntimeAdmissionError>
	where
		F: Future<Output = ()> + Send + 'static,
		Work: FnOnce(Self) -> F + Send + 'static,
	{
		super::task::spawn_owned_task(
			Arc::clone(&self.shared),
			K::owned_id(self.id),
			self,
			runtime,
			work_fn,
		)
	}
}
impl<K: RuntimeWorkKind> fmt::Debug for WorkPermit<K> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("RuntimeWorkPermit")
			.field("id", &K::owned_id(self.id))
			.field("ownership", &self.ownership)
			.finish_non_exhaustive()
	}
}
impl<K: RuntimeWorkKind> Drop for WorkPermit<K> {
	fn drop(&mut self) {
		if !self.is_active() {
			return;
		}
		let mut state = self.shared.lock();
		let entry = K::registry_mut(&mut state)
			.entries
			.get_mut(&self.id)
			.expect("active ownership retains its runtime membership");
		entry.state = K::State::ABANDONED;
		if entry.terminal_diagnostic.is_none() {
			entry.terminal_diagnostic = K::owner_abandonment_diagnostic();
		}
		let changed = Arc::clone(&entry.changed);
		self.shared.fence(&mut state, K::OWNER_ABANDONED);
		self.shared.notify_work_changed(&changed);
	}
}

pub(crate) struct CallerWorkPermit<K: RuntimeWorkKind>
where
	K::Callers: ResourceCallerWork,
{
	pub(super) shared: Arc<SharedAdmission>,
	pub(super) id: K::Id,
}
impl<K: RuntimeWorkKind> Drop for CallerWorkPermit<K>
where
	K::Callers: ResourceCallerWork,
{
	fn drop(&mut self) {
		let mut state = self.shared.lock();
		let entry = K::registry_mut(&mut state)
			.entries
			.get_mut(&self.id)
			.expect("caller work retains its runtime membership");
		entry.callers.finish();
		self.shared.notify_work_changed(&entry.changed);
	}
}

impl<K: RuntimeWorkKind<State = RuntimeResourceWorkState>> WorkPermit<K>
where
	K::Callers: ResourceCallerWork,
{
	pub(crate) fn retain_diagnostic(
		&self,
		source: Arc<dyn std::error::Error + Send + Sync>,
	) -> Result<(), RuntimeAdmissionError> {
		if !self.is_active() {
			return Err(K::STATE_MISMATCH);
		}
		let mut state = self.shared.lock();
		let entry = K::registry_mut(&mut state)
			.entries
			.get_mut(&self.id)
			.ok_or(K::ABSENT)?;
		if !matches!(
			entry.state,
			RuntimeResourceWorkState::Acquiring
				| RuntimeResourceWorkState::Held
				| RuntimeResourceWorkState::Closing
		) {
			return Err(K::STATE_MISMATCH);
		}
		entry.terminal_diagnostic = Some(RuntimeWorkTerminalError(source));
		self.shared.notify_work_changed(&entry.changed);
		Ok(())
	}
	pub(crate) fn confirm_held(&self) -> Result<(), RuntimeAdmissionError> {
		let mut state = self.shared.lock();
		self.shared.fence_elapsed(&mut state);
		state.require_open()?;
		let entry = K::registry_mut(&mut state)
			.entries
			.get_mut(&self.id)
			.ok_or(K::ABSENT)?;
		if entry.state != RuntimeResourceWorkState::Acquiring || entry.continuation.is_retained() {
			return Err(K::STATE_MISMATCH);
		}
		entry.state = RuntimeResourceWorkState::Held;
		self.shared.notify_work_changed(&entry.changed);
		Ok(())
	}
	pub(crate) fn try_start_caller_work(
		&self,
	) -> Result<CallerWorkPermit<K>, RuntimeAdmissionError> {
		if !self.is_active() {
			return Err(K::STATE_MISMATCH);
		}
		let mut state = self.shared.lock();
		self.shared.fence_elapsed(&mut state);
		state.require_open()?;
		let entry = K::registry_mut(&mut state)
			.entries
			.get_mut(&self.id)
			.ok_or(K::ABSENT)?;
		if entry.state != RuntimeResourceWorkState::Held {
			return Err(K::STATE_MISMATCH);
		}
		entry
			.callers
			.start(entry.continuation)
			.map_err(|error| match error {
				CallerWorkError::StillActive => K::STILL_ACTIVE,
				CallerWorkError::CountExhausted => RuntimeAdmissionError::CountExhausted,
			})?;
		self.shared.notify_work_changed(&entry.changed);
		Ok(CallerWorkPermit {
			shared: Arc::clone(&self.shared),
			id: self.id,
		})
	}
	pub(crate) fn begin_closing(&self) -> Result<(), RuntimeAdmissionError> {
		let mut state = self.shared.lock();
		let entry = K::registry_mut(&mut state)
			.entries
			.get_mut(&self.id)
			.ok_or(K::ABSENT)?;
		entry
			.callers
			.require_closing()
			.map_err(|_| K::STILL_ACTIVE)?;
		match entry.state {
			RuntimeResourceWorkState::Acquiring | RuntimeResourceWorkState::Held => {
				entry.state = RuntimeResourceWorkState::Closing
			}
			RuntimeResourceWorkState::Closing => return Ok(()),
			_ => return Err(K::STATE_MISMATCH),
		}
		self.shared.notify_work_changed(&entry.changed);
		Ok(())
	}
}

impl RuntimeAdmissionTracker {
	pub(super) fn try_register_work<K: RuntimeWorkKind>(
		&self,
		identity: K::Identity,
	) -> Result<WorkPermit<K>, RuntimeAdmissionError> {
		let mut state = self.0.lock();
		self.0.fence_elapsed(&mut state);
		state.require_open()?;
		let id = K::registry_mut(&mut state).register(identity)?;
		self.0.changed.notify_waiters();
		Ok(WorkPermit {
			shared: Arc::clone(&self.0),
			id,
			ownership: WorkPermitOwnership::Retained,
		})
	}
}
