//! Fixed selected-writer conversion of native Cache/Topic payloads and their atomic Work cursor.

use std::{collections::BTreeSet, num::NonZeroU32};

use primitives_rs::time::Deadline;

use crate::fleet::payload::scope::PayloadScopeRecordFields;
use crate::fleet::payload::scope::conversion::NativePayloadConversionError;
use crate::records::postgres::HeldNativeRow;
use crate::records::{NamespacePageRequest, RecordIdentity};
use crate::runtime::admission::upgrade::{NativeUpgradeAdmissionError, NativeUpgradeInvocation};
use crate::scope::managed::ManagedScopeRegistry;
use crate::scope::{ScopeError, ScopeIdentity};
use crate::upgrade::policy::payload_stage::{
	BoxedPrimitivePayloadWriterStageError, PrimitivePayloadWriterStage,
	PrimitivePayloadWriterStageFields,
};
use crate::upgrade::policy::{InstallationUpgradeCandidate, UpgradeCompatibilityError};
use crate::upgrade::work::payload_checkpoint::{
	PayloadCheckpointContext, PayloadScanProgress, PayloadScanUpdate,
};
use crate::upgrade::work::{PgUpgradeStatus, UpgradeWork, UpgradeWorkError};

use super::{ControlProgressError, PgControlProgressLock};
use crate::records::postgres::StateStorageError;
use crate::records::postgres::scan::RecordPageError;

pub(in crate::upgrade) struct PayloadConversionPageFields<'a> {
	pub(crate) invocation: &'a NativeUpgradeInvocation<'a>,
	pub(crate) candidate: &'a InstallationUpgradeCandidate,
	pub(crate) registry: &'a ManagedScopeRegistry,
	pub(crate) scope: &'a ScopeIdentity,
	pub(crate) limit: NonZeroU32,
	pub(crate) conflict_attempts: NonZeroU32,
	pub(crate) deadline: Deadline,
}

