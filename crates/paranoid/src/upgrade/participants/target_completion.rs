//! Atomic supported full-target completion under one fresh exclusive participant inspection.

use crate::upgrade::observation::{
	NativeUpgradeObservedControl, NativeUpgradeObservedControlFields,
};
use crate::upgrade::participants::ParticipantInspectionFields;
use crate::upgrade::wait::{
	deadline_after_positive_wait, require_before_deadline, require_valid_wait, tokio_instant,
};
use primitives_rs::time::Deadline;

use std::{sync::Arc, time::Duration};

use crate::db::WriteTx;
use crate::db::transaction::run_registered_transaction_write;
use crate::fleet::counter::state::CounterReaderRetirementError;
use crate::runtime::admission::bindings::InstallationStorageRoute;
use crate::runtime::admission::upgrade::NativeUpgradeInvocation;
use crate::runtime::targets::ResolvedNativeRuntimeTarget;
use crate::scope::ScopeIdentity;
use crate::upgrade::policy::UpgradeCompatibilityError;
use crate::upgrade::policy::assessment::{
	ParticipantPolicyAssessment, ParticipantPolicyAssessmentFields,
};
use crate::upgrade::policy::control_replacement::EffectiveControlReplacement;
use crate::upgrade::work::target_completion::TargetCompletionWorkFields;
use crate::upgrade::work::{
	PgUpgradeStatus, SourceGoal, UpgradeSourceOrigin, UpgradeWork, UpgradeWorkError,
};

use super::{
	NativeUpgradeParticipantsError, ParticipantInspectionBudget, ParticipantInspectionCoverage,
	ParticipantInspectionSummary, PgNativeUpgradeParticipants,
};

mod preparation;
use preparation::{TargetComponentPreparationFields, prepare_components};

pub(in crate::upgrade) struct TargetCompletionRequest<'a> {
	pub(crate) invocation: NativeUpgradeInvocation<'a>,
	pub(crate) resolved: &'a ResolvedNativeRuntimeTarget,
	pub(crate) participant_budget: ParticipantInspectionBudget,
	pub(crate) completion_wait_timeout: Duration,
}

