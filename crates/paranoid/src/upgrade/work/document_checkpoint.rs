//! Pure selected-writer conversion cursors bound to complete document stage policies.
//!
//! A cursor records progress metadata; it proves no page, writer barrier, coverage, or readiness.

use crate::installation::control_state::InstallationControlState;
use crate::records::RecordKey;
use crate::scope::{ScopeError, ScopeIdentity, ScopeOwner, ScopePolicy};
use crate::upgrade::policy::InstallationUpgradeCandidate;
use crate::upgrade::policy::document_readiness::DocumentReadinessStage;
use crate::upgrade::policy::document_stage::DocumentWriterStage;
use crate::upgrade::policy::request::matches_document_scope;

use super::document_verification::{DocumentVerificationError, DocumentVerificationProgress};
use super::{
	CompleteUpgradeTarget, PgUpgradeStatus, UpgradeSourceOrigin, UpgradeWork,
	UpgradeWorkCheckpointContext, UpgradeWorkError,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::upgrade) enum DocumentCheckpointProcedure {
	DocumentSelectedWriterConversionV1,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::upgrade) enum DocumentScanProgress {
	Scanning { after_key: Option<RecordKey> },
	Scanned { last_key: Option<RecordKey> },
}

pub(in crate::upgrade) enum DocumentScanUpdate {
	Advance { after_key: RecordKey },
	Scanned { last_key: Option<RecordKey> },
}

pub(in crate::upgrade) struct DocumentConversionCheckpoint {
	pub(super) invariant: ScopePolicy,
	pub(super) procedure: DocumentCheckpointProcedure,
	pub(super) progress: DocumentScanProgress,
}
impl DocumentConversionCheckpoint {
	pub(crate) fn invariant(&self) -> &ScopePolicy {
		&self.invariant
	}
	pub(crate) fn procedure(&self) -> DocumentCheckpointProcedure {
		self.procedure
	}
	pub(crate) fn progress(&self) -> &DocumentScanProgress {
		&self.progress
	}
	fn matches(&self, other: &Self) -> bool {
		self.procedure == other.procedure
			&& self.progress == other.progress
			&& matches_document_scope(&self.invariant, &other.invariant)
	}
}
impl std::fmt::Debug for DocumentConversionCheckpoint {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("DocumentConversionCheckpoint")
			.field("procedure", &self.procedure)
			.field("progress", &self.progress)
			.finish_non_exhaustive()
	}
}

/// Observed pure inputs; no transaction, controller, or record authority is carried here.
pub(in crate::upgrade) struct DocumentCheckpointContext<'a> {
	pub(crate) control: &'a InstallationControlState,
	pub(crate) candidate: &'a InstallationUpgradeCandidate,
	pub(crate) origin: UpgradeSourceOrigin,
	pub(crate) expected: &'a UpgradeWorkCheckpointContext,
}

