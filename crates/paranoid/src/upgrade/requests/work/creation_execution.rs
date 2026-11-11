//! Fixed creation retains exact attempts and distinguishes write endings from recovery reads.

use std::sync::Arc;

use crate::db::transaction::{run_registered_transaction_read, run_registered_transaction_write};
use crate::db::transaction_ending::{
	TransactionEndingDisposition, TransactionEndingReceipt, TransactionEndingSnapshot,
};
use crate::installation::control_state::InstallationControlState;
use crate::runtime::admission::upgrade::NativeUpgradeEnrollment;
use crate::runtime::targets::NativeRuntimeTargetError;
use crate::upgrade::observation::{
	NativeUpgradeObservedControl, NativeUpgradeObservedControlFields,
};
use crate::upgrade::work::CompleteUpgradeTarget;
use crate::upgrade::work::UpgradeWork;

use super::creation::{
	RetainedPreparedAttempt, UpgradeCreationError, UpgradeCreationIntent,
	UpgradeCreationPreparation, UpgradeCreationRecovery,
};
use super::{NativeUpgradeRequestError, PgNativeUpgradeRequests};

/// The library owns this state; diagnostic reads do not replace its uncertain write baseline.
pub(in crate::upgrade) struct NativeUpgradeCreateState {
	intent: Arc<UpgradeCreationIntent>,
	state: CreationState,
}

enum CreationState {
	Fresh,
	Attempt {
		ending: TransactionEndingReceipt,
		condition: CreationCondition,
	},
}

enum CreationCondition {
	Unprepared,
	Prepared(PreparedWrite),
	Existing {
		target: CompleteUpgradeTarget,
		prior_write: Option<PreparedWrite>,
	},
}

struct CreationWrite {
	attempt: Box<RetainedPreparedAttempt>,
	ending: TransactionEndingReceipt,
}

enum PreparedWrite {
	RecoveryRequired(CreationWrite),
	SameConditionObserved(CreationWrite),
}

impl PreparedWrite {
	fn write(&self) -> &CreationWrite {
		match self {
			Self::RecoveryRequired(write) | Self::SameConditionObserved(write) => write,
		}
	}
	fn into_write(self) -> CreationWrite {
		match self {
			Self::RecoveryRequired(write) | Self::SameConditionObserved(write) => write,
		}
	}
}

impl CreationCondition {
	fn prepared_write(&self) -> Option<&PreparedWrite> {
		match self {
			Self::Unprepared => None,
			Self::Prepared(write) => Some(write),
			Self::Existing { prior_write, .. } => prior_write.as_ref(),
		}
	}
	fn map_prepared_write(&mut self, map_fn: impl FnOnce(PreparedWrite) -> Option<PreparedWrite>) {
		let prior = std::mem::replace(self, Self::Unprepared);
		*self = match prior {
			Self::Unprepared => Self::Unprepared,
			Self::Prepared(write) => match map_fn(write) {
				Some(write) => Self::Prepared(write),
				None => Self::Unprepared,
			},
			Self::Existing {
				target,
				prior_write,
			} => Self::Existing {
				target,
				prior_write: prior_write.and_then(map_fn),
			},
		};
	}
}

pub(in crate::upgrade) enum NativeCreatePrepared {
	Existing(Arc<UpgradeWork>),
	Write(Arc<UpgradeWork>),
}

