//! One actual task owner for lease, upgrade, Throttler and Semaphore work in the same runtime.

use super::diagnostic::RuntimeWorkTerminalError;
use super::membership::LocalWorkState;
use super::{
	AdmissionState, Arc, LocalWorkRecord, Notify, RuntimeAdmissionError,
	RuntimeAdmissionFenceReason, RuntimeAdmissionTracker, RuntimeOwnedWorkId, SharedAdmission, fmt,
};
use std::future::Future;
use tokio::{
	runtime::Handle,
	task::{JoinError, JoinHandle},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeTaskStatus {
	Unregistered,
	Reserved,
	Running,
	Joining,
	Joined,
}
impl RuntimeTaskStatus {
	pub(super) fn is_pending(self) -> bool {
		!matches!(self, Self::Unregistered | Self::Joined)
	}
}

pub(super) enum RuntimeTaskOwnership {
	Unregistered,
	Reserved,
	Running(JoinHandle<()>),
	Joining,
	Joined,
}
impl RuntimeTaskOwnership {
	pub(super) fn status(&self) -> RuntimeTaskStatus {
		match self {
			Self::Unregistered => RuntimeTaskStatus::Unregistered,
			Self::Reserved => RuntimeTaskStatus::Reserved,
			Self::Running(_) => RuntimeTaskStatus::Running,
			Self::Joining => RuntimeTaskStatus::Joining,
			Self::Joined => RuntimeTaskStatus::Joined,
		}
	}
	pub(super) fn is_pending(&self) -> bool {
		self.status().is_pending()
	}
}

impl RuntimeOwnedWorkId {
	fn absent_error(self) -> RuntimeAdmissionError {
		match self {
			Self::Lease(_) => RuntimeAdmissionError::LeaseWorkAbsent,
			Self::Upgrade(_) => RuntimeAdmissionError::UpgradeWorkAbsent,
			Self::Throttler(_) => RuntimeAdmissionError::ThrottlerWorkAbsent,
			Self::Semaphore(_) => RuntimeAdmissionError::SemaphoreWorkAbsent,
		}
	}
	fn abandoned_error(self, source: Option<RuntimeWorkTerminalError>) -> RuntimeAdmissionError {
		match self {
			Self::Lease(work) => RuntimeAdmissionError::LeaseTaskAbandoned {
				work_id: work,
				source,
			},
			Self::Upgrade(work) => RuntimeAdmissionError::UpgradeTaskAbandoned {
				work_id: work,
				source,
			},
			Self::Throttler(work) => RuntimeAdmissionError::ThrottlerTaskAbandoned {
				work_id: work,
				source,
			},
			Self::Semaphore(work) => RuntimeAdmissionError::SemaphoreTaskAbandoned {
				work_id: work,
				source,
			},
		}
	}
	fn fence_reason(self) -> RuntimeAdmissionFenceReason {
		match self {
			Self::Lease(_) => RuntimeAdmissionFenceReason::LeaseTaskAbandoned,
			Self::Upgrade(_) => RuntimeAdmissionFenceReason::UpgradeTaskAbandoned,
			Self::Throttler(_) => RuntimeAdmissionFenceReason::ThrottlerTaskAbandoned,
			Self::Semaphore(_) => RuntimeAdmissionFenceReason::SemaphoreTaskAbandoned,
		}
	}
}

impl AdmissionState {
	fn task_diagnostic(&self, id: RuntimeOwnedWorkId) -> Option<RuntimeWorkTerminalError> {
		self.local_work(id).and_then(LocalWorkRecord::diagnostic)
	}
	fn task(&self, id: RuntimeOwnedWorkId) -> Option<&RuntimeTaskOwnership> {
		self.local_work(id).map(LocalWorkRecord::task)
	}
	fn task_mut(&mut self, id: RuntimeOwnedWorkId) -> Option<&mut RuntimeTaskOwnership> {
		self.local_work_mut(id).map(LocalWorkRecord::task_mut)
	}
	fn work_changed(&self, id: RuntimeOwnedWorkId) -> Option<Arc<Notify>> {
		self.local_work(id).map(LocalWorkRecord::changed)
	}
	fn is_work_resolved(&self, id: RuntimeOwnedWorkId) -> bool {
		self.local_work(id).is_some_and(|entry| {
			matches!(
				entry.local_state(),
				LocalWorkState::Finished | LocalWorkState::Unresolved
			)
		})
	}
	fn is_work_abandoned(&self, id: RuntimeOwnedWorkId) -> bool {
		self.local_work(id)
			.is_some_and(|entry| matches!(entry.local_state(), LocalWorkState::Abandoned))
	}
	fn remove_finished(&mut self, id: RuntimeOwnedWorkId) {
		match id {
			RuntimeOwnedWorkId::Lease(id) => self.lease_work.remove_finished(id),
			RuntimeOwnedWorkId::Upgrade(id) => self.upgrade_work.remove_finished(id),
			RuntimeOwnedWorkId::Throttler(id) => self.throttler_work.remove_finished(id),
			RuntimeOwnedWorkId::Semaphore(id) => self.semaphore_work.remove_finished(id),
		}
	}
	fn mark_task_abandoned(&mut self, id: RuntimeOwnedWorkId, join_error: Option<JoinError>) {
		let diagnostic = self
			.local_work_mut(id)
			.expect("a task retains its runtime membership")
			.mark_abandoned();
		if let Some(join_error) = join_error {
			*diagnostic = Some(RuntimeWorkTerminalError(Arc::new(RuntimeTaskJoinError {
				prior_error: diagnostic.take(),
				join_error,
			})));
		}
	}
}

/// The actual join cause and an earlier unresolved cause remain independently accessible.
pub(crate) struct RuntimeTaskJoinError {
	prior_error: Option<RuntimeWorkTerminalError>,
	join_error: JoinError,
}
impl fmt::Debug for RuntimeTaskJoinError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("RuntimeTaskJoinError")
			.field("has_prior_cause", &self.prior_error.is_some())
			.field("join_cause_retained", &true)
			.finish()
	}
}
impl RuntimeTaskJoinError {
	pub(crate) fn prior_error(&self) -> Option<&RuntimeWorkTerminalError> {
		self.prior_error.as_ref()
	}
	pub(crate) fn join_error(&self) -> &JoinError {
		&self.join_error
	}
}
impl fmt::Display for RuntimeTaskJoinError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("runtime owned task did not join normally")
	}
}
impl std::error::Error for RuntimeTaskJoinError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(&self.join_error)
	}
}

