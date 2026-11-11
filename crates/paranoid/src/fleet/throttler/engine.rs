//! Private managed Throttler group execution and caller-owned task accounting.
//!
//! Persistent operation ownership retains exact targets and receipts through cancellation.
//! Queued delivery requires known commit and actual local membership before caller work.

use std::{collections::BTreeSet, sync::Arc, time::Duration};
use tokio::time::Instant;

use super::model::ThrottlerTaskOutcome;
use super::state::outcome::{
	ThrottlerAcquireGroupFields, ThrottlerAcquireScopeGroupFields,
	ThrottlerPendingOutcomeGroupFields, ThrottlerPendingOutcomeScopeGroupFields,
	ThrottlerUndeliveredGroupFields, ThrottlerUndeliveredScopeGroupFields,
};
use super::state::{ThrottlerAcquisitionResult, ThrottlerSettings};
use crate::db::{DatabaseError, TransactionFailure, WritePool};
use crate::fleet::primitive_scope::{
	PrimitiveScopeBindingError, RetainedThrottlerScopeCode, ThrottlerResourceWriteFields,
};
use crate::fleet::{HolderId, ThrottlerKey};
use crate::installation::ProtocolError;
use crate::records::postgres::group_attempt::{
	GroupAttemptError, GroupAttemptRejectionError, PgRecordGroupAttempt, PgRecordGroupAttemptBudget,
};
use crate::records::postgres::{PgPointStorage, StateStorageError};
use crate::records::{InvalidGroupMutationError, RecordIdentity};
use crate::runtime::admission::PgNativeAdmission;
use crate::runtime::admission::owned_primitive::OwnedPrimitiveAdmissionError;
use crate::runtime::admission::owned_primitive::{
	OwnedPrimitiveCleanupPhase, OwnedPrimitiveOriginOrdering,
};
use crate::runtime::local_admission::RuntimeAdmissionError;
use crate::runtime::local_admission::throttler_work::{
	RuntimeThrottlerWorkIdentity, RuntimeThrottlerWorkIdentityFields,
};

mod control;
mod delivery;
mod driver;
mod options;
mod pending;
pub(crate) use control::NativeThrottlerPointError;
pub(crate) use delivery::{NativeThrottlerClient, NativeThrottlerStartResult};
pub(crate) use driver::{
	NativeThrottlerAccountingObservation, NativeThrottlerLocalObservation, NativeThrottlerObserver,
};
pub(crate) use options::NativeThrottlerProbeHeartbeatPolicyFields;
pub(crate) use options::{NativeThrottlerStartOptions, NativeThrottlerStartOptionsFields};
pub(crate) use options::{ThrottlerDriverTiming, ThrottlerDriverTimingFields};
pub(crate) use pending::NativeThrottlerOperation;

/// Explicit positive waits; owned COMMIT/ROLLBACK are never wrapped in a timeout. An
/// acquisition and the dispatch of its outcome end with the runtime's confirmed lifetime instead
/// of a wait of their own.
#[derive(Clone, Copy)]
pub(crate) struct ThrottlerExecutionTiming {
	physical_proof_attempt_timeout: Duration,
	cleanup_attempt_timeout: Duration,
	ending_wait_timeout: Duration,
}
/// Explicit wait budgets for registered Throttler execution and settlement.
pub(crate) struct ThrottlerExecutionTimingFields {
	/// The positive budget for one physical-ordering proof attempt.
	pub physical_proof_attempt_timeout: Duration,
	/// The positive budget for one exact undelivered cleanup attempt.
	pub cleanup_attempt_timeout: Duration,
	/// The positive budget for observing an owned ending and local settlement.
	pub ending_wait_timeout: Duration,
}
/// A zero wait duration supplied to Throttler execution or driver timing.
#[derive(Debug, thiserror::Error)]
#[error("Throttler timing waits must be positive")]
pub(crate) struct ThrottlerTimingError;

impl ThrottlerExecutionTiming {
	/// Checks that every supplied execution wait is positive.
	/// Actual operations separately check additions to their current local clock.
	pub fn new(fields: ThrottlerExecutionTimingFields) -> Result<Self, ThrottlerTimingError> {
		if fields.physical_proof_attempt_timeout.is_zero()
			|| fields.cleanup_attempt_timeout.is_zero()
			|| fields.ending_wait_timeout.is_zero()
		{
			return Err(ThrottlerTimingError);
		}
		Ok(Self {
			physical_proof_attempt_timeout: fields.physical_proof_attempt_timeout,
			cleanup_attempt_timeout: fields.cleanup_attempt_timeout,
			ending_wait_timeout: fields.ending_wait_timeout,
		})
	}
}