/// Both outcomes require known owned COMMIT and attribute no earlier uncertain completion.
#[derive(Debug)]
pub(in crate::upgrade) enum TargetCompletionResult {
	Completed {
		work: UpgradeWork,
		observed: NativeUpgradeObservedControl,
	},
	AlreadyCompleted {
		work: UpgradeWork,
		observed: NativeUpgradeObservedControl,
	},
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum TargetCompletionError {
	#[error("target completion resolved code differs from its installation")]
	ResolvedCodeAssociationMismatch,
	#[error("target completion protected wait must be positive and fit the local clock")]
	InvalidCompletionWaitTimeout,
	#[error("target completion protected work exceeded its configured wait")]
	CompletionWaitElapsed,
	#[error("target completion source goal is not satisfied by the installed route")]
	SourceGoalNotSatisfied {
		goal: SourceGoal,
		route: InstallationStorageRoute,
	},
	#[error("target completion requires source-data retirement for this component change")]
	DataRetirementRequired { component: String },
	#[error("target completion component addition requires a named supported procedure")]
	ComponentAdditionRequiresProcedure { component: String },
	#[error("target completion component change requires a named supported procedure")]
	ComponentChangeRequiresProcedure { component: String },
	#[error("target completion Counter reader narrowing failed")]
	Counter(#[source] Box<CounterReaderRetirementError>),
	#[error("target completion requires the targeted scope policy to be final")]
	ScopeNotFinal { scope: ScopeIdentity },
	#[error("completed Work does not match the effective full target")]
	CompletedStateMismatch,
	#[error("target completion requires a complete compatible participant inspection")]
	ParticipantInspection {
		summary: Box<ParticipantInspectionSummary>,
	},
}
impl From<TargetCompletionError> for NativeUpgradeParticipantsError {
	fn from(source: TargetCompletionError) -> Self {
		Self::TargetCompletion(Box::new(source))
	}
}

impl PgNativeUpgradeParticipants {
	pub(in crate::upgrade) async fn complete_target(
		&self,
		mut request: TargetCompletionRequest<'_>,
	) -> Result<TargetCompletionResult, NativeUpgradeParticipantsError> {
		request.invocation.require_bindings(
			&self.config.installation_binding,
			&self.config.control_storage,
			&self.config.administration,
		)?;
		if !Arc::ptr_eq(
			request.resolved.code().installation(),
			&self.config.installation_binding,
		) || !Arc::ptr_eq(
			request.resolved.code().administration(),
			&self.config.administration,
		) {
			return Err(TargetCompletionError::ResolvedCodeAssociationMismatch.into());
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
		require_valid_wait(
			request.completion_wait_timeout,
			TargetCompletionError::InvalidCompletionWaitTimeout,
		)?;
		let registration = request.invocation.prepare_transaction()?;
		run_registered_transaction_write(
			&self.config.control_pool,
			"db.native_upgrade_participants.complete_target",
			registration,
			async |tx| {
				request.invocation.require_transaction(tx)?;
				let mut completion = tx.track_state_mutation();
				let deadline = deadline_after_positive_wait(
					request.completion_wait_timeout,
					TargetCompletionError::InvalidCompletionWaitTimeout,
				)?;
				let interruption = tx.statement_interruption_marker();
				let outcome = tokio::time::timeout_at(
					tokio_instant(deadline),
					self.complete_target_in_transaction(tx, &request, deadline),
				)
				.await
				.map_err(|_| {
					interruption.mark_interrupted();
					TargetCompletionError::CompletionWaitElapsed
				})??;
				require_before_deadline(deadline, TargetCompletionError::CompletionWaitElapsed)?;
				completion.complete();
				Ok(outcome)
			},
		)
		.await
	}

	async fn complete_target_in_transaction(
		&self,
		tx: &mut WriteTx<'_>,
		request: &TargetCompletionRequest<'_>,
		deadline: Deadline,
	) -> Result<TargetCompletionResult, NativeUpgradeParticipantsError> {
		require_before_deadline(deadline, TargetCompletionError::CompletionWaitElapsed)?;
		let pin_until = deadline_after_positive_wait(
			self.config.control_pin_wait_timeout,
			NativeUpgradeParticipantsError::InvalidControlPinWaitTimeout,
		)?;
		let interruption = tx.statement_interruption_marker();
		let (source_phase, mut physical) =
			tokio::time::timeout_at(tokio_instant(pin_until), async {
				let phase = self.config.installation_binding.pin_source(tx).await?;
				let physical = self
					.config
					.control_storage
					.lock_exclusive_for_participant_work(tx, self.config.conflict_attempts)
					.await?;
				Ok::<_, NativeUpgradeParticipantsError>((phase, physical))
			})
			.await
			.map_err(|_| {
				interruption.mark_interrupted();
				NativeUpgradeParticipantsError::ControlPinWaitElapsed
			})??;
		let (decoded, route, parsed_backend) =
			self.decode_control_with_registration(physical.snapshot(), source_phase)?;
		let origin = match route {
			InstallationStorageRoute::NativeFresh => UpgradeSourceOrigin::Fresh,
			_ => UpgradeSourceOrigin::Imported,
		};
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
		let expected = work.checkpoint_context();
		work.require_checkpoint_context(&current, candidate, origin, &expected)?;
		if work.status() == PgUpgradeStatus::Cancelled {
			return Err(UpgradeWorkError::TerminalWork.into());
		}
		require_source_goal(work.target().source_goal(), route)?;
		work.require_source_checkpoint_invariants(
			current.component_policies(),
			current.scope_policies(),
		)?;
		work.require_document_checkpoint_invariants(current.scope_policies())?;
		work.require_document_verification_invariants(current.scope_policies())?;
		work.require_payload_checkpoint_invariants(current.scope_policies())?;
		work.require_consumed_target_progress()
			.map_err(UpgradeWorkError::from)?;
		if let Some(scopes) = work.target().request().scope_replacements() {
			for target in scopes.policies() {
				if !current
					.scope_policies()
					.lookup(target.identity())
					.is_ok_and(|policy| policy.matches_policy(target))
				{
					return Err(TargetCompletionError::ScopeNotFinal {
						scope: target.identity().clone(),
					}
					.into());
				}
			}
		}
		self.require_control_writer(&current)?;
		let components = prepare_components(TargetComponentPreparationFields {
			current: &current,
			target: work.target(),
			code: request.resolved.code(),
			route,
		})?;
		physical
			.authenticate_upgrade(&current, &request.invocation)
			.await?;
		require_before_deadline(deadline, TargetCompletionError::CompletionWaitElapsed)?;
		if work.status() == PgUpgradeStatus::Completed {
			if current.component_policies().len() != components.len()
				|| !current.component_policies().iter().all(|policy| {
					components
						.iter()
						.any(|target| policy.matches_policy(target))
				}) {
				return Err(TargetCompletionError::CompletedStateMismatch.into());
			}
			physical.keep().await?;
			require_before_deadline(deadline, TargetCompletionError::CompletionWaitElapsed)?;
			return Ok(TargetCompletionResult::AlreadyCompleted {
				observed: NativeUpgradeObservedControl::new(NativeUpgradeObservedControlFields {
					control: current,
					route,
					backend: parsed_backend,
				}),
				work,
			});
		}
		let generations = work.complete_target(TargetCompletionWorkFields {
			control: &current,
			candidate,
			origin,
			expected: &expected,
			components: &components,
			scopes: current.scope_policies(),
		})?;
		let declaration = self.config.administration.declaration();
		let assessment = ParticipantPolicyAssessment::new(ParticipantPolicyAssessmentFields {
			administration: &declaration,
			installation_id: current.installation_id(),
			components: &components,
			scopes: current.scope_policies(),
			participant: self.config.administration.participant_schema(),
			control: self.config.administration.control_component(),
		})?;
		require_before_deadline(deadline, TargetCompletionError::CompletionWaitElapsed)?;
		let inspection = physical
			.inspect_participants(ParticipantInspectionFields {
				inspector: self,
				current: &current,
				proposed: &assessment,
				budget: request.participant_budget,
			})
			.await?;
		require_before_deadline(deadline, TargetCompletionError::CompletionWaitElapsed)?;
		if inspection.coverage != ParticipantInspectionCoverage::Complete
			|| !inspection.deferred.is_empty()
		{
			return Err(TargetCompletionError::ParticipantInspection {
				summary: Box::new(inspection),
			}
			.into());
		}
		let mut replacement = EffectiveControlReplacement::new(current);
		let fields = replacement.fields_mut();
		fields.component_policies = components;
		fields.generation = generations.next;
		fields.pending_upgrade = Some(work.encode_json()?);
		let updated = replacement
			.finish()
			.map_err(UpgradeCompatibilityError::from)?;
		require_before_deadline(deadline, TargetCompletionError::CompletionWaitElapsed)?;
		let now = physical.observe_time().await?;
		let encoded = self.encode_current_control(&updated, now)?;
		require_before_deadline(deadline, TargetCompletionError::CompletionWaitElapsed)?;
		physical.write(encoded).await?;
		require_before_deadline(deadline, TargetCompletionError::CompletionWaitElapsed)?;
		Ok(TargetCompletionResult::Completed {
			observed: NativeUpgradeObservedControl::new(NativeUpgradeObservedControlFields {
				control: updated,
				route,
				backend: parsed_backend,
			}),
			work,
		})
	}
}

fn require_source_goal(
	goal: SourceGoal,
	route: InstallationStorageRoute,
) -> Result<(), TargetCompletionError> {
	let satisfied = match goal {
		SourceGoal::Unchanged => true,
		SourceGoal::WritersRetired => matches!(
			route,
			InstallationStorageRoute::SourceRelocation | InstallationStorageRoute::NativeImported
		),
		SourceGoal::DataRetired => route == InstallationStorageRoute::NativeImported,
	};
	if satisfied {
		Ok(())
	} else {
		Err(TargetCompletionError::SourceGoalNotSatisfied { goal, route })
	}
}
