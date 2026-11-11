//! Fixed source-data retirement with fresh source/native checks and atomic effective metadata.

use crate::upgrade::observation::{
	NativeUpgradeObservedControl, NativeUpgradeObservedControlFields,
};
use crate::upgrade::participants::ParticipantInspectionFields;
use crate::upgrade::wait::{
	deadline_after_positive_wait, require_before_deadline, require_valid_wait, tokio_instant,
};
use primitives_rs::time::Deadline;

use std::time::Duration;

use crate::db::transaction::run_registered_transaction_write;
use crate::db::{DatabaseError, SchemaError, TransactionFailure, WriteTx};
use crate::installation::creation::{NativeInstallationError, source_data_retired_registration};
use crate::installation::v10::source_gate::{
	NativeSourceGatePhase, NativeSourceRetirementError, PinnedV10SourceGate,
};
use crate::installation::v10::{PgV10SourceRole, SourceDataEmptinessError};
use crate::runtime::admission::bindings::{
	InstallationStorageRoute, PgInstallationAdmissionBinding,
};
use crate::runtime::admission::upgrade::NativeUpgradeInvocation;
use crate::runtime::targets::ResolvedNativeRuntimeTarget;
use crate::upgrade::participants::SourceDataInspectionFields;
use crate::upgrade::policy::UpgradeCompatibilityError;
use crate::upgrade::policy::assessment::{
	ParticipantPolicyAssessment, ParticipantPolicyAssessmentFields,
};
use crate::upgrade::policy::control_replacement::EffectiveControlReplacement;
use crate::upgrade::progress::v10::source_invariants::SourceRetirementCheckpointError;
use crate::upgrade::work::v10::source_checkpoint::SourceCheckpointContext;
use crate::upgrade::work::v10::source_retirement::SourceDataRetirementWorkFields;
use crate::upgrade::work::{
	PgUpgradeStatus, SourceGoal, UpgradeSourceOrigin, UpgradeWork, UpgradeWorkError,
};

use super::super::{
	NativeUpgradeParticipantsError, ParticipantInspectionBudget, ParticipantInspectionCoverage,
	ParticipantInspectionSummary, PgNativeUpgradeParticipants,
};

pub(in crate::upgrade) struct SourceDataRetirementRequest<'a> {
	pub(crate) invocation: NativeUpgradeInvocation<'a>,
	pub(crate) resolved: &'a ResolvedNativeRuntimeTarget,
	pub(crate) source_lock_wait_timeout: Duration,
	/// Bounds protected work after source exclusion; owned COMMIT/ROLLBACK keeps its own lifetime.
	pub(crate) retirement_wait_timeout: Duration,
	pub(crate) participant_budget: ParticipantInspectionBudget,
}

/// Both variants require known owned COMMIT. AlreadyRetired supplies no historical attribution.
#[derive(Debug)]
pub(in crate::upgrade) enum SourceDataRetirementResult {
	Retired {
		work: UpgradeWork,
		observed: NativeUpgradeObservedControl,
	},
	AlreadyRetired {
		work: UpgradeWork,
		observed: NativeUpgradeObservedControl,
	},
}

