//! Fixed selected-writer conversion under the actual progress guard transaction.
//!
//! A completed page grants no final coverage, source retirement or query readiness.

use crate::runtime::targets::ResolvedNativeRuntimeTarget;

use std::{collections::BTreeSet, num::NonZeroU32};

use crate::runtime::admission::upgrade::{NativeUpgradeAdmissionError, NativeUpgradeInvocation};

use crate::kv::document::schema::managed::{ManagedDocumentError, ManagedDocumentRecord};
use crate::records::postgres::HeldNativeRow;
use crate::records::{NamespacePageRequest, RecordIdentity};
use crate::scope::{ScopeError, ScopeIdentity};
use crate::upgrade::policy::UpgradeCompatibilityError;
use crate::upgrade::policy::document_stage::{
	BoxedDocumentStageError, DocumentWriterStage, ResolvedDocumentTargets,
};
use crate::upgrade::work::document_checkpoint::{
	DocumentCheckpointContext, DocumentScanProgress, DocumentScanUpdate,
};
use crate::upgrade::work::{PgUpgradeStatus, UpgradeWork, UpgradeWorkError};

use super::{ControlProgressError, PgControlProgressLock};
use crate::records::postgres::StateStorageError;
use crate::records::postgres::scan::RecordPageError;

pub(in crate::upgrade) struct DocumentConversionPageFields<'a> {
	pub(crate) invocation: &'a NativeUpgradeInvocation<'a>,
	pub(crate) resolved: &'a ResolvedNativeRuntimeTarget,
	pub(crate) scope: &'a ScopeIdentity,
	pub(crate) limit: NonZeroU32,
	pub(crate) conflict_attempts: NonZeroU32,
}