pub(crate) struct NativeThrottlerBindingsFields {
	pub(crate) code: Arc<RetainedThrottlerScopeCode>,
	pub(crate) admission: Arc<PgNativeAdmission>,
	pub(crate) execution_pool: WritePool,
	pub(crate) group_budget: PgRecordGroupAttemptBudget,
	pub(crate) timing: ThrottlerExecutionTiming,
	pub(crate) driver_timing: ThrottlerDriverTiming,
	pub(crate) originating_runtime: tokio::runtime::Handle,
}
pub(crate) struct NativeThrottlerBindings {
	code: Arc<RetainedThrottlerScopeCode>,
	admission: Arc<PgNativeAdmission>,
	storage: PgPointStorage,
	execution_pool: WritePool,
	group_budget: PgRecordGroupAttemptBudget,
	timing: ThrottlerExecutionTiming,
	driver_timing: ThrottlerDriverTiming,
	originating_runtime: tokio::runtime::Handle,
}
impl NativeThrottlerBindings {
	/// Configuration resolution supplies no current role, physical readiness or authority.
	pub(crate) fn new(fields: NativeThrottlerBindingsFields) -> Result<Self, NativeThrottlerError> {
		let storage = fields
			.admission
			.app_storage(fields.code.declaration().identity())
			.map_err(OwnedPrimitiveAdmissionError::from)?;
		Ok(Self {
			code: fields.code,
			admission: fields.admission,
			storage,
			execution_pool: fields.execution_pool,
			group_budget: fields.group_budget,
			timing: fields.timing,
			driver_timing: fields.driver_timing,
			originating_runtime: fields.originating_runtime,
		})
	}
}
#[derive(Clone)]
pub(crate) struct PgNativeThrottler {
	bindings: Arc<NativeThrottlerBindings>,
	identity: RuntimeThrottlerWorkIdentity,
	settings: ThrottlerSettings,
}
impl PgNativeThrottler {
	pub(crate) fn scope(&self) -> &crate::scope::ScopeIdentity {
		self.identity.scope()
	}
	pub(crate) fn key(&self) -> &ThrottlerKey {
		self.identity.resource()
	}
	pub(crate) fn settings(&self) -> &ThrottlerSettings {
		&self.settings
	}

	pub(crate) fn new(
		bindings: Arc<NativeThrottlerBindings>,
		key: ThrottlerKey,
		settings: ThrottlerSettings,
	) -> Self {
		let identity = RuntimeThrottlerWorkIdentity::new(RuntimeThrottlerWorkIdentityFields {
			scope: bindings.code.declaration().identity(),
			resource: &key,
		});
		Self {
			bindings,
			identity,
			settings,
		}
	}
	fn identities(
		&self,
		outcome_id: Option<&super::state::outcome::ThrottlerOutcomeId>,
	) -> BTreeSet<RecordIdentity> {
		let mut identities = BTreeSet::from([self.identity.record().clone()]);
		if let Some(outcome_id) = outcome_id {
			identities.insert(
				self.bindings
					.code
					.declaration()
					.outcome_record_identity(self.identity.resource(), outcome_id),
			);
		}
		identities
	}
}

/// Actual observed cancellation cause, without grant or cleanup permission.
#[derive(Clone, Copy, Debug)]
pub(crate) enum NativeThrottlerCancellationReason {
	ClientClosed,
	RuntimeStopped,
}

/// Acquisition diagnostics supply no permit, token, caller scope or extension authority.
#[derive(Debug)]
pub(crate) enum NativeThrottlerAcquisitionObservation {
	Acquired,
	Denied(NativeThrottlerDenial),
}
/// Why a settled acquisition did not acquire.
#[derive(Debug)]
pub(crate) enum NativeThrottlerDenial {
	Throttled { retry_after: Option<Duration> },
	CircuitOpen,
}

