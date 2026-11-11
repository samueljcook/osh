use super::observation::{
	ObservationGuard, ObservationKind, ObservedOutcome, TaskEventKind, TaskInfo, emit,
};
use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::panic::{AssertUnwindSafe, Location};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::{BoxFuture, FutureExt, Shared, WeakShared};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, Span};

use super::runtime::{
	ClearCache, CtxRole, RunCtx, RunId, RunRole, RuntimeState, TaskCtx, TaskIdentity, WaitGuard,
	allocate_id, lock,
};
use super::shared_cache::{RetainedResultInsertion, SharedCache};

/// What a task body returns and what a task call resolves to.
pub type Result<Output, Failure> = std::result::Result<Output, Error<Failure>>;

/// A task body with its future boxed so that one task type holds any body.
type TaskBody<State, Input, Output, Failure> =
	Arc<dyn Fn(TaskCtx<State>, Input) -> BoxFuture<'static, Result<Output, Failure>> + Send + Sync>;

/// How long a shared task keeps a successful result after its run completes.
#[derive(Clone, Copy, Debug)]
pub enum RetentionTtl {
	/// Keeps the result for this positive duration after completion. An expiry
	/// beyond Tokio's clock range does not expire during that clock's lifetime.
	ExpiresAfter(Duration),
	/// Keeps the result until it is invalidated or evicted.
	Forever,
}

/// How many completed successful results a shared task keeps in one runtime.
#[derive(Clone, Copy, Debug)]
pub enum RetentionLimit {
	/// Evicts the least recently used results above this limit.
	Bounded(NonZeroUsize),
	/// Keeps any number of results.
	Unbounded,
}

/// How task contexts in one runtime share work. Without a sharing policy,
/// a task memoizes its outcomes within each context only.
#[derive(Clone, Copy, Debug)]
pub enum TaskSharing {
	/// Concurrent calls in different contexts join one run. A completed
	/// outcome is not kept for later contexts.
	InFlight,
	/// Concurrent calls in different contexts join one run, and successful
	/// results are kept for later contexts.
	RetainResults {
		/// How long a successful result is kept after its run completes.
		ttl: RetentionTtl,
		/// How many successful results are kept.
		result_limit: RetentionLimit,
	},
}

/// A sharing policy with a zero retention duration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskSharingError;

impl fmt::Display for TaskSharingError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("task retention duration must be positive")
	}
}

impl std::error::Error for TaskSharingError {}

/// An application failure, a cancellation, or a dependency cycle.
#[derive(Debug)]
pub enum Error<Failure> {
	/// The body failed. Every caller of the run receives the same failure.
	Failed(Arc<Failure>),
	/// The call was cancelled. A cancellation is never memoized or retained.
	Cancelled,
	/// The call would make runs wait on one another forever.
	Cycle {
		/// Where the task that the rejected call tried to run was defined.
		task_location: &'static Location<'static>,
	},
}

impl<Failure> Clone for Error<Failure> {
	fn clone(&self) -> Self {
		match self {
			Self::Failed(error) => Self::Failed(Arc::clone(error)),
			Self::Cancelled => Self::Cancelled,
			Self::Cycle { task_location } => Self::Cycle { task_location },
		}
	}
}

impl<Failure> From<Failure> for Error<Failure> {
	fn from(error: Failure) -> Self {
		Self::Failed(Arc::new(error))
	}
}

impl<Failure> Error<Failure> {
	/// Reports whether this outcome is a cancellation.
	pub fn is_cancelled(&self) -> bool {
		matches!(self, Self::Cancelled)
	}
}

impl<Failure: fmt::Display> fmt::Display for Error<Failure> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Failed(error) => error.fmt(formatter),
			Self::Cancelled => formatter.write_str("task cancelled"),
			Self::Cycle { task_location } => write!(
				formatter,
				"task cycle detected while resolving the task defined at {task_location}"
			),
		}
	}
}