impl UpgradeWork {
	/// Rebases only one actual Ready addition after durable fixed-range verification.
	/// The executor must persist its actual Ready policy and generation atomically with this Work.
	pub(in crate::upgrade) fn record_verified_document_readiness(
		&mut self,
		context: DocumentCheckpointContext<'_>,
		stage: &DocumentReadinessStage,
	) -> Result<super::UpgradeWorkGenerationChange, UpgradeWorkError> {
		self.require_document_context(&context, stage.writer_stage())?;
		if self.status() != PgUpgradeStatus::Building {
			return Err(DocumentCheckpointError::BuildingRequired.into());
		}
		let position = self
			.document_conversion_checkpoints()
			.iter()
			.position(|checkpoint| checkpoint.invariant.identity() == stage.policy().identity())
			.ok_or(DocumentCheckpointError::NotStarted)?;
		let checkpoint = &self.document_conversion_checkpoints()[position];
		if !stage.writer_stage().matches_policy(&checkpoint.invariant) {
			return Err(DocumentCheckpointError::CurrentInvariantMismatch.into());
		}
		if !matches!(checkpoint.progress, DocumentScanProgress::Scanned { .. }) {
			return Err(DocumentCheckpointError::ReadinessRequiresScanned.into());
		}
		let verification_position = self
			.document_verification_checkpoints()
			.iter()
			.position(|checkpoint| checkpoint.invariant.identity() == stage.policy().identity())
			.ok_or(DocumentVerificationError::ReadinessNotVerified)?;
		let verification = &self.document_verification_checkpoints()[verification_position];
		if !stage.writer_stage().matches_policy(&verification.invariant) {
			return Err(DocumentVerificationError::CurrentInvariantMismatch.into());
		}
		let DocumentVerificationProgress::Verified { last_key } = verification.progress() else {
			return Err(DocumentVerificationError::ReadinessNotVerified.into());
		};
		let verified_last_key = last_key.cloned();
		let targeted = self
			.target
			.request()
			.document_scopes()
			.and_then(|scopes| scopes.lookup(stage.policy().identity()).ok())
			.ok_or(DocumentCheckpointError::ScopeNotTargeted)?;
		let ScopeOwner::Document(targeted) = targeted.owner() else {
			return Err(DocumentCheckpointError::NonDocumentInvariant.into());
		};
		if !targeted.is_index_ready(stage.index()) {
			return Err(DocumentCheckpointError::ReadinessIndexNotTargeted.into());
		}
		require_target_closure(&self.target, stage.policy())?;
		let source_rebases = self.prepare_source_readiness_rebase(context.control, stage)?;
		let generations = self.advance_effective_build(
			context.control,
			context.candidate,
			context.origin,
			context.expected,
		)?;
		let progress = self.building_progress_mut()?;
		for (index, invariant) in source_rebases {
			progress.source_checkpoints[index].invariant = invariant;
		}
		progress.document_conversion_checkpoints[position] = DocumentConversionCheckpoint {
			invariant: stage.policy().clone(),
			procedure: DocumentCheckpointProcedure::DocumentSelectedWriterConversionV1,
			progress: DocumentScanProgress::Scanned {
				last_key: verified_last_key,
			},
		};
		// The opaque stage constructs exactly one Ready addition with every data policy field preserved.
		progress.document_verification_checkpoints[verification_position].invariant =
			stage.policy().clone();
		Ok(generations)
	}
	pub(in crate::upgrade) fn document_conversion_checkpoints(
		&self,
	) -> &[DocumentConversionCheckpoint] {
		self.progress().map_or(&[], |progress| {
			progress.document_conversion_checkpoints.as_slice()
		})
	}
	pub(in crate::upgrade) fn require_document_checkpoint_invariants(
		&self,
		current: &crate::scope::ScopePolicySet,
	) -> Result<(), UpgradeWorkError> {
		self.require_document_conversion_invariants(current, None)
	}
	pub(super) fn require_document_conversion_invariants(
		&self,
		current: &crate::scope::ScopePolicySet,
		excluded: Option<usize>,
	) -> Result<(), UpgradeWorkError> {
		for (index, checkpoint) in self.document_conversion_checkpoints().iter().enumerate() {
			if excluded == Some(index) {
				continue;
			}
			let actual = current
				.lookup(checkpoint.invariant.identity())
				.map_err(DocumentCheckpointError::Scope)?;
			if !checkpoint.invariant.matches_policy(actual) {
				return Err(DocumentCheckpointError::CurrentInvariantMismatch.into());
			}
		}
		Ok(())
	}
	pub(in crate::upgrade) fn document_conversion_checkpoint(
		&self,
		scope: &ScopeIdentity,
	) -> Option<&DocumentConversionCheckpoint> {
		self.document_conversion_checkpoints()
			.iter()
			.find(|checkpoint| checkpoint.invariant.identity() == scope)
	}
	pub(super) fn require_document_context(
		&self,
		context: &DocumentCheckpointContext<'_>,
		stage: &DocumentWriterStage,
	) -> Result<(), UpgradeWorkError> {
		self.require_checkpoint_context(
			context.control,
			context.candidate,
			context.origin,
			context.expected,
		)?;
		if matches!(
			self.status(),
			PgUpgradeStatus::Completed | PgUpgradeStatus::Cancelled
		) {
			return Err(UpgradeWorkError::TerminalWork);
		}
		let current = context
			.control
			.scope_policies()
			.lookup(stage.policy().identity())
			.map_err(DocumentCheckpointError::Scope)?;
		if !stage.matches_policy(current) {
			return Err(DocumentCheckpointError::CurrentInvariantMismatch.into());
		}
		require_target_closure(&self.target, stage.policy())?;
		Ok(())
	}
	/// Records a first or subsequent page atomically in this value. The executor must commit
	/// the actual page and this metadata together; this method proves no page was visited.
	pub(in crate::upgrade) fn record_document_scan(
		&mut self,
		context: DocumentCheckpointContext<'_>,
		stage: &DocumentWriterStage,
		update: DocumentScanUpdate,
	) -> Result<(), UpgradeWorkError> {
		self.require_document_context(&context, stage)?;
		let position = self
			.document_conversion_checkpoints()
			.iter()
			.position(|checkpoint| checkpoint.invariant.identity() == stage.policy().identity());
		let after_key = match position {
			None => None,
			Some(index) => {
				let checkpoint = &self.document_conversion_checkpoints()[index];
				if !stage.matches_policy(&checkpoint.invariant) {
					return Err(DocumentCheckpointError::CurrentInvariantMismatch.into());
				}
				let DocumentScanProgress::Scanning { after_key } = &checkpoint.progress else {
					return Err(DocumentCheckpointError::AlreadyScanned.into());
				};
				after_key.as_ref()
			}
		};
		let next = match update {
			DocumentScanUpdate::Advance { after_key: next } => {
				if after_key.is_some_and(|previous| next <= *previous) {
					return Err(DocumentCheckpointError::CursorNotAdvanced.into());
				}
				DocumentScanProgress::Scanning {
					after_key: Some(next),
				}
			}
			DocumentScanUpdate::Scanned { last_key } => {
				if after_key
					.is_some_and(|previous| last_key.as_ref().is_none_or(|last| last < previous))
				{
					return Err(DocumentCheckpointError::CursorRewind.into());
				}
				DocumentScanProgress::Scanned { last_key }
			}
		};
		match position {
			Some(index) => {
				self.building_progress_mut()?
					.document_conversion_checkpoints[index]
					.progress = next
			}
			None => self
				.building_progress_mut()?
				.document_conversion_checkpoints
				.push(DocumentConversionCheckpoint {
					invariant: stage.policy().clone(),
					procedure: DocumentCheckpointProcedure::DocumentSelectedWriterConversionV1,
					progress: next,
				}),
		}
		Ok(())
	}
}

