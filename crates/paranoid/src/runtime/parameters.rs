//! Paranoid's fixed internal runtime timings and budgets. Applications do not configure them.
//!
//! - Participation lasts 30 seconds and renews every 10 seconds, so a renewal can fail twice
//!   before the participation expires. Expired registrations can remain stored until
//!   participant inspection removes them. Each participation control operation may take 15
//!   seconds. A renewal that times out ends the participation, and an upgrade step can make a
//!   renewal wait for the 5-second v10 lock wait and then the 5 seconds of its protected work,
//!   so the deadline is longer than both together.
//! - An acquisition of a lease, Semaphore, or Throttler, and the dispatch of a Throttler
//!   outcome, end only with the runtime's confirmed lifetime so that waiting for a key under
//!   contention is not a failure. Each other lease control operation, and each ordering proof
//!   attempt and cleanup attempt, may take 5 seconds. Retries of proofs, cleanups, releases, and
//!   outcomes run every 200 milliseconds, and 3 consecutive cleanup, release, or outcome
//!   failures end the retries.
//! - A write that meets a serialization failure or a write conflict runs at most 5 times, and
//!   Paranoid attempts to acquire a changing record group at most 5 times. A bounded record group holds at most 64 records.
//! - After a runtime fails during startup, it waits at most 5 seconds for its own cleanup.

use std::num::{NonZeroU32, NonZeroUsize};
use std::time::Duration;

use crate::fleet::lease::driver_timing::{LeaseControlTiming, LeaseControlTimingFields};
use crate::fleet::semaphore::engine::{
	SemaphoreDriverTiming, SemaphoreDriverTimingFields, SemaphoreExecutionTiming,
	SemaphoreExecutionTimingFields,
};
use crate::fleet::throttler::engine::{
	ThrottlerDriverTiming, ThrottlerDriverTimingFields, ThrottlerExecutionTiming,
	ThrottlerExecutionTimingFields,
};
use crate::runtime::participation::{ParticipationTiming, ParticipationTimingFields};

use super::semaphore::SemaphoreRuntimeConfig;
use super::throttler::ThrottlerRuntimeConfig;

const OPERATION_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_INTERVAL: Duration = Duration::from_millis(200);
const MAX_CONSECUTIVE_FAILURES: u32 = 3;

pub(crate) const CONFLICT_ATTEMPTS: NonZeroU32 = NonZeroU32::new(5).expect("nonzero");
pub(crate) const WHOLE_SET_ATTEMPTS: NonZeroU32 = NonZeroU32::new(5).expect("nonzero");
pub(crate) const GROUP_IDENTITY_COUNT: NonZeroUsize = NonZeroUsize::new(64).expect("nonzero");
pub(crate) const STARTUP_CLEANUP_WAIT: Duration = Duration::from_secs(5);

pub(crate) fn participation_timing() -> ParticipationTiming {
	ParticipationTiming::new(ParticipationTimingFields {
		participation_duration: Duration::from_secs(30),
		renewal_interval: Duration::from_secs(10),
		control_operation_timeout: Duration::from_secs(15),
	})
	.expect("Paranoid's participation timing is valid")
}

pub(crate) fn lease_timing() -> LeaseControlTiming {
	LeaseControlTiming::new(LeaseControlTimingFields {
		operation_timeout: OPERATION_TIMEOUT,
		physical_proof_attempt_timeout: OPERATION_TIMEOUT,
		physical_proof_retry_interval: RETRY_INTERVAL,
		cleanup_attempt_timeout: OPERATION_TIMEOUT,
		cleanup_retry_interval: RETRY_INTERVAL,
		max_consecutive_cleanup_failures: MAX_CONSECUTIVE_FAILURES,
	})
	.expect("Paranoid's lease timing is valid")
}

fn max_consecutive_failures() -> NonZeroUsize {
	NonZeroUsize::new(MAX_CONSECUTIVE_FAILURES as usize).expect("nonzero")
}