impl<Failure: fmt::Debug + fmt::Display> std::error::Error for Error<Failure> {}

/// Input-keyed asynchronous work. Calls with equal input in one task context
/// share one run and its outcome.
///
/// A task body reads the application state from [`TaskCtx::state`]. Every
/// other fact that affects a result, including every caller-dependent fact
/// such as a viewer or a tenant, belongs in the input so that a result
/// depends only on the task, its input, and the state of its runtime.
///
/// A task is usually a static in a `std::sync::LazyLock`.
pub struct Task<State, Input, Output, Failure> {
	identity: Arc<TaskIdentity>,
	body_fn: TaskBody<State, Input, Output, Failure>,
	sharing: Option<TaskSharing>,
	location: &'static Location<'static>,
}

static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

impl<State, Input, Output, Failure> Task<State, Input, Output, Failure>
where
	State: Send + Sync + 'static,
	Input: Clone + Eq + Hash + Send + Sync + 'static,
	Output: Send + Sync + 'static,
	Failure: Send + Sync + 'static,
{
	/// Defines a task without starting work. The body receives the task
	/// context for nested task calls and the input. Cycle errors name the
	/// location of this call.
	#[track_caller]
	pub fn new<BodyFn, BodyFuture>(body_fn: BodyFn) -> Self
	where
		BodyFn: Fn(TaskCtx<State>, Input) -> BodyFuture + Send + Sync + 'static,
		BodyFuture: Future<Output = Result<Output, Failure>> + Send + 'static,
	{
		Self {
			identity: Arc::new(TaskIdentity::new(allocate_id(&NEXT_TASK_ID))),
			body_fn: Arc::new(move |ctx, input| body_fn(ctx, input).boxed()),
			sharing: None,
			location: Location::caller(),
		}
	}

	/// Shares this task's work across the task contexts of a runtime.
	///
	/// Returns [`TaskSharingError`] when a retention duration is zero.
	pub fn set_sharing(
		mut self,
		sharing: TaskSharing,
	) -> std::result::Result<Self, TaskSharingError> {
		if let TaskSharing::RetainResults {
			ttl: RetentionTtl::ExpiresAfter(ttl),
			..
		} = sharing
			&& ttl.is_zero()
		{
			return Err(TaskSharingError);
		}
		self.sharing = Some(sharing);
		Ok(self)
	}

	/// Resolves an input in this context: it returns a memoized outcome,
	/// joins the run in progress, or starts a run. Each run executes as its
	/// own Tokio task in the tracing span of the call that started it.
	/// Dropping the returned future ends this caller's part in the run, and
	/// the run stops when its last caller leaves.
	///
	/// # Panics
	/// When a task body panics, exactly one caller waiting on that run
	/// resumes the panic. The run is neither memoized nor retained, and the
	/// other callers start or join a replacement run.
	pub async fn run(&self, ctx: &TaskCtx<State>, input: Input) -> Result<Arc<Output>, Failure> {
		let mut observation = ObservationGuard::new(
			&ctx.state.runtime.events,
			self.info(),
			ObservationKind::Call,
		);
		let outcome = self.run_inner(ctx, input).await;
		if let Some(observation) = &mut observation {
			observation.finish(classify_outcome(&outcome));
		}
		outcome
	}

	/// Returns process-local identity and source metadata for telemetry filtering.
	pub fn info(&self) -> TaskInfo {
		TaskInfo {
			id: self.identity.id,
			location: self.location,
		}
	}

	async fn run_inner(&self, ctx: &TaskCtx<State>, input: Input) -> Result<Arc<Output>, Failure> {
		let span = Span::current();
		loop {
			let outcome = match &ctx.role {
				CtxRole::Root => {
					self.select_local_run(ctx, &input, &span)
						.into_outcome(self.location)
						.await
				}
				CtxRole::Run(run) => {
					if run.cancel.is_cancelled() {
						return Err(Error::Cancelled);
					}
					let selection = self.select_local_run(ctx, &input, &span);
					// Polling the selection first starts a new run before cancellation is
					// observed, so the run's cleanup guard always exists.
					let outcome = tokio::select! {
						biased;
						outcome = selection.into_outcome(self.location) => outcome,
						() = run.cancel.cancelled() => return Err(Error::Cancelled),
					};
					if matches!(outcome, RunOutcome::Settled(_)) && run.cancel.is_cancelled() {
						return Err(Error::Cancelled);
					}
					outcome
				}
			};
			match outcome {
				RunOutcome::Settled(outcome) => return outcome,
				RunOutcome::Panicked(panic) => {
					if let Some(payload) = panic.take() {
						std::panic::resume_unwind(payload);
					}
				}
			}
		}
	}

	fn select_local_run(
		&self,
		ctx: &TaskCtx<State>,
		input: &Input,
		span: &Span,
	) -> RunSelection<Output, Failure> {
		let runtime = &ctx.state.runtime;
		let cache = ctx
			.state
			.caches
			.get_or_create::<LocalCache<Input, Output, Failure>>(self.identity.id);
		let mut entries = lock(&cache);
		let found = match entries.get(input) {
			Some(LocalEntry::Done(outcome)) => {
				emit(&runtime.events, self.info(), TaskEventKind::MemoizedHit);
				Some(RunSelection::Outcome(outcome.clone()))
			}
			Some(LocalEntry::Running(entry)) => {
				let selected = join_run(entry, runtime, ctx.role.run_id());
				if let Some(selection) = &selected {
					emit(
						&runtime.events,
						self.info(),
						if matches!(selection, RunSelection::Cycle(_)) {
							TaskEventKind::CycleRejected
						} else {
							TaskEventKind::ContextRunJoined
						},
					);
				}
				selected
			}
			None => None,
		};
		found.unwrap_or_else(|| {
			let run_id = runtime.create_run_id();
			let run = create_local_run(LocalRun {
				cache: Arc::clone(&cache),
				run_id,
				ctx: RunCtx {
					state: Arc::clone(&ctx.state),
					run: RunRole {
						run_id,
						cancel: CancellationToken::new(),
					},
				},
				input: input.clone(),
				definition: self.definition(),
				span: span.clone(),
			});
			let (entry, selection) = start_run(runtime, ctx.role.run_id(), run_id, run);
			entries.insert(input.clone(), LocalEntry::Running(entry));
			selection
		})
	}

	fn definition(&self) -> TaskDefinition<State, Input, Output, Failure> {
		TaskDefinition {
			identity: Arc::clone(&self.identity),
			body_fn: Arc::clone(&self.body_fn),
			sharing: self.sharing,
			location: self.location,
		}
	}

	/// Removes this input's memoized outcome in the context and its shared
	/// state in the runtime. A run in progress may still finish for its
	/// callers, but it cannot restore the removed entries.
	pub fn invalidate(&self, ctx: &TaskCtx<State>, input: &Input) {
		if let Some(shared) = ctx
			.state
			.runtime
			.shared_caches
			.find::<Mutex<SharedCache<Input, Output, Failure>>>(self.identity.id)
		{
			let mut shared = lock(&shared);
			shared.remove(input);
		}
		if let Some(local) = ctx
			.state
			.caches
			.find::<LocalCache<Input, Output, Failure>>(self.identity.id)
		{
			lock(&local).remove(input);
		}
		emit(
			&ctx.state.runtime.events,
			self.info(),
			TaskEventKind::Invalidated,
		);
	}

	/// Removes each entry for which `predicate_fn` returns `true` for its input: the
	/// memoized outcomes in the context, and the runs in progress and the
	/// retained results in the runtime. A run in progress may still finish
	/// for its callers, but it cannot store its result afterward.
	///
	/// `predicate_fn` runs once for each input while this task holds the lock on
	/// a collection with an entry for that input, and its result applies to every
	/// collection. It must be cheap, and it must not call this task.
	pub fn invalidate_where(
		&self,
		ctx: &TaskCtx<State>,
		mut predicate_fn: impl FnMut(&Input) -> bool,
	) {
		let mut selections = HashMap::new();
		let mut predicate_fn = |input: &Input| {
			if let Some(&selected) = selections.get(input) {
				return selected;
			}
			let selected = predicate_fn(input);
			selections.insert(input.clone(), selected);
			selected
		};
		if let Some(shared) = ctx
			.state
			.runtime
			.shared_caches
			.find::<Mutex<SharedCache<Input, Output, Failure>>>(self.identity.id)
		{
			let mut shared = lock(&shared);
			shared.remove_where(&mut predicate_fn);
		}
		if let Some(local) = ctx
			.state
			.caches
			.find::<LocalCache<Input, Output, Failure>>(self.identity.id)
		{
			lock(&local).retain(|input, _| !predicate_fn(input));
		}
		emit(
			&ctx.state.runtime.events,
			self.info(),
			TaskEventKind::Invalidated,
		);
	}

	/// Removes every input's entries for this task in the context and in the
	/// runtime.
	pub fn invalidate_all(&self, ctx: &TaskCtx<State>) {
		if let Some(shared) = ctx
			.state
			.runtime
			.shared_caches
			.find::<Mutex<SharedCache<Input, Output, Failure>>>(self.identity.id)
		{
			shared.clear();
		}
		if let Some(local) = ctx
			.state
			.caches
			.find::<LocalCache<Input, Output, Failure>>(self.identity.id)
		{
			lock(&local).clear();
		}
		emit(
			&ctx.state.runtime.events,
			self.info(),
			TaskEventKind::Invalidated,
		);
	}
}

