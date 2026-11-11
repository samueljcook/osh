//! Queue job representation transitions under exclusive control and participant inspection.

use crate::upgrade::observation::{
	NativeUpgradeObservedControl, NativeUpgradeObservedControlFields,
};
use crate::upgrade::participants::ParticipantInspectionFields;
use crate::upgrade::wait::{deadline_after_positive_wait, tokio_instant};

use crate::queue::{
	QueueJobComponent,
	upgrade::{QueueJobPayloadStage, policy_stage, writer_stage},
};
use crate::runtime::admission::upgrade::NativeUpgradeInvocation;
use crate::runtime::targets::ResolvedNativeRuntimeTarget;
use crate::upgrade::policy::UpgradeCompatibilityError;
use crate::upgrade::policy::assessment::{
	ParticipantPolicyAssessment, ParticipantPolicyAssessmentFields,
};
use crate::upgrade::policy::control_replacement::EffectiveControlReplacement;
use crate::upgrade::work::queue_checkpoint::{
	QueueJobCheckpointContext, QueueJobCheckpointError, QueueJobScanProgress,
};
use crate::upgrade::work::{PgUpgradeStatus, UpgradeSourceOrigin, UpgradeWork, UpgradeWorkError};

use super::{
	NativeUpgradeParticipantsError, ParticipantInspectionBudget, ParticipantInspectionCoverage,
	PgNativeUpgradeParticipants,
};
use crate::db::WriteTx;
use crate::db::transaction::run_registered_transaction_write;
use crate::runtime::admission::bindings::InstallationStorageRoute;

pub(in crate::upgrade) struct QueueJobPolicyChangeRequest<'a> {
	pub(crate) invocation: NativeUpgradeInvocation<'a>,
	pub(crate) resolved: &'a ResolvedNativeRuntimeTarget,
	pub(crate) kind: QueueJobPolicyChangeKind,
	pub(crate) participant_budget: ParticipantInspectionBudget,
}

#[derive(Clone, Copy)]
pub(in crate::upgrade) enum QueueJobPolicyChangeKind {
	PromoteWriter,
	FinalizeReaders,
}
impl QueueJobPolicyChangeKind {
	fn transaction_label(self) -> &'static str {
		match self {
			Self::PromoteWriter => "db.native_upgrade_participants.promote_queue_job_writer",
			Self::FinalizeReaders => "db.native_upgrade_participants.finalize_queue_job_readers",
		}
	}
}

/// Returned only after known owned COMMIT. This is a current postcondition,
/// without historical commit attribution, row coverage, or index readiness.
#[derive(Debug)]
pub(in crate::upgrade) struct QueueJobPolicyChangeResult {
	pub(crate) work: UpgradeWork,
	pub(crate) observed: NativeUpgradeObservedControl,
}

impl PgNativeUpgradeParticipants {
	/// Changes the Queue job representation after checking participants and conversion progress.
	/// No caller transaction, callback, raw control value or permission proof is accepted.
	pub(in crate::upgrade) async fn change_queue_job_policy(
		&self,
		mut request: QueueJobPolicyChangeRequest<'_>,
	) -> Result<QueueJobPolicyChangeResult, NativeUpgradeParticipantsError> {
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
				self.change_queue_job_policy_in_transaction(tx, &request)
					.await
			},
		)
		.await
	}

	async fn change_queue_job_policy_in_transaction(
		&self,
		tx: &mut WriteTx<'_>,
		request: &QueueJobPolicyChangeRequest<'_>,
	) -> Result<QueueJobPolicyChangeResult, NativeUpgradeParticipantsError> {
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
		if !matches!(
			route,
			InstallationStorageRoute::NativeFresh | InstallationStorageRoute::NativeImported
		) {
			return Err(NativeUpgradeParticipantsError::QueueJobNativeRouteRequired);
		}
		work.require_queue_job_checkpoint_invariant(current.component_policies())?;
		let installed = current
			.component(QueueJobComponent::component_identifier())
			.ok_or(UpgradeWorkError::from(
				QueueJobCheckpointError::ComponentAbsent,
			))?;
		let target = work.queue_job_target().map_err(UpgradeWorkError::from)?;
		if installed.matches_policy(target) && work.queue_job_checkpoint().is_none() {
			physical.keep().await?;
			return Ok(QueueJobPolicyChangeResult {
				observed: NativeUpgradeObservedControl::new(NativeUpgradeObservedControlFields {
					control: current,
					route,
					backend: parsed_backend,
				}),
				work,
			});
		}
		let policy = match request.kind {
			QueueJobPolicyChangeKind::PromoteWriter => writer_stage(installed, target)?,
			QueueJobPolicyChangeKind::FinalizeReaders => {
				if policy_stage(installed)? != QueueJobPayloadStage::Separating
					|| !work.queue_job_checkpoint().is_some_and(|checkpoint| {
						matches!(checkpoint.progress(), QueueJobScanProgress::Scanned { .. })
					}) {
					return Err(
						UpgradeWorkError::from(QueueJobCheckpointError::ScanIncomplete).into(),
					);
				}
				target.clone()
			}
		};
		if matches!(request.kind, QueueJobPolicyChangeKind::PromoteWriter)
			&& installed.matches_policy(&policy)
			&& work.queue_job_checkpoint().is_some()
		{
			physical.keep().await?;
			return Ok(QueueJobPolicyChangeResult {
				observed: NativeUpgradeObservedControl::new(NativeUpgradeObservedControlFields {
					control: current,
					route,
					backend: parsed_backend,
				}),
				work,
			});
		}
		if matches!(
			work.status(),
			PgUpgradeStatus::Completed | PgUpgradeStatus::Cancelled
		) {
			return Err(UpgradeWorkError::TerminalWork.into());
		}
		let components = current
			.component_policies()
			.iter()
			.map(|existing| {
				if existing.component_identifier() == policy.component_identifier() {
					policy.clone()
				} else {
					existing.clone()
				}
			})
			.collect::<Vec<_>>();
		work.require_source_checkpoint_invariants(&components, current.scope_policies())?;
		self.require_control_writer(&current)?;
		let administration_declaration = self.config.administration.declaration();
		let inspection = {
			let assessment = ParticipantPolicyAssessment::new(ParticipantPolicyAssessmentFields {
				administration: &administration_declaration,
				installation_id: current.installation_id(),
				components: &components,
				scopes: current.scope_policies(),
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
			return Err(NativeUpgradeParticipantsError::QueueJobInspection {
				summary: Box::new(inspection),
			});
		}
		let context = QueueJobCheckpointContext {
			control: &current,
			candidate: request.resolved.candidate(),
			origin,
			expected: &checkpoint,
		};
		let generations = match request.kind {
			QueueJobPolicyChangeKind::PromoteWriter => {
				work.promote_queue_job_writer(context, &policy)?
			}
			QueueJobPolicyChangeKind::FinalizeReaders => {
				work.finalize_queue_job_readers(context)?
			}
		};
		let mut replacement = EffectiveControlReplacement::new(current);
		let fields = replacement.fields_mut();
		fields.component_policies = components;
		fields.generation = generations.next;
		fields.pending_upgrade = Some(work.encode_json()?);
		let updated = replacement
			.finish()
			.map_err(UpgradeCompatibilityError::from)?;
		let now = physical.observe_time().await?;
		let record = self.encode_current_control(&updated, now)?;
		physical.write(record).await?;
		Ok(QueueJobPolicyChangeResult {
			observed: NativeUpgradeObservedControl::new(NativeUpgradeObservedControlFields {
				control: updated,
				route,
				backend: parsed_backend,
			}),
			work,
		})
	}
}