struct TaskCompletion {
	shared: Arc<SharedAdmission>,
	id: RuntimeOwnedWorkId,
	state: TaskReturnState,
}

enum TaskReturnState {
	Running,
	Returned,
}
impl Drop for TaskCompletion {
	fn drop(&mut self) {
		let mut state = self.shared.lock();
		if matches!(self.state, TaskReturnState::Running) || !state.is_work_resolved(self.id) {
			state.mark_task_abandoned(self.id, None);
			self.shared.fence(&mut state, self.id.fence_reason());
		}
		// The task's handle reports the end only after this drop, so a later reap observes it.
		state.ended_tasks.push(self.id);
		let work_changed = state.work_changed(self.id);
		self.shared
			.notify_owned_work_changed(work_changed.as_deref());
	}
}

/// A cancelled waiter restores ownership of the actual handle to the same registry.
struct TaskJoin {
	shared: Arc<SharedAdmission>,
	id: RuntimeOwnedWorkId,
	state: TaskJoinState,
}

enum TaskJoinState {
	Joining(JoinHandle<()>),
	Joined,
}
impl Drop for TaskJoin {
	fn drop(&mut self) {
		if let TaskJoinState::Joining(task) =
			std::mem::replace(&mut self.state, TaskJoinState::Joined)
		{
			let mut state = self.shared.lock();
			*state
				.task_mut(self.id)
				.expect("joining task retains membership") = RuntimeTaskOwnership::Running(task);
			let work_changed = state.work_changed(self.id);
			self.shared
				.notify_owned_work_changed(work_changed.as_deref());
		}
	}
}