/// What a run needs from its task's definition.
struct TaskDefinition<State, Input, Output, Failure> {
	identity: Arc<TaskIdentity>,
	body_fn: TaskBody<State, Input, Output, Failure>,
	sharing: Option<TaskSharing>,
	location: &'static Location<'static>,
}

type Outcome<Output, Failure> = Result<Arc<Output>, Failure>;
type Run<Output, Failure> = Shared<BoxFuture<'static, RunOutcome<Output, Failure>>>;

/// How a run ended, as observed by every caller waiting on it.
enum RunOutcome<Output, Failure> {
	Settled(Outcome<Output, Failure>),
	Panicked(Arc<PanicPayload>),
}

impl<Output, Failure> Clone for RunOutcome<Output, Failure> {
	fn clone(&self) -> Self {
		match self {
			Self::Settled(outcome) => Self::Settled(outcome.clone()),
			Self::Panicked(panic) => Self::Panicked(Arc::clone(panic)),
		}
	}
}

/// A body's panic, which the first waiting caller to take it resumes.
struct PanicPayload(Mutex<PanicPayloadState>);

enum PanicPayloadState {
	Available(Box<dyn Any + Send>),
	Claimed,
}

impl PanicPayload {
	fn new(payload: Box<dyn Any + Send>) -> Arc<Self> {
		Arc::new(Self(Mutex::new(PanicPayloadState::Available(payload))))
	}

