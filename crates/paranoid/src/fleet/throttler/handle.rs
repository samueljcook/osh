//! Same-runtime Throttler operations and managed delivery without public permit authority.

mod error;
mod task;

pub use error::{PgThrottlerError, PgThrottlerErrorKind};
pub use task::{
	PgThrottlerHold, PgThrottlerRelease, PgThrottlerTaskCtx, PgThrottlerTaskResult,
	PgThrottlerTryRunTaskResult,
};

use crate::db::{Database, WriteDatabase};
use crate::fleet::throttler::engine::PgNativeThrottler;
use crate::fleet::{ThrottlerKey, ThrottlerSettings, ThrottlerStatus};
use crate::runtime::throttler::NativeRuntimeThrottlerHandleError;
use crate::scope::ScopeIdentity;

/// How a run that holds a half-open circuit's probe renews the probe's reservation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PgThrottlerProbeHeartbeat {
	/// The Throttler has no circuit, so no run holds a probe.
	NoCircuit,
	/// The run renews the probe's reservation every `interval`, which must be positive and
	/// shorter than the circuit's probe window, and stops renewing after
	/// `max_consecutive_failures` consecutive failed renewals.
	Every {
		/// The time between renewals.
		interval: std::time::Duration,
		/// The consecutive failed renewals after which the run stops renewing.
		max_consecutive_failures: std::num::NonZeroUsize,
	},
}

/// The inputs of a Throttler handle.
#[derive(Clone, Debug, PartialEq)]
pub struct PgThrottlerFields {
	/// The scope that declares the Throttler's code.
	pub scope: ScopeIdentity,
	/// The Throttler's key.
	pub key: ThrottlerKey,
	/// The Throttler's rate, concurrency, and circuit settings, which every operation checks
	/// against the stored record.
	pub settings: ThrottlerSettings,
	/// How a probe renews its reservation. It must be `NoCircuit` exactly when `settings` has
	/// no circuit.
	pub probe_heartbeat: PgThrottlerProbeHeartbeat,
	/// The positive wait of [`PgThrottler::run_task`] before its next attempt after a denial
	/// that reports no wait of its own: a denial by the concurrency limit, or an open circuit.
	/// A denial by the rate limit reports the wait until the next token.
	pub acquire_retry_interval: std::time::Duration,
}

