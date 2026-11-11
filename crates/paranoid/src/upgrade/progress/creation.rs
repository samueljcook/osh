//! Fixed creation consumes actual canonical control while the attempt remains owner-retained.

use crate::runtime::admission::upgrade::NativeUpgradeEnrollment;
use crate::upgrade::requests::work::creation_execution::{
	NativeCreatePrepared, NativeUpgradeCreateState, NativeUpgradeCreationObservation,
};

use super::{ControlProgressError, PgControlProgressLock, cleanup_rejection};

impl PgControlProgressLock<'_, '_> {
	/// Retains the exact prewrite condition before authentication or mutation can await.
	/// A matching current request is observed without replacing it, including terminal work.
	pub(in crate::upgrade) async fn create_enrolled_work(
		mut self,
		enrollment: &NativeUpgradeEnrollment<'_>,
		creation: &mut NativeUpgradeCreateState,
	) -> Result<NativeUpgradeCreationObservation, ControlProgressError> {
		let prepared = (|| {
			let intent = creation.intent();
			if intent.origin() != self.origin {
				return Err(ControlProgressError::CreateOriginMismatch);
			}
			let intended = intent.identity();
			let enrolled = enrollment.identity();
			if intended.installation_id != enrolled.installation_id
				|| intended.request_id != enrolled.request_id
			{
				return Err(ControlProgressError::EnrolledWorkMismatch);
			}
			let declaration = self.progress_storage.administration.declaration();
			let candidate = intent.candidate();
			candidate.require_control_target(
				&self.control,
				&declaration,
				self.progress_storage.administration.participant_schema(),
				self.progress_storage.administration.control_component(),
			)?;
			if let Some(scopes) = candidate.scope_replacements() {
				self.progress_storage
					.installation
					.check_scope_policies(scopes)?;
			}
			creation
				.prepare_current(&self.control)
				.map_err(ControlProgressError::from)
		})();
		let prepared = match prepared {
			Ok(prepared) => prepared,
			Err(error) => return Err(cleanup_rejection(self.physical, error).await),
		};
		let work = match &prepared {
			NativeCreatePrepared::Existing(work) | NativeCreatePrepared::Write(work) => work,
		};
		if let Err(error) = self.authenticate_enrolled_work(enrollment, work).await {
			return Err(cleanup_rejection(self.physical, error).await);
		}
		match prepared {
			NativeCreatePrepared::Existing(work) => {
				let observed = self.keep_observed().await?;
				Ok(NativeUpgradeCreationObservation { observed, work })
			}
			NativeCreatePrepared::Write(work) => {
				let observed = self.persist_work(&work).await?;
				Ok(NativeUpgradeCreationObservation { observed, work })
			}
		}
	}
}
