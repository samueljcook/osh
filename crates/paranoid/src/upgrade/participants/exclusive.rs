//! Diagnostic participant inspection retained inside one exact exclusive write transaction.
//!
//! This interval exposes neither transaction access nor control writes. Complete participant
//! observations supply no source, data, index readiness, or effective promotion proof.

use crate::installation::control_state::InstallationControlState;
use crate::upgrade::policy::InstallationUpgradeCandidate;
use crate::upgrade::wait::{deadline_after_positive_wait, tokio_instant};

use super::{
	NativeUpgradeParticipantsError, ParticipantInspectionBudget, ParticipantInspectionSummary,
	PgNativeUpgradeParticipants,
};
use crate::db::WriteTx;
use crate::records::postgres::StateMutationCompletion;

pub(crate) mod kernel;

/// Owns the exact transaction borrow, actual installed state, and local executable candidate.
/// No participant proof or mutable control value can outlive this exclusive interval.
/// A failed, cancelled, dropped, or forgotten inspection requires rollback.
pub(in crate::upgrade) struct PgParticipantUpgradeInterval<'a, 'tx> {
	participants: &'a PgNativeUpgradeParticipants,
	tx: &'a mut WriteTx<'tx>,
	control: InstallationControlState,
	candidate: &'a InstallationUpgradeCandidate,
	completion: StateMutationCompletion,
}

impl PgNativeUpgradeParticipants {
	pub(super) async fn lock_diagnostic<'a, 'tx>(
		&'a self,
		tx: &'a mut WriteTx<'tx>,
		candidate: &'a InstallationUpgradeCandidate,
	) -> Result<PgParticipantUpgradeInterval<'a, 'tx>, NativeUpgradeParticipantsError> {
		let deadline = deadline_after_positive_wait(
			self.config.control_pin_wait_timeout,
			NativeUpgradeParticipantsError::InvalidControlPinWaitTimeout,
		)?;
		let completion = tx.track_state_mutation();
		// Acquisition ends before any participant page or mutation and does not wrap commit.
		let interruption = tx.statement_interruption_marker();
		let (source_phase, control_snapshot) =
			tokio::time::timeout_at(tokio_instant(deadline), async {
				let source_phase = self.config.installation_binding.pin_source(tx).await?;
				let snapshot = self
					.config
					.control_storage
					.read_pinned_exclusive(tx)
					.await?;
				Ok::<_, NativeUpgradeParticipantsError>((source_phase, snapshot))
			})
			.await
			.map_err(|_| {
				interruption.mark_interrupted();
				NativeUpgradeParticipantsError::ControlPinWaitElapsed
			})??;
		let (decoded, _route) = self.decode_control(&control_snapshot, source_phase)?;
		let control = decoded.state();
		let declaration = self.config.administration.declaration();
		let administrative_policy = control
			.component(declaration.identifier())
			.ok_or(NativeUpgradeParticipantsError::AdministrativePolicyAbsent)?;
		let administration = declaration.bind(administrative_policy)?;
		administration.bind_schema(self.config.administration.control_component().schema())?;
		candidate.require_control_target(
			control,
			&declaration,
			self.config.administration.participant_schema(),
			self.config.administration.control_component(),
		)?;
		if let Some(scopes) = candidate.scope_replacements() {
			self.config
				.installation_binding
				.check_scope_policies(scopes)?;
		}
		Ok(PgParticipantUpgradeInterval {
			participants: self,
			tx,
			control: decoded.into_state(),
			candidate,
			completion,
		})
	}
}

impl PgParticipantUpgradeInterval<'_, '_> {
	/// Scans once under the retained transaction and then finishes the interval, which permits
	/// the commit of partial fences without an effective update. Cancellation or an error leaves
	/// the interval unfinished, so the transaction must roll back. The returned summary carries
	/// no exclusive authority or later promotion proof.
	pub(super) async fn inspect_and_finish(
		mut self,
		budget: ParticipantInspectionBudget,
	) -> Result<ParticipantInspectionSummary, NativeUpgradeParticipantsError> {
		let summary = kernel::inspect(&mut self, budget).await?;
		self.completion.complete();
		Ok(summary)
	}
}
