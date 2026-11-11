use super::observation::{
	EventSender, ObservationOptions, ObservationOptionsError, TaskEvent, TaskEventKind,
};
use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use tokio::sync::broadcast;

use tokio_util::sync::CancellationToken;

/// Owner of the sharing lifetime for a group of task contexts, and holder of
/// the application state that task bodies read.
///
/// Clones share ownership. Contexts keep the runtime's shared work without
/// owning it, so dropping the last clone closes the runtime even while
/// contexts still exist.
pub struct TaskRuntime<State> {
	owner: Arc<RuntimeOwner<State>>,
}

impl<State> Clone for TaskRuntime<State> {
	fn clone(&self) -> Self {
		Self {
			owner: Arc::clone(&self.owner),
		}
	}
}

impl<State> TaskRuntime<State> {
	/// Creates a runtime that holds `state`, which every task context of the
	/// runtime gives to task bodies through [`TaskCtx::state`]. `state` is the
	/// state itself or an `Arc` of it that other code also holds.
	///
	/// Retention expiry reads Tokio's clock, so a test can pause and advance
	/// time with `tokio::time`.
	pub fn new(state: impl Into<Arc<State>>) -> Self {
		Self::create(state.into(), None)
	}

	/// Creates a runtime with bounded passive telemetry. No subscriber callback
	/// runs inside task execution. Subscribe before starting work to receive it.
	pub fn new_with_observation(
		state: impl Into<Arc<State>>,
		options: ObservationOptions,
	) -> Result<Self, ObservationOptionsError> {
		if options.event_capacity.get() > usize::MAX / 2 {
			return Err(ObservationOptionsError);
		}
		let (sender, _) = broadcast::channel(options.event_capacity.get());
		Ok(Self::create(state.into(), Some(sender)))
	}

	/// Subscribes to future events, or returns `None` when observation is disabled.
	/// A slow subscriber receives `RecvError::Lagged`; execution never waits for it.
	/// Runtime closure does not close this channel while task contexts still exist;
	/// context-local tasks can continue after shared execution has closed.
	pub fn subscribe_events(&self) -> Option<broadcast::Receiver<TaskEvent>> {
		self.owner
			.state
			.events
			.as_ref()
			.map(broadcast::Sender::subscribe)
	}

	fn create(state: Arc<State>, events: EventSender) -> Self {
		Self {
			owner: Arc::new(RuntimeOwner {
				state: Arc::new(RuntimeState {
					cancel: CancellationToken::new(),
					next_run_id: AtomicU64::new(1),
					waits: Mutex::new(WaitGraph::default()),
					shared_caches: Arc::new(SharedCaches {
						caches: Mutex::default(),
						events: events.clone(),
					}),
					events,
					shared_admission: AtomicU8::new(SharedAdmissionState::Open as u8),
				}),
				app_state: state,
			}),
		}
	}

	/// The application state that this runtime holds.
	pub fn state(&self) -> &Arc<State> {
		&self.owner.app_state
	}

	/// Creates an independent memoization context for one unit of work, such
	/// as a request. Clones of the context share its memoized outcomes.
	pub fn task_ctx(&self) -> TaskCtx<State> {
		TaskCtx {
			state: Arc::new(CtxState {
				runtime: Arc::clone(&self.owner.state),
				app_state: Arc::clone(&self.owner.app_state),
				caches: TaskCaches::default(),
			}),
			role: CtxRole::Root,
		}
	}

	/// Stops shared runs and releases retained results. Shared tasks cannot
	/// start again in this runtime. Closing twice has no further effect.
	pub fn close(&self) {
		self.owner.state.close();
	}
}

struct RuntimeOwner<State> {
	state: Arc<RuntimeState>,
	app_state: Arc<State>,
}

impl<State> Drop for RuntimeOwner<State> {
	fn drop(&mut self) {
		self.state.close();
	}
}

/// The memoized outcomes of one unit of work, attached to a runtime. Every
/// task call takes one. Dropping a call's future ends that caller's part in
/// its run, and a run stops when its last caller leaves.
pub struct TaskCtx<State> {
	pub(crate) state: Arc<CtxState<State>>,
	pub(crate) role: CtxRole,
}

impl<State> Clone for TaskCtx<State> {
	fn clone(&self) -> Self {
		Self {
			state: Arc::clone(&self.state),
			role: self.role.clone(),
		}
	}
}

impl<State> TaskCtx<State> {
	/// The application state of the runtime that created this context.
	pub fn state(&self) -> &Arc<State> {
		&self.state.app_state
	}
}

/// Whether a context belongs to a unit of work or to the body of one run.
#[derive(Clone)]
pub(crate) enum CtxRole {
	/// A context from [`TaskRuntime::task_ctx`]. Nothing cancels it.
	Root,
	/// The context that a run's body receives.
	Run(RunRole),
}

