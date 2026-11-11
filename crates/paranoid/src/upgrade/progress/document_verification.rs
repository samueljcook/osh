//! One fixed document projection-verification batch under the canonical progress guard.
//!
//! Stored maps compare against the same selected body and metadata. No row is repaired,
//! re-encoded or refetched; a completed checkpoint supplies no catalog or Ready permission.

use crate::runtime::targets::ResolvedNativeRuntimeTarget;

use std::num::NonZeroU32;

use primitives_rs::time::Deadline;

use crate::db::TransactionFailure;
use crate::installation::contract::StateContractError;
use crate::installation::creation::{DocumentIndexLedgerSlot, NativeInstallationError};
use crate::kv::document::postgres::index::managed::{
	PgManagedDocumentIndexBinding, PgManagedDocumentIndexBindingFields, PgManagedDocumentIndexError,
};
use crate::kv::document::schema::IndexContractIdentity;
use crate::kv::document::schema::managed::{ManagedDocumentError, ManagedDocumentRecord};
use crate::records::postgres::scan::{
	RecordMaximumValuedKey, RecordPageError, RecordVerificationPageRequest,
};
use crate::records::{NamespacePageRequest, RecordIdentity};
use crate::runtime::admission::bindings::InstallationAdmissionBindingError;
use crate::runtime::admission::upgrade::{NativeUpgradeAdmissionError, NativeUpgradeInvocation};
use crate::scope::{ScopeError, ScopeIdentity, ScopeOwner};
use crate::upgrade::policy::UpgradeCompatibilityError;
use crate::upgrade::policy::document_stage::{
	BoxedDocumentStageError, DocumentWriterStage, ResolvedDocumentTargets,
};
use crate::upgrade::work::document_checkpoint::{DocumentCheckpointContext, DocumentScanProgress};
use crate::upgrade::work::document_verification::{
	DocumentVerificationBatch, DocumentVerificationExtent, DocumentVerificationProgress,
	DocumentVerificationUpdate,
};
use crate::upgrade::work::{PgUpgradeStatus, UpgradeWork, UpgradeWorkError};

use super::{ControlProgressError, PgControlProgressLock};

pub(in crate::upgrade) struct DocumentVerificationPageFields<'a> {
	pub(crate) invocation: &'a NativeUpgradeInvocation<'a>,
	pub(crate) resolved: &'a ResolvedNativeRuntimeTarget,
	pub(crate) scope: &'a ScopeIdentity,
	pub(crate) index: &'a IndexContractIdentity,
	pub(crate) limit: NonZeroU32,
	pub(crate) deadline: Deadline,
}

