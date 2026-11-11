//! One fixed physical index registration under enrolled source/shared and native/exclusive pins.

use crate::upgrade::observation::{
	NativeUpgradeObservedControl, NativeUpgradeObservedControlFields,
};
use crate::upgrade::participants::ParticipantInspectionFields;
use crate::upgrade::wait::{deadline_after_positive_wait, require_valid_wait, tokio_instant};

use std::time::Duration;

use crate::db::transaction::run_registered_transaction_write;
use crate::db::{DatabaseError, PgIdentifier, TransactionFailure, WriteTx};
use crate::installation::creation::{DocumentIndexLedgerSlot, NativeInstallationError};
use crate::kv::document::postgres::index::managed::{
	PgManagedDocumentIndexBinding, PgManagedDocumentIndexBindingFields, PgManagedDocumentIndexError,
};
use crate::kv::document::postgres::index::physical::{
	PgDocumentIndexCatalogObservation, PgDocumentIndexPhysicalError,
};
use crate::kv::document::schema::IndexContractIdentity;
use crate::kv::document::schema::managed::ManagedDocumentRegistry;
use crate::runtime::admission::bindings::InstallationStorageRoute;
use crate::runtime::admission::upgrade::NativeUpgradeInvocation;
use crate::scope::{ScopeIdentity, ScopeOwner};
use crate::upgrade::participants::PgDocumentIndexInspectionError;
use crate::upgrade::policy::InstallationUpgradeCandidate;
use crate::upgrade::policy::UpgradeCompatibilityError;
use crate::upgrade::policy::assessment::{
	ParticipantPolicyAssessment, ParticipantPolicyAssessmentFields,
};
use crate::upgrade::policy::control_replacement::EffectiveControlReplacement;
use crate::upgrade::policy::document_stage::ResolvedDocumentTargets;
use crate::upgrade::work::{PgUpgradeStatus, UpgradeSourceOrigin, UpgradeWork, UpgradeWorkError};

use super::{
	NativeUpgradeParticipantsError, ParticipantInspectionBudget, ParticipantInspectionCoverage,
	ParticipantInspectionSummary, PgNativeUpgradeParticipants,
};

pub(in crate::upgrade) struct DocumentIndexRegistrationRequestFields<'a> {
	pub(crate) invocation: NativeUpgradeInvocation<'a>,
	pub(crate) final_candidate: &'a InstallationUpgradeCandidate,
	pub(crate) registry: &'a ManagedDocumentRegistry,
	pub(crate) scope: &'a ScopeIdentity,
	pub(crate) index: &'a IndexContractIdentity,
	pub(crate) index_name: PgIdentifier,
	pub(crate) participant_budget: ParticipantInspectionBudget,
	pub(crate) registration_wait_timeout: Duration,
}

/// The wait covers protected work after owned BEGIN, not acquisition, BEGIN or ending.
pub(in crate::upgrade) struct DocumentIndexRegistrationRequest<'a> {
	fields: DocumentIndexRegistrationRequestFields<'a>,
}
impl<'a> DocumentIndexRegistrationRequest<'a> {
	pub(crate) fn new(
		fields: DocumentIndexRegistrationRequestFields<'a>,
	) -> Result<Self, DocumentIndexRegistrationError> {
		require_valid_wait(
			fields.registration_wait_timeout,
			DocumentIndexRegistrationError::InvalidWaitTimeout,
		)?;
		Ok(Self { fields })
	}
}