	fn take(&self) -> Option<Box<dyn Any + Send>> {
		match std::mem::replace(&mut *lock(&self.0), PanicPayloadState::Claimed) {
			PanicPayloadState::Available(payload) => Some(payload),
			PanicPayloadState::Claimed => None,
		}
	}
}

type LocalCache<Input, Output, Failure> = Mutex<HashMap<Input, LocalEntry<Output, Failure>>>;

enum LocalEntry<Output, Failure> {
	Running(RunningEntry<Output, Failure>),
	Done(Outcome<Output, Failure>),
}

/// A cache's weak registration for a run. Shared future ownership determines
/// whether a caller can join it; an expired registration requires replacement.
pub(super) struct RunningEntry<Output, Failure> {
	pub(super) run_id: RunId,
	run: WeakShared<BoxFuture<'static, RunOutcome<Output, Failure>>>,
}

impl<Input: Clone + Eq + Hash, Output, Failure> Default for SharedCache<Input, Output, Failure> {
	fn default() -> Self {
		Self::new()
	}
}

impl<Input: Clone + Eq + Hash, Output, Failure> ClearCache
	for Mutex<SharedCache<Input, Output, Failure>>
{
	fn clear(&self) {
		lock(self).clear();
	}
}

enum RunSelection<Output, Failure> {
	Outcome(Outcome<Output, Failure>),
	Wait {
		run: Run<Output, Failure>,
		wait: Option<WaitGuard>,
	},
	Cycle(Run<Output, Failure>),
}