/// Exact recorded successful completion outputs, without grant or cleanup authority.
#[derive(Debug)]
pub(crate) enum NativeThrottlerCompletionObservation {
	DeniedAcquisition,
	ConsumedOutcome,
	UndeliveredCleanup,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeThrottlerError {
	#[error("native Throttler acquisition was cancelled")]
	AcquisitionCancelled {
		reason: NativeThrottlerCancellationReason,
	},
	#[error("native Throttler driver is closed")]
	DriverClosed,
	#[error("native Throttler delivery has no fresh local task permission")]
	DeliveryRejected,
	#[error("native Throttler probe heartbeat policy does not match its configured profile")]
	InvalidProbeHeartbeatPolicy,
	#[error("the rate-limit cost requires a rate limit with sufficient capacity")]
	InvalidRateLimitCost,
	#[error("native Throttler probe extension no longer matches confirmed local authority")]
	ProbeExtensionLost,
	#[error("native Throttler resource preparation failed")]
	Record {
		identity: RecordIdentity,
		#[source]
		source: Box<Self>,
	},
	#[error("native Throttler waits must be positive")]
	InvalidTiming,
	#[error("native Throttler operation is not in the required private phase")]
	OperationState,
	#[error("native Throttler protected work exceeded its configured wait")]
	ProtectedTimeout,
	#[error("native Throttler ending or local continuation remains pending")]
	EndingPending,
	#[error("native Throttler admission failed")]
	Admission(#[from] OwnedPrimitiveAdmissionError),
	#[error("native Throttler local ownership failed")]
	Local(#[from] RuntimeAdmissionError),
	#[error("native Throttler scoped preparation failed")]
	Preparation(#[source] Box<PrimitiveScopeBindingError>),
	#[error("native Throttler group acquisition was rejected")]
	GroupRejected(#[from] GroupAttemptRejectionError),
	#[error("native Throttler group work failed")]
	Group(#[source] Box<GroupAttemptError>),
	#[error("native Throttler prepared mutation was rejected")]
	Mutation(#[from] InvalidGroupMutationError),
	#[error("native Throttler preparation failed and group restoration failed")]
	PreparationCleanup {
		#[source]
		preparation_error: Box<PrimitiveScopeBindingError>,
		cleanup_error: Box<GroupAttemptError>,
	},
	#[error("native Throttler resource preparation failed and point restoration failed")]
	PointPreparationCleanup {
		#[source]
		preparation_error: Box<PrimitiveScopeBindingError>,
		cleanup_error: Box<StateStorageError>,
	},
	#[error("native Throttler record storage failed")]
	Storage(#[source] Box<StateStorageError>),
	#[error("native Throttler physical proof failed and local settlement also failed")]
	PhysicalSettlementFailed {
		#[source]
		operation_error: Box<Self>,
		settlement_error: Box<Self>,
	},
	#[error("native Throttler cause retention also failed")]
	DiagnosticRetention {
		#[source]
		operation_error: Arc<Self>,
		retention_error: RuntimeAdmissionError,
	},
	#[error("native Throttler database work failed")]
	Database(#[source] Box<DatabaseError>),
	#[error("native Throttler protocol failed")]
	Protocol(#[source] ProtocolError),
	/// The work failed, and the rollback of its transaction failed in the database. The source
	/// is the work's failure.
	#[error("native Throttler work failed and rollback also failed")]
	RollbackFailed {
		#[source]
		operation_error: Box<Self>,
		rollback_error: Box<DatabaseError>,
	},
}
impl From<StateStorageError> for NativeThrottlerError {
	fn from(error: StateStorageError) -> Self {
		Self::Storage(Box::new(error))
	}
}
impl From<PrimitiveScopeBindingError> for NativeThrottlerError {
	fn from(error: PrimitiveScopeBindingError) -> Self {
		Self::Preparation(Box::new(error))
	}
}
impl TransactionFailure for NativeThrottlerError {
	fn from_protocol_error(error: ProtocolError) -> Self {
		Self::Protocol(error)
	}
	fn from_database_error(error: DatabaseError) -> Self {
		Self::Database(Box::new(error))
	}
	fn from_rollback_failure(operation_error: Self, rollback_error: DatabaseError) -> Self {
		Self::RollbackFailed {
			operation_error: Box::new(operation_error),
			rollback_error: Box::new(rollback_error),
		}
	}
	fn involves_database_failure(&self) -> bool {
		match self {
			Self::Record { source, .. } => source.involves_database_failure(),
			Self::Storage(error)
			| Self::PointPreparationCleanup {
				cleanup_error: error,
				..
			} => error.database_error().is_some(),
			Self::Admission(error) => error.involves_database_failure(),
			Self::Group(error)
			| Self::PreparationCleanup {
				cleanup_error: error,
				..
			} => error.involves_database_failure(),
			Self::PhysicalSettlementFailed {
				operation_error,
				settlement_error,
			} => {
				operation_error.involves_database_failure()
					|| settlement_error.involves_database_failure()
			}
			Self::Database(_) | Self::RollbackFailed { .. } => true,
			Self::DiagnosticRetention {
				operation_error, ..
			} => operation_error.involves_database_failure(),
			_ => false,
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Record { source, .. } => source.commit_outcome_is_unknown(),
			Self::Storage(error)
			| Self::PointPreparationCleanup {
				cleanup_error: error,
				..
			} => error
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
			Self::Admission(error) => error.commit_outcome_is_unknown(),
			Self::Group(error)
			| Self::PreparationCleanup {
				cleanup_error: error,
				..
			} => error.commit_outcome_is_unknown(),
			Self::Database(error) => error.commit_outcome_is_unknown(),
			Self::PhysicalSettlementFailed {
				operation_error,
				settlement_error,
			} => {
				operation_error.commit_outcome_is_unknown()
					|| settlement_error.commit_outcome_is_unknown()
			}
			Self::DiagnosticRetention {
				operation_error, ..
			} => operation_error.commit_outcome_is_unknown(),
			// The rollback after a failed operation sends no `COMMIT`, so only the operation's
			// own failure can leave a commit outcome unknown.
			Self::RollbackFailed {
				operation_error, ..
			} => operation_error.commit_outcome_is_unknown(),
			_ => false,
		}
	}
}
fn deadline(timeout: Duration) -> Result<Instant, NativeThrottlerError> {
	Instant::now()
		.checked_add(timeout)
		.ok_or(NativeThrottlerError::InvalidTiming)
}
fn require_before_deadline(deadline: Instant) -> Result<(), NativeThrottlerError> {
	if Instant::now() >= deadline {
		Err(NativeThrottlerError::ProtectedTimeout)
	} else {
		Ok(())
	}
}

impl From<GroupAttemptError> for NativeThrottlerError {
	fn from(error: GroupAttemptError) -> Self {
		Self::Group(Box::new(error))
	}
}

#[derive(Debug)]
pub(crate) struct NativeThrottlerDiagnosticError {
	primary_error: Option<Arc<NativeThrottlerError>>,
	cleanup_error: Option<Arc<NativeThrottlerError>>,
	acquisition_ending: Option<Arc<crate::db::transaction_ending::TransactionEndingResult>>,
	outcome_ending: Option<Arc<crate::db::transaction_ending::TransactionEndingResult>>,
	heartbeat_ending: Option<Arc<crate::db::transaction_ending::TransactionEndingResult>>,
	heartbeat_error: Option<Arc<NativeThrottlerError>>,
	latest_ending: Option<Arc<crate::db::transaction_ending::TransactionEndingResult>>,
	unsettled_ending: Option<Arc<crate::db::transaction_ending::TransactionEndingResult>>,
}

impl std::fmt::Display for NativeThrottlerDiagnosticError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(
			"native Throttler operation retains operation or transaction-ending diagnostics",
		)
	}
}
impl std::error::Error for NativeThrottlerDiagnosticError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		self.primary_error
			.as_ref()
			.or(self.cleanup_error.as_ref())
			.map(|error| error.as_ref() as _)
			.or_else(|| {
				self.acquisition_ending
					.as_ref()
					.and_then(|ending| ending.error())
					.map(|error| error as _)
			})
			.or_else(|| {
				self.outcome_ending
					.as_ref()
					.and_then(|ending| ending.error())
					.map(|error| error as _)
			})
			.or_else(|| {
				self.heartbeat_error
					.as_ref()
					.map(|error| error.as_ref() as _)
			})
			.or_else(|| {
				self.heartbeat_ending
					.as_ref()
					.and_then(|ending| ending.error())
					.map(|error| error as _)
			})
			.or_else(|| {
				self.latest_ending
					.as_ref()
					.and_then(|ending| ending.error())
					.map(|error| error as _)
			})
			.or_else(|| {
				self.unsettled_ending
					.as_ref()
					.and_then(|ending| ending.error())
					.map(|error| error as _)
			})
	}
}

impl PgNativeThrottler {
	pub(crate) fn start_options(
		&self,
		fields: NativeThrottlerStartOptionsFields,
	) -> Result<NativeThrottlerStartOptions, NativeThrottlerError> {
		if match self.settings.rate() {
			Some(rate) => fields.rate_limit_cost.get() > rate.capacity,
			None => fields.rate_limit_cost != std::num::NonZeroU32::MIN,
		} {
			return Err(NativeThrottlerError::InvalidRateLimitCost);
		}
		let probe_heartbeat = match (self.settings.circuit(), fields.probe_heartbeat) {
			(
				Some(control),
				Some(NativeThrottlerProbeHeartbeatPolicyFields {
					interval,
					max_consecutive_failures,
				}),
			) if !interval.is_zero() && interval < control.probe_window() => {
				Some(options::NativeThrottlerProbeHeartbeatPolicy {
					interval,
					max_consecutive_failures,
				})
			}
			(None, None) => None,
			_ => return Err(NativeThrottlerError::InvalidProbeHeartbeatPolicy),
		};
		Ok(NativeThrottlerStartOptions {
			rate_limit_cost: fields.rate_limit_cost,
			holder: fields.holder,
			settings: self.settings,
			probe_heartbeat,
		})
	}
}