/// Exact actual creation/recovery state; the registered owner publishes it after known ending.
pub(in crate::upgrade) struct NativeUpgradeCreationObservation {
	pub(crate) observed: NativeUpgradeObservedControl,
	pub(crate) work: Arc<UpgradeWork>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeCreateExecutionError {
	#[error("fresh creation belongs to different runtime code or enrollment")]
	BindingMismatch,
	#[error("fresh creation ending remains open or pending")]
	EndingPending,
	#[error("fresh creation requires an enrolled recovery observation")]
	RecoveryRequired,
	#[error("fresh creation has no prepared write to recover")]
	NoPreparedAttempt,
	#[error("fresh creation has no registered transaction ending")]
	NoRegisteredEnding,
	#[error("known committed upgrade creation has no matching recorded work")]
	CommittedWorkMissing,
	#[error("fresh creation prewrite or recovery condition failed")]
	Preparation(#[from] UpgradeCreationError),
	#[error("fresh creation target does not bind its code")]
	Code(#[from] NativeRuntimeTargetError),
}

impl NativeUpgradeCreateState {
	pub(crate) fn new(intent: Arc<UpgradeCreationIntent>) -> Self {
		Self {
			intent,
			state: CreationState::Fresh,
		}
	}
	pub(crate) fn intent(&self) -> &Arc<UpgradeCreationIntent> {
		&self.intent
	}
	fn condition(&self) -> Option<&CreationCondition> {
		match &self.state {
			CreationState::Fresh => None,
			CreationState::Attempt { condition, .. } => Some(condition),
		}
	}
	fn condition_mut(&mut self) -> Option<&mut CreationCondition> {
		match &mut self.state {
			CreationState::Fresh => None,
			CreationState::Attempt { condition, .. } => Some(condition),
		}
	}
	fn last_create_ending(&self) -> Option<&TransactionEndingReceipt> {
		match &self.state {
			CreationState::Fresh => None,
			CreationState::Attempt { ending, .. } => Some(ending),
		}
	}
	pub(crate) fn prepared_attempt(&self) -> Option<&RetainedPreparedAttempt> {
		self.condition()?
			.prepared_write()
			.map(|write| write.write().attempt.as_ref())
	}
	fn prepared_write_ending(&self) -> Option<&TransactionEndingReceipt> {
		self.condition()?
			.prepared_write()
			.map(|write| &write.write().ending)
	}
	fn retain_existing(
		&mut self,
		work: Box<UpgradeWork>,
	) -> Result<Arc<UpgradeWork>, NativeCreateExecutionError> {
		if let Some(CreationCondition::Existing { target, .. }) = self.condition()
			&& !target.matches_target(work.target())
		{
			return Err(UpgradeCreationError::IdentityConflict.into());
		}
		let condition = self
			.condition_mut()
			.ok_or(NativeCreateExecutionError::NoRegisteredEnding)?;
		let prior_write = match std::mem::replace(condition, CreationCondition::Unprepared) {
			CreationCondition::Unprepared => None,
			CreationCondition::Prepared(write) => Some(write),
			CreationCondition::Existing { prior_write, .. } => prior_write,
		};
		*condition = CreationCondition::Existing {
			target: work.target().clone(),
			prior_write,
		};
		Ok(Arc::from(work))
	}
	fn prepare_next_create(&mut self) -> Result<(), NativeCreateExecutionError> {
		if let Some(receipt) = self.last_create_ending()
			&& !matches!(receipt.snapshot(), TransactionEndingSnapshot::Terminal(_))
		{
			return Err(NativeCreateExecutionError::EndingPending);
		}
		let Some(write) = self.condition().and_then(CreationCondition::prepared_write) else {
			return Ok(());
		};
		let TransactionEndingSnapshot::Terminal(outcome) = write.write().ending.snapshot() else {
			return Err(NativeCreateExecutionError::EndingPending);
		};
		if outcome.disposition() == TransactionEndingDisposition::KnownRollback {
			self.condition_mut()
				.expect("prepared write has its condition")
				.map_prepared_write(|_| None);
		} else if matches!(write, PreparedWrite::RecoveryRequired(_)) {
			return Err(NativeCreateExecutionError::RecoveryRequired);
		}
		Ok(())
	}
	fn begin_create(&mut self, ending: TransactionEndingReceipt) {
		let mut condition = match std::mem::replace(&mut self.state, CreationState::Fresh) {
			CreationState::Fresh => CreationCondition::Unprepared,
			CreationState::Attempt { condition, .. } => condition,
		};
		condition
			.map_prepared_write(|write| Some(PreparedWrite::RecoveryRequired(write.into_write())));
		self.state = CreationState::Attempt { ending, condition };
	}
	fn record_same_condition(&mut self) {
		self.condition_mut()
			.expect("recovery retains its creation condition")
			.map_prepared_write(|write| {
				Some(PreparedWrite::SameConditionObserved(write.into_write()))
			});
	}

	/// Called only by the fixed canonical guard, before participant IO or a write await.
	pub(in crate::upgrade) fn prepare_current(
		&mut self,
		current: &InstallationControlState,
	) -> Result<NativeCreatePrepared, NativeCreateExecutionError> {
		let prepared = match self.prepared_attempt() {
			Some(attempt) => match attempt.compare_recovery(current)? {
				UpgradeCreationRecovery::Existing(work) => {
					return self
						.retain_existing(work)
						.map(NativeCreatePrepared::Existing);
				}
				UpgradeCreationRecovery::RetrySameCondition => None,
			},
			None => match self.intent.prepare(current)? {
				UpgradeCreationPreparation::Existing(work) => {
					return self
						.retain_existing(work)
						.map(NativeCreatePrepared::Existing);
				}
				UpgradeCreationPreparation::Prepared(attempt) => Some(attempt),
			},
		};
		if matches!(self.condition(), Some(CreationCondition::Existing { .. })) {
			return Err(UpgradeCreationError::HistoryAmbiguity {
				change: super::creation::UpgradeCreationHistoryChange::PendingSlot,
			}
			.into());
		}
		let attempt = prepared
			.as_deref()
			.or(self.prepared_attempt())
			.ok_or(NativeCreateExecutionError::NoPreparedAttempt)?;
		self.intent
			.code()
			.resolve(Arc::new(attempt.work().target().clone()))?;
		let ending = self
			.last_create_ending()
			.ok_or(NativeCreateExecutionError::NoRegisteredEnding)?
			.clone();
		let condition = self
			.condition_mut()
			.expect("registered ending has its condition");
		let attempt = match prepared {
			Some(attempt) => attempt,
			None => match std::mem::replace(condition, CreationCondition::Unprepared) {
				CreationCondition::Prepared(write) => write.into_write().attempt,
				CreationCondition::Unprepared | CreationCondition::Existing { .. } => {
					unreachable!("retained prepared attempt was checked")
				}
			},
		};
		let work = Arc::clone(attempt.shared_work());
		*condition = CreationCondition::Prepared(PreparedWrite::RecoveryRequired(CreationWrite {
			attempt,
			ending,
		}));
		Ok(NativeCreatePrepared::Write(work))
	}
}

pub(in crate::upgrade) struct NativeUpgradeWorkCreationRequest<'a> {
	pub(crate) enrollment: NativeUpgradeEnrollment<'a>,
	pub(crate) state: &'a mut NativeUpgradeCreateState,
}

pub(in crate::upgrade) enum NativeUpgradeCreateRecoveryObservation {
	Existing(Box<NativeUpgradeCreationObservation>),
	RetrySameCondition,
}

impl PgNativeUpgradeRequests {
	fn require_creation_bindings(
		&self,
		request: &NativeUpgradeWorkCreationRequest<'_>,
	) -> Result<(), NativeUpgradeRequestError> {
		self.require_enrollment_bindings(&request.enrollment)?;
		let intent = request.state.intent();
		if intent.identity() != request.enrollment.identity()
			|| !Arc::ptr_eq(intent.code().administration(), &self.config.administration)
			|| !Arc::ptr_eq(
				intent.code().installation(),
				&self.config.installation_binding,
			) {
			return Err(NativeCreateExecutionError::BindingMismatch.into());
		}
		Ok(())
	}

	/// Retains the same actual control/backend and existing Work Arc without a status reread.
	pub(in crate::upgrade) async fn request_work_observation(
		&self,
		mut request: NativeUpgradeWorkCreationRequest<'_>,
	) -> Result<NativeUpgradeCreationObservation, NativeUpgradeRequestError> {
		self.require_creation_bindings(&request)?;
		request.state.prepare_next_create()?;
		let progress = self.progress_storage()?;
		let registration = request.enrollment.prepare_transaction()?;
		request.state.begin_create(registration.receipt());
		run_registered_transaction_write(
			&self.config.control_pool,
			"db.native_upgrade_requests.request_work",
			registration,
			async |tx| {
				request.enrollment.require_transaction(tx)?;
				let lock = progress
					.lock(
						tx,
						self.config.conflict_attempts,
						self.config.control_pin_wait_timeout,
					)
					.await?;
				Ok::<_, NativeUpgradeRequestError>(
					lock.create_enrolled_work(&request.enrollment, request.state)
						.await?,
				)
			},
		)
		.await
	}

	pub(in crate::upgrade) async fn recover_creation_observation(
		&self,
		mut request: NativeUpgradeWorkCreationRequest<'_>,
	) -> Result<NativeUpgradeCreateRecoveryObservation, NativeUpgradeRequestError> {
		self.require_creation_bindings(&request)?;
		let attempt = request
			.state
			.prepared_attempt()
			.ok_or(NativeCreateExecutionError::NoPreparedAttempt)?;
		let receipt = request
			.state
			.last_create_ending()
			.ok_or(NativeCreateExecutionError::NoRegisteredEnding)?;
		if !matches!(receipt.snapshot(), TransactionEndingSnapshot::Terminal(_)) {
			return Err(NativeCreateExecutionError::EndingPending.into());
		}
		let registration = request.enrollment.prepare_transaction()?;
		let (recovered, observed) = run_registered_transaction_read(
			&self.config.control_pool,
			"db.native_upgrade_requests.recover_creation",
			registration,
			async |tx| {
				request.enrollment.require_transaction(tx)?;
				let observed = self.read_control_observation(tx).await?;
				let current = observed.decoded.state();
				let recovered = attempt
					.compare_recovery(current)
					.map_err(NativeCreateExecutionError::from)?;
				request.enrollment.authenticate_current(tx, current).await?;
				let observed =
					NativeUpgradeObservedControl::new(NativeUpgradeObservedControlFields {
						control: observed.decoded.into_state(),
						route: observed.route,
						backend: observed.backend,
					});
				Ok::<_, NativeUpgradeRequestError>((recovered, observed))
			},
		)
		.await?;
		match recovered {
			UpgradeCreationRecovery::Existing(work) => {
				Ok(NativeUpgradeCreateRecoveryObservation::Existing(Box::new(
					NativeUpgradeCreationObservation {
						work: request.state.retain_existing(work)?,
						observed,
					},
				)))
			}
			UpgradeCreationRecovery::RetrySameCondition => {
				if let Some(ending) = request.state.prepared_write_ending()
					&& let TransactionEndingSnapshot::Terminal(outcome) = ending.snapshot()
					&& outcome.disposition() == TransactionEndingDisposition::KnownCommit
				{
					return Err(NativeCreateExecutionError::CommittedWorkMissing.into());
				}
				request.state.record_same_condition();
				Ok(NativeUpgradeCreateRecoveryObservation::RetrySameCondition)
			}
		}
	}
}
