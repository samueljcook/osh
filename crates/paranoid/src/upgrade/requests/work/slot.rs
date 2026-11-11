//! The installation's storage route and the upgrade that its control record holds.

use crate::db::run_transaction_read;
use crate::runtime::admission::bindings::InstallationStorageRoute;
use crate::runtime::local_admission::upgrade_work::RuntimeUpgradeWorkIdentity;
use crate::upgrade::work::UpgradeWork;

use super::{NativeUpgradeRequestError, PgNativeUpgradeRequests};

/// The recorded upgrade's identity and whether it can still change the installation.
pub(crate) struct NativeUpgradeSlotEntry {
	pub(crate) identity: RuntimeUpgradeWorkIdentity,
	pub(crate) unfinished: bool,
}

pub(crate) struct NativeUpgradeSlotObservation {
	pub(crate) route: InstallationStorageRoute,
	pub(crate) entry: Option<NativeUpgradeSlotEntry>,
	/// The installed scope policies.
	pub(crate) scopes: crate::scope::ScopePolicySet,
}

impl PgNativeUpgradeRequests {
	/// Reads the storage route and the recorded upgrade, without enrolling in that upgrade.
	pub(crate) async fn read_upgrade_slot(
		&self,
	) -> Result<NativeUpgradeSlotObservation, NativeUpgradeRequestError> {
		run_transaction_read(
			&self.config.control_pool,
			"db.native_upgrade_requests.read_upgrade_slot",
			async |tx| {
				let observed = self.read_control_observation(tx).await?;
				let entry = match observed.decoded.state().pending_upgrade() {
					None => None,
					Some(metadata) => {
						let work = UpgradeWork::decode_json(metadata)?;
						let identity = work.target().request().identity();
						Some(NativeUpgradeSlotEntry {
							identity: RuntimeUpgradeWorkIdentity::from_parts(
								identity.installation_id,
								identity.request_id,
							),
							unfinished: work.is_unfinished(),
						})
					}
				};
				Ok(NativeUpgradeSlotObservation {
					route: observed.route,
					entry,
					scopes: observed.decoded.state().scope_policies().clone(),
				})
			},
		)
		.await
	}
}
