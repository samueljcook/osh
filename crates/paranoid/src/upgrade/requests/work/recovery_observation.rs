//! Exact uncertain-mutation ordering and enrolled observation through a write-capable owner.
//!
//! A complete physical fetch orders the original attempt without attributing its outcome.
//! Neither retained ordering nor parsed state authorizes a later mutation or proves drainage.

use crate::upgrade::wait::{
	deadline_after_positive_wait, require_before_deadline, require_valid_wait, tokio_instant,
};
use std::{fmt, sync::Arc, time::Duration};

use crate::db::transaction::run_registered_transaction_write;
use crate::db::transaction_ending::{
	TransactionEndingDisposition, TransactionEndingReceipt, TransactionEndingResult,
	TransactionEndingSnapshot,
};
use crate::installation::control_storage::ExclusiveControlOrderingProof;
use crate::runtime::admission::upgrade::{NativeUpgradeAdmissionError, NativeUpgradeEnrollment};
use crate::runtime::local_admission::upgrade_work::RuntimeUpgradeWorkIdentity;

use super::observation::NativeUpgradeControllerObservation;
use super::{NativeUpgradeRequestError, PgNativeUpgradeRequests};

pub(crate) struct NativeUpgradeRecoveryOriginFields {
	pub(crate) identity: RuntimeUpgradeWorkIdentity,
	pub(crate) receipt: TransactionEndingReceipt,
}

/// The original uncertain operation remains distinct from every subsequent recovery ending.
#[derive(Clone)]
pub(crate) struct NativeUpgradeRecoveryOrigin {
	identity: RuntimeUpgradeWorkIdentity,
	ending: Arc<TransactionEndingResult>,
}

impl NativeUpgradeRecoveryOrigin {
	pub(crate) fn new(
		fields: NativeUpgradeRecoveryOriginFields,
	) -> Result<Self, NativeUpgradeRequestError> {
		let ending = match fields.receipt.snapshot() {
			TransactionEndingSnapshot::Terminal(ending)
				if matches!(
					ending.disposition(),
					TransactionEndingDisposition::Unknown
						| TransactionEndingDisposition::UnconfirmedDisposal
						| TransactionEndingDisposition::TaskAbandoned
				) =>
			{
				ending
			}
			TransactionEndingSnapshot::Terminal(_) => {
				return Err(NativeUpgradeRequestError::RecoveryOriginNotUncertain);
			}
			_ => return Err(NativeUpgradeAdmissionError::PreviousEndingPending.into()),
		};
		Ok(Self {
			identity: fields.identity,
			ending,
		})
	}
	pub(crate) fn ending(&self) -> &TransactionEndingResult {
		&self.ending
	}
}

impl fmt::Debug for NativeUpgradeRecoveryOrigin {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("NativeUpgradeRecoveryOrigin")
			.finish_non_exhaustive()
	}
}

/// Physical ordering owns its complete exclusive-fetch proof even when record parsing fails.
pub(in crate::upgrade) enum NativeUpgradeRecoveryState {
	Unordered(NativeUpgradeRecoveryOrigin),
	Ordered(NativeUpgradeRecoveryOrdering),
}

pub(in crate::upgrade) struct NativeUpgradeRecoveryOrdering {
	origin: NativeUpgradeRecoveryOrigin,
	_proof: ExclusiveControlOrderingProof,
}

impl NativeUpgradeRecoveryState {
	pub(crate) fn origin(&self) -> &NativeUpgradeRecoveryOrigin {
		match self {
			Self::Unordered(origin) => origin,
			Self::Ordered(ordering) => &ordering.origin,
		}
	}
	pub(in crate::upgrade) fn is_ordered(&self) -> bool {
		matches!(self, Self::Ordered(_))
	}
	fn record_ordered(&mut self, proof: ExclusiveControlOrderingProof) {
		*self = Self::Ordered(NativeUpgradeRecoveryOrdering {
			origin: self.origin().clone(),
			_proof: proof,
		});
	}
}

pub(in crate::upgrade) struct NativeUpgradeRecoveryReadRequest<'a> {
	pub(crate) enrollment: NativeUpgradeEnrollment<'a>,
	pub(crate) recovery: &'a mut NativeUpgradeRecoveryState,
	/// Bounds protected observation, while actual owned COMMIT/ROLLBACK retains its lifetime.
	pub(crate) recovery_wait_timeout: Duration,
}

impl PgNativeUpgradeRequests {
	/// Takes an exclusive physical observation without changing data or metadata.
	/// Successful parsed output requires known COMMIT; physical ordering survives later errors.
	pub(in crate::upgrade) async fn read_pending_work_recovery_observation(
		&self,
		mut request: NativeUpgradeRecoveryReadRequest<'_>,
	) -> Result<NativeUpgradeControllerObservation, NativeUpgradeRequestError> {
		self.require_enrollment_bindings(&request.enrollment)?;
		if request.recovery.origin().identity != request.enrollment.identity() {
			return Err(NativeUpgradeRequestError::WrongRequest);
		}
		require_valid_wait(
			request.recovery_wait_timeout,
			NativeUpgradeRequestError::InvalidRecoveryWaitTimeout,
		)?;
		let registration = request.enrollment.prepare_transaction()?;
		run_registered_transaction_write(
			&self.config.control_pool,
			"db.native_upgrade_requests.read_pending_work_recovery_observation",
			registration,
			async |tx| {
				request.enrollment.require_transaction(tx)?;
				let deadline = deadline_after_positive_wait(
					request.recovery_wait_timeout,
					NativeUpgradeRequestError::InvalidRecoveryWaitTimeout,
				)?;
				let interruption = tx.statement_interruption_marker();
				let observed = tokio::time::timeout_at(tokio_instant(deadline), async {
					let pin_deadline = self.deadline()?;
					let pin_interruption = tx.statement_interruption_marker();
					let (phase, physical) =
						tokio::time::timeout_at(tokio_instant(pin_deadline), async {
							let phase = self.config.installation_binding.pin_source(tx).await?;
							let physical = self
								.config
								.control_storage
								.read_pinned_exclusive_physical(tx)
								.await?;
							Ok::<_, NativeUpgradeRequestError>((phase, physical))
						})
						.await
						.map_err(|_| {
							pin_interruption.mark_interrupted();
							NativeUpgradeRequestError::PinWaitElapsed
						})??;
					let (proof, record) = physical.into_parts();
					request.recovery.record_ordered(proof);
					let observed = self.verify_control_observation(&record?, phase)?;
					self.authenticate_observed_work(tx, &request.enrollment, observed)
						.await
				})
				.await
				.map_err(|_| {
					interruption.mark_interrupted();
					NativeUpgradeRequestError::RecoveryWaitElapsed
				})??;
				require_before_deadline(deadline, NativeUpgradeRequestError::RecoveryWaitElapsed)?;
				Ok(observed)
			},
		)
		.await
	}
}