/// Only known owned COMMIT publishes either result. These are physical observations,
/// without coverage, Ready, admission or attribution to an earlier uncertain commit.
#[derive(Debug)]
pub(in crate::upgrade) enum DocumentIndexRegistrationResult {
	Registered {
		work: UpgradeWork,
		observed: NativeUpgradeObservedControl,
	},
	AlreadyRegistered {
		work: UpgradeWork,
		observed: NativeUpgradeObservedControl,
	},
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum DocumentIndexRegistrationError {
	#[error("document index registration wait must be positive and fit the monotonic clock")]
	InvalidWaitTimeout,
	#[error(
		"document index protected registration wait elapsed with statement completion unconfirmed"
	)]
	WaitElapsed,
	#[error("document index binding does not resolve code and placement")]
	Binding(#[source] Box<PgManagedDocumentIndexError>),
	#[error("document index physical catalog or expression failed")]
	Physical(#[source] Box<PgDocumentIndexPhysicalError>),
	#[error("document index complete backend registration failed")]
	Backend(#[source] Box<NativeInstallationError>),
	#[error("document index container validation failed")]
	StorageValidation(#[source] Box<NativeInstallationError>),
	#[error("document index current scope is not owned by its document schema")]
	CurrentOwnerMismatch,
	#[error("document index is absent from the pending maintenance target")]
	IndexNotTargeted,
	#[error("document index is absent from current installed maintenance")]
	IndexNotMaintained,
	#[error("document index current maintenance differs from its ordered shape")]
	CurrentShapeMismatch,
	#[error("document index current Ready reference has no recorded ledger entry")]
	ReadyLedgerAbsent,
	#[error("document index recorded physical relation is absent")]
	RecordedIndexAbsent,
	#[error("document index requested name is occupied without its ledger entry")]
	UnregisteredNameOccupied,
	#[error("document index created relation is absent during same-transaction inspection")]
	CreatedIndexAbsent,
	#[error(
		"document index registration requires a complete compatible current-policy participant inspection"
	)]
	ParticipantInspection {
		summary: Box<ParticipantInspectionSummary>,
	},
}
impl DocumentIndexRegistrationError {
	pub(crate) fn physical(error: PgDocumentIndexPhysicalError) -> Self {
		Self::Physical(Box::new(error))
	}
	fn binding(error: PgManagedDocumentIndexError) -> Self {
		Self::Binding(Box::new(error))
	}
	fn backend(error: NativeInstallationError) -> Self {
		Self::Backend(Box::new(error))
	}
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Physical(error) => error.database_error(),
			Self::Backend(error) => error.database_error(),
			Self::StorageValidation(error) => error.database_error(),
			_ => None,
		}
	}
	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::Backend(error) => error.involves_database_failure(),
			Self::StorageValidation(error) => error.involves_database_failure(),
			_ => self.database_error().is_some(),
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Backend(error) => error.commit_outcome_is_unknown(),
			Self::StorageValidation(error) => error.commit_outcome_is_unknown(),
			_ => self
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
		}
	}
}
impl From<DocumentIndexRegistrationError> for NativeUpgradeParticipantsError {
	fn from(error: DocumentIndexRegistrationError) -> Self {
		Self::DocumentIndex(Box::new(error))
	}
}

impl From<PgDocumentIndexInspectionError> for DocumentIndexRegistrationError {
	fn from(error: PgDocumentIndexInspectionError) -> Self {
		match error {
			PgDocumentIndexInspectionError::StorageValidation(error) => {
				Self::StorageValidation(error)
			}
			PgDocumentIndexInspectionError::Physical(error) => Self::Physical(error),
		}
	}
}

impl PgNativeUpgradeParticipants {
	/// Creates and records one exact physical index atomically, preserving all logical policies.
	pub(in crate::upgrade) async fn register_document_index(
		&self,
		mut request: DocumentIndexRegistrationRequest<'_>,
	) -> Result<DocumentIndexRegistrationResult, NativeUpgradeParticipantsError> {
		request.fields.invocation.require_bindings(
			&self.config.installation_binding,
			&self.config.control_storage,
			&self.config.administration,
		)?;
		let registration = request.fields.invocation.prepare_transaction()?;
		run_registered_transaction_write(
			&self.config.control_pool,
			"db.native_upgrade_participants.register_document_index",
			registration,
			async |tx| {
				request.fields.invocation.require_transaction(tx)?;
				let deadline = deadline_after_positive_wait(
					request.fields.registration_wait_timeout,
					DocumentIndexRegistrationError::InvalidWaitTimeout,
				)?;
				let interruption = tx.statement_interruption_marker();
				tokio::time::timeout_at(
					tokio_instant(deadline),
					self.register_document_index_in_transaction(tx, &request.fields),
				)
				.await
				.map_err(|_| {
					interruption.mark_interrupted();
					DocumentIndexRegistrationError::WaitElapsed
				})?
			},
		)
		.await
	}

