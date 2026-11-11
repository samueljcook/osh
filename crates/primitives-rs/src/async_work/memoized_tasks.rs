//! Input-keyed asynchronous work with memoization and explicit sharing.
//!
//! A [`Task`] is input-keyed work with an ordinary async function as its body.
//! Calls with equal input in one [`TaskCtx`] share one run, and the context
//! keeps the run's success or failure for later calls. Without a share, a
//! task memoizes within each context only. [`TaskSharing::InFlight`] also lets
//! concurrent calls in different contexts of one [`TaskRuntime`] join one run.
//! [`TaskSharing::RetainResults`] also keeps successful results for later contexts,
//! with an expiry and a limit per task.
//!
//! A [`TaskRuntime`] holds the application state, and a task body reads it
//! from [`TaskCtx::state`]. Every other fact that affects a result belongs in
//! the task's input.
//!
//! Every run executes its body as its own Tokio task, in the tracing span of
//! the call that started the run, so independent runs can execute on separate
//! threads. A body that calls independent tasks runs them in parallel by
//! joining the calls, for example with `tokio::try_join!`. The callers of a run
//! own it jointly: dropping one caller's future keeps the run for the others,
//! and the run stops when its last caller leaves. A shared run keeps the
//! context of the call that started it for its nested calls. Invalidation
//! prevents a run in progress from restoring a removed entry. A call that
//! would make runs wait on one another forever fails with [`Error::Cycle`].
//!
//! Retention expiry reads Tokio's clock, so a test can pause and advance time
//! with `tokio::time`. Each call of a task with [`TaskSharing::RetainResults`], and each
//! result that the task retains, removes the task's expired results from its
//! runtime so that an expired result leaves without a call that has its input.
//! Dropping a task releases its shared cache, including the results that it
//! retains, in every runtime.
//! [`TaskRuntime::close`] and dropping the last runtime clone stop shared runs
//! and release retained results, even while task contexts still exist.
//!
//! [`TaskRuntime::new_with_observation`] enables a bounded event channel for
//! execution outcomes, cache selections, sharing, retention, and invalidation.
//! [`TaskRuntime::subscribe_events`] returns a Tokio broadcast receiver. Slow
//! receivers report lag without delaying task execution. Events include task
//! identities and definition locations, but no application inputs or results.

mod observation;
mod runtime;
mod shared_cache;
mod task;

pub use runtime::{TaskCtx, TaskRuntime};
pub use task::{Error, Result, RetentionLimit, RetentionTtl, Task, TaskSharing, TaskSharingError};

pub use observation::{
	ObservationOptions, ObservationOptionsError, ObservedOutcome, TaskEvent, TaskEventKind,
	TaskInfo,
};
