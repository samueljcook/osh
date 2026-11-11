//! Fixed owned source retirement with actual stage inspection and atomic effective control write.

use crate::upgrade::observation::{
	NativeUpgradeObservedControl, NativeUpgradeObservedControlFields,
};
use crate::upgrade::participants::ParticipantInspectionFields;
use crate::upgrade::wait::{deadline_after_positive_wait, tokio_instant};

use std::time::Duration;

use crate::runtime::admission::upgrade::NativeUpgradeInvocation;

use crate::fleet::counter::state::CounterComponent;
use crate::upgrade::policy::InstallationUpgradeCandidate;
use crate::upgrade::policy::UpgradeCompatibilityError;
use crate::upgrade::policy::control_replacement::EffectiveControlReplacement;
use crate::upgrade::policy::v10::{
	SourceWriterRetirementAssessmentFields, SourceWriterRetirementStage,
	SourceWriterRetirementStageFields,
};
use crate::upgrade::work::{
	PgUpgradeStatus, SourceGoal, UpgradeSourceOrigin, UpgradeWork, UpgradeWorkError,
};

use super::super::{
	NativeUpgradeParticipantsError, ParticipantInspectionBudget, ParticipantInspectionCoverage,
	PgNativeUpgradeParticipants,
};
use crate::db::WriteTx;
use crate::db::transaction::run_registered_transaction_write;
use crate::installation::creation::source_writers_retired_registration;
use crate::installation::v10::source_gate::{NativeSourceRetirementError, PinnedV10SourceGate};
use crate::runtime::admission::bindings::{
	InstallationStorageRoute, PgInstallationAdmissionBinding,
};

pub(in crate::upgrade) struct SourceWriterRetirementRequest<'a> {
	pub(crate) invocation: NativeUpgradeInvocation<'a>,
	pub(crate) final_candidate: &'a InstallationUpgradeCandidate,
	pub(crate) source_lock_wait_timeout: Duration,
	pub(crate) participant_budget: ParticipantInspectionBudget,
}

/// Both variants are published only after this owned transaction has known COMMIT.
/// AlreadyRetired observes a current postcondition without attributing a prior unknown ending.
#[derive(Debug)]
pub(in crate::upgrade) enum SourceWriterRetirementResult {
	Retired {
		work: UpgradeWork,
		observed: NativeUpgradeObservedControl,
	},
	AlreadyRetired {
		work: UpgradeWork,
		observed: NativeUpgradeObservedControl,
	},
}

pub(in crate::upgrade) type NativeSourceWriterRetirementError =
	NativeSourceRetirementError<NativeUpgradeParticipantsError>;