impl CtxRole {
	/// The run that waits for calls made through this context, if any.
	pub(crate) fn run_id(&self) -> Option<RunId> {
		match self {
			Self::Root => None,
			Self::Run(run) => Some(run.run_id),
		}
	}
}

/// A run's identity and the cancellation that stops it.
#[derive(Clone)]
pub(crate) struct RunRole {
	pub(crate) run_id: RunId,
	pub(crate) cancel: CancellationToken,
}

/// The context of one run: the memoized outcomes of the context that started
/// the run, the run's cancellation, and the run as the caller of nested calls.
pub(crate) struct RunCtx<State> {
	pub(crate) state: Arc<CtxState<State>>,
	pub(crate) run: RunRole,
}

impl<State> RunCtx<State> {
	/// The context that the run's body receives.
	pub(crate) fn into_task_ctx(self) -> TaskCtx<State> {
		TaskCtx {
			state: self.state,
			role: CtxRole::Run(self.run),
		}
	}
}

pub(crate) struct CtxState<State> {
	pub(crate) runtime: Arc<RuntimeState>,
	pub(crate) app_state: Arc<State>,
	pub(crate) caches: TaskCaches,
}

pub(crate) struct RuntimeState {
	pub(crate) events: EventSender,
	shared_admission: AtomicU8,
	pub(crate) cancel: CancellationToken,
	next_run_id: AtomicU64,
	waits: Mutex<WaitGraph>,
	pub(crate) shared_caches: Arc<SharedCaches>,
}

#[derive(Clone, Copy)]
#[repr(u8)]
enum SharedAdmissionState {
	Open,
	Closed,
}

impl RuntimeState {
	fn close(&self) {
		let first_close = self
			.shared_admission
			.compare_exchange(
				SharedAdmissionState::Open as u8,
				SharedAdmissionState::Closed as u8,
				Ordering::AcqRel,
				Ordering::Acquire,
			)
			.is_ok();
		// Every closer completes cancellation and cache clearing before returning,
		// including a closer that races with the first close operation.
		self.cancel.cancel();
		self.shared_caches.clear();
		if first_close && let Some(sender) = &self.events {
			let _ = sender.send(TaskEvent {
				at: tokio::time::Instant::now(),
				task: None,
				kind: TaskEventKind::RuntimeClosed,
			});
		}
	}

	pub(crate) fn shared_execution_is_closed(&self) -> bool {
		match self.shared_admission.load(Ordering::Acquire) {
			value if value == SharedAdmissionState::Open as u8 => false,
			value if value == SharedAdmissionState::Closed as u8 => true,
			_ => unreachable!("shared admission states are Open and Closed"),
		}
	}

	pub(crate) fn create_run_id(&self) -> RunId {
		RunId(allocate_id(&self.next_run_id))
	}

	pub(crate) fn start_wait(
		self: &Arc<Self>,
		caller: Option<RunId>,
		target: RunId,
	) -> std::result::Result<Option<WaitGuard>, ()> {
		let Some(caller) = caller else {
			return Ok(None);
		};
		let mut graph = lock(&self.waits);
		if graph.reaches(WaitPath {
			from: target,
			to: caller,
		}) {
			return Err(());
		}
		graph
			.edges
			.entry(caller)
			.or_default()
			.entry(target)
			.and_modify(|count| *count = count.checked_add(1).expect("waiter count fits usize"))
			.or_insert(NonZeroUsize::new(1).expect("one is nonzero"));
		Ok(Some(WaitGuard {
			runtime: Arc::clone(self),
			caller,
			target,
		}))
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RunId(pub(crate) u64);

struct WaitPath {
	from: RunId,
	to: RunId,
}

#[derive(Default)]
struct WaitGraph {
	edges: HashMap<RunId, HashMap<RunId, NonZeroUsize>>,
}

impl WaitGraph {
	fn reaches(&self, path: WaitPath) -> bool {
		let mut visited = HashSet::new();
		let mut pending = vec![path.from];
		while let Some(run) = pending.pop() {
			if run == path.to {
				return true;
			}
			if visited.insert(run)
				&& let Some(targets) = self.edges.get(&run)
			{
				pending.extend(targets.keys());
			}
		}
		false
	}
}

pub(crate) struct WaitGuard {
	runtime: Arc<RuntimeState>,
	caller: RunId,
	target: RunId,
}

impl Drop for WaitGuard {
	fn drop(&mut self) {
		let mut graph = lock(&self.runtime.waits);
		if let Some(targets) = graph.edges.get_mut(&self.caller) {
			if let Some(count) = targets.get_mut(&self.target) {
				match NonZeroUsize::new(count.get() - 1) {
					Some(remaining) => *count = remaining,
					None => {
						targets.remove(&self.target);
					}
				}
			}
			if targets.is_empty() {
				graph.edges.remove(&self.caller);
			}
		}
	}
}

type ErasedCache = Arc<dyn Any + Send + Sync>;

#[derive(Default)]
pub(crate) struct TaskCaches(Mutex<HashMap<u64, ErasedCache>>);

impl TaskCaches {
	pub(crate) fn get_or_create<Cache: Default + Send + Sync + 'static>(
		&self,
		id: u64,
	) -> Arc<Cache> {
		let cache = Arc::clone(
			lock(&self.0)
				.entry(id)
				.or_insert_with(|| Arc::new(Cache::default())),
		);
		cache
			.downcast()
			.unwrap_or_else(|_| unreachable!("one task identity has one cache type"))
	}

	pub(crate) fn find<Cache: Send + Sync + 'static>(&self, id: u64) -> Option<Arc<Cache>> {
		Some(
			Arc::clone(lock(&self.0).get(&id)?)
				.downcast()
				.unwrap_or_else(|_| unreachable!("one task identity has one cache type")),
		)
	}
}