impl<Output, Failure> RunSelection<Output, Failure> {
	async fn into_outcome(
		self,
		task_location: &'static Location<'static>,
	) -> RunOutcome<Output, Failure> {
		match self {
			Self::Outcome(outcome) => RunOutcome::Settled(outcome),
			Self::Wait { run, wait } => {
				let outcome = run.await;
				drop(wait);
				outcome
			}
			Self::Cycle(run) => {
				drop(run);
				RunOutcome::Settled(Err(Error::Cycle { task_location }))
			}
		}
	}
}

fn join_run<Output, Failure>(
	entry: &RunningEntry<Output, Failure>,
	runtime: &Arc<RuntimeState>,
	caller: Option<RunId>,
) -> Option<RunSelection<Output, Failure>> {
	let run = entry.run.upgrade()?;
	Some(match runtime.start_wait(caller, entry.run_id) {
		Ok(wait) => RunSelection::Wait { run, wait },
		Err(()) => RunSelection::Cycle(run),
	})
}

fn start_run<Output, Failure>(
	runtime: &Arc<RuntimeState>,
	caller: Option<RunId>,
	run_id: RunId,
	future: BoxFuture<'static, RunOutcome<Output, Failure>>,
) -> (RunningEntry<Output, Failure>, RunSelection<Output, Failure>) {
	let run = future.shared();
	let entry = RunningEntry {
		run_id,
		run: run.downgrade().expect("an unpolled run has not completed"),
	};
	let wait = runtime
		.start_wait(caller, run_id)
		.unwrap_or_else(|()| unreachable!("a new run waits on nothing"));
	(entry, RunSelection::Wait { run, wait })
}

struct LocalRun<State, Input, Output, Failure> {
	cache: Arc<LocalCache<Input, Output, Failure>>,
	ctx: RunCtx<State>,
	run_id: RunId,
	input: Input,
	definition: TaskDefinition<State, Input, Output, Failure>,
	span: Span,
}

fn create_local_run<State, Input, Output, Failure>(
	run: LocalRun<State, Input, Output, Failure>,
) -> BoxFuture<'static, RunOutcome<Output, Failure>>
where
	State: Send + Sync + 'static,
	Input: Clone + Eq + Hash + Send + Sync + 'static,
	Output: Send + Sync + 'static,
	Failure: Send + Sync + 'static,
{
	async move {
		let mut cleanup = LocalRunGuard {
			state: LocalRunGuardState::Running {
				cache: Arc::clone(&run.cache),
				input: run.input.clone(),
				run_id: run.run_id,
				cancel: run.ctx.run.cancel.clone(),
			},
		};
		let outcome = match run.definition.sharing {
			None => {
				spawn_body(BodyRun {
					task: TaskInfo {
						id: run.definition.identity.id,
						location: run.definition.location,
					},
					body_fn: run.definition.body_fn,
					ctx: run.ctx,
					input: run.input.clone(),
					span: run.span,
				})
				.await
			}
			Some(sharing) => {
				call_shared(SharedCall {
					identity: run.definition.identity,
					sharing,
					location: run.definition.location,
					body_fn: run.definition.body_fn,
					ctx: run.ctx,
					input: run.input.clone(),
					span: run.span,
				})
				.await
			}
		};
		if let RunOutcome::Settled(settled) = &outcome
			&& !matches!(settled, Err(Error::Cancelled))
		{
			let mut cache = lock(&run.cache);
			if matches!(
				cache.get(&run.input),
				Some(LocalEntry::Running(entry)) if entry.run_id == run.run_id
			) {
				cache.insert(run.input, LocalEntry::Done(settled.clone()));
			}
			cleanup.state = LocalRunGuardState::Completed;
		}
		outcome
	}
	.boxed()
}

