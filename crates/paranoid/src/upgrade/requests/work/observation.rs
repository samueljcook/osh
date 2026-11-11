//! One enrolled diagnostic control, route, Work and backend observation after known rollback.

use std::sync::Arc;

use crate::db::Tx;
use crate::db::transaction::run_registered_transaction_read;
use crate::installation::control_state::InstallationControlState;
use crate::installation::creation::PgBackendRegistration;
use crate::runtime::admission::bindings::InstallationStorageRoute;
use crate::runtime::admission::upgrade::NativeUpgradeEnrollment;
use crate::upgrade::observation::{
	NativeUpgradeObservedControl, NativeUpgradeObservedControlFields,
};
use crate::upgrade::requests::NativeUpgradeControlObservation;
use crate::upgrade::work::UpgradeWork;

use super::{NativeUpgradeRequestError, NativeUpgradeWorkReadRequest, PgNativeUpgradeRequests};

/// Owned parsed data grants no transaction, source exclusion, participant or promotion authority.
pub(in crate::upgrade) struct NativeUpgradeControllerObservation {
	observed: NativeUpgradeObservedControl,
	work: Option<Arc<UpgradeWork>>,
}

pub(in crate::upgrade) struct NativeUpgradeControllerObservationFields {
	pub(in crate::upgrade) observed: NativeUpgradeObservedControl,
	pub(in crate::upgrade) work: Option<Arc<UpgradeWork>>,
}

impl NativeUpgradeControllerObservation {
	pub(in crate::upgrade) fn new(fields: NativeUpgradeControllerObservationFields) -> Self {
		Self {
			observed: fields.observed,
			work: fields.work,
		}
	}
	pub(crate) fn control(&self) -> &InstallationControlState {
		self.observed.control()
	}
	pub(crate) fn route(&self) -> InstallationStorageRoute {
		self.observed.route()
	}
	pub(crate) fn backend(&self) -> &PgBackendRegistration {
		self.observed.backend()
	}
	pub(crate) fn work(&self) -> Option<&UpgradeWork> {
		self.work.as_deref()
	}
	pub(in crate::upgrade) fn shared_work(&self) -> Option<&Arc<UpgradeWork>> {
		self.work.as_ref()
	}
}

impl PgNativeUpgradeRequests {
	/// Reads one exact enrolled request without requiring executable code for its pending target.
	/// Absence attributes no historical outcome to an earlier uncertain creation or mutation.
	pub(in crate::upgrade) async fn read_pending_work_observation(
		&self,
		mut request: NativeUpgradeWorkReadRequest<'_>,
	) -> Result<NativeUpgradeControllerObservation, NativeUpgradeRequestError> {
		self.require_enrollment_bindings(&request.enrollment)?;
		let registration = request.enrollment.prepare_transaction()?;
		run_registered_transaction_read(
			&self.config.control_pool,
			"db.native_upgrade_requests.read_pending_work",
			registration,
			async |tx| {
				request.enrollment.require_transaction(tx)?;
				let observed = self.read_control_observation(tx).await?;
				self.authenticate_observed_work(tx, &request.enrollment, observed)
					.await
			},
		)
		.await
	}

	pub(in crate::upgrade::requests) async fn authenticate_observed_work(
		&self,
		tx: &mut Tx<'_>,
		enrollment: &NativeUpgradeEnrollment<'_>,
		observed: NativeUpgradeControlObservation,
	) -> Result<NativeUpgradeControllerObservation, NativeUpgradeRequestError> {
		let current = observed.decoded.state();
		let work = current
			.pending_upgrade()
			.map(UpgradeWork::decode_json)
			.transpose()?;
		if let Some(work) = &work {
			work.require_control_context(current, self.work_origin())?;
			let identity = work.target().request().identity();
			let expected = enrollment.identity();
			if identity.installation_id != expected.installation_id
				|| identity.request_id != expected.request_id
			{
				return Err(NativeUpgradeRequestError::WrongRequest);
			}
		}
		enrollment.authenticate_current(tx, current).await?;
		let work = work.map(Arc::new);
		let observed = NativeUpgradeObservedControl::new(NativeUpgradeObservedControlFields {
			control: observed.decoded.into_state(),
			route: observed.route,
			backend: observed.backend,
		});
		Ok(NativeUpgradeControllerObservation::new(
			NativeUpgradeControllerObservationFields { observed, work },
		))
	}
}