/// Every observation is tentative until the enclosing owned transaction has a known COMMIT.
pub(in crate::upgrade) struct PayloadConversionPageResult {
	pub(crate) observed: crate::upgrade::observation::NativeUpgradeObservedControl,
	pub(crate) work: UpgradeWork,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PayloadConversionPageError {
	#[error("native payload conversion operation deadline elapsed")]
	DeadlineElapsed,
	#[error("native payload conversion private participant admission failed")]
	Admission(#[from] NativeUpgradeAdmissionError),
	#[error("native payload conversion progress guard failed")]
	Progress(#[from] ControlProgressError),
	#[error("native payload conversion full Work context failed")]
	Work(#[from] UpgradeWorkError),
	#[error("native payload conversion complete final candidate failed")]
	Candidate(#[from] UpgradeCompatibilityError),
	#[error("native payload conversion current scope failed")]
	Scope(#[from] ScopeError),
	#[error("native payload conversion writer stage failed")]
	Stage(#[source] Box<BoxedPrimitivePayloadWriterStageError>),
	#[error("native payload conversion scope is absent from the complete Work target")]
	ScopeNotTargeted,
	#[error("native payload conversion selected writer stage is not installed")]
	WriterStageNotInstalled,
	#[error("native payload conversion checkpoint differs from its complete installed stage")]
	CheckpointInvariantMismatch,
	#[error("native payload conversion installation placement failed")]
	Placement(#[from] crate::runtime::admission::bindings::InstallationAdmissionBindingError),
	#[error("native payload conversion physical discovery failed")]
	Page(#[from] RecordPageError),
	#[error("native payload conversion canonical acquisition or persistence failed")]
	Storage(#[from] StateStorageError),
	#[error("native payload conversion envelope or payload code failed")]
	Preparation(#[source] Box<NativePayloadConversionError>),
}

impl PayloadConversionPageError {
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

fn require_deadline(deadline: Deadline) -> Result<(), PayloadConversionPageError> {
	if tokio::time::Instant::now().into_std() >= deadline.instant() {
		Err(PayloadConversionPageError::DeadlineElapsed)
	} else {
		Ok(())
	}
}

impl PgControlProgressLock<'_, '_> {
	/// Consumes the exact held Work guard; no application value or arbitrary control write escapes.
	pub(in crate::upgrade) async fn convert_payload_page(
		mut self,
		fields: PayloadConversionPageFields<'_>,
	) -> Result<PayloadConversionPageResult, PayloadConversionPageError> {
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
		work.require_checkpoint_context(&self.control, fields.candidate, self.origin, &expected)?;
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
		work.require_payload_checkpoint_invariants(self.control.scope_policies())?;
		work.require_document_checkpoint_invariants(self.control.scope_policies())?;
		fields.candidate.require_control_target(
			&self.control,
			&self.progress_storage.administration.declaration(),
			self.progress_storage.administration.participant_schema(),
			self.progress_storage.administration.control_component(),
		)?;
		self.progress_storage
			.installation
			.check_scope_policies(self.control.scope_policies())?;
		if let Some(scopes) = fields.candidate.scope_replacements() {
			self.progress_storage
				.installation
				.check_scope_policies(scopes)?;
		}
		self.require_conversion_writer()?;
		let target = work
			.target()
			.request()
			.scope_replacements()
			.and_then(|scopes| scopes.lookup(fields.scope).ok())
			.ok_or(PayloadConversionPageError::ScopeNotTargeted)?
			.clone();
		let current = self.control.scope_policies().lookup(fields.scope)?;
		let stage = PrimitivePayloadWriterStage::new(PrimitivePayloadWriterStageFields {
			registry: fields.registry,
			current,
			target: &target,
		})
		.map_err(|error| PayloadConversionPageError::Stage(Box::new(error)))?;
		if !stage.matches_policy(current) {
			return Err(PayloadConversionPageError::WriterStageNotInstalled);
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
					fields.candidate,
					stage.policy(),
				)
				.await?;
			contracts.require_payload_support()?;
		}
		require_deadline(fields.deadline)?;
		let storage = self
			.progress_storage
			.installation
			.app_storage(fields.scope)?;
		let checkpoint = work.payload_checkpoint(fields.scope);
		if let Some(checkpoint) = checkpoint {
			if !stage.matches_policy(checkpoint.invariant()) {
				return Err(PayloadConversionPageError::CheckpointInvariantMismatch);
			}
			if matches!(checkpoint.progress(), PayloadScanProgress::Scanned { .. }) {
				let observed = self.keep_observed().await?;
				require_deadline(fields.deadline)?;
				return Ok(PayloadConversionPageResult { observed, work });
			}
		}
		let after_key = checkpoint
			.and_then(|checkpoint| match checkpoint.progress() {
				PayloadScanProgress::Scanning { after_key } => after_key.as_ref(),
				PayloadScanProgress::Scanned { .. } => {
					unreachable!("scanned checkpoint returned before discovery")
				}
			})
			.cloned();
		let request =
			NamespacePageRequest::new(fields.scope.namespace().clone(), after_key, fields.limit);
		let (identities, last_key, scanned) = {
			let keys = storage
				.read_conversion_keys(self.physical.companion_tx(), &request)
				.await?;
			let last_key = keys
				.last()
				.cloned()
				.or_else(|| request.after_key().cloned());
			let scanned = keys.len() < fields.limit.get() as usize;
			let identities = keys
				.into_iter()
				.map(|key| RecordIdentity::new(fields.scope.namespace().clone(), key))
				.collect::<BTreeSet<RecordIdentity>>();
			(identities, last_key, scanned)
		};
		require_deadline(fields.deadline)?;
		let (records, write_guard) = storage
			.lock_conversion_batch(
				self.physical.companion_tx(),
				identities,
				fields.conflict_attempts,
			)
			.await?
			.into_records_and_guard();
		require_deadline(fields.deadline)?;
		let mut prepared = Vec::new();
		for (identity, row) in records {
			if let HeldNativeRow::Value(record) = row {
				let encoded = stage
					.prepare_conversion(PayloadScopeRecordFields {
						identity: &identity,
						record: &record,
					})
					.map_err(|error| PayloadConversionPageError::Preparation(Box::new(error)))?;
				prepared.push((identity, encoded));
			}
		}
		let update = if scanned {
			PayloadScanUpdate::Scanned { last_key }
		} else {
			PayloadScanUpdate::Advance {
				after_key: last_key.expect("a full nonzero page has a last key"),
			}
		};
		work.record_payload_scan(
			PayloadCheckpointContext {
				control: &self.control,
				candidate: fields.candidate,
				origin: self.origin,
				expected: &expected,
			},
			&stage,
			update,
		)?;
		require_deadline(fields.deadline)?;
		let written = write_guard.write_batch(prepared).await?;
		require_deadline(fields.deadline)?;

		written.cleanup_reservations().await?;
		require_deadline(fields.deadline)?;
		let observed = self.persist_work(&work).await?;
		require_deadline(fields.deadline)?;
		Ok(PayloadConversionPageResult { observed, work })
	}
}