impl PgNativeUpgradeParticipants {
	/// Requires a live [`PinnedV10SourceGate`] holding exclusive source protocol authority.
	async fn retire_source_writers_in_transaction(
		&self,
		pinned_gate: &PinnedV10SourceGate<'_>,
		tx: &mut WriteTx<'_>,
		request: &SourceWriterRetirementRequest<'_>,
	) -> Result<SourceWriterRetirementResult, NativeUpgradeParticipantsError> {
		let source_phase = pinned_gate.phase();
		let PgInstallationAdmissionBinding::Imported { native, source } =
			self.config.installation_binding.as_ref()
		else {
			return Err(NativeUpgradeParticipantsError::ImportedInstallationRequired);
		};
		let invocation = &request.invocation;
		let final_candidate = request.final_candidate;
		let participant_budget = request.participant_budget;
		let deadline = deadline_after_positive_wait(
			self.config.control_pin_wait_timeout,
			NativeUpgradeParticipantsError::InvalidControlPinWaitTimeout,
		)?;
		let interruption = tx.statement_interruption_marker();
		let mut physical = tokio::time::timeout_at(
			tokio_instant(deadline),
			self.config
				.control_storage
				.lock_exclusive_for_participant_work(tx, self.config.conflict_attempts),
		)
		.await
		.map_err(|_| {
			interruption.mark_interrupted();
			NativeUpgradeParticipantsError::ControlPinWaitElapsed
		})??;
		// Current control is decoded only from this exact canonical guard snapshot.
		let (decoded, route, parsed_backend) =
			self.decode_control_with_registration(physical.snapshot(), Some(source_phase))?;
		let current = decoded.into_state();
		final_candidate.require_control_target(
			&current,
			&self.config.administration.declaration(),
			self.config.administration.participant_schema(),
			self.config.administration.control_component(),
		)?;
		if let Some(scopes) = final_candidate.scope_replacements() {
			self.config
				.installation_binding
				.check_scope_policies(scopes)?;
		}
		let mut work = UpgradeWork::decode_json(
			current
				.pending_upgrade()
				.ok_or(NativeUpgradeParticipantsError::WorkAbsent)?,
		)?;
		if !work.target().matches_target(invocation.target()) {
			return Err(UpgradeWorkError::WorkNotCurrent.into());
		}
		let checkpoint = work.checkpoint_context();
		work.require_checkpoint_context(
			&current,
			final_candidate,
			UpgradeSourceOrigin::Imported,
			&checkpoint,
		)?;
		work.require_source_checkpoint_invariants(
			current.component_policies(),
			current.scope_policies(),
		)?;
		physical.authenticate_upgrade(&current, invocation).await?;
		match route {
			InstallationStorageRoute::NativeFresh => {
				Err(NativeUpgradeParticipantsError::ImportedInstallationRequired)
			}
			InstallationStorageRoute::SourceRelocation
			| InstallationStorageRoute::NativeImported => {
				physical.keep().await?;
				Ok(SourceWriterRetirementResult::AlreadyRetired {
					observed: NativeUpgradeObservedControl::new(
						NativeUpgradeObservedControlFields {
							control: current,
							route,
							backend: parsed_backend,
						},
					),
					work,
				})
			}
			InstallationStorageRoute::SourceCoexistence => {
				if matches!(
					work.status(),
					PgUpgradeStatus::Completed | PgUpgradeStatus::Cancelled
				) {
					return Err(UpgradeWorkError::TerminalWork.into());
				}
				self.require_control_writer(&current)?;
				let counter = CounterComponent::new();
				let stage = SourceWriterRetirementStage::new(SourceWriterRetirementStageFields {
					control: &current,
					final_candidate,
					counter: &counter,
				})
				.map_err(|error| {
					NativeUpgradeParticipantsError::SourceWriterStage(Box::new(error))
				})?;
				work.require_source_checkpoint_invariants(
					stage.policies(),
					current.scope_policies(),
				)?;
				let administration_declaration = self.config.administration.declaration();
				let inspection = {
					let assessment = stage.assessment(SourceWriterRetirementAssessmentFields {
						administration: &administration_declaration,
						participant: self.config.administration.participant_schema(),
						control_component: self.config.administration.control_component(),
					})?;
					physical
						.inspect_participants(ParticipantInspectionFields {
							inspector: self,
							current: &current,
							proposed: &assessment,
							budget: participant_budget,
						})
						.await?
				};
				if inspection.coverage != ParticipantInspectionCoverage::Complete
					|| !inspection.deferred.is_empty()
				{
					return Err(NativeUpgradeParticipantsError::SourceWriterInspection {
						summary: Box::new(inspection),
					});
				}
				let policies = stage.into_policies();
				let generations = work.advance_effective_build(
					&current,
					final_candidate,
					UpgradeSourceOrigin::Imported,
					&checkpoint,
				)?;
				let declaration = self.config.administration.declaration();
				let administration = declaration.bind(
					current
						.component(declaration.identifier())
						.ok_or(NativeUpgradeParticipantsError::AdministrativePolicyAbsent)?,
				)?;
				let backend = administration
					.bind_schema(self.config.administration.backend_schema().schema())?;
				let registration = source_writers_retired_registration(
					&current,
					native,
					source,
					&backend,
					parsed_backend,
				)
				.map_err(|error| {
					NativeUpgradeParticipantsError::SourceWriterRegistration(Box::new(error))
				})?;
				let (registration, parsed_backend) = registration.into_parts();
				let mut replacement = EffectiveControlReplacement::new(current);
				let fields = replacement.fields_mut();
				fields.component_policies = policies;
				fields.generation = generations.next;
				fields.backend_registration = registration;
				fields.pending_upgrade = Some(work.encode_json()?);
				let updated = replacement
					.finish()
					.map_err(UpgradeCompatibilityError::from)?;
				let now = physical.observe_time().await?;
				let encoded = self.encode_current_control(&updated, now)?;
				physical.write(encoded).await?;
				Ok(SourceWriterRetirementResult::Retired {
					observed: NativeUpgradeObservedControl::new(
						NativeUpgradeObservedControlFields {
							control: updated,
							route: InstallationStorageRoute::SourceRelocation,
							backend: parsed_backend,
						},
					),
					work,
				})
			}
		}
	}

	/// Owns the effective source barrier and returns only a known-committed postcondition.
	pub(in crate::upgrade) async fn retire_source_writers(
		&self,
		mut request: SourceWriterRetirementRequest<'_>,
	) -> Result<SourceWriterRetirementResult, NativeSourceWriterRetirementError> {
		request
			.invocation
			.require_bindings(
				&self.config.installation_binding,
				&self.config.control_storage,
				&self.config.administration,
			)
			.map_err(NativeUpgradeParticipantsError::from)
			.map_err(NativeSourceRetirementError::Controller)?;
		let PgInstallationAdmissionBinding::Imported { source, .. } =
			self.config.installation_binding.as_ref()
		else {
			return Err(NativeSourceRetirementError::Controller(
				NativeUpgradeParticipantsError::ImportedInstallationRequired,
			));
		};
		if request.invocation.target().source_goal() == SourceGoal::Unchanged {
			return Err(NativeSourceRetirementError::Controller(
				NativeUpgradeParticipantsError::SourceRetirementGoalRequired,
			));
		}
		if request.source_lock_wait_timeout.is_zero() {
			return Err(NativeSourceRetirementError::InvalidSourceLockWaitTimeout);
		}
		let gate = source.gate();
		let registration = request
			.invocation
			.prepare_transaction()
			.map_err(NativeUpgradeParticipantsError::from)
			.map_err(NativeSourceRetirementError::Controller)?;
		run_registered_transaction_write(
			&self.config.control_pool,
			"db.native_source_gate.retire_source_writers",
			registration,
			async move |tx| {
				request
					.invocation
					.require_transaction(tx)
					.map_err(NativeUpgradeParticipantsError::from)
					.map_err(NativeSourceRetirementError::Controller)?;
				let pinned_gate = gate
					.pin_exclusive_for_retirement(tx, request.source_lock_wait_timeout)
					.await?;
				let result = self
					.retire_source_writers_in_transaction(&pinned_gate, tx, &request)
					.await
					.map_err(NativeSourceRetirementError::Controller)?;
				pinned_gate.activate_retirement_if_needed(tx).await?;
				Ok(result)
			},
		)
		.await
	}
}