/// This result is tentative until the enclosing owned transaction has a known COMMIT.
pub(in crate::upgrade) struct DocumentConversionPageResult {
	pub(crate) observed: crate::upgrade::observation::NativeUpgradeObservedControl,
	pub(crate) work: UpgradeWork,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum DocumentConversionPageError {
	#[error("document conversion private participant admission failed")]
	Admission(#[from] NativeUpgradeAdmissionError),
	#[error("document conversion progress guard failed")]
	Progress(#[from] ControlProgressError),
	#[error("document conversion full work context failed")]
	Work(#[from] UpgradeWorkError),
	#[error("document conversion final candidate failed")]
	Candidate(#[from] UpgradeCompatibilityError),
	#[error("document conversion current scope failed")]
	Scope(#[from] ScopeError),
	#[error("document conversion target or writer stage failed")]
	Stage(#[source] Box<BoxedDocumentStageError>),
	#[error("document conversion selected scope is absent from the complete target")]
	ScopeNotTargeted,
	#[error("document conversion writer stage is not the current installed policy")]
	WriterStageNotInstalled,
	#[error("document conversion checkpoint invariant differs from the current writer stage")]
	CheckpointInvariantMismatch,
	#[error("document conversion native placement failed")]
	Placement(#[from] crate::runtime::admission::bindings::InstallationAdmissionBindingError),
	#[error("document conversion physical discovery failed")]
	Page(#[from] RecordPageError),
	#[error("document conversion canonical acquisition or write failed")]
	Storage(#[from] StateStorageError),
	#[error("document conversion reader or selected producer failed")]
	Preparation(#[source] Box<ManagedDocumentError>),
}

impl DocumentConversionPageError {
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
			Self::Storage(error) => error.database_error(),
			_ => None,
		}
	}

	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::Admission(error) => error.involves_database_failure(),
			Self::Progress(error) => error.involves_database_failure(),
			Self::Placement(error) => error.involves_database_failure(),
			Self::Storage(error) | Self::Page(RecordPageError::Record(error)) => {
				error.database_error().is_some()
			}
			Self::Page(RecordPageError::Database(_)) => true,
			_ => false,
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Admission(error) => error.commit_outcome_is_unknown(),
			Self::Progress(error) => error.commit_outcome_is_unknown(),
			Self::Placement(error) => error.commit_outcome_is_unknown(),
			Self::Storage(error) | Self::Page(RecordPageError::Record(error)) => error
				.database_error()
				.is_some_and(crate::db::DatabaseError::commit_outcome_is_unknown),
			Self::Page(RecordPageError::Database(error)) => error.commit_outcome_is_unknown(),
			_ => false,
		}
	}
}

impl PgControlProgressLock<'_, '_> {
	/// Converts exactly one discovered canonical set and writes its progress in this transaction.
	/// No transaction, callback, arbitrary encoded control write or application value escapes.
	pub(in crate::upgrade) async fn convert_document_page(
		mut self,
		fields: DocumentConversionPageFields<'_>,
	) -> Result<DocumentConversionPageResult, DocumentConversionPageError> {
		let candidate = fields.resolved.candidate();
		let registry = fields.resolved.code().document_registry();
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
		work.require_source_checkpoint_invariants(
			self.control.component_policies(),
			self.control.scope_policies(),
		)?;
		if matches!(
			work.status(),
			PgUpgradeStatus::Completed | PgUpgradeStatus::Cancelled
		) {
			return Err(UpgradeWorkError::TerminalWork.into());
		}
		fields
			.invocation
			.authenticate_current(self.physical.companion_tx(), &self.control)
			.await?;
		candidate.require_control_target(
			&self.control,
			&self.progress_storage.administration.declaration(),
			self.progress_storage.administration.participant_schema(),
			self.progress_storage.administration.control_component(),
		)?;
		if let Some(scopes) = candidate.scope_replacements() {
			self.progress_storage
				.installation
				.check_scope_policies(scopes)?;
		}
		self.require_conversion_writer()?;
		let resolved = ResolvedDocumentTargets::resolve(registry, work.target())
			.map_err(|error| DocumentConversionPageError::Stage(Box::new(error)))?;
		let target = resolved
			.targets()
			.and_then(|targets| {
				targets
					.iter()
					.find(|target| target.policy().identity() == fields.scope)
			})
			.ok_or(DocumentConversionPageError::ScopeNotTargeted)?;
		let current = self.control.scope_policies().lookup(fields.scope)?;
		let stage = DocumentWriterStage::new(target, current)
			.map_err(|error| DocumentConversionPageError::Stage(Box::new(error)))?;
		if !stage.matches_policy(current) {
			return Err(DocumentConversionPageError::WriterStageNotInstalled);
		}

		let storage = self
			.progress_storage
			.installation
			.app_storage(fields.scope)?;
		let checkpoint = work.document_conversion_checkpoint(fields.scope);
		if let Some(checkpoint) = checkpoint {
			if !stage.matches_policy(checkpoint.invariant()) {
				return Err(DocumentConversionPageError::CheckpointInvariantMismatch);
			}
			if matches!(checkpoint.progress(), DocumentScanProgress::Scanned { .. }) {
				let observed = self.keep_observed().await?;
				return Ok(DocumentConversionPageResult { observed, work });
			}
		}
		let after_key = checkpoint
			.and_then(|checkpoint| match checkpoint.progress() {
				DocumentScanProgress::Scanning { after_key } => after_key.as_ref(),
				DocumentScanProgress::Scanned { .. } => {
					unreachable!("completed checkpoint returns before discovery")
				}
			})
			.cloned();
		let page_request =
			NamespacePageRequest::new(fields.scope.namespace().clone(), after_key, fields.limit);
		let (identities, last_key, scanned) = {
			let keys = storage
				.read_conversion_keys(self.physical.companion_tx(), &page_request)
				.await?;
			let last_key = keys
				.last()
				.cloned()
				.or_else(|| page_request.after_key().cloned());
			let scanned = keys.len() < fields.limit.get() as usize;
			let identities = keys
				.into_iter()
				.map(|key| RecordIdentity::new(fields.scope.namespace().clone(), key))
				.collect::<BTreeSet<RecordIdentity>>();
			(identities, last_key, scanned)
		};
		let (records, write_guard) = storage
			.lock_conversion_batch(
				self.physical.companion_tx(),
				identities,
				fields.conflict_attempts,
			)
			.await?
			.into_records_and_guard();
		let mut prepared = Vec::new();
		for (identity, row) in records {
			if let HeldNativeRow::Value(record) = row {
				let encoded = stage
					.prepare_conversion(ManagedDocumentRecord {
						identity: &identity,
						record: &record,
					})
					.map_err(|error| DocumentConversionPageError::Preparation(Box::new(error)))?;
				prepared.push((identity, encoded));
			}
		}
		let update = if scanned {
			DocumentScanUpdate::Scanned { last_key }
		} else {
			DocumentScanUpdate::Advance {
				after_key: last_key.expect("a full nonzero page has a last key"),
			}
		};
		work.record_document_scan(
			DocumentCheckpointContext {
				control: &self.control,
				candidate,
				origin: self.origin,
				expected: &expected,
			},
			&stage,
			update,
		)?;
		let written = write_guard.write_batch(prepared).await?;
		written.cleanup_reservations().await?;
		let observed = self.persist_work(&work).await?;
		Ok(DocumentConversionPageResult { observed, work })
	}

	pub(super) fn require_conversion_writer(&self) -> Result<(), ControlProgressError> {
		let declaration = self.progress_storage.administration.declaration();
		let policy = self
			.control
			.component(declaration.identifier())
			.ok_or(ControlProgressError::AdministrativePolicyAbsent)?;
		let bound = declaration.bind(policy)?;
		self.progress_storage
			.administration
			.require_bound_policy(&bound)?;
		let schema = bound.bind_schema(
			self.progress_storage
				.administration
				.control_component()
				.schema(),
		)?;
		self.progress_storage
			.administration
			.control_component()
			.require_bound_writer(&schema)?;
		Ok(())
	}
}
