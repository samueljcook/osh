//! Fixed one-scope writer-stage promotion under actual source/shared and control/exclusive pins.

use crate::upgrade::observation::{
	NativeUpgradeObservedControl, NativeUpgradeObservedControlFields,
};
use crate::upgrade::participants::ParticipantInspectionFields;
use crate::upgrade::wait::{deadline_after_positive_wait, tokio_instant};

use crate::runtime::admission::upgrade::NativeUpgradeInvocation;
use crate::runtime::targets::ResolvedNativeRuntimeTarget;
use crate::scope::ScopeIdentity;
use crate::upgrade::policy::UpgradeCompatibilityError;
use crate::upgrade::policy::assessment::{
	ParticipantPolicyAssessment, ParticipantPolicyAssessmentFields,
};
use crate::upgrade::policy::control_replacement::EffectiveControlReplacement;
use crate::upgrade::work::{PgUpgradeStatus, UpgradeSourceOrigin, UpgradeWork, UpgradeWorkError};

use super::{
	NativeUpgradeParticipantsError, ParticipantInspectionBudget, ParticipantInspectionCoverage,
	PgNativeUpgradeParticipants,
};
use crate::db::WriteTx;
use crate::db::transaction::run_registered_transaction_write;
use crate::runtime::admission::bindings::InstallationStorageRoute;

pub(in crate::upgrade) struct ScopeWriterPromotionRequest<'a> {
	pub(crate) invocation: NativeUpgradeInvocation<'a>,
	pub(crate) resolved: &'a ResolvedNativeRuntimeTarget,
	pub(crate) kind: ScopeWriterKind,
	pub(crate) scope: &'a ScopeIdentity,
	pub(crate) participant_budget: ParticipantInspectionBudget,
}

mod policy;
pub(in crate::upgrade) use policy::ScopeWriterKind;

/// Returned only after known owned COMMIT. This is a current postcondition,
/// without historical commit attribution, row coverage, or index readiness.
#[derive(Debug)]
pub(in crate::upgrade) struct ScopeWriterPromotionResult {
	pub(crate) work: UpgradeWork,
	pub(crate) observed: NativeUpgradeObservedControl,
}

impl PgNativeUpgradeParticipants {
	/// Installs one actual intermediate scope policy, preserving all other policies.
	/// No caller transaction, callback, raw control value or permission proof is accepted.
	pub(in crate::upgrade) async fn promote_scope_writer(
		&self,
		mut request: ScopeWriterPromotionRequest<'_>,
	) -> Result<ScopeWriterPromotionResult, NativeUpgradeParticipantsError> {
		request.invocation.require_bindings(
			&self.config.installation_binding,
			&self.config.control_storage,
			&self.config.administration,
		)?;
		if !request
			.invocation
			.target()
			.matches_candidate(request.resolved.candidate())
		{
			return Err(UpgradeWorkError::TargetMismatch.into());
		}
		let registration = request.invocation.prepare_transaction()?;
		run_registered_transaction_write(
			&self.config.control_pool,
			request.kind.transaction_label(),
			registration,
			async |tx| {
				request.invocation.require_transaction(tx)?;
				self.promote_scope_writer_in_transaction(tx, &request).await
			},
		)
		.await
	}

