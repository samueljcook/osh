//! The durable Queue's handle [`PgQueue`], which a [`PgRuntime`](crate::runtime::PgRuntime)
//! builds, and the Queue's task definitions, registries, jobs, and workers.
//!
//! A [`TaskDef`] specifies the name, the payload type, and the [`JobPolicy`] of one task.
//! Producers enqueue jobs with the definition, and workers register their handlers with the
//! same definition, so both sides agree on the task. Pass a pool to enqueue in a transaction of
//! its own, or pass a transaction when enqueueing must commit with app-owned state changes.
//! Scheduled `run_at_or_after` timestamps and retry due times are evaluated against the PostgreSQL
//! database clock so that clock skew between application processes does not advance or delay job
//! eligibility.
//!
//! ```rust,no_run
//! # async fn example(
//! #     runtime: &paranoid::runtime::PgRuntime,
//! #     pool: &paranoid::db::WritePool,
//! # ) -> Result<(), Box<dyn std::error::Error>> {
//! use paranoid::queue::{EnqueueOptions, JobPolicy, JobTimeout, PgQueueFields, TaskDef, TaskName};
//!
//! const BILLING_ROLLUP: TaskDef<String> = TaskDef::new(
//!     TaskName::from_static("billing.rollup.v1"),
//!     JobPolicy {
//!         max_retries: 5,
//!         timeout: JobTimeout::NoTimeout,
//!     },
//! );
//!
//! let queue = runtime.queue(PgQueueFields {
//!     payload_json_size_limit: 1 << 20,
//! })?;
//! let result = queue
//!     .enqueue(
//!         pool,
//!         &BILLING_ROLLUP,
//!         &"acct_123".to_owned(),
//!         EnqueueOptions {
//!             run_at_or_after: None,
//!             dedupe_key: None,
//!         },
//!     )
//!     .await?;
//! assert!(!result.deduplicated);
//! # Ok(())
//! # }
//! ```
//!
//! # Workers commit on their own
//!
//! A worker ([`PgQueue::start_worker`]) commits its claims,
//! heartbeats, job outcomes, and maintenance in transactions of its own. If the application
//! rolls back a transaction that it holds, that committed work stays, and so do the effects that
//! a handler had outside the database. The writes of a handler through
//! [`JobExecutionCtx::pool`] also commit apart from the completion of the job. A worker needs
//! pool connections in addition to the connections that the application's transactions hold;
//! see [`PoolConfig::max_connections`](crate::db::PoolConfig::max_connections).

use crate::db::{
	DatabaseOperationKind, RetryClassification, WritePool, WriteTx,
	finish_pool_owned_write_transaction_and_preserve_rollback_error, owned_portable_query,
};
use crate::installation::ComponentSchemaMigrationVersion;
use primitives_rs::id::time_sortable as id;
use serde::Serialize;
use std::fmt;
use std::marker::PhantomData;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) mod claim_token;
pub(crate) mod constants;
pub(crate) mod job_model;
mod operation_names;
pub(crate) mod operations;
pub(crate) mod pause;
mod preparation;
pub(crate) mod runtime_helpers;
pub(crate) mod v10;
pub(crate) mod validation;
pub(crate) mod worker;
pub(crate) use v10::source_schema::{
	PublishedV10QueueLayout, QueueSourceSchemaTables,
	read_published_v10_layout_in_current_transaction,
	validate_published_v10_physical_in_current_transaction,
};
mod auxiliary_state;
pub(crate) mod engine;
mod handle;
mod names;
pub(crate) mod postgres;
pub(crate) use auxiliary_state::QueuePauseScope;
mod group_state;
pub(crate) use group_state::{QueueAuxiliaryComponent, QueueGroupError};
pub(crate) use v10::source_auxiliary_bridge::{
	QueueAuxiliarySourceBridge, QueueAuxiliarySourceImportError, parse_source_pause_scope,
	source_pause_key,
};
pub(crate) use v10::source_bridge::QueueSourceBridge;
pub(crate) use v10::source_imports::QueueSourceImportDefs;
pub(crate) use v10::source_imports::{
	QueueInitialSourcePoliciesFields, QueueInitialSourcePolicyError, QueueInitialSourceSelection,
	QueueSourceImportBindingError,
};
mod state;
pub(crate) use state::{NativePayloadEncoding, QueueJobComponent, QueueJobInitialPolicyFields};
mod record_transitions;
pub use record_transitions::{
	QueueDeadLetterPolicy, QueueRecoveryPolicy, QueueRecoveryPolicyError, QueueRecoveryPolicyFields,
};
mod recovery_settings;
mod task_def;
use worker::steps::{
	NativeFinishedDeletion, NativeWorkerCleanup, NativeWorkerRecovery, start_native_worker,
};

use operations::*;
use pause::*;
use preparation::native_enqueue_request;
use runtime_helpers::*;
pub(crate) use v10::{QueueColumn, QueueSchemaLayout};
use validation::*;
use worker::job::*;
use worker::model::*;

use claim_token::{WORKER_CLAIM_TOKEN_LEN, WorkerClaimToken};

mod error;

pub use constants::*;
pub use error::{
	DedupeKeyError, DeleteFinishedJobsError, EnqueueRequestError, GenerateWorkerOwnerIdError,
	HeartbeatError, JobPolicyError, PersistedRowError, QueueRuntimeError, QueueRuntimeErrorKind,
	RecoverStaleClaimsError, RegisterTaskError, RetryPolicyError, RunAtOrAfterMicrosError,
	RunAtOrAfterSystemTimeError, StartWorkerError, TaskNameError, WorkerClaimError,
	WorkerConfigError, WorkerJobError, WorkerNameError, WorkerOwnerIdError, WorkerPersistenceError,
	WorkerRequeueError, WorkerRunError, WorkerStepError,
};
use error::{FromNativeQueueError, JobNotRunningCause, WorkerStepFailure};
pub use handle::{
	PgClaimDueFields, PgClaimedJob, PgCleanupOptions, PgCleanupResult, PgDeleteDeadLetterResult,
	PgJobChangeResult, PgJobTaskFilter, PgListDeadLettersOptions, PgListDeadLettersResult,
	PgListJobsOptions, PgListJobsResult, PgQueue, PgQueueError, PgQueueErrorKind, PgQueueFields,
	PgQueuePauseFields, PgQueuePauseTarget, PgQueueReader, PgReclaimOptions,
	PgRequeueDeadLetterResult, PgRequeueOptions, PgWorkerCleanup, PgWorkerCleanupFields,
	PgWorkerConfig, PgWorkerFields,
};
pub use job_model::*;
use names::check_worker_owner_id;
pub use names::{DedupeKey, TaskName, WorkerName};
pub use task_def::{JobPolicy, JobTimeout, TaskDef};
pub use worker::model::{
	JobExecutionCtx, RetryBackoffFn, RetryPolicy, TaskError, TaskRegistry, WorkerHandle,
	WorkerOwnerId, WorkerRunLoopSummary, WorkerShutdownOutcome,
};

use operation_names::*;

pub(crate) use state::upgrade;