struct SharedCall<State, Input, Output, Failure> {
	identity: Arc<TaskIdentity>,
	sharing: TaskSharing,
	location: &'static Location<'static>,
	body_fn: TaskBody<State, Input, Output, Failure>,
	ctx: RunCtx<State>,
	input: Input,
	span: Span,
}

async fn call_shared<State, Input, Output, Failure>(
	call: SharedCall<State, Input, Output, Failure>,
) -> RunOutcome<Output, Failure>
where
	State: Send + Sync + 'static,
	Input: Clone + Eq + Hash + Send + Sync + 'static,
	Output: Send + Sync + 'static,
	Failure: Send + Sync + 'static,
{
	let runtime = &call.ctx.state.runtime;
	if runtime.shared_execution_is_closed() {
		return RunOutcome::Settled(Err(Error::Cancelled));
	}
	let cache = runtime
		.shared_caches
		.get_or_create::<Mutex<SharedCache<Input, Output, Failure>>>(&call.identity);
	let selection = {
		let mut state = lock(&cache);
		if runtime.shared_execution_is_closed() {
			return RunOutcome::Settled(Err(Error::Cancelled));
		}
		let task_info = TaskInfo {
			id: call.identity.id,
			location: call.location,
		};
		let (retained, expired) = match call.sharing {
			TaskSharing::RetainResults { .. } => state.get_retained(&call.input, Instant::now()),
			TaskSharing::InFlight => (None, 0),
		};
		if expired > 0 {
			emit(
				&runtime.events,
				task_info,
				TaskEventKind::ResultsExpired { count: expired },
			);
		}
		if let Some(value) = retained {
			emit(&runtime.events, task_info, TaskEventKind::RetainedHit);
			RunSelection::Outcome(Ok(value))
		} else if let Some(selection) = state
			.running(&call.input)
			.and_then(|entry| join_run(entry, runtime, Some(call.ctx.run.run_id)))
		{
			emit(
				&runtime.events,
				task_info,
				if matches!(selection, RunSelection::Cycle(_)) {
					TaskEventKind::CycleRejected
				} else {
					TaskEventKind::RuntimeRunJoined
				},
			);
			selection
		} else {
			let run_id = runtime.create_run_id();
			let future = run_shared(SharedRun {
				task: TaskInfo {
					id: call.identity.id,
					location: call.location,
				},
				cache: Arc::clone(&cache),
				ctx: RunCtx {
					state: Arc::clone(&call.ctx.state),
					run: RunRole {
						run_id,
						cancel: runtime.cancel.child_token(),
					},
				},
				run_id,
				body_fn: call.body_fn,
				sharing: call.sharing,
				input: call.input.clone(),
				span: call.span,
			})
			.boxed();
			let (entry, selection) = start_run(runtime, Some(call.ctx.run.run_id), run_id, future);
			state.start_run(call.input, entry);
			selection
		}
	};
	selection.into_outcome(call.location).await
}

struct SharedRun<State, Input: Eq + Hash, Output, Failure> {
	task: TaskInfo,
	cache: Arc<Mutex<SharedCache<Input, Output, Failure>>>,
	ctx: RunCtx<State>,
	run_id: RunId,
	body_fn: TaskBody<State, Input, Output, Failure>,
	sharing: TaskSharing,
	input: Input,
	span: Span,
}

