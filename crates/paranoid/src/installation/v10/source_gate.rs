//! Physical exclusion of source-format writers on the configured installation protocol.
//!
//! Imported operation ordering is source gate, native control, then protected records.
//! The fixed owned retirement executor validates the imported binding and persists source
//! retirement under exclusive native control.
//! Source-gate retirement and that control write commit in one owned transaction.

use std::fmt;
use std::time::Duration;

use primitives_rs::time::Deadline;

use crate::db::{
	AdmissionError, DatabaseError, SchemaError, StateMutationCompletion, TransactionFailure, Tx,
	WriteTx,
};
use crate::installation::protocol::{
	LEGACY_PROTOCOL, ProtocolDef, ProtocolLock, ProtocolUpgradePlan, ProtocolUpgradePolicy,
};
use crate::installation::protocol_schema::record_activation;
use crate::installation::{Protocol, ProtocolError};

/// This exact definition excludes published v10 source mutators after retirement.
/// It describes source exclusion, not the selected native document representation.
const RETIRED_SOURCE_PROTOCOL: ProtocolDef =
	ProtocolDef::new(3, "paranoid.protocol.native-source-retired.v1");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeSourceGatePhase {
	PublishedV10,
	Retired,
}

/// One actual source relation and its exact admitted physical definitions.
#[derive(Clone)]
pub(crate) struct PgNativeSourceGate {
	protocol: Protocol,
	policy: ProtocolUpgradePolicy,
}

/// Proof that the current write transaction holds [`ProtocolLock::Exclusive`] on the v10 source
/// protocol relation before acquiring exclusive native control.
pub(crate) struct PinnedV10SourceGate<'a> {
	gate: &'a PgNativeSourceGate,
	active: ProtocolDef,
	phase: NativeSourceGatePhase,
	completion: StateMutationCompletion,
}

impl PinnedV10SourceGate<'_> {
	pub(crate) fn phase(&self) -> NativeSourceGatePhase {
		self.phase
	}

	/// Activates the retired source protocol when the active source protocol has not yet retired,
	/// then marks the source-gate mutation tracker complete.
	pub(crate) async fn activate_retirement_if_needed<E>(
		mut self,
		tx: &mut WriteTx<'_>,
	) -> Result<(), NativeSourceRetirementError<E>> {
		if self
			.gate
			.policy
			.plan(&self.active)
			.map_err(NativeSourceRetirementError::Protocol)?
			== ProtocolUpgradePlan::Upgrade
		{
			record_activation(tx, &self.gate.protocol, &self.gate.policy)
				.await
				.map_err(NativeSourceRetirementError::Schema)?;
		}
		self.completion.complete();
		Ok(())
	}

	/// Marks the source-gate tracker complete after verifying data retirement under an already
	/// retired source protocol.
	pub(crate) fn complete_without_activation(mut self) {
		self.completion.complete();
	}
}

impl PgNativeSourceGate {
	pub(crate) fn new(protocol: Protocol) -> Self {
		Self {
			protocol,
			policy: ProtocolUpgradePolicy::new(RETIRED_SOURCE_PROTOCOL, vec![LEGACY_PROTOCOL])
				.expect("source retirement declares distinct positive predecessor epochs"),
		}
	}

	pub(crate) fn protocol(&self) -> &Protocol {
		&self.protocol
	}

	/// Pins source-aware reads or mutations before any native control lock.
	/// The returned phase is an observation; it neither selects a decoder nor grants authority.
	pub(crate) async fn pin_shared(
		&self,
		tx: &mut Tx<'_>,
	) -> Result<NativeSourceGatePhase, SchemaError> {
		let mut completion = tx.track_state_mutation();
		let active = self
			.protocol
			.read_def(tx, ProtocolLock::Shared, "db.native_source_gate.pin_shared")
			.await
			.map_err(schema_admission_error)?;
		let phase = self.phase(&active)?;
		completion.complete();
		Ok(phase)
	}