	async fn register_document_index_in_transaction(
		&self,
		tx: &mut WriteTx<'_>,
		request: &DocumentIndexRegistrationRequestFields<'_>,
	) -> Result<DocumentIndexRegistrationResult, NativeUpgradeParticipantsError> {
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
		let declaration = self.config.administration.declaration();
		request.final_candidate.require_control_target(
			&current,
			&declaration,
			self.config.administration.participant_schema(),
			self.config.administration.control_component(),
		)?;
		self.config
			.installation_binding
			.check_scope_policies(current.scope_policies())?;
		if let Some(scopes) = request.final_candidate.scope_replacements() {
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
		work.require_checkpoint_context(&current, request.final_candidate, origin, &checkpoint)?;
		work.require_source_checkpoint_invariants(
			current.component_policies(),
			current.scope_policies(),
		)?;
		self.require_control_writer(&current)?;
		physical
			.authenticate_upgrade(&current, &request.invocation)
			.await?;

		let targets = ResolvedDocumentTargets::resolve(request.registry, work.target())
			.map_err(|error| NativeUpgradeParticipantsError::DocumentStage(Box::new(error)))?;
		let target = targets
			.targets()
			.and_then(|all| {
				all.iter()
					.find(|target| target.policy().identity() == request.scope)
			})
			.ok_or_else(
				|| NativeUpgradeParticipantsError::DocumentScopeNotTargeted {
					scope: request.scope.clone(),
				},
			)?;
		let ScopeOwner::Document(target_document) = target.policy().owner() else {
			return Err(DocumentIndexRegistrationError::IndexNotTargeted.into());
		};
		if !target_document
			.maintenance_indexes()
			.any(|index| index.identity() == request.index)
		{
			return Err(DocumentIndexRegistrationError::IndexNotTargeted.into());
		}
		let current_scope = current
			.scope_policies()
			.lookup(request.scope)
			.map_err(|source| NativeUpgradeParticipantsError::DocumentScope {
				scope: request.scope.clone(),
				source,
			})?;
		let ScopeOwner::Document(document) = current_scope.owner() else {
			return Err(DocumentIndexRegistrationError::CurrentOwnerMismatch.into());
		};
		if document.schema().identifier() != target.registration().schema_identifier() {
			return Err(DocumentIndexRegistrationError::CurrentOwnerMismatch.into());
		}
		let native = self.config.administration.native_bindings();
		let binding = PgManagedDocumentIndexBinding::new(PgManagedDocumentIndexBindingFields {
			registration: target.registration(),
			index: request.index,
			native,
			index_name: request.index_name.clone(),
		})
		.map_err(DocumentIndexRegistrationError::binding)?;
		let maintained = document
			.maintenance_indexes()
			.find(|index| index.identity() == request.index)
			.ok_or(DocumentIndexRegistrationError::IndexNotMaintained)?;
		if !maintained.matches_declaration(binding.index()) {
			return Err(DocumentIndexRegistrationError::CurrentShapeMismatch.into());
		}
		let administration_policy = current
			.component(declaration.identifier())
			.ok_or(NativeUpgradeParticipantsError::AdministrativePolicyAbsent)?;
		let administrative = declaration.bind(administration_policy)?;
		let backend =
			administrative.bind_schema(self.config.administration.backend_schema().schema())?;
		let slot = parsed_backend
			.require_document_index_slot(binding.descriptor())
			.map_err(NativeInstallationError::from)
			.map_err(DocumentIndexRegistrationError::backend)?;
		if slot == DocumentIndexLedgerSlot::Absent && document.is_index_ready(request.index) {
			return Err(DocumentIndexRegistrationError::ReadyLedgerAbsent.into());
		}
		let observation = physical
			.inspect_document_index(&binding)
			.await
			.map_err(DocumentIndexRegistrationError::from)?;
		let capacity = match (slot, observation) {
			(
				DocumentIndexLedgerSlot::RecordedMatching { .. },
				PgDocumentIndexCatalogObservation::Present,
			) => {
				physical.keep().await?;
				return Ok(DocumentIndexRegistrationResult::AlreadyRegistered {
					observed: NativeUpgradeObservedControl::new(
						NativeUpgradeObservedControlFields {
							control: current,
							route,
							backend: parsed_backend,
						},
					),
					work,
				});
			}
			(
				DocumentIndexLedgerSlot::RecordedMatching { .. },
				PgDocumentIndexCatalogObservation::Absent { .. },
			) => return Err(DocumentIndexRegistrationError::RecordedIndexAbsent.into()),
			(DocumentIndexLedgerSlot::Absent, PgDocumentIndexCatalogObservation::Present) => {
				return Err(DocumentIndexRegistrationError::UnregisteredNameOccupied.into());
			}
			(
				DocumentIndexLedgerSlot::Absent,
				PgDocumentIndexCatalogObservation::Absent { capacity },
			) => capacity,
		};
		if matches!(
			work.status(),
			PgUpgradeStatus::Completed | PgUpgradeStatus::Cancelled
		) {
			return Err(UpgradeWorkError::TerminalWork.into());
		}
		let updated_backend = native
			.encode_document_index_registration(
				&current,
				&backend,
				parsed_backend,
				binding.descriptor(),
			)
			.map_err(DocumentIndexRegistrationError::backend)?;
		let (updated_backend, parsed_backend) = updated_backend.into_parts();
		let inspection = {
			let assessment = ParticipantPolicyAssessment::new(ParticipantPolicyAssessmentFields {
				administration: &declaration,
				installation_id: current.installation_id(),
				components: current.component_policies(),
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
			return Err(DocumentIndexRegistrationError::ParticipantInspection {
				summary: Box::new(inspection),
			}
			.into());
		}
		physical
			.create_and_inspect_document_index(&binding, &capacity)
			.await?;
		let generations =
			work.advance_effective_build(&current, request.final_candidate, origin, &checkpoint)?;
		let mut replacement = EffectiveControlReplacement::new(current);
		let fields = replacement.fields_mut();
		fields.generation = generations.next;
		fields.pending_upgrade = Some(work.encode_json()?);
		fields.backend_registration = updated_backend;
		let updated = replacement
			.finish()
			.map_err(UpgradeCompatibilityError::from)?;
		let now = physical.observe_time().await?;
		let record = self.encode_current_control(&updated, now)?;
		physical.write(record).await?;
		Ok(DocumentIndexRegistrationResult::Registered {
			observed: NativeUpgradeObservedControl::new(NativeUpgradeObservedControlFields {
				control: updated,
				route,
				backend: parsed_backend,
			}),
			work,
		})
	}
}