async fn run_shared<State, Input, Output, Failure>(
	run: SharedRun<State, Input, Output, Failure>,
) -> RunOutcome<Output, Failure>
where
	State: Send + Sync + 'static,
	Input: Clone + Eq + Hash + Send + Sync + 'static,
	Output: Send + Sync + 'static,
	Failure: Send + Sync + 'static,
{
	let _cleanup = SharedRunGuard {
		cache: Arc::clone(&run.cache),
		input: run.input.clone(),
		run_id: run.run_id,
	};
	let runtime = Arc::clone(&run.ctx.state.runtime);
	let outcome = spawn_body(BodyRun {
		task: run.task,
		body_fn: run.body_fn,
		ctx: run.ctx,
		input: run.input.clone(),
		span: run.span,
	})
	.await;
	if runtime.shared_execution_is_closed() {
		return RunOutcome::Settled(Err(Error::Cancelled));
	}
	if let (TaskSharing::RetainResults { ttl, result_limit }, RunOutcome::Settled(Ok(value))) =
		(run.sharing, &outcome)
	{
		let now = Instant::now();
		let expires_at = match ttl {
			// An expiry beyond the clock's range never arrives.
			RetentionTtl::ExpiresAfter(ttl) => now.checked_add(ttl),
			RetentionTtl::Forever => None,
		};
		let mut cache = lock(&run.cache);
		if !runtime.shared_execution_is_closed()
			&& let Some((expired, evicted)) = cache.retain_result(
				run.run_id,
				RetainedResultInsertion {
					input: run.input,
					value: Arc::clone(value),
					now,
					expires_at,
					limit: result_limit,
				},
			) {
			if expired > 0 {
				emit(
					&runtime.events,
					run.task,
					TaskEventKind::ResultsExpired { count: expired },
				);
			}
			emit(&runtime.events, run.task, TaskEventKind::ResultRetained);
			if evicted > 0 {
				emit(
					&runtime.events,
					run.task,
					TaskEventKind::ResultsEvicted { count: evicted },
				);
			}
		}
	}
	outcome
}

/// One execution of a body: its context, its input, and the tracing span of
/// the call that started the run.
struct BodyRun<State, Input, Output, Failure> {
	task: TaskInfo,
	body_fn: TaskBody<State, Input, Output, Failure>,
	ctx: RunCtx<State>,
	input: Input,
	span: Span,
}

/// Runs the body as its own Tokio task so that independent runs can execute
/// on separate threads.
async fn spawn_body<State, Input, Output, Failure>(
	run: BodyRun<State, Input, Output, Failure>,
) -> RunOutcome<Output, Failure>
where
	State: Send + Sync + 'static,
	Input: Send + 'static,
	Output: Send + Sync + 'static,
	Failure: Send + Sync + 'static,
{
	let cancel_on_abort = run.ctx.run.cancel.clone();
	let span = run.span.clone();
	let mut owned = BodyExecution {
		state: BodyExecutionState::Running {
			join: tokio::spawn(execute_body(run).instrument(span)),
			cancel: cancel_on_abort,
		},
	};
	let joined = owned.join().await;
	match joined {
		Ok(outcome) => outcome,
		Err(error) if error.is_panic() => {
			RunOutcome::Panicked(PanicPayload::new(error.into_panic()))
		}
		Err(_) => RunOutcome::Settled(Err(Error::Cancelled)),
	}
}