pub(super) fn spawn_owned_task<Owner, F, Work>(
	shared: Arc<SharedAdmission>,
	id: RuntimeOwnedWorkId,
	owner: Owner,
	runtime: &Handle,
	work_fn: Work,
) -> Result<(), RuntimeAdmissionError>
where
	Owner: Send + 'static,
	F: Future<Output = ()> + Send + 'static,
	Work: FnOnce(Owner) -> F + Send + 'static,
{
	{
		let mut state = shared.lock();
		let task = state.task_mut(id).ok_or_else(|| id.absent_error())?;
		if !matches!(task, RuntimeTaskOwnership::Unregistered) {
			return Err(RuntimeAdmissionError::OwnedTaskStateMismatch);
		}
		*task = RuntimeTaskOwnership::Reserved;
		let work_changed = state.work_changed(id);
		shared.notify_owned_work_changed(work_changed.as_deref());
	}
	let completion = TaskCompletion {
		shared: Arc::clone(&shared),
		id,
		state: TaskReturnState::Running,
	};
	let task = runtime.spawn(async move {
		let mut completion = completion;
		work_fn(owner).await;
		completion.state = TaskReturnState::Returned;
	});
	{
		let mut state = shared.lock();
		let slot = state
			.task_mut(id)
			.expect("reserved task retains membership");
		assert!(
			matches!(slot, RuntimeTaskOwnership::Reserved),
			"one reserved task receives one handle"
		);
		*slot = RuntimeTaskOwnership::Running(task);
		let work_changed = state.work_changed(id);
		shared.notify_owned_work_changed(work_changed.as_deref());
	}
	Ok(())
}

impl RuntimeAdmissionTracker {
	pub(super) async fn join_owned_task(
		&self,
		id: RuntimeOwnedWorkId,
	) -> Result<(), RuntimeAdmissionError> {
		let task = {
			let mut state = self.0.lock();
			if state.task(id).is_none() {
				return Ok(());
			}
			if !state.is_work_resolved(id) {
				return Err(RuntimeAdmissionError::OwnedTaskStateMismatch);
			}
			let slot = state
				.task_mut(id)
				.expect("resolved task retains membership");
			let task = match std::mem::replace(slot, RuntimeTaskOwnership::Joining) {
				RuntimeTaskOwnership::Running(task) => task,
				other => {
					*slot = other;
					return Ok(());
				}
			};
			let work_changed = state.work_changed(id);
			self.0.notify_owned_work_changed(work_changed.as_deref());
			task
		};
		let mut join = TaskJoin {
			shared: Arc::clone(&self.0),
			id,
			state: TaskJoinState::Joining(task),
		};
		let result = match &mut join.state {
			TaskJoinState::Joining(task) => task.await,
			TaskJoinState::Joined => unreachable!("actual join retains its handle"),
		};
		join.state = TaskJoinState::Joined;
		let mut state = self.0.lock();
		*state.task_mut(id).expect("joined task retains membership") = RuntimeTaskOwnership::Joined;
		let work_changed = state.work_changed(id);
		let outcome = if result.is_err() || state.is_work_abandoned(id) {
			state.mark_task_abandoned(id, result.err());
			self.0.fence(&mut state, id.fence_reason());
			Err(id.abandoned_error(state.task_diagnostic(id)))
		} else {
			state.remove_finished(id);
			Ok(())
		};
		self.0.notify_owned_work_changed(work_changed.as_deref());
		outcome
	}
}

impl SharedAdmission {
	/// Observes only already-finished handles; it neither runs caller work nor waits. It visits
	/// only work after its task future has ended.
	pub(super) fn reap_ready_tasks(&self, state: &mut AdmissionState) {
		let mut context = std::task::Context::from_waker(std::task::Waker::noop());
		let mut index = 0;
		while let Some(&id) = state.ended_tasks.get(index) {
			let task = match state.task_mut(id) {
				Some(RuntimeTaskOwnership::Running(task)) => task,
				// The handle is not yet registered, or a join holds it and may return it.
				Some(RuntimeTaskOwnership::Reserved | RuntimeTaskOwnership::Joining) => {
					index += 1;
					continue;
				}
				// A join observed the end, or the work is no longer registered.
				Some(RuntimeTaskOwnership::Unregistered | RuntimeTaskOwnership::Joined) | None => {
					state.ended_tasks.swap_remove(index);
					continue;
				}
			};
			if !task.is_finished() {
				index += 1;
				continue;
			}
			let std::task::Poll::Ready(result) = std::pin::Pin::new(task).poll(&mut context) else {
				index += 1;
				continue;
			};
			state.ended_tasks.swap_remove(index);
			*state.task_mut(id).expect("ready task retains membership") =
				RuntimeTaskOwnership::Joined;
			let work_changed = state.work_changed(id);
			if result.is_err() || !state.is_work_resolved(id) {
				state.mark_task_abandoned(id, result.err());
				self.fence(state, id.fence_reason());
			} else {
				state.remove_finished(id);
			}
			self.notify_owned_work_changed(work_changed.as_deref());
		}
	}
}