pub(crate) fn semaphore() -> SemaphoreRuntimeConfig {
	SemaphoreRuntimeConfig {
		execution_timing: SemaphoreExecutionTiming::new(SemaphoreExecutionTimingFields {
			physical_proof_attempt_timeout: OPERATION_TIMEOUT,
			cleanup_attempt_timeout: OPERATION_TIMEOUT,
			ending_wait_timeout: OPERATION_TIMEOUT,
		})
		.expect("Paranoid's Semaphore execution timing is valid"),
		driver_timing: SemaphoreDriverTiming::new(SemaphoreDriverTimingFields {
			physical_proof_retry_interval: RETRY_INTERVAL,
			cleanup_retry_interval: RETRY_INTERVAL,
			max_consecutive_cleanup_failures: max_consecutive_failures(),
			release_retry_interval: RETRY_INTERVAL,
			max_consecutive_release_failures: max_consecutive_failures(),
		})
		.expect("Paranoid's Semaphore driver timing is valid"),
	}
}

pub(crate) fn throttler() -> ThrottlerRuntimeConfig {
	ThrottlerRuntimeConfig {
		execution_timing: ThrottlerExecutionTiming::new(ThrottlerExecutionTimingFields {
			physical_proof_attempt_timeout: OPERATION_TIMEOUT,
			cleanup_attempt_timeout: OPERATION_TIMEOUT,
			ending_wait_timeout: OPERATION_TIMEOUT,
		})
		.expect("Paranoid's Throttler execution timing is valid"),
		driver_timing: ThrottlerDriverTiming::new(ThrottlerDriverTimingFields {
			physical_proof_retry_interval: RETRY_INTERVAL,
			cleanup_retry_interval: RETRY_INTERVAL,
			max_consecutive_cleanup_failures: max_consecutive_failures(),
			outcome_retry_interval: RETRY_INTERVAL,
			max_consecutive_outcome_failures: max_consecutive_failures(),
		})
		.expect("Paranoid's Throttler driver timing is valid"),
	}
}

/// The longest wait for the installation's control and v10 pins during upgrade work.
pub(crate) const UPGRADE_CONTROL_PIN_WAIT: Duration = OPERATION_TIMEOUT;

/// An upgrade driver waits 5 seconds for each transaction ending and each recovery
/// observation, retries a transient failure every 200 milliseconds up to 5 consecutive times,
/// and runs at most 64 steps before it yields to the runtime's other tasks.
pub(crate) fn upgrade_driver_options()
-> crate::upgrade::engine::driver::options::NativeUpgradeDriverOptions {
	use crate::upgrade::engine::driver::options::{
		NativeUpgradeDriverOptions, NativeUpgradeDriverOptionsFields,
	};
	NativeUpgradeDriverOptions::new(NativeUpgradeDriverOptionsFields {
		ending_wait_timeout: OPERATION_TIMEOUT,
		retry_interval: RETRY_INTERVAL,
		recovery_wait_timeout: OPERATION_TIMEOUT,
		max_consecutive_transient_failures: NonZeroU32::new(5).expect("nonzero"),
		max_steps_per_pass: NonZeroU32::new(64).expect("nonzero"),
	})
	.expect("Paranoid's upgrade driver options are valid")
}

/// An upgrade converts at most 500 rows per page; after a step ends because one of its waits
/// elapsed, the upgrade halves the page size, and after each step that succeeds, it doubles
/// the page size again up to 500. A v10 conversion stops taking keys at half of its wait. A
/// participant inspection reads at most 10,000 participant rows in pages of 100; when an inspection
/// reaches that limit, the upgrade deletes the expired registrations among those rows and
/// runs the step again. Protected work and v10 exclusion each wait at most 5 seconds.
pub(crate) fn upgrade_execution_limits()
-> crate::upgrade::engine::execution::limits::NativeUpgradeExecutionLimits {
	use crate::upgrade::engine::execution::limits::{
		NativeUpgradeExecutionLimits, NativeUpgradeExecutionLimitsFields,
	};
	use crate::upgrade::participants::ParticipantInspectionBudget;
	NativeUpgradeExecutionLimits::new(NativeUpgradeExecutionLimitsFields {
		limit: NonZeroU32::new(500).expect("nonzero"),
		participant_budget: ParticipantInspectionBudget {
			max_rows: NonZeroU32::new(10_000).expect("nonzero"),
			max_page_queries: NonZeroU32::new(100).expect("nonzero"),
			page_size: NonZeroU32::new(100).expect("nonzero"),
		},
		protected_work_wait_timeout: OPERATION_TIMEOUT,
		source_lock_wait_timeout: OPERATION_TIMEOUT,
	})
	.expect("Paranoid's upgrade execution limits are valid")
}
