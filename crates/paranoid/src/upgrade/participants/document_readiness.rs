//! Fixed exclusive publication of one actually verified document Ready index.
//!
//! Durable full-map verification, real catalog inspection and one compatible inspection are
//! separate required inputs. This operation performs no document scan, projection or DDL.

use crate::runtime::targets::ResolvedNativeRuntimeTarget;
use crate::upgrade::wait::{
	deadline_after_positive_wait, require_before_deadline, require_valid_wait, tokio_instant,
};
use primitives_rs::time::Deadline;

use crate::upgrade::observation::{
	NativeUpgradeObservedControl, NativeUpgradeObservedControlFields,
};
use crate::upgrade::participants::ParticipantInspectionFields;

use std::{sync::Arc, time::Duration};

use crate::db::transaction::run_registered_transaction_write;
use crate::db::{DatabaseError, TransactionFailure, WriteTx};
use crate::installation::creation::{DocumentIndexLedgerSlot, NativeInstallationError};
use crate::kv::document::postgres::index::managed::{
	PgManagedDocumentIndexBinding, PgManagedDocumentIndexBindingFields, PgManagedDocumentIndexError,
};
use crate::kv::document::postgres::index::physical::PgDocumentIndexCatalogObservation;
use crate::kv::document::schema::IndexContractIdentity;
use crate::runtime::admission::bindings::InstallationStorageRoute;
use crate::runtime::admission::upgrade::NativeUpgradeInvocation;
use crate::scope::{ScopeIdentity, ScopeOwner};
use crate::upgrade::participants::PgDocumentIndexInspectionError;
use crate::upgrade::policy::UpgradeCompatibilityError;
use crate::upgrade::policy::assessment::{
	ParticipantPolicyAssessment, ParticipantPolicyAssessmentFields,
};
use crate::upgrade::policy::control_replacement::EffectiveControlReplacement;
use crate::upgrade::policy::document_readiness::{
	DocumentReadinessStage, DocumentReadinessStageError,
};
use crate::upgrade::policy::document_stage::{
	BoxedDocumentStageError, DocumentWriterStage, ResolvedDocumentTargets,
};
use crate::upgrade::work::document_checkpoint::{DocumentCheckpointContext, DocumentScanProgress};
use crate::upgrade::work::document_verification::DocumentVerificationProgress;
use crate::upgrade::work::{PgUpgradeStatus, UpgradeSourceOrigin, UpgradeWork, UpgradeWorkError};

use super::{
	NativeUpgradeParticipantsError, ParticipantInspectionBudget, ParticipantInspectionCoverage,
	ParticipantInspectionSummary, PgNativeUpgradeParticipants,
};

pub(in crate::upgrade) struct DocumentReadyRequestFields<'a> {
	pub(crate) invocation: NativeUpgradeInvocation<'a>,
	pub(crate) resolved: &'a ResolvedNativeRuntimeTarget,
	pub(crate) scope: &'a ScopeIdentity,
	pub(crate) index: &'a IndexContractIdentity,
	pub(crate) participant_budget: ParticipantInspectionBudget,
	/// Bounds protected awaited work after owned BEGIN, excluding owned ending.
	pub(crate) publication_wait_timeout: Duration,
}

pub(in crate::upgrade) struct DocumentReadyRequest<'a> {
	fields: DocumentReadyRequestFields<'a>,
}
impl<'a> DocumentReadyRequest<'a> {
	pub(crate) fn new(fields: DocumentReadyRequestFields<'a>) -> Result<Self, DocumentReadyError> {
		require_valid_wait(
			fields.publication_wait_timeout,
			DocumentReadyError::InvalidWaitTimeout,
		)?;
		Ok(Self { fields })
	}
}