	/// Acquires [`ProtocolLock::Exclusive`] on the source protocol relation within `wait` and
	/// returns a proof token required by native source-writer and source-data retirement.
	pub(crate) async fn pin_exclusive_for_retirement<'a, E>(
		&'a self,
		tx: &mut WriteTx<'_>,
		wait: Duration,
	) -> Result<PinnedV10SourceGate<'a>, NativeSourceRetirementError<E>> {
		let completion = tx.track_state_mutation();
		let active = self.lock_exclusive_retirement_def(tx, wait).await?;
		let phase = self
			.phase(&active)
			.map_err(NativeSourceRetirementError::Schema)?;
		Ok(PinnedV10SourceGate {
			gate: self,
			active,
			phase,
			completion,
		})
	}

	/// Fixed retirement runners retain this relation pin through their owned transaction ending.
	async fn lock_exclusive_retirement_def<E>(
		&self,
		tx: &mut Tx<'_>,
		wait: Duration,
	) -> Result<ProtocolDef, NativeSourceRetirementError<E>> {
		if wait.is_zero() {
			return Err(NativeSourceRetirementError::InvalidSourceLockWaitTimeout);
		}
		let until = Deadline::new_from_base(tokio::time::Instant::now().into_std(), wait)
			.map_err(|_| NativeSourceRetirementError::InvalidSourceLockWaitTimeout)?;
		let interruption = tx.statement_interruption_marker();
		tokio::time::timeout_at(
			tokio::time::Instant::from_std(until.instant()),
			self.protocol.read_def(
				tx,
				ProtocolLock::Exclusive,
				"db.native_source_gate.lock_retirement",
			),
		)
		.await
		.map_err(|_| {
			interruption.mark_interrupted();
			NativeSourceRetirementError::SourceLockWaitElapsed
		})?
		.map_err(|error| NativeSourceRetirementError::Schema(schema_admission_error(error)))
	}

	fn phase(&self, active: &ProtocolDef) -> Result<NativeSourceGatePhase, SchemaError> {
		match self.policy.plan(active)? {
			ProtocolUpgradePlan::AlreadyCurrent => Ok(NativeSourceGatePhase::Retired),
			ProtocolUpgradePlan::Upgrade => Ok(NativeSourceGatePhase::PublishedV10),
		}
	}
}

fn schema_admission_error(error: AdmissionError) -> SchemaError {
	match error {
		AdmissionError::Protocol(error) => SchemaError::Protocol(error),
		AdmissionError::Database(error) => SchemaError::Database(error),
	}
}

/// A timeout during source lock/read cannot have written retirement state.
/// Cleanup and uncertain commit retain their exact transaction failure causes.
#[derive(Debug)]
pub(crate) enum NativeSourceRetirementError<E> {
	InvalidSourceLockWaitTimeout,
	SourceLockWaitElapsed,
	Schema(SchemaError),
	Controller(E),
	Database(DatabaseError),
	Protocol(ProtocolError),
	RollbackFailed {
		operation_error: Box<Self>,
		rollback_error: DatabaseError,
	},
}

impl<E> fmt::Display for NativeSourceRetirementError<E> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(match self {
			Self::InvalidSourceLockWaitTimeout => {
				"source retirement lock wait must be positive and fit the local clock"
			}
			Self::SourceLockWaitElapsed => {
				"source retirement lock/read wait elapsed before any retirement write"
			}
			Self::Schema(_) => "source retirement physical protocol failed",
			Self::Controller(_) => "source retirement native controller operation failed",
			Self::Database(_) => "source retirement transaction database work failed",
			Self::Protocol(_) => "source retirement transaction protocol failed",
			Self::RollbackFailed { .. } => "source retirement failed and rollback also failed",
		})
	}
}

impl<E: std::error::Error + 'static> std::error::Error for NativeSourceRetirementError<E> {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Schema(error) => Some(error),
			Self::Controller(error) => Some(error),
			Self::Database(error) => Some(error),
			Self::Protocol(error) => Some(error),
			Self::RollbackFailed {
				operation_error, ..
			} => Some(operation_error.as_ref()),
			Self::InvalidSourceLockWaitTimeout | Self::SourceLockWaitElapsed => None,
		}
	}
}

impl<E: TransactionFailure> TransactionFailure for NativeSourceRetirementError<E> {
	fn from_protocol_error(error: ProtocolError) -> Self {
		Self::Protocol(error)
	}
	fn from_database_error(error: DatabaseError) -> Self {
		Self::Database(error)
	}
	fn from_rollback_failure(operation_error: Self, rollback_error: DatabaseError) -> Self {
		Self::RollbackFailed {
			operation_error: Box::new(operation_error),
			rollback_error,
		}
	}
	fn involves_database_failure(&self) -> bool {
		match self {
			Self::Schema(error) => error.involves_database_failure(),
			Self::Controller(error) => error.involves_database_failure(),
			Self::Database(_) | Self::RollbackFailed { .. } => true,
			Self::InvalidSourceLockWaitTimeout
			| Self::SourceLockWaitElapsed
			| Self::Protocol(_) => false,
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Schema(error) => error.commit_outcome_is_unknown(),
			Self::Controller(error) => error.commit_outcome_is_unknown(),
			Self::Database(error) => error.commit_outcome_is_unknown(),
			Self::RollbackFailed {
				operation_error, ..
			} => operation_error.commit_outcome_is_unknown(),
			Self::InvalidSourceLockWaitTimeout
			| Self::SourceLockWaitElapsed
			| Self::Protocol(_) => false,
		}
	}
}