pub(in crate::upgrade) type NativeSourceDataRetirementError =
	NativeSourceRetirementError<NativeUpgradeParticipantsError>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum SourceDataRetirementCauseError {
	#[error("source data retirement requires the retired source gate")]
	SourceWritersNotRetired,
	#[error("source data retirement requires its DataRetired goal")]
	DataRetiredGoalRequired,
	#[error("source data retirement requires the relocation route")]
	SourceRelocationRequired,
	#[error("source data retirement resolved code differs from its installation or administration")]
	ResolvedCodeAssociationMismatch,
	#[error("source data retirement protected wait must be positive and fit the local clock")]
	InvalidRetirementWaitTimeout,
	#[error("source data retirement protected work exceeded its configured wait")]
	RetirementWaitElapsed,
	#[error("source data retirement selected checkpoint requirement failed")]
	Checkpoints(#[source] Box<SourceRetirementCheckpointError>),
	#[error("source data retirement physical source validation failed")]
	SourceSchema(#[source] SchemaError),
	#[error("source data retirement complete source emptiness observation failed")]
	Emptiness(#[source] SourceDataEmptinessError),
	#[error("source data retirement has nonempty source data roles")]
	NonemptySourceRoles { roles: Vec<PgV10SourceRole> },
	#[error("source data retirement backend phase preparation failed")]
	Backend(#[source] Box<NativeInstallationError>),
	#[error("source data retirement requires a complete compatible participant inspection")]
	ParticipantInspection {
		summary: Box<ParticipantInspectionSummary>,
	},
}
impl SourceDataRetirementCauseError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Checkpoints(source) => source.database_error(),
			Self::SourceSchema(source) => source.database_error(),
			Self::Emptiness(source) => source.database_error(),
			Self::Backend(source) => source.database_error(),
			_ => None,
		}
	}
	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::Checkpoints(source) => source.involves_database_failure(),
			Self::SourceSchema(source) => source.involves_database_failure(),
			Self::Backend(source) => source.involves_database_failure(),
			_ => self.database_error().is_some(),
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Checkpoints(source) => source.commit_outcome_is_unknown(),
			Self::SourceSchema(source) => source.commit_outcome_is_unknown(),
			Self::Backend(source) => source.commit_outcome_is_unknown(),
			_ => self
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
		}
	}
}
impl From<SourceDataRetirementCauseError> for NativeUpgradeParticipantsError {
	fn from(source: SourceDataRetirementCauseError) -> Self {
		Self::SourceData(Box::new(source))
	}
}