pub(crate) trait ClearCache {
	fn clear(&self);
}

struct SharedCacheEntry {
	cache: ErasedCache,
	clear_fn: fn(&dyn Any),
}

/// What identifies a task in the shared caches of every runtime that it has
/// used. When the last holder drops the identity, which is the task together
/// with every run of it, the identity removes the task's shared cache from
/// each of those runtimes.
pub(crate) struct TaskIdentity {
	pub(crate) id: u64,
	runtimes: Mutex<UsedRuntimes>,
}

/// The runtimes with shared-cache entries for one task.
struct UsedRuntimes {
	caches: Vec<Weak<SharedCaches>>,
	/// The length at which the next registration first removes the runtimes
	/// that no longer exist so that the removal costs a constant amount for
	/// each registration.
	prune_at: usize,
}

const MIN_PRUNE_LENGTH: usize = 8;

impl TaskIdentity {
	pub(crate) fn new(id: u64) -> Self {
		Self {
			id,
			runtimes: Mutex::new(UsedRuntimes {
				caches: Vec::new(),
				prune_at: MIN_PRUNE_LENGTH,
			}),
		}
	}

	fn register(&self, caches: Weak<SharedCaches>) {
		let mut used = lock(&self.runtimes);
		if used.caches.len() >= used.prune_at {
			used.caches.retain(|caches| caches.strong_count() > 0);
			used.prune_at = (used.caches.len() * 2).max(MIN_PRUNE_LENGTH);
		}
		used.caches.push(caches);
	}
}

impl Drop for TaskIdentity {
	fn drop(&mut self) {
		let used = std::mem::take(&mut lock(&self.runtimes).caches);
		for caches in used.iter().filter_map(Weak::upgrade) {
			caches.remove(self.id);
		}
	}
}

pub(crate) struct SharedCaches {
	caches: Mutex<HashMap<u64, SharedCacheEntry>>,
	events: EventSender,
}

impl SharedCaches {
	pub(crate) fn get_or_create<Cache: ClearCache + Default + Send + Sync + 'static>(
		self: &Arc<Self>,
		task: &TaskIdentity,
	) -> Arc<Cache> {
		let cache = Arc::clone(
			&lock(&self.caches)
				.entry(task.id)
				.or_insert_with(|| {
					task.register(Arc::downgrade(self));
					SharedCacheEntry {
						cache: Arc::new(Cache::default()),
						clear_fn: |cache| {
							cache
								.downcast_ref::<Cache>()
								.expect("registered cache type")
								.clear()
						},
					}
				})
				.cache,
		);
		cache
			.downcast()
			.unwrap_or_else(|_| unreachable!("one task identity has one shared cache type"))
	}

	pub(crate) fn find<Cache: Send + Sync + 'static>(&self, id: u64) -> Option<Arc<Cache>> {
		Some(
			Arc::clone(&lock(&self.caches).get(&id)?.cache)
				.downcast()
				.unwrap_or_else(|_| unreachable!("one task identity has one shared cache type")),
		)
	}

	/// Removes a task's cache without clearing it so that a run in progress
	/// keeps the cache that it holds.
	fn remove(&self, id: u64) {
		let removed = lock(&self.caches).remove(&id);
		if removed.is_some()
			&& let Some(sender) = &self.events
		{
			let _ = sender.send(TaskEvent {
				at: tokio::time::Instant::now(),
				task: None,
				kind: TaskEventKind::TaskCacheRemoved { task_id: id },
			});
		}
		drop(removed);
	}

	fn clear(&self) {
		let entries = std::mem::take(&mut *lock(&self.caches));
		for entry in entries.into_values() {
			(entry.clear_fn)(entry.cache.as_ref());
		}
	}
}

pub(crate) fn lock<Value>(mutex: &Mutex<Value>) -> MutexGuard<'_, Value> {
	mutex
		.lock()
		.unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn allocate_id(counter: &AtomicU64) -> u64 {
	let mut current = counter.load(Ordering::Relaxed);
	loop {
		let next = current
			.checked_add(1)
			.expect("task identifier space exhausted");
		match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
			Ok(_) => return current,
			Err(observed) => current = observed,
		}
	}
}