pub(super) fn matches_checkpoints(
	left: &[DocumentConversionCheckpoint],
	right: &[DocumentConversionCheckpoint],
) -> bool {
	left.len() == right.len()
		&& left
			.iter()
			.all(|checkpoint| right.iter().any(|other| checkpoint.matches(other)))
}

pub(super) fn validate_checkpoints(
	target: &CompleteUpgradeTarget,
	status: PgUpgradeStatus,
	checkpoints: &[DocumentConversionCheckpoint],
) -> Result<(), DocumentCheckpointError> {
	if !checkpoints.is_empty() {
		match status {
			PgUpgradeStatus::Queued => return Err(DocumentCheckpointError::QueuedProgress),
			PgUpgradeStatus::Completed => return Err(DocumentCheckpointError::CompletedProgress),
			PgUpgradeStatus::Building | PgUpgradeStatus::Cancelled => {}
		}
	}
	for (index, checkpoint) in checkpoints.iter().enumerate() {
		if checkpoints[..index]
			.iter()
			.any(|prior| prior.invariant.identity() == checkpoint.invariant.identity())
		{
			return Err(DocumentCheckpointError::DuplicateScope { index });
		}
		require_target_closure(target, &checkpoint.invariant)?;
	}
	Ok(())
}

pub(super) fn require_target_closure(
	target: &CompleteUpgradeTarget,
	invariant: &ScopePolicy,
) -> Result<(), DocumentCheckpointError> {
	let target = target
		.request()
		.document_scopes()
		.and_then(|scopes| scopes.lookup(invariant.identity()).ok())
		.ok_or(DocumentCheckpointError::ScopeNotTargeted)?;
	let (ScopeOwner::Document(current), ScopeOwner::Document(target)) =
		(invariant.owner(), target.owner())
	else {
		return Err(DocumentCheckpointError::NonDocumentInvariant);
	};
	if current.schema().identifier() != target.schema().identifier() {
		return Err(DocumentCheckpointError::SchemaFamilyMismatch);
	}
	if current.schema().writer() != target.schema().writer() {
		return Err(DocumentCheckpointError::FinalWriterMismatch);
	}
	for required in target.schema().required_readers() {
		if !current.schema().required_readers().contains(required) {
			return Err(DocumentCheckpointError::TargetReaderMissing {
				version: required.version,
			});
		}
	}
	for (index, required) in target.maintenance_indexes().enumerate() {
		if !current
			.maintenance_indexes()
			.any(|actual| actual.matches_descriptor(required))
		{
			return Err(DocumentCheckpointError::TargetIndexMissing { index });
		}
	}
	Ok(())
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum DocumentCheckpointError {
	#[error("document reader completion prepared current scopes differ from control")]
	CompletionCurrentMismatch { scope: ScopeIdentity },
	#[error("document reader completion final scope differs from the immutable Work target")]
	CompletionTargetMismatch { scope: ScopeIdentity },
	#[error("document reader completion requires this scope's conversion checkpoint")]
	CompletionCheckpointMissing { scope: ScopeIdentity },
	#[error("document reader completion requires this scope's current conversion invariant")]
	CompletionInvariantMismatch { scope: ScopeIdentity },
	#[error("document reader completion requires this scope's scanned conversion checkpoint")]
	CompletionScanIncomplete { scope: ScopeIdentity },
	#[error("document reader completion existing verification differs from its current stage")]
	CompletionVerificationMismatch { scope: ScopeIdentity },
	#[error("document reader completion requires its existing verification to be Verified")]
	CompletionVerificationIncomplete { scope: ScopeIdentity },
	#[error("document finalization verification has no matching conversion checkpoint")]
	CompletionVerificationOrphan { scope: ScopeIdentity },
	#[error("document readiness index is absent from the pending Ready target")]
	ReadinessIndexNotTargeted,
	#[error("document readiness requires an Scanned writer-conversion checkpoint")]
	ReadinessRequiresScanned,
	#[error("document checkpoint {index} repeats a scope")]
	DuplicateScope { index: usize },
	#[error("queued work cannot contain document progress")]
	QueuedProgress,
	#[error("completed work cannot contain document progress")]
	CompletedProgress,
	#[error("document checkpoint scope is absent from the pending document replacements")]
	ScopeNotTargeted,
	#[error("document checkpoint invariant must describe a document owner")]
	NonDocumentInvariant,
	#[error("document checkpoint invariant belongs to another schema family")]
	SchemaFamilyMismatch,
	#[error("document checkpoint invariant must select the final writer")]
	FinalWriterMismatch,
	#[error("document checkpoint invariant omits target reader version {version} or its envelope")]
	TargetReaderMissing { version: u32 },
	#[error("document checkpoint invariant omits target index {index} or its full shape")]
	TargetIndexMissing { index: usize },
	#[error("document checkpoint scope policy lookup failed")]
	Scope(#[source] ScopeError),
	#[error("document checkpoint invariant differs from current scope or bound stage")]
	CurrentInvariantMismatch,
	#[error("document conversion checkpoint does not exist")]
	NotStarted,
	#[error("document conversion progress requires building work")]
	BuildingRequired,
	#[error("document conversion scan is already marked scanned")]
	AlreadyScanned,
	#[error("document conversion cursor must advance strictly bytewise")]
	CursorNotAdvanced,
	#[error("document conversion terminal cursor cannot rewind or discard a prior key")]
	CursorRewind,
}