/// Returned only after this exact owned transaction has known successful COMMIT.
/// AlreadyReady observes a checked current postcondition without historical commit attribution.
#[derive(Debug)]
pub(in crate::upgrade) enum DocumentReadyResult {
	Published {
		work: UpgradeWork,
		observed: NativeUpgradeObservedControl,
	},
	AlreadyReady {
		work: UpgradeWork,
		observed: NativeUpgradeObservedControl,
	},
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum DocumentReadyError {
	#[error("document Ready resolved code or target differs from its invocation")]
	BindingMismatch,
	#[error("document Ready publication wait must be positive and fit the local clock")]
	InvalidWaitTimeout,
	#[error("document Ready protected publication deadline elapsed")]
	DeadlineElapsed,
	#[error("document Ready awaited work timed out with statement completion unconfirmed")]
	WaitElapsed,
	#[error("document Ready requires a targeted document scope")]
	ScopeNotTargeted,
	#[error("document Ready index is absent from the final Ready target")]
	IndexNotTargeted,
	#[error("document Ready current writer stage failed")]
	WriterStage(#[source] Box<BoxedDocumentStageError>),
	#[error("document Ready single-index policy stage failed")]
	ReadyStage(#[source] Box<DocumentReadinessStageError>),
	#[error("document Ready requires the installed selected writer stage")]
	WriterStageNotInstalled,
	#[error("document Ready requires current Scanned and Verified checkpoints")]
	VerificationNotCurrent,
	#[error("document Ready index has no recorded physical association")]
	IndexNotRecorded,
	#[error("document Ready index is not maintained with its full declared shape")]
	IndexShapeMismatch,
	#[error("document Ready recorded index is physically absent")]
	RecordedIndexAbsent,
	#[error("document Ready backend association failed")]
	Backend(#[source] Box<NativeInstallationError>),
	#[error("document Ready index binding failed")]
	Binding(#[source] Box<PgManagedDocumentIndexError>),
	#[error("document Ready physical inspection failed")]
	Inspection(#[source] Box<PgDocumentIndexInspectionError>),
	#[error(
		"document Ready requires a complete compatible prospective-policy participant inspection"
	)]
	ParticipantInspection {
		summary: Box<ParticipantInspectionSummary>,
	},
}
impl DocumentReadyError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Backend(error) => error.database_error(),
			Self::Inspection(error) => error.database_error(),
			_ => None,
		}
	}
	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::Backend(error) => error.involves_database_failure(),
			Self::Inspection(error) => error.involves_database_failure(),
			_ => false,
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Backend(error) => error.commit_outcome_is_unknown(),
			Self::Inspection(error) => error.commit_outcome_is_unknown(),
			_ => false,
		}
	}
}
impl From<DocumentReadyError> for NativeUpgradeParticipantsError {
	fn from(error: DocumentReadyError) -> Self {
		Self::DocumentReadiness(Box::new(error))
	}
}

impl PgNativeUpgradeParticipants {
	/// Publishes one requested Ready reference after same-transaction physical/inspection checks.
	/// It never scans document bodies, reruns projectors, creates or repairs an index.
	pub(in crate::upgrade) async fn publish_document_ready(
		&self,
		mut request: DocumentReadyRequest<'_>,
	) -> Result<DocumentReadyResult, NativeUpgradeParticipantsError> {
		request.fields.invocation.require_bindings(
			&self.config.installation_binding,
			&self.config.control_storage,
			&self.config.administration,
		)?;
		if !Arc::ptr_eq(
			request.fields.resolved.code().administration(),
			&self.config.administration,
		) || !Arc::ptr_eq(
			request.fields.resolved.code().installation(),
			&self.config.installation_binding,
		) || !request
			.fields
			.resolved
			.target()
			.matches_target(request.fields.invocation.target())
		{
			return Err(DocumentReadyError::BindingMismatch.into());
		}

		let registration = request.fields.invocation.prepare_transaction()?;
		run_registered_transaction_write(
			&self.config.control_pool,
			"db.native_upgrade_participants.publish_document_ready",
			registration,
			async |tx| {
				request.fields.invocation.require_transaction(tx)?;
				let deadline = deadline_after_positive_wait(
					request.fields.publication_wait_timeout,
					DocumentReadyError::InvalidWaitTimeout,
				)?;
				let interruption = tx.statement_interruption_marker();
				tokio::time::timeout_at(
					tokio_instant(deadline),
					self.publish_document_ready_in_transaction(tx, &request.fields, deadline),
				)
				.await
				.map_err(|_| {
					interruption.mark_interrupted();
					DocumentReadyError::WaitElapsed
				})?
			},
		)
		.await
	}

