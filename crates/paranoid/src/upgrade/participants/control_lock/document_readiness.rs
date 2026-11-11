//! Fixed document-only private authentication on the held exclusive transaction.

use crate::installation::control_state::InstallationControlState;
use crate::runtime::admission::bindings::InstallationStorageRoute;
use crate::runtime::admission::upgrade::{
	HeldUpgradeControl, NativeUpgradeAdmissionError, NativeUpgradeInvocation,
};
use crate::scope::ScopePolicy;
use crate::upgrade::policy::InstallationUpgradeCandidate;

use super::PgParticipantControlLock;

impl PgParticipantControlLock<'_, '_> {
	/// Actual current Document support authenticates once without releasing or repinning this
	/// guard.
	pub(in crate::upgrade) async fn authenticate_document_readiness(
		&mut self,
		current: &InstallationControlState,
		invocation: &NativeUpgradeInvocation<'_>,
		route: InstallationStorageRoute,
		candidate: &InstallationUpgradeCandidate,
		actual_stage: &ScopePolicy,
	) -> Result<(), NativeUpgradeAdmissionError> {
		let (tx, proof) = self.physical.companion_tx_with_proof();
		let contracts = invocation
			.native_scope_maintenance_contracts(
				tx,
				HeldUpgradeControl::new(proof, current, route),
				candidate,
				actual_stage,
			)
			.await?;
		contracts.require_document_support()
	}
}