async fn execute_body<State, Input, Output, Failure>(
	run: BodyRun<State, Input, Output, Failure>,
) -> RunOutcome<Output, Failure>
where
	State: Send + Sync + 'static,
	Input: Send + 'static,
	Output: Send + Sync + 'static,
	Failure: Send + Sync + 'static,
{
	if run.ctx.run.cancel.is_cancelled() {
		return RunOutcome::Settled(Err(Error::Cancelled));
	}
	let mut observation = ObservationGuard::new(
		&run.ctx.state.runtime.events,
		run.task,
		ObservationKind::Execution {
			run_id: run.ctx.run.run_id.0,
		},
	);
	let BodyRun {
		body_fn,
		ctx,
		input,
		..
	} = run;
	let cancel = ctx.run.cancel.clone();
	let ctx = ctx.into_task_ctx();
	// The body's state is discarded after a panic, and the run's caches use
	// poison-tolerant locks, so observing the unwind cannot expose broken state.
	// The body function is called inside the guarded future, so a panic before
	// it returns its future is captured as well.
	let outcome = tokio::select! {
		biased;
		outcome = AssertUnwindSafe(async move { body_fn(ctx, input).await }).catch_unwind() => outcome,
		() = cancel.cancelled() => return RunOutcome::Settled(Err(Error::Cancelled)),
	};
	let outcome = match outcome {
		Err(payload) => RunOutcome::Panicked(PanicPayload::new(payload)),
		Ok(_) if cancel.is_cancelled() => RunOutcome::Settled(Err(Error::Cancelled)),
		Ok(outcome) => RunOutcome::Settled(outcome.map(Arc::new)),
	};
	if let Some(observation) = &mut observation {
		observation.finish(match &outcome {
			RunOutcome::Panicked(_) => ObservedOutcome::Panicked,
			RunOutcome::Settled(result) => classify_outcome(result),
		});
	}
	outcome
}

struct BodyExecution<Output> {
	state: BodyExecutionState<Output>,
}

enum BodyExecutionState<Output> {
	Running {
		join: JoinHandle<Output>,
		cancel: CancellationToken,
	},
	Joined,
}

impl<Output> BodyExecution<Output> {
	async fn join(&mut self) -> std::result::Result<Output, tokio::task::JoinError> {
		let BodyExecutionState::Running { join, .. } = &mut self.state else {
			unreachable!("a task body is joined once");
		};
		let joined = join.await;
		self.state = BodyExecutionState::Joined;
		joined
	}
}

impl<Output> Drop for BodyExecution<Output> {
	fn drop(&mut self) {
		if let BodyExecutionState::Running { join, cancel } = &self.state {
			cancel.cancel();
			join.abort();
		}
	}
}

struct LocalRunGuard<Input: Eq + Hash, Output, Failure> {
	state: LocalRunGuardState<Input, Output, Failure>,
}

enum LocalRunGuardState<Input: Eq + Hash, Output, Failure> {
	Running {
		cancel: CancellationToken,
		cache: Arc<LocalCache<Input, Output, Failure>>,
		input: Input,
		run_id: RunId,
	},
	Completed,
}

impl<Input: Eq + Hash, Output, Failure> Drop for LocalRunGuard<Input, Output, Failure> {
	fn drop(&mut self) {
		let LocalRunGuardState::Running {
			cancel,
			cache,
			input,
			run_id,
		} = &self.state
		else {
			return;
		};
		cancel.cancel();
		let mut cache = lock(cache);
		if matches!(
			cache.get(input),
			Some(LocalEntry::Running(entry)) if entry.run_id == *run_id
		) {
			cache.remove(input);
		}
	}
}

struct SharedRunGuard<Input: Clone + Eq + Hash, Output, Failure> {
	cache: Arc<Mutex<SharedCache<Input, Output, Failure>>>,
	input: Input,
	run_id: RunId,
}

impl<Input: Clone + Eq + Hash, Output, Failure> Drop for SharedRunGuard<Input, Output, Failure> {
	fn drop(&mut self) {
		let mut cache = lock(&self.cache);
		cache.remove_running(&self.input, self.run_id);
	}
}

fn classify_outcome<T, E>(outcome: &Result<T, E>) -> ObservedOutcome {
	match outcome {
		Ok(_) => ObservedOutcome::Succeeded,
		Err(Error::Failed(_)) => ObservedOutcome::Failed,
		Err(Error::Cancelled) => ObservedOutcome::Cancelled,
		Err(Error::Cycle { .. }) => ObservedOutcome::Cycle,
	}
}