impl PgNativeUpgradeParticipants {
	/// Requires a live [`PinnedV10SourceGate`] holding SourceExclusive before NativeExclusive.
	async fn retire_source_data_in_transaction(
		&self,
		pinned_gate: &PinnedV10SourceGate<'_>,
		tx: &mut WriteTx<'_>,
		request: &SourceDataRetirementRequest<'_>,
		retirement_deadline: Deadline,
	) -> Result<SourceDataRetirementResult, NativeUpgradeParticipantsError> {
		let source_phase = pinned_gate.phase();
		require_before_deadline(
			retirement_deadline,
			SourceDataRetirementCauseError::RetirementWaitElapsed,
		)?;
		if source_phase != NativeSourceGatePhase::Retired {
			return Err(SourceDataRetirementCauseError::SourceWritersNotRetired.into());
		}
		let PgInstallationAdmissionBinding::Imported { native, source } =
			self.config.installation_binding.as_ref()
		else {
			return Err(NativeUpgradeParticipantsError::ImportedInstallationRequired);
		};
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
		let (decoded, route, parsed_backend) =
			self.decode_control_with_registration(physical.snapshot(), Some(source_phase))?;
		let current = decoded.into_state();
		let candidate = request.resolved.candidate();
		candidate.require_control_target(
			&current,
			&self.config.administration.declaration(),
			self.config.administration.participant_schema(),
			self.config.administration.control_component(),
		)?;
		self.config
			.installation_binding
			.check_scope_policies(current.scope_policies())?;
		if let Some(scopes) = candidate.scope_replacements() {
			self.config
				.installation_binding
				.check_scope_policies(scopes)?;
		}
		let mut work = UpgradeWork::decode_json(
			current
				.pending_upgrade()
				.ok_or(NativeUpgradeParticipantsError::WorkAbsent)?,
		)?;
		if !work.target().matches_target(request.invocation.target())
			|| !work.target().matches_target(request.resolved.target())
		{
			return Err(UpgradeWorkError::WorkNotCurrent.into());
		}
		let checkpoint = work.checkpoint_context();
		work.require_checkpoint_context(
			&current,
			candidate,
			UpgradeSourceOrigin::Imported,
			&checkpoint,
		)?;
		if matches!(
			work.status(),
			PgUpgradeStatus::Completed | PgUpgradeStatus::Cancelled
		) {
			return Err(UpgradeWorkError::TerminalWork.into());
		}
		if work.target().source_goal() != SourceGoal::DataRetired {
			return Err(SourceDataRetirementCauseError::DataRetiredGoalRequired.into());
		}
		work.require_source_checkpoint_invariants(
			current.component_policies(),
			current.scope_policies(),
		)?;
		work.require_document_checkpoint_invariants(current.scope_policies())?;
		work.require_document_verification_invariants(current.scope_policies())?;
		work.require_payload_checkpoint_invariants(current.scope_policies())?;
		self.require_control_writer(&current)?;
		if route == InstallationStorageRoute::NativeImported {
			physical
				.observe_retired_source_data_postcondition(&current, &request.invocation, source)
				.await?;
			require_before_deadline(
				retirement_deadline,
				SourceDataRetirementCauseError::RetirementWaitElapsed,
			)?;
			physical.keep().await?;
			return Ok(SourceDataRetirementResult::AlreadyRetired {
				observed: NativeUpgradeObservedControl::new(NativeUpgradeObservedControlFields {
					control: current,
					route,
					backend: parsed_backend,
				}),
				work,
			});
		}
		if route != InstallationStorageRoute::SourceRelocation {
			return Err(SourceDataRetirementCauseError::SourceRelocationRequired.into());
		}
		require_before_deadline(
			retirement_deadline,
			SourceDataRetirementCauseError::RetirementWaitElapsed,
		)?;
		let observed = physical
			.inspect_source_data_retirement(SourceDataInspectionFields {
				current: &current,
				work: &work,
				request,
				administration: &self.config.administration,
			})
			.await?;
		require_before_deadline(
			retirement_deadline,
			SourceDataRetirementCauseError::RetirementWaitElapsed,
		)?;
		let declaration = self.config.administration.declaration();
		let assessment = ParticipantPolicyAssessment::new(ParticipantPolicyAssessmentFields {
			administration: &declaration,
			installation_id: current.installation_id(),
			components: current.component_policies(),
			scopes: current.scope_policies(),
			participant: self.config.administration.participant_schema(),
			control: self.config.administration.control_component(),
		})?;
		let inspection = physical
			.inspect_participants(ParticipantInspectionFields {
				inspector: self,
				current: &current,
				proposed: &assessment,
				budget: request.participant_budget,
			})
			.await?;
		require_before_deadline(
			retirement_deadline,
			SourceDataRetirementCauseError::RetirementWaitElapsed,
		)?;
		if inspection.coverage != ParticipantInspectionCoverage::Complete
			|| !inspection.deferred.is_empty()
		{
			return Err(SourceDataRetirementCauseError::ParticipantInspection {
				summary: Box::new(inspection),
			}
			.into());
		}
		let generations = work.complete_source_data_retirement(SourceDataRetirementWorkFields {
			context: SourceCheckpointContext {
				control: &current,
				candidate,
				origin: UpgradeSourceOrigin::Imported,
				expected: &checkpoint,
			},
			selected: &observed.selected,
		})?;
		let administration = declaration.bind(
			current
				.component(declaration.identifier())
				.ok_or(NativeUpgradeParticipantsError::AdministrativePolicyAbsent)?,
		)?;
		let backend =
			administration.bind_schema(self.config.administration.backend_schema().schema())?;
		let registration =
			source_data_retired_registration(&current, native, source, &backend, parsed_backend)
				.map_err(|error| SourceDataRetirementCauseError::Backend(Box::new(error)))?;
		let (registration, parsed_backend) = registration.into_parts();
		let mut replacement = EffectiveControlReplacement::new(current);
		let fields = replacement.fields_mut();
		fields.generation = generations.next;
		fields.backend_registration = registration;
		fields.pending_upgrade = Some(work.encode_json()?);
		let updated = replacement
			.finish()
			.map_err(UpgradeCompatibilityError::from)?;
		require_before_deadline(
			retirement_deadline,
			SourceDataRetirementCauseError::RetirementWaitElapsed,
		)?;
		let now = physical.observe_time().await?;
		let encoded = self.encode_current_control(&updated, now)?;
		require_before_deadline(
			retirement_deadline,
			SourceDataRetirementCauseError::RetirementWaitElapsed,
		)?;
		physical.write(encoded).await?;
		require_before_deadline(
			retirement_deadline,
			SourceDataRetirementCauseError::RetirementWaitElapsed,
		)?;
		Ok(SourceDataRetirementResult::Retired {
			observed: NativeUpgradeObservedControl::new(NativeUpgradeObservedControlFields {
				control: updated,
				route: InstallationStorageRoute::NativeImported,
				backend: parsed_backend,
			}),
			work,
		})
	}

