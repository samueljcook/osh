use std::fmt;
use std::{num::NonZeroUsize, panic::Location, time::Duration};
use tokio::{sync::broadcast, time::Instant};

/// Bounded passive runtime observation. Slow subscribers receive Tokio's
/// `RecvError::Lagged`; observation never backpressures task execution.
#[derive(Clone, Copy, Debug)]
pub struct ObservationOptions {
	/// Maximum retained events, shared by all subscribers. Tokio rounds capacity
	/// up to a power of two. This is telemetry, not a durable audit log.
	pub event_capacity: NonZeroUsize,
}
/// An observation capacity that exceeds Tokio broadcast's supported range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObservationOptionsError;
impl fmt::Display for ObservationOptionsError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("task event capacity exceeds usize::MAX / 2")
	}
}
impl std::error::Error for ObservationOptionsError {}

/// Process-local task identity and definition location. No inputs or outputs are exposed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskInfo {
	/// Unique identity of this task definition within the process.
	pub id: u64,
	/// Source location of task construction.
	pub location: &'static Location<'static>,
}
/// Outcome classification without application values, errors, or panic payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservedOutcome {
	/// The operation succeeded.
	Succeeded,
	/// The task returned an application failure.
	Failed,
	/// Cancellation or the last caller's departure stopped the operation.
	Cancelled,
	/// A dependency cycle rejected the operation.
	Cycle,
	/// A task panic interrupted the operation.
	Panicked,
}
/// An observed transition. Cache selections do not imply body execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskEventKind {
	/// This call used a completed context-local outcome.
	MemoizedHit,
	/// This call joined a context-local run.
	ContextRunJoined,
	/// This context joined a run shared across contexts.
	RuntimeRunJoined,
	/// This context used a retained successful result.
	RetainedHit,
	/// A dependency cycle rejected a selected run.
	CycleRejected,
	/// A real task body is about to execute.
	ExecutionStarted {
		/// The runtime-local run identity.
		run_id: u64,
	},
	/// A real task body completed, panicked, or was dropped after cancellation.
	ExecutionFinished {
		/// The identity from its matching start event.
		run_id: u64,
		/// The outcome before context memoization or shared retention.
		outcome: ObservedOutcome,
		/// Elapsed Tokio time, including suspended time.
		duration: Duration,
	},
	/// One public `Task::run` call completed or its future was dropped.
	CallFinished {
		/// The caller-visible outcome, or cancellation when its future was dropped.
		outcome: ObservedOutcome,
		/// Elapsed Tokio time for this call.
		duration: Duration,
	},
	/// A successful result entered shared retention.
	ResultRetained,
	/// Expired shared results were removed by a lookup or insertion.
	ResultsExpired {
		/// Number of removed results.
		count: usize,
	},
	/// Capacity enforcement removed least recently used results.
	ResultsEvicted {
		/// Number of removed results.
		count: usize,
	},
	/// An explicit invalidation operation completed; already-absent entries are unchanged.
	Invalidated,
	/// Dropping a task definition removed its shared cache from this runtime.
	TaskCacheRemoved {
		/// Process-local identity of the removed task.
		task_id: u64,
	},
	/// Runtime closure cancelled shared runs and cleared shared caches.
	RuntimeClosed,
}
/// A bounded telemetry event. Concurrent events follow channel send order;
/// timestamps do not establish a total order across threads.
#[derive(Clone, Debug)]
pub struct TaskEvent {
	/// Tokio clock reading when the transition was observed.
	pub at: Instant,
	/// Task metadata, absent for runtime-wide closure and cache-removal events.
	pub task: Option<TaskInfo>,
	/// The observed transition.
	pub kind: TaskEventKind,
}

pub(crate) type EventSender = Option<broadcast::Sender<TaskEvent>>;
pub(crate) fn emit(sender: &EventSender, task: TaskInfo, kind: TaskEventKind) {
	if let Some(sender) = sender {
		let _ = sender.send(TaskEvent {
			at: Instant::now(),
			task: Some(task),
			kind,
		});
	}
}
pub(crate) struct ObservationGuard {
	sender: broadcast::Sender<TaskEvent>,
	task: TaskInfo,
	kind: ObservationKind,
	started: Instant,
	completion: ObservationCompletion,
}

pub(super) enum ObservationKind {
	Call,
	Execution { run_id: u64 },
}

enum ObservationCompletion {
	Pending,
	Finished(ObservedOutcome),
}
impl ObservationGuard {
	pub(super) fn new(sender: &EventSender, task: TaskInfo, kind: ObservationKind) -> Option<Self> {
		let sender = sender.as_ref()?.clone();
		let started = Instant::now();
		if let ObservationKind::Execution { run_id } = kind {
			let _ = sender.send(TaskEvent {
				at: started,
				task: Some(task),
				kind: TaskEventKind::ExecutionStarted { run_id },
			});
		}
		Some(Self {
			sender,
			task,
			kind,
			started,
			completion: ObservationCompletion::Pending,
		})
	}

	pub(super) fn finish(&mut self, outcome: ObservedOutcome) {
		assert!(
			matches!(self.completion, ObservationCompletion::Pending),
			"a task observation finishes once"
		);
		self.completion = ObservationCompletion::Finished(outcome);
	}
}
impl Drop for ObservationGuard {
	fn drop(&mut self) {
		let at = Instant::now();
		let duration = at.saturating_duration_since(self.started);
		let outcome = if std::thread::panicking() {
			ObservedOutcome::Panicked
		} else {
			match self.completion {
				ObservationCompletion::Pending => ObservedOutcome::Cancelled,
				ObservationCompletion::Finished(outcome) => outcome,
			}
		};
		let kind = match self.kind {
			ObservationKind::Execution { run_id } => TaskEventKind::ExecutionFinished {
				run_id,
				outcome,
				duration,
			},
			ObservationKind::Call => TaskEventKind::CallFinished { outcome, duration },
		};
		let _ = self.sender.send(TaskEvent {
			at,
			task: Some(self.task),
			kind,
		});
	}
}
