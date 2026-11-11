//! Creation, observation, and cancellation of the upgrade work that the installation's control
//! state records, the recovery of these operations after a transaction with an unknown result,
//! and the decoding of the recorded slot.
//!
//! These operations preserve effective installed policy and generation. Recovered targets
//! still require actual code binding and controller proofs before any effective transition.

use super::{
	Arc, NativeUpgradeRequestError, PgInstallationAdmissionBinding, PgNativeUpgradeRequests,
};
use crate::db::transaction::run_registered_transaction_write;
use crate::runtime::admission::upgrade::NativeUpgradeEnrollment;
use crate::upgrade::progress::PgControlProgressStorage;
use crate::upgrade::work::{UpgradeSourceOrigin, UpgradeWork};

pub(crate) mod creation;
pub(in crate::upgrade) mod creation_execution;
pub(crate) mod observation;
pub(in crate::upgrade) mod recovery_observation;
pub(crate) mod slot;

pub(in crate::upgrade) struct NativeUpgradeWorkCancellationRequest<'a> {
	pub(crate) enrollment: NativeUpgradeEnrollment<'a>,
	pub(crate) expected: &'a UpgradeWork,
}

/// Known local request identity; no target code or guessed generation is required to observe it.
pub(in crate::upgrade) struct NativeUpgradeWorkReadRequest<'a> {
	pub(crate) enrollment: NativeUpgradeEnrollment<'a>,
}

/// Stored work returned only after its owned transaction has a known committed ending.
#[derive(Debug)]
pub(in crate::upgrade) struct CommittedUpgradeWork {
	work: Arc<UpgradeWork>,
}
impl CommittedUpgradeWork {
	pub(in crate::upgrade) fn into_work(self) -> Arc<UpgradeWork> {
		self.work
	}
}

impl PgNativeUpgradeRequests {
	fn work_origin(&self) -> UpgradeSourceOrigin {
		match self.config.installation_binding.as_ref() {
			PgInstallationAdmissionBinding::Fresh { .. } => UpgradeSourceOrigin::Fresh,
			PgInstallationAdmissionBinding::Imported { .. } => UpgradeSourceOrigin::Imported,
		}
	}

	fn progress_storage(&self) -> Result<PgControlProgressStorage<'_>, NativeUpgradeRequestError> {
		Ok(PgControlProgressStorage::new(
			&self.config.installation_binding,
			&self.config.control_storage,
			&self.config.administration,
		)?)
	}

	/// Cancels exact recognized work without executable support for its pending target.
	pub(in crate::upgrade) async fn cancel_pending_work(
		&self,
		mut request: NativeUpgradeWorkCancellationRequest<'_>,
	) -> Result<CommittedUpgradeWork, NativeUpgradeRequestError> {
		self.require_enrollment_bindings(&request.enrollment)?;
		let progress = self.progress_storage()?;
		let registration = request.enrollment.prepare_transaction()?;
		let work = run_registered_transaction_write(
			&self.config.control_pool,
			"db.native_upgrade_requests.cancel_pending_work",
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
					lock.cancel_work(&request.enrollment, request.expected)
						.await?,
				)
			},
		)
		.await?;
		Ok(CommittedUpgradeWork {
			work: Arc::new(work),
		})
	}

	fn require_enrollment_bindings(
		&self,
		enrollment: &NativeUpgradeEnrollment<'_>,
	) -> Result<(), NativeUpgradeRequestError> {
		enrollment.require_bindings(
			&self.config.installation_binding,
			&self.config.control_storage,
			&self.config.administration,
		)?;
		Ok(())
	}
}