/// One Throttler in a declared Throttler scope of the runtime's installation.
///
/// Each operation checks the scope's installed policy and fails when the runtime does not use
/// the role that the operation needs: reading for `fetch_status`, and writing for runs, `reset`,
/// and the circuit operations. Holding a handle does not keep the runtime running.
///
/// A run does not stop its task when its concurrency slot or probe reservation ends, so another
/// run can take them while the task still runs. Make the task safe to overlap with another run.
/// A rate-limit token that a run took stays used, whatever the task's outcome.
#[derive(Clone)]
pub struct PgThrottler {
	rate_limit_cost: std::num::NonZeroU32,
	handle: PgNativeThrottler,
	probe_heartbeat:
		Option<crate::fleet::throttler::engine::NativeThrottlerProbeHeartbeatPolicyFields>,
	holder_prefix: crate::fleet::HolderPrefix,
	acquire_retry_interval: std::time::Duration,
}
impl PgThrottler {
	pub(crate) fn from_native(
		handle: PgNativeThrottler,
		probe_heartbeat: PgThrottlerProbeHeartbeat,
		holder_prefix: crate::fleet::HolderPrefix,
		acquire_retry_interval: std::time::Duration,
	) -> Result<Self, PgThrottlerError> {
		if acquire_retry_interval.is_zero()
			|| primitives_rs::time::Deadline::new_after(acquire_retry_interval).is_err()
		{
			return Err(PgThrottlerError::retry_interval());
		}
		let probe_heartbeat = match probe_heartbeat {
			PgThrottlerProbeHeartbeat::NoCircuit => None,
			PgThrottlerProbeHeartbeat::Every {
				interval,
				max_consecutive_failures,
			} => Some(
				crate::fleet::throttler::engine::NativeThrottlerProbeHeartbeatPolicyFields {
					interval,
					max_consecutive_failures,
				},
			),
		};
		handle
			.start_options(
				crate::fleet::throttler::engine::NativeThrottlerStartOptionsFields {
					holder: None,
					rate_limit_cost: std::num::NonZeroU32::MIN,
					probe_heartbeat,
				},
			)
			.map_err(PgThrottlerError::operation)?;
		Ok(Self {
			rate_limit_cost: std::num::NonZeroU32::MIN,
			handle,
			probe_heartbeat,
			holder_prefix,
			acquire_retry_interval,
		})
	}
	pub(crate) fn construction_error(
		source: NativeRuntimeThrottlerHandleError,
	) -> PgThrottlerError {
		PgThrottlerError::construction(source)
	}
	/// Sets the number of rate-limit tokens that each run consumes atomically.
	///
	/// The handle must have a rate limit, and the cost must not exceed its capacity.
	/// Handles with different costs share the same stored budget. Concurrency and
	/// circuit accounting still apply once per run, and consumed tokens are never
	/// refunded. A newly constructed handle consumes one token per run.
	pub fn with_rate_limit_cost(
		mut self,
		cost: std::num::NonZeroU32,
	) -> Result<Self, PgThrottlerError> {
		if self
			.settings()
			.rate()
			.is_none_or(|rate| cost.get() > rate.capacity())
		{
			return Err(PgThrottlerError::operation(
				crate::fleet::throttler::engine::NativeThrottlerError::InvalidRateLimitCost,
			));
		}
		self.rate_limit_cost = cost;
		Ok(self)
	}
	/// Returns the rate-limit token cost of each run through this handle.
	pub fn rate_limit_cost(&self) -> std::num::NonZeroU32 {
		self.rate_limit_cost
	}
	/// The Throttler's scope.
	pub fn scope(&self) -> &ScopeIdentity {
		self.handle.scope()
	}
	/// The Throttler's key.
	pub fn key(&self) -> &ThrottlerKey {
		self.handle.key()
	}
	/// The settings that every operation checks against the stored Throttler.
	pub fn settings(&self) -> &ThrottlerSettings {
		self.handle.settings()
	}
	/// Reads the Throttler's current rate tokens, held concurrency slots, and circuit state. An
	/// absent Throttler reads as unused; the read does not create it.
	pub async fn fetch_status(
		&self,
		database: impl Database,
	) -> Result<ThrottlerStatus, PgThrottlerError> {
		self.handle
			.fetch_status(database)
			.await
			.map_err(PgThrottlerError::point)
	}
	/// Resets the Throttler to unused: full rate tokens, no held concurrency slots, and a closed
	/// circuit with no recorded failures. The stored settings stay. Running tasks keep running.
	/// In a caller's transaction, the change takes effect when the caller commits.
	pub async fn reset(&self, database: impl WriteDatabase) -> Result<(), PgThrottlerError> {
		self.handle
			.reset(database)
			.await
			.map_err(PgThrottlerError::point)
	}
	/// Opens the Throttler's circuit as consecutive failures would open it. When the settings have
	/// no circuit, it fails with [`PgThrottlerErrorKind::State`] without database work.
	pub async fn open_circuit(&self, database: impl WriteDatabase) -> Result<(), PgThrottlerError> {
		self.handle
			.open_circuit(database)
			.await
			.map_err(PgThrottlerError::point)
	}
	/// Closes the Throttler's circuit. When the settings have no circuit, it fails with
	/// [`PgThrottlerErrorKind::State`] without database work.
	pub async fn close_circuit(
		&self,
		database: impl WriteDatabase,
	) -> Result<(), PgThrottlerError> {
		self.handle
			.close_circuit(database)
			.await
			.map_err(PgThrottlerError::point)
	}
}
impl std::fmt::Debug for PgThrottler {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("PgThrottler").finish_non_exhaustive()
	}
}