/// Tentative until the enclosing owned transaction has a known successful COMMIT.
pub(in crate::upgrade) struct DocumentVerificationPageResult {
	pub(crate) observed: crate::upgrade::observation::NativeUpgradeObservedControl,
	pub(crate) work: UpgradeWork,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum DocumentVerificationRecordError {
	#[error("selected stored representation or its projection failed")]
	Preparation(#[source] Box<ManagedDocumentError>),
	#[error("stored complete projection map differs from its selected-body projection")]
	ProjectionMismatch,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum DocumentVerificationPageError {
	#[error("document verification private participant admission failed")]
	Admission(#[from] NativeUpgradeAdmissionError),
	#[error("document verification progress guard failed")]
	Progress(#[from] ControlProgressError),
	#[error("document verification full work context failed")]
	Work(#[from] UpgradeWorkError),
	#[error("document verification final candidate failed")]
	Candidate(#[from] UpgradeCompatibilityError),
	#[error("document verification current scope failed")]
	Scope(#[from] ScopeError),
	#[error("document verification administrative schema binding failed")]
	Contract(#[from] StateContractError),
	#[error("document verification writer stage failed")]
	Stage(#[source] Box<BoxedDocumentStageError>),
	#[error("document verification native placement failed")]
	Placement(#[from] InstallationAdmissionBindingError),
	#[error("document verification full backend registration failed")]
	Backend(#[source] Box<NativeInstallationError>),
	#[error("document verification index binding failed")]
	IndexBinding(#[source] Box<PgManagedDocumentIndexError>),
	#[error("document verification requires a targeted document scope")]
	ScopeNotTargeted,
	#[error("document verification index is absent from the Ready target")]
	IndexNotTargeted,
	#[error("document verification requires the installed writer stage")]
	WriterStageNotInstalled,
	#[error("document verification requires scanned selected-writer conversion")]
	ConversionNotScanned,
	#[error("document verification checkpoint differs from its installed stage")]
	CheckpointInvariantMismatch,
	#[error("document verification index has no recorded physical association")]
	IndexNotRecorded,
	#[error("document verification requested index is not maintained with its full shape")]
	IndexShapeMismatch,
	#[error("document verification protected-work deadline elapsed")]
	DeadlineElapsed,
	#[error("document verification bounded physical observation failed")]
	Page(#[from] RecordPageError),
	#[error("document verification found an unreadable or inconsistent stored record")]
	Record {
		identity: RecordIdentity,
		#[source]
		source: Box<DocumentVerificationRecordError>,
	},
}

impl DocumentVerificationPageError {
	/// Returns an actual retained driver cause without replacing the primary typed failure.
	pub(crate) fn database_error(&self) -> Option<&crate::db::DatabaseError> {
		match self {
			Self::Admission(error) => error.database_error(),
			Self::Progress(error) => error.database_error(),
			Self::Placement(error) => error.database_error(),
			Self::Page(crate::records::postgres::scan::RecordPageError::Database(error)) => {
				Some(error)
			}
			Self::Page(crate::records::postgres::scan::RecordPageError::Record(error)) => {
				error.database_error()
			}
			Self::Backend(error) => error.database_error(),
			_ => None,
		}
	}

	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::Admission(error) => error.involves_database_failure(),
			Self::Progress(error) => error.involves_database_failure(),
			Self::Placement(error) => error.involves_database_failure(),
			Self::Backend(error) => error.involves_database_failure(),
			Self::Page(RecordPageError::Database(_)) => true,
			Self::Page(RecordPageError::Record(error)) => error.database_error().is_some(),
			_ => false,
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Admission(error) => error.commit_outcome_is_unknown(),
			Self::Progress(error) => error.commit_outcome_is_unknown(),
			Self::Placement(error) => error.commit_outcome_is_unknown(),
			Self::Backend(error) => error.commit_outcome_is_unknown(),
			Self::Page(RecordPageError::Database(error)) => error.commit_outcome_is_unknown(),
			Self::Page(RecordPageError::Record(error)) => error
				.database_error()
				.is_some_and(crate::db::DatabaseError::commit_outcome_is_unknown),
			_ => false,
		}
	}
}

fn require_deadline(deadline: Deadline) -> Result<(), DocumentVerificationPageError> {
	if tokio::time::Instant::now().into_std() >= deadline.instant() {
		Err(DocumentVerificationPageError::DeadlineElapsed)
	} else {
		Ok(())
	}
}

impl PgControlProgressLock<'_, '_> {
	/// Verifies one closed range page and persists only its full Work checkpoint.
	pub(in crate::upgrade) async fn verify_document_page(
		mut self,
		fields: DocumentVerificationPageFields<'_>,
	) -> Result<DocumentVerificationPageResult, DocumentVerificationPageError> {
		let candidate = fields.resolved.candidate();
		let registry = fields.resolved.code().document_registry();
		require_deadline(fields.deadline)?;
		let mut work = UpgradeWork::decode_json(
			self.control
				.pending_upgrade()
				.ok_or(ControlProgressError::WorkAbsent)?,
		)?;
		if !work.target().matches_target(fields.invocation.target()) {
			return Err(UpgradeWorkError::WorkNotCurrent.into());
		}
		let expected = work.checkpoint_context();
		work.require_checkpoint_context(&self.control, candidate, self.origin, &expected)?;
		if matches!(
			work.status(),
			PgUpgradeStatus::Completed | PgUpgradeStatus::Cancelled
		) {
			return Err(UpgradeWorkError::TerminalWork.into());
		}
		work.require_source_checkpoint_invariants(
			self.control.component_policies(),
			self.control.scope_policies(),
		)?;
		work.require_document_verification_invariants(self.control.scope_policies())?;
		let administration = self.progress_storage.administration.declaration();
		candidate.require_control_target(
			&self.control,
			&administration,
			self.progress_storage.administration.participant_schema(),
			self.progress_storage.administration.control_component(),
		)?;
		self.progress_storage
			.installation
			.check_scope_policies(self.control.scope_policies())?;
		if let Some(scopes) = candidate.scope_replacements() {
			self.progress_storage
				.installation
				.check_scope_policies(scopes)?;
		}
		let resolved = ResolvedDocumentTargets::resolve(registry, work.target())
			.map_err(|error| DocumentVerificationPageError::Stage(Box::new(error)))?;
		let target = resolved
			.targets()
			.and_then(|targets| {
				targets
					.iter()
					.find(|target| target.policy().identity() == fields.scope)
			})
			.ok_or(DocumentVerificationPageError::ScopeNotTargeted)?;
		let ScopeOwner::Document(target_document) = target.policy().owner() else {
			return Err(DocumentVerificationPageError::ScopeNotTargeted);
		};
		if !target_document.is_index_ready(fields.index) {
			return Err(DocumentVerificationPageError::IndexNotTargeted);
		}
		let current_scope = self.control.scope_policies().lookup(fields.scope)?;
		let stage = DocumentWriterStage::new(target, current_scope)
			.map_err(|error| DocumentVerificationPageError::Stage(Box::new(error)))?;
		if !stage.matches_policy(current_scope) {
			return Err(DocumentVerificationPageError::WriterStageNotInstalled);
		}

		let conversion = work
			.document_conversion_checkpoint(fields.scope)
			.ok_or(DocumentVerificationPageError::ConversionNotScanned)?;
		if !stage.matches_policy(conversion.invariant())
			|| !matches!(conversion.progress(), DocumentScanProgress::Scanned { .. })
		{
			return Err(DocumentVerificationPageError::ConversionNotScanned);
		}
		let native = self.progress_storage.administration.native_bindings();
		let recorded = self
			.backend
			.document_indexes()
			.iter()
			.find(|descriptor| {
				descriptor.scope() == fields.scope && descriptor.index().identity() == fields.index
			})
			.ok_or(DocumentVerificationPageError::IndexNotRecorded)?;
		let binding = PgManagedDocumentIndexBinding::new(PgManagedDocumentIndexBindingFields {
			registration: target.registration(),
			index: fields.index,
			native,
			index_name: recorded.index_name().clone(),
		})
		.map_err(|error| DocumentVerificationPageError::IndexBinding(Box::new(error)))?;
		if self
			.backend
			.require_document_index_slot(binding.descriptor())
			.map_err(NativeInstallationError::from)
			.map_err(|error| DocumentVerificationPageError::Backend(Box::new(error)))?
			== DocumentIndexLedgerSlot::Absent
		{
			return Err(DocumentVerificationPageError::IndexNotRecorded);
		}
		let ScopeOwner::Document(document) = current_scope.owner() else {
			return Err(DocumentVerificationPageError::ScopeNotTargeted);
		};
		if !document.maintenance_indexes().any(|descriptor| {
			descriptor.identity() == fields.index && descriptor.matches_declaration(binding.index())
		}) {
			return Err(DocumentVerificationPageError::IndexShapeMismatch);
		}
		{
			let (tx, proof) = self.physical.companion_tx_with_proof();
			let contracts = fields
				.invocation
				.native_scope_maintenance_contracts(
					tx,
					crate::runtime::admission::upgrade::HeldUpgradeControl::new(
						proof,
						&self.control,
						self.route,
					),
					candidate,
					stage.policy(),
				)
				.await?;
			contracts.require_document_support()?;
		}
		require_deadline(fields.deadline)?;
		if let Some(checkpoint) = work.document_verification(fields.scope) {
			if !stage.matches_policy(checkpoint.invariant()) {
				return Err(DocumentVerificationPageError::CheckpointInvariantMismatch);
			}
			if matches!(
				checkpoint.progress(),
				DocumentVerificationProgress::Verified { .. }
			) {
				let observed = self.keep_observed().await?;
				require_deadline(fields.deadline)?;
				return Ok(DocumentVerificationPageResult { observed, work });
			}
		}
		let existing = work.document_verification(fields.scope);
		let (extent, after_key) = match existing {
			Some(checkpoint) => {
				let DocumentVerificationProgress::Scanning { after_key } = checkpoint.progress()
				else {
					unreachable!("Verified returns before discovery");
				};
				(checkpoint.extent().cloned(), after_key.cloned())
			}
			None => {
				let maximum = binding
					.storage()
					.read_maximum_valued_key(self.physical.companion_tx(), fields.scope.namespace())
					.await?;
				let extent = match maximum.maximum() {
					RecordMaximumValuedKey::Empty => DocumentVerificationExtent::Empty,
					RecordMaximumValuedKey::Key(key) => DocumentVerificationExtent::Through {
						upper_key: key.clone(),
					},
				};
				(extent, None)
			}
		};
		require_deadline(fields.deadline)?;
		let (update, _inspected, _verified) = match &extent {
			DocumentVerificationExtent::Empty => (
				DocumentVerificationUpdate::Verified { last_key: None },
				0,
				true,
			),
			DocumentVerificationExtent::Through { upper_key } => {
				let request = RecordVerificationPageRequest::new(
					NamespacePageRequest::new(
						fields.scope.namespace().clone(),
						after_key.clone(),
						fields.limit,
					),
					upper_key.clone(),
				)?;
				let page = binding
					.storage()
					.read_verification_page(self.physical.companion_tx(), &request)
					.await?;
				for entry in page.entries() {
					require_deadline(fields.deadline)?;
					let projected = target
						.registration()
						.prepare_selected_writer_projection(
							stage.policy(),
							ManagedDocumentRecord {
								identity: entry.identity(),
								record: entry.record(),
							},
						)
						.map_err(|error| DocumentVerificationPageError::Record {
							identity: entry.identity().clone(),
							source: Box::new(DocumentVerificationRecordError::Preparation(
								Box::new(error),
							)),
						})?;
					require_deadline(fields.deadline)?;
					if projected.index_fields() != entry.record().index_fields() {
						return Err(DocumentVerificationPageError::Record {
							identity: entry.identity().clone(),
							source: Box::new(DocumentVerificationRecordError::ProjectionMismatch),
						});
					}
				}
				let inspected = page.entries().len() as u32;
				let last_key = page
					.entries()
					.last()
					.map(|entry| entry.identity().key().clone())
					.or(after_key);
				let verified =
					inspected < fields.limit.get() || last_key.as_ref() == Some(upper_key);
				let update = if verified {
					DocumentVerificationUpdate::Verified { last_key }
				} else {
					DocumentVerificationUpdate::Advance {
						after_key: last_key.expect("a full positive-budget page has a last key"),
					}
				};
				(update, inspected, verified)
			}
		};
		require_deadline(fields.deadline)?;
		let batch = if existing.is_some() {
			DocumentVerificationBatch::Continue { update }
		} else {
			DocumentVerificationBatch::Start { extent, update }
		};
		work.record_document_verification_batch(
			DocumentCheckpointContext {
				control: &self.control,
				candidate,
				origin: self.origin,
				expected: &expected,
			},
			&stage,
			batch,
		)?;
		let observed = self.persist_work(&work).await?;
		require_deadline(fields.deadline)?;
		Ok(DocumentVerificationPageResult { observed, work })
	}
}
