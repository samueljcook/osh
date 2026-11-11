//! Fixed Document finalization and Cache/Topic reader retirement under one exclusive interval.

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
use crate::runtime::admission::bindings::InstallationStorageRoute;
use crate::runtime::admission::upgrade::NativeUpgradeInvocation;
use crate::runtime::admission::upgrade::contracts::require_scope_maintenance_route;
use crate::runtime::targets::ResolvedNativeRuntimeTarget;
use crate::scope::{ScopeError, ScopeIdentity};
use crate::upgrade::policy::UpgradeCompatibilityError;
use crate::upgrade::policy::assessment::{
	ParticipantPolicyAssessment, ParticipantPolicyAssessmentFields,
};
use crate::upgrade::policy::control_replacement::EffectiveControlReplacement;
use crate::upgrade::policy::document_reader::BoxedDocumentReaderCompletionError;
use crate::upgrade::policy::payload_reader::BoxedPrimitivePayloadReaderCompletionError;
use crate::upgrade::work::{PgUpgradeStatus, UpgradeSourceOrigin, UpgradeWork, UpgradeWorkError};

use super::{
	NativeUpgradeParticipantsError, ParticipantInspectionBudget, ParticipantInspectionCoverage,
	ParticipantInspectionSummary, PgNativeUpgradeParticipants,
};

mod stage;
use stage::{ScopeReaderStage, ScopeReaderStageFields};

pub(in crate::upgrade) struct ScopeFinalizationRequest<'a> {
	pub(crate) invocation: NativeUpgradeInvocation<'a>,
	pub(crate) resolved: &'a ResolvedNativeRuntimeTarget,
	pub(crate) scope: &'a ScopeIdentity,
	pub(crate) participant_budget: ParticipantInspectionBudget,
	/// Bounds all protected work; owned COMMIT/ROLLBACK keeps its own lifetime.
	pub(crate) retirement_wait_timeout: Duration,
}

