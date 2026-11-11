//! Canonical exclusive control authority retained through one participant inspection and write.

use std::num::NonZeroU32;

use crate::db::WriteTx;
use crate::installation::control_state::InstallationControlState;
use crate::installation::control_storage::{
	ControlPinError, PgControlRecordLock, PgControlStorage,
};
use crate::records::postgres::PgPointStorage;
use crate::records::postgres::scan::{RecordMaximumRead, RecordPageError};
use crate::records::{EncodedRecord, PointRecordRead, StateTimestampMicros};
use crate::scope::ScopeIdentity;
use crate::upgrade::participants::exclusive::kernel;
use crate::upgrade::participants::{
	NativeUpgradeParticipantsError, ParticipantInspectionBudget, ParticipantInspectionCoverage,
	ParticipantInspectionSummary, PgNativeUpgradeParticipants,
};
use crate::upgrade::policy::assessment::ParticipantPolicyAssessment;

mod document_index;
mod document_readiness;
pub(in crate::upgrade) mod v10;
pub(crate) use document_index::PgDocumentIndexInspectionError;

enum InspectionState {
	NotStarted,
	Started,
	Compatible,
}

/// Actual selected contexts borrowed for one physical current-and-proposed participant inspection.
pub(in crate::upgrade) struct ParticipantInspectionFields<'a, 'policy> {
	pub(crate) inspector: &'a PgNativeUpgradeParticipants,
	pub(crate) current: &'a InstallationControlState,
	pub(crate) proposed: &'a ParticipantPolicyAssessment<'policy>,
	pub(crate) budget: ParticipantInspectionBudget,
}

/// Only exclusive canonical acquisition constructs this guard. Failed or abandoned inspection
/// retains mandatory rollback; its transaction and physical guard cannot be extracted.
pub(in crate::upgrade) struct PgParticipantControlLock<'a, 'tx> {
	physical: PgControlRecordLock<'a, 'tx>,
	inspection: InspectionState,
}

impl PgControlStorage {
	pub(in crate::upgrade) async fn lock_exclusive_for_participant_work<'a, 'tx>(
		&'a self,
		tx: &'a mut WriteTx<'tx>,
		conflict_attempts: NonZeroU32,
	) -> Result<PgParticipantControlLock<'a, 'tx>, ControlPinError> {
		let physical = self.lock_exclusive(tx, conflict_attempts).await?;
		Ok(PgParticipantControlLock {
			physical,
			inspection: InspectionState::NotStarted,
		})
	}
}

impl PgParticipantControlLock<'_, '_> {
	/// Authenticates the private runtime under this exact exclusive transaction without repinning.
	pub(in crate::upgrade) async fn authenticate_upgrade(
		&mut self,
		current: &InstallationControlState,
		invocation: &crate::runtime::admission::upgrade::NativeUpgradeInvocation<'_>,
	) -> Result<(), NativeUpgradeParticipantsError> {
		invocation
			.authenticate_current(self.physical.companion_tx(), current)
			.await?;
		Ok(())
	}

	pub(crate) fn snapshot(&self) -> &PointRecordRead {
		self.physical.snapshot()
	}

	/// Observes the exact mapped namespace under this retained exclusive interval.
	/// The fixed actor checks placement and current absence before requesting this read.
	pub(in crate::upgrade) async fn observe_scope_namespace(
		&mut self,
		storage: &PgPointStorage,
		scope: &ScopeIdentity,
	) -> Result<RecordMaximumRead, RecordPageError> {
		storage
			.read_maximum_valued_key(self.physical.companion_tx(), scope.namespace())
			.await
	}

	/// Checks current then proposed policies in one scan under the actual exclusive guard.
	/// A second attempt invalidates compatibility even when an earlier scan completed.
	pub(in crate::upgrade) async fn inspect_participants(
		&mut self,
		fields: ParticipantInspectionFields<'_, '_>,
	) -> Result<ParticipantInspectionSummary, NativeUpgradeParticipantsError> {
		let prior = std::mem::replace(&mut self.inspection, InspectionState::Started);
		if !matches!(prior, InspectionState::NotStarted) {
			return Err(NativeUpgradeParticipantsError::InspectionAlreadyPerformed);
		}
		fields.inspector.require_control_guard(&self.physical)?;
		let result =
			kernel::inspect_current_and_proposed_policy(self.physical.companion_tx(), fields)
				.await?;
		if result.coverage == ParticipantInspectionCoverage::Complete && result.deferred.is_empty()
		{
			self.inspection = InspectionState::Compatible;
		}
		Ok(result)
	}

	pub(crate) async fn observe_time(&mut self) -> Result<StateTimestampMicros, ControlPinError> {
		self.physical.observe_time().await
	}

	/// Only this guard's one complete compatible inspection permits an effective write.
	pub(crate) async fn write(
		self,
		record: EncodedRecord,
	) -> Result<(), NativeUpgradeParticipantsError> {
		if !matches!(self.inspection, InspectionState::Compatible) {
			return Err(NativeUpgradeParticipantsError::InspectionNotCompleted);
		}
		self.physical.write(record).await?;
		Ok(())
	}

	/// An unchanged postcondition can finish without scanning; a failed scan cannot finish.
	pub(crate) async fn keep(self) -> Result<(), NativeUpgradeParticipantsError> {
		if matches!(self.inspection, InspectionState::Started) {
			return Err(NativeUpgradeParticipantsError::InspectionNotCompleted);
		}
		self.physical.keep().await?;
		Ok(())
	}
}
