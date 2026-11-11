//! The coordination handles that a [`PgRuntime`](crate::runtime::PgRuntime) builds (leases,
//! Mutexes, Crons, Once, coalescing caches, Semaphores, Throttlers, Counters, and Topics), the
//! rate limiter [`PgRateLimiter`], and their names, durations, tokens, settings, and outcomes.
//!
//! A key such as [`MutexKey`] or [`CounterKey`] names one shared record, so every process that
//! shares the record must use the same key text. Two processes that spell a key differently use
//! two different records.
//!
//! # Work that commits on its own
//!
//! A task run (`run_task` and `try_run_task` of a Mutex, Once, Semaphore, or Throttler), a
//! Cron's loop, and a coalescing cache's `fetch_or_compute` take no transaction of the
//! application. Each of them commits its claims, renewals, releases, and stored state in
//! transactions of its own. If the application rolls back a transaction that it holds, that
//! committed work stays, and so do the effects that a task had outside the database. Queue
//! workers behave the same way.
//!
//! # Claims, expiry, and fencing
//!
//! Every claim expires. A lease claim of a Mutex, Once, Cron, or coalescing cache expires after
//! its [`ClaimDuration`] unless a renewal extends it, and a Semaphore slot expires after its
//! [`max_hold_duration`](SemaphoreSettingsFields::max_hold_duration), which nothing extends. A
//! run does not stop its task when it loses its claim. The task can wait for the loss with
//! [`PgLeaseTaskCtx::wait_until_leadership_lost`],
//! and the run reports the loss after the task returns, for example as
//! [`MutexLeadership::Lost`], so a task that outlasts its claim can run at the same time as the
//! next holder of the same key. A lease claim carries a [`FencingToken`] that an external
//! resource can use to reject an expired holder. Semaphore and Throttler slots carry none.
//!
//! # Status reads
//!
//! A holder or status read, such as a [`SemaphoreStatus`] or a [`ThrottlerStatus`], describes
//! the state at the time of the read. It reserves nothing, and the state can change before the
//! application acts on it, so the application must not treat it as permission to proceed. To
//! proceed, the application must hold a claim.

pub use self::lease::{
	ClaimDuration, ClaimDurationError, CoordinationKeyError, FencingToken, HolderId, HolderIdError,
	HolderPrefix, HolderPrefixError, LeaseKey, MAX_HOLDER_PREFIX_LEN, PersistedLeaseError,
};
use crate::installation::ComponentSchemaMigrationVersion;
use crate::kv::{KeyError as KvKeyError, KeyPrefix as KvKeyPrefix};
use std::fmt;
use std::time::Duration;

// Shared with `db` internals; externally invisible.
pub(crate) mod acquire_retry_jitter;
mod acquisition_capability;
mod constants;
mod error;
pub(crate) mod lease;
pub(crate) mod throttler;
pub(crate) mod topic;

pub(crate) mod cache;
pub(crate) mod counter;
pub(crate) mod cron;
mod keys;
pub(crate) mod mutex;
pub(crate) mod namespace_keys;
pub(crate) mod once;
pub(crate) mod payload;
pub(crate) mod primitive_scope;
mod private_capability_wire;
pub(crate) mod rate_limiter;
pub(crate) mod resource;
pub(crate) mod scalar;
pub use semaphore::state::{SemaphoreSettings, SemaphoreSettingsError, SemaphoreSettingsFields};
pub use throttler::state::{
	ThrottlerCircuitSettingsFields, ThrottlerConcurrencySettingsFields, ThrottlerSettings,
	ThrottlerSettingsError, ThrottlerSettingsFields,
};

pub(crate) mod scalar_codec;
pub(crate) mod semaphore;
pub(crate) mod v10;

pub(crate) use constants::*;
pub use constants::{MAX_SETTINGS_DURATION, MAX_SLOT_COUNT};
pub(crate) use error::{InvalidTopicEventSequenceSuffixError, NegativeTopicSequenceError};
pub use error::{KeyError, RandomnessError};
pub use error::{
	ThrottlerCircuitSettingsError, ThrottlerConcurrencySettingsError, ThrottlerRateSettingsError,
};
pub use keys::{
	CoalescingCacheKey, CounterKey, CronKey, MutexKey, OnceKey, RootKey, SemaphoreKey,
	SubscriptionKey, ThrottlerKey, TopicKey,
};
pub use mutex::{
	MutexLeadership, PgMutex, PgMutexFields, PgMutexTaskResult, PgMutexTryRunTaskResult,
};
pub(crate) use semaphore::model::SemaphoreClaim;
pub use semaphore::model::{SemaphoreClaimSnapshot, SemaphoreStatus};
pub use throttler::{CircuitState, RateLimit, ThrottlerStatus};

pub use cache::handle::{
	PgCoalescingCache, PgCoalescingCacheComputeResult, PgCoalescingCacheComputeTimeout,
	PgCoalescingCacheEntryFields, PgCoalescingCacheError, PgCoalescingCacheErrorKind,
	PgCoalescingCacheFetchResult, PgCoalescingCacheFields, PgCoalescingCacheOptions,
	PgCoalescingCacheOptionsFields, PgCoalescingCachePublication,
};
pub use counter::handle::{PgCounter, PgCounterError, PgCounterErrorKind, PgCounterFields};
pub use cron::{PgCron, PgCronError, PgCronFields, PgCronRunError, PgCronTaskErrorAction};
pub use lease::handle::{
	PgLeadership, PgLease, PgLeaseClaim, PgLeaseError, PgLeaseErrorKind, PgLeaseFields,
	PgLeaseHolder, PgLeaseKeyError, PgLeaseRelease, PgLeaseRunError, PgLeaseRunErrorKind,
	PgLeaseRunOptions, PgLeaseRunOptionsError, PgLeaseRunOptionsFields, PgLeaseTaskCtx,
};
pub use once::handle::{
	PgOnce, PgOnceCompletion, PgOnceCompletionResult, PgOnceError, PgOnceErrorKind, PgOnceFields,
	PgOnceRunTaskResult, PgOnceTaskResult, PgOnceTryRunTaskResult,
};
pub use rate_limiter::{
	PgRateLimitInput, PgRateLimitResult, PgRateLimiter, PgRateLimiterError, PgRateLimiterFields,
};
pub use semaphore::handle::{
	PgSemaphore, PgSemaphoreError, PgSemaphoreErrorKind, PgSemaphoreFields, PgSemaphoreHold,
	PgSemaphoreRelease, PgSemaphoreTaskCtx, PgSemaphoreTaskResult, PgSemaphoreTryRunTaskResult,
};
pub use throttler::handle::{
	PgThrottler, PgThrottlerError, PgThrottlerErrorKind, PgThrottlerFields, PgThrottlerHold,
	PgThrottlerProbeHeartbeat, PgThrottlerRelease, PgThrottlerTaskCtx, PgThrottlerTaskResult,
	PgThrottlerTryRunTaskResult,
};
pub use topic::handle::{
	PgTopic, PgTopicCursorFields, PgTopicError, PgTopicErrorKind, PgTopicEvent, PgTopicFields,
	PgTopicPublication, PgTopicReadOptions, PgTopicReadResult, PgTopicSubscription,
	PgTopicSubscriptionFields,
};

pub(crate) use acquire_retry_jitter::generate_jittered_acquire_retry_delay;
use acquisition_capability::*;
use namespace_keys::*;