	pub(in crate::upgrade) async fn retire_source_data(
		&self,
		mut request: SourceDataRetirementRequest<'_>,
	) -> Result<SourceDataRetirementResult, NativeSourceDataRetirementError> {
		let check = || -> Result<(), NativeUpgradeParticipantsError> {
			request.invocation.require_bindings(
				&self.config.installation_binding,
				&self.config.control_storage,
				&self.config.administration,
			)?;
			if !std::sync::Arc::ptr_eq(
				request.resolved.code().installation(),
				&self.config.installation_binding,
			) || !std::sync::Arc::ptr_eq(
				request.resolved.code().administration(),
				&self.config.administration,
			) {
				return Err(SourceDataRetirementCauseError::ResolvedCodeAssociationMismatch.into());
			}
			if !request
				.invocation
				.target()
				.matches_target(request.resolved.target())
				|| !request
					.invocation
					.target()
					.matches_candidate(request.resolved.candidate())
			{
				return Err(UpgradeWorkError::TargetMismatch.into());
			}
			if request.invocation.target().source_goal() != SourceGoal::DataRetired {
				return Err(SourceDataRetirementCauseError::DataRetiredGoalRequired.into());
			}
			require_valid_wait(
				request.retirement_wait_timeout,
				SourceDataRetirementCauseError::InvalidRetirementWaitTimeout,
			)?;
			Ok(())
		};
		check().map_err(NativeSourceRetirementError::Controller)?;
		let PgInstallationAdmissionBinding::Imported { source, .. } =
			self.config.installation_binding.as_ref()
		else {
			return Err(NativeSourceRetirementError::Controller(
				NativeUpgradeParticipantsError::ImportedInstallationRequired,
			));
		};
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
			"db.native_source_gate.retire_source_data",
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
				if request.retirement_wait_timeout.is_zero() {
					return Err(NativeSourceRetirementError::Controller(
						SourceDataRetirementCauseError::InvalidRetirementWaitTimeout.into(),
					));
				}
				let deadline = Deadline::new_from_base(
					tokio::time::Instant::now().into_std(),
					request.retirement_wait_timeout,
				)
				.map_err(|_| {
					NativeSourceRetirementError::Controller(
						SourceDataRetirementCauseError::InvalidRetirementWaitTimeout.into(),
					)
				})?;
				let interruption = tx.statement_interruption_marker();
				let result = tokio::time::timeout_at(
					tokio::time::Instant::from_std(deadline.instant()),
					self.retire_source_data_in_transaction(&pinned_gate, tx, &request, deadline),
				)
				.await
				.map_err(|_| {
					interruption.mark_interrupted();
					NativeSourceRetirementError::Controller(
						SourceDataRetirementCauseError::RetirementWaitElapsed.into(),
					)
				})?
				.map_err(NativeSourceRetirementError::Controller)?;
				if tokio::time::Instant::now().into_std() >= deadline.instant() {
					return Err(NativeSourceRetirementError::Controller(
						SourceDataRetirementCauseError::RetirementWaitElapsed.into(),
					));
				}
				pinned_gate.complete_without_activation();
				Ok(result)
			},
		)
		.await
	}
}