	async fn publish_document_ready_in_transaction(
		&self,
		tx: &mut WriteTx<'_>,
		request: &DocumentReadyRequestFields<'_>,
		deadline: Deadline,
	) -> Result<DocumentReadyResult, NativeUpgradeParticipantsError> {
		let final_candidate = request.resolved.candidate();
		let registry = request.resolved.code().document_registry();
		require_before_deadline(deadline, DocumentReadyError::DeadlineElapsed)?;
		let pin_deadline = deadline_after_positive_wait(
			self.config.control_pin_wait_timeout,
			NativeUpgradeParticipantsError::InvalidControlPinWaitTimeout,
		)?;
		let interruption = tx.statement_interruption_marker();
		let (source_phase, mut physical) =
			tokio::time::timeout_at(tokio_instant(pin_deadline), async {
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
		let current = decoded.into_state();
		let origin = match route {
			InstallationStorageRoute::NativeFresh => UpgradeSourceOrigin::Fresh,
			InstallationStorageRoute::SourceCoexistence
			| InstallationStorageRoute::SourceRelocation
			| InstallationStorageRoute::NativeImported => UpgradeSourceOrigin::Imported,
		};
		let mut work = UpgradeWork::decode_json(
			current
				.pending_upgrade()
				.ok_or(NativeUpgradeParticipantsError::WorkAbsent)?,
		)?;
		if !work.target().matches_target(request.invocation.target()) {
			return Err(UpgradeWorkError::WorkNotCurrent.into());
		}
		let expected = work.checkpoint_context();
		work.require_checkpoint_context(&current, final_candidate, origin, &expected)?;
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
		work.require_document_verification_invariants(current.scope_policies())?;
		let administration = self.config.administration.declaration();
		final_candidate.require_control_target(
			&current,
			&administration,
			self.config.administration.participant_schema(),
			self.config.administration.control_component(),
		)?;
		self.config
			.installation_binding
			.check_scope_policies(current.scope_policies())?;
		if let Some(scopes) = final_candidate.scope_replacements() {
			self.config
				.installation_binding
				.check_scope_policies(scopes)?;
		}
		self.require_control_writer(&current)?;
		let resolved = ResolvedDocumentTargets::resolve(registry, work.target())
			.map_err(|error| DocumentReadyError::WriterStage(Box::new(error)))?;
		let target = resolved
			.targets()
			.and_then(|targets| {
				targets
					.iter()
					.find(|target| target.policy().identity() == request.scope)
			})
			.ok_or(DocumentReadyError::ScopeNotTargeted)?;
		let ScopeOwner::Document(target_document) = target.policy().owner() else {
			return Err(DocumentReadyError::ScopeNotTargeted.into());
		};
		if !target_document.is_index_ready(request.index) {
			return Err(DocumentReadyError::IndexNotTargeted.into());
		}
		let current_scope = current
			.scope_policies()
			.lookup(request.scope)
			.map_err(|source| NativeUpgradeParticipantsError::DocumentScope {
				scope: request.scope.clone(),
				source,
			})?;
		let stage = DocumentWriterStage::new(target, current_scope)
			.map_err(|error| DocumentReadyError::WriterStage(Box::new(error)))?;
		if !stage.matches_policy(current_scope) {
			return Err(DocumentReadyError::WriterStageNotInstalled.into());
		}
		let conversion = work
			.document_conversion_checkpoint(request.scope)
			.ok_or(DocumentReadyError::VerificationNotCurrent)?;
		let verification = work
			.document_verification(request.scope)
			.ok_or(DocumentReadyError::VerificationNotCurrent)?;
		if !stage.matches_policy(conversion.invariant())
			|| !matches!(conversion.progress(), DocumentScanProgress::Scanned { .. })
			|| !stage.matches_policy(verification.invariant())
			|| !matches!(
				verification.progress(),
				DocumentVerificationProgress::Verified { .. }
			) {
			return Err(DocumentReadyError::VerificationNotCurrent.into());
		}
		let native = self.config.administration.native_bindings();
		let recorded = parsed_backend
			.document_indexes()
			.iter()
			.find(|descriptor| {
				descriptor.scope() == request.scope
					&& descriptor.index().identity() == request.index
			})
			.ok_or(DocumentReadyError::IndexNotRecorded)?;
		let binding = PgManagedDocumentIndexBinding::new(PgManagedDocumentIndexBindingFields {
			registration: target.registration(),
			index: request.index,
			native,
			index_name: recorded.index_name().clone(),
		})
		.map_err(|error| DocumentReadyError::Binding(Box::new(error)))?;
		if parsed_backend
			.require_document_index_slot(binding.descriptor())
			.map_err(NativeInstallationError::from)
			.map_err(|error| DocumentReadyError::Backend(Box::new(error)))?
			== DocumentIndexLedgerSlot::Absent
		{
			return Err(DocumentReadyError::IndexNotRecorded.into());
		}
		let ScopeOwner::Document(document) = current_scope.owner() else {
			return Err(DocumentReadyError::ScopeNotTargeted.into());
		};
		if !document.maintenance_indexes().any(|descriptor| {
			descriptor.identity() == request.index
				&& descriptor.matches_declaration(binding.index())
		}) {
			return Err(DocumentReadyError::IndexShapeMismatch.into());
		}
		physical
			.authenticate_document_readiness(
				&current,
				&request.invocation,
				route,
				final_candidate,
				stage.policy(),
			)
			.await?;

		require_before_deadline(deadline, DocumentReadyError::DeadlineElapsed)?;
		let observation = physical
			.inspect_document_index(&binding)
			.await
			.map_err(|error| DocumentReadyError::Inspection(Box::new(error)))?;
		if !matches!(observation, PgDocumentIndexCatalogObservation::Present) {
			return Err(DocumentReadyError::RecordedIndexAbsent.into());
		}
		require_before_deadline(deadline, DocumentReadyError::DeadlineElapsed)?;
		if document.is_index_ready(request.index) {
			physical.keep().await?;
			require_before_deadline(deadline, DocumentReadyError::DeadlineElapsed)?;
			return Ok(DocumentReadyResult::AlreadyReady {
				observed: NativeUpgradeObservedControl::new(NativeUpgradeObservedControlFields {
					control: current,
					route,
					backend: parsed_backend,
				}),
				work,
			});
		}
		let readiness = DocumentReadinessStage::new(target, current_scope, request.index)
			.map_err(|error| DocumentReadyError::ReadyStage(Box::new(error)))?;
		let scopes = current
			.scope_policies()
			.with_replaced_scope(readiness.policy())
			.map_err(|source| NativeUpgradeParticipantsError::DocumentScope {
				scope: request.scope.clone(),
				source,
			})?;
		let inspection = {
			let assessment = ParticipantPolicyAssessment::new(ParticipantPolicyAssessmentFields {
				administration: &administration,
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
			return Err(DocumentReadyError::ParticipantInspection {
				summary: Box::new(inspection),
			}
			.into());
		}
		require_before_deadline(deadline, DocumentReadyError::DeadlineElapsed)?;
		let generations = work.record_verified_document_readiness(
			DocumentCheckpointContext {
				control: &current,
				candidate: final_candidate,
				origin,
				expected: &expected,
			},
			&readiness,
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
		require_before_deadline(deadline, DocumentReadyError::DeadlineElapsed)?;
		physical.write(record).await?;
		require_before_deadline(deadline, DocumentReadyError::DeadlineElapsed)?;
		Ok(DocumentReadyResult::Published {
			observed: NativeUpgradeObservedControl::new(NativeUpgradeObservedControlFields {
				control: updated,
				route,
				backend: parsed_backend,
			}),
			work,
		})
	}
}