/// The committed scope policy and work checkpoint after finalization.
#[derive(Debug)]
pub(in crate::upgrade) struct ScopeFinalizationResult {
	pub(crate) work: UpgradeWork,
	pub(crate) observed: NativeUpgradeObservedControl,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ScopeFinalizationError {
	#[error("scope reader retirement resolved code differs from its installation")]
	ResolvedCodeAssociationMismatch,
	#[error("scope reader retirement protected wait must be positive and fit the local clock")]
	InvalidRetirementWaitTimeout,
	#[error("scope reader retirement protected work exceeded its configured wait")]
	RetirementWaitElapsed,
	#[error("scope reader retirement scope is absent from the complete final target")]
	ScopeNotTargeted { scope: ScopeIdentity },
	#[error("scope reader retirement requires a Document or schema-owned Cache/Topic scope")]
	UnsupportedScope { scope: ScopeIdentity },
	#[error("scope reader retirement scope policy is unavailable")]
	Scope {
		scope: ScopeIdentity,
		#[source]
		source: ScopeError,
	},
	#[error("scope reader retirement Document preparation failed")]
	Document(#[source] Box<BoxedDocumentReaderCompletionError>),
	#[error("scope reader retirement Cache/Topic preparation failed")]
	Payload(#[source] Box<BoxedPrimitivePayloadReaderCompletionError>),
	#[error("scope reader retirement requires a complete compatible participant inspection")]
	ParticipantInspection {
		scope: ScopeIdentity,
		summary: Box<ParticipantInspectionSummary>,
	},
}
impl From<ScopeFinalizationError> for NativeUpgradeParticipantsError {
	fn from(source: ScopeFinalizationError) -> Self {
		Self::ScopeReader(Box::new(source))
	}
}

impl PgNativeUpgradeParticipants {
	/// Installs one exact final scope policy and consumes only its validated completed progress.
	pub(in crate::upgrade) async fn finalize_scope(
		&self,
		mut request: ScopeFinalizationRequest<'_>,
	) -> Result<ScopeFinalizationResult, NativeUpgradeParticipantsError> {
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
			return Err(ScopeFinalizationError::ResolvedCodeAssociationMismatch.into());
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
			request.retirement_wait_timeout,
			ScopeFinalizationError::InvalidRetirementWaitTimeout,
		)?;
		if !request
			.resolved
			.target()
			.request()
			.scope_replacements()
			.is_some_and(|scopes| scopes.lookup(request.scope).is_ok())
		{
			return Err(ScopeFinalizationError::ScopeNotTargeted {
				scope: request.scope.clone(),
			}
			.into());
		}
		let registration = request.invocation.prepare_transaction()?;
		run_registered_transaction_write(
			&self.config.control_pool,
			"db.native_upgrade_participants.finalize_scope",
			registration,
			async |tx| {
				request.invocation.require_transaction(tx)?;
				let mut completion = tx.track_state_mutation();
				let deadline = deadline_after_positive_wait(
					request.retirement_wait_timeout,
					ScopeFinalizationError::InvalidRetirementWaitTimeout,
				)?;
				let interruption = tx.statement_interruption_marker();
				let outcome = tokio::time::timeout_at(
					tokio_instant(deadline),
					self.retire_scope_readers_in_transaction(tx, &request, deadline),
				)
				.await
				.map_err(|_| {
					interruption.mark_interrupted();
					ScopeFinalizationError::RetirementWaitElapsed
				})??;
				require_before_deadline(deadline, ScopeFinalizationError::RetirementWaitElapsed)?;
				completion.complete();
				Ok(outcome)
			},
		)
		.await
	}

	async fn retire_scope_readers_in_transaction(
		&self,
		tx: &mut WriteTx<'_>,
		request: &ScopeFinalizationRequest<'_>,
		retirement_deadline: Deadline,
	) -> Result<ScopeFinalizationResult, NativeUpgradeParticipantsError> {
		require_before_deadline(
			retirement_deadline,
			ScopeFinalizationError::RetirementWaitElapsed,
		)?;
		let pin_deadline = deadline_after_positive_wait(
			self.config.control_pin_wait_timeout,
			NativeUpgradeParticipantsError::InvalidControlPinWaitTimeout,
		)?;
		let interruption = tx.statement_interruption_marker();
		let (source_phase, mut physical) =
			tokio::time::timeout_at(tokio_instant(pin_deadline), async {
				let source_phase = self.config.installation_binding.pin_source(tx).await?;
				let physical = self
					.config
					.control_storage
					.lock_exclusive_for_participant_work(tx, self.config.conflict_attempts)
					.await?;
				Ok::<_, NativeUpgradeParticipantsError>((source_phase, physical))
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
			InstallationStorageRoute::SourceCoexistence
			| InstallationStorageRoute::SourceRelocation
			| InstallationStorageRoute::NativeImported => UpgradeSourceOrigin::Imported,
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
		work.require_checkpoint_context(&current, candidate, origin, &checkpoint)?;
		if matches!(
			work.status(),
			PgUpgradeStatus::Completed | PgUpgradeStatus::Cancelled
		) {
			return Err(UpgradeWorkError::TerminalWork.into());
		}
		work.require_source_checkpoint_invariants(
			current.component_policies(),
			current.scope_policies(),
		)?;
		work.require_document_checkpoint_invariants(current.scope_policies())?;
		work.require_document_verification_invariants(current.scope_policies())?;
		work.require_payload_checkpoint_invariants(current.scope_policies())?;
		self.require_control_writer(&current)?;
		let target = work
			.target()
			.request()
			.scope_replacements()
			.and_then(|scopes| scopes.lookup(request.scope).ok())
			.ok_or_else(|| ScopeFinalizationError::ScopeNotTargeted {
				scope: request.scope.clone(),
			})?
			.clone();
		let installed = current
			.scope_policies()
			.lookup(request.scope)
			.map_err(|source| ScopeFinalizationError::Scope {
				scope: request.scope.clone(),
				source,
			})?;
		let stage = ScopeReaderStage::new(ScopeReaderStageFields {
			code: request.resolved.code(),
			current: installed,
			target: &target,
		})?;
		let prepared = stage.prepare(current.scope_policies())?;
		require_scope_maintenance_route(route, installed)?;
		physical
			.authenticate_upgrade(&current, &request.invocation)
			.await?;
		require_before_deadline(
			retirement_deadline,
			ScopeFinalizationError::RetirementWaitElapsed,
		)?;
		if installed.matches_policy(&target) && !stage.has_progress(&work, request.scope) {
			physical.keep().await?;
			require_before_deadline(
				retirement_deadline,
				ScopeFinalizationError::RetirementWaitElapsed,
			)?;
			return Ok(ScopeFinalizationResult {
				observed: NativeUpgradeObservedControl::new(NativeUpgradeObservedControlFields {
					control: current,
					route,
					backend: parsed_backend,
				}),
				work,
			});
		}
		let generations =
			prepared.complete_work(&mut work, &current, candidate, origin, &checkpoint)?;
		let declaration = self.config.administration.declaration();
		let assessment = ParticipantPolicyAssessment::new(ParticipantPolicyAssessmentFields {
			administration: &declaration,
			installation_id: current.installation_id(),
			components: current.component_policies(),
			scopes: prepared.scopes(),
			participant: self.config.administration.participant_schema(),
			control: self.config.administration.control_component(),
		})?;
		require_before_deadline(
			retirement_deadline,
			ScopeFinalizationError::RetirementWaitElapsed,
		)?;
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
			ScopeFinalizationError::RetirementWaitElapsed,
		)?;
		if inspection.coverage != ParticipantInspectionCoverage::Complete
			|| !inspection.deferred.is_empty()
		{
			return Err(ScopeFinalizationError::ParticipantInspection {
				scope: request.scope.clone(),
				summary: Box::new(inspection),
			}
			.into());
		}
		let scopes = prepared.into_scopes();
		drop(stage);
		let mut replacement = EffectiveControlReplacement::new(current);
		let fields = replacement.fields_mut();
		fields.scope_policies = scopes;
		fields.generation = generations.next;
		fields.pending_upgrade = Some(work.encode_json()?);
		let updated = replacement
			.finish()
			.map_err(UpgradeCompatibilityError::from)?;
		require_before_deadline(
			retirement_deadline,
			ScopeFinalizationError::RetirementWaitElapsed,
		)?;
		let now = physical.observe_time().await?;
		let encoded = self.encode_current_control(&updated, now)?;
		require_before_deadline(
			retirement_deadline,
			ScopeFinalizationError::RetirementWaitElapsed,
		)?;
		physical.write(encoded).await?;
		require_before_deadline(
			retirement_deadline,
			ScopeFinalizationError::RetirementWaitElapsed,
		)?;
		Ok(ScopeFinalizationResult {
			observed: NativeUpgradeObservedControl::new(NativeUpgradeObservedControlFields {
				control: updated,
				route,
				backend: parsed_backend,
			}),
			work,
		})
	}
}