	async fn promote_scope_writer_in_transaction(
		&self,
		tx: &mut WriteTx<'_>,
		request: &ScopeWriterPromotionRequest<'_>,
	) -> Result<ScopeWriterPromotionResult, NativeUpgradeParticipantsError> {
		let deadline = deadline_after_positive_wait(
			self.config.control_pin_wait_timeout,
			NativeUpgradeParticipantsError::InvalidControlPinWaitTimeout,
		)?;
		let interruption = tx.statement_interruption_marker();
		let (source_phase, mut physical) =
			tokio::time::timeout_at(tokio_instant(deadline), async {
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
		let current = decoded.into_state();
		let origin = match route {
			InstallationStorageRoute::NativeFresh => UpgradeSourceOrigin::Fresh,
			InstallationStorageRoute::SourceCoexistence
			| InstallationStorageRoute::SourceRelocation
			| InstallationStorageRoute::NativeImported => UpgradeSourceOrigin::Imported,
		};
		request.resolved.candidate().require_control_target(
			&current,
			&self.config.administration.declaration(),
			self.config.administration.participant_schema(),
			self.config.administration.control_component(),
		)?;
		self.config
			.installation_binding
			.check_scope_policies(current.scope_policies())?;
		if let Some(scopes) = request.resolved.candidate().scope_replacements() {
			self.config
				.installation_binding
				.check_scope_policies(scopes)?;
		}
		let mut work = UpgradeWork::decode_json(
			current
				.pending_upgrade()
				.ok_or(NativeUpgradeParticipantsError::WorkAbsent)?,
		)?;
		if !work.target().matches_target(request.invocation.target()) {
			return Err(UpgradeWorkError::WorkNotCurrent.into());
		}
		let checkpoint = work.checkpoint_context();
		work.require_checkpoint_context(
			&current,
			request.resolved.candidate(),
			origin,
			&checkpoint,
		)?;
		work.require_source_checkpoint_invariants(
			current.component_policies(),
			current.scope_policies(),
		)?;
		physical
			.authenticate_upgrade(&current, &request.invocation)
			.await?;
		let Some(policy) =
			request
				.kind
				.prepare_policy(request.resolved.code(), &current, &work, request.scope)?
		else {
			physical.keep().await?;
			return Ok(ScopeWriterPromotionResult {
				observed: NativeUpgradeObservedControl::new(NativeUpgradeObservedControlFields {
					control: current,
					route,
					backend: parsed_backend,
				}),
				work,
			});
		};
		if matches!(
			work.status(),
			PgUpgradeStatus::Completed | PgUpgradeStatus::Cancelled
		) {
			return Err(UpgradeWorkError::TerminalWork.into());
		}
		let scopes = current
			.scope_policies()
			.with_replaced_scope(&policy)
			.map_err(|source| request.kind.scope_error(request.scope, source))?;
		request.kind.require_invariants(&work, &scopes)?;
		work.require_source_checkpoint_invariants(current.component_policies(), &scopes)?;
		self.require_control_writer(&current)?;
		let administration_declaration = self.config.administration.declaration();
		let inspection = {
			let assessment = ParticipantPolicyAssessment::new(ParticipantPolicyAssessmentFields {
				administration: &administration_declaration,
				installation_id: current.installation_id(),
				components: current.component_policies(),
				scopes: &scopes,
				participant: self.config.administration.participant_schema(),
				control: self.config.administration.control_component(),
			})?;
			physical
				.inspect_participants(ParticipantInspectionFields {
					inspector: self,
					current: &current,
					proposed: &assessment,
					budget: request.participant_budget,
				})
				.await?
		};
		if inspection.coverage != ParticipantInspectionCoverage::Complete
			|| !inspection.deferred.is_empty()
		{
			return Err(request.kind.inspection_error(request.scope, inspection));
		}
		let generations = work.advance_effective_build(
			&current,
			request.resolved.candidate(),
			origin,
			&checkpoint,
		)?;
		let mut replacement = EffectiveControlReplacement::new(current);
		let fields = replacement.fields_mut();
		fields.scope_policies = scopes;
		fields.generation = generations.next;
		fields.pending_upgrade = Some(work.encode_json()?);
		let updated = replacement
			.finish()
			.map_err(UpgradeCompatibilityError::from)?;
		let now = physical.observe_time().await?;
		let record = self.encode_current_control(&updated, now)?;
		physical.write(record).await?;
		Ok(ScopeWriterPromotionResult {
			observed: NativeUpgradeObservedControl::new(NativeUpgradeObservedControlFields {
				control: updated,
				route,
				backend: parsed_backend,
			}),
			work,
		})
	}
}
