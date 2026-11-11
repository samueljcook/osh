//! Named atomic Work preparation for one final Document policy.
//!
//! Scanned and optional Verified metadata grant no effective completion or transaction authority.

use crate::upgrade::policy::document_reader::DocumentReaderCompletion;

use super::document_checkpoint::{
	DocumentCheckpointContext, DocumentCheckpointError, DocumentScanProgress,
};
use super::document_verification::DocumentVerificationProgress;
use super::{PgUpgradeStatus, UpgradeWork, UpgradeWorkError, UpgradeWorkGenerationChange};

pub(in crate::upgrade) struct DocumentReaderRetirementWorkFields<'a, 'prepared> {
	pub(crate) context: DocumentCheckpointContext<'a>,
	pub(crate) completion: &'a DocumentReaderCompletion<'prepared>,
}
impl UpgradeWork {
	/// The fixed executor must persist the final scope and any paired consumption together.
	/// No progress is required when the complete schema policy is unchanged.
	/// Existing conversion and verification must be Scanned and Verified, respectively.
	pub(in crate::upgrade) fn complete_document_reader_retirement(
		&mut self,
		fields: DocumentReaderRetirementWorkFields<'_, '_>,
	) -> Result<UpgradeWorkGenerationChange, UpgradeWorkError> {
		let context = fields.context;
		let completion = fields.completion;
		let stage = completion.writer_stage();
		let scope = stage.policy().identity();
		self.require_document_context(&context, stage)?;
		if !completion
			.current_scopes()
			.matches_policies(context.control.scope_policies())
		{
			return Err(DocumentCheckpointError::CompletionCurrentMismatch {
				scope: scope.clone(),
			}
			.into());
		}
		if !self
			.target
			.request()
			.document_scopes()
			.and_then(|scopes| scopes.lookup(scope).ok())
			.is_some_and(|target| target.matches_policy(stage.final_policy()))
		{
			return Err(DocumentCheckpointError::CompletionTargetMismatch {
				scope: scope.clone(),
			}
			.into());
		}
		let position = self
			.document_conversion_checkpoints()
			.iter()
			.position(|checkpoint| checkpoint.invariant.identity() == scope);
		let verification_position = self
			.document_verification_checkpoints()
			.iter()
			.position(|checkpoint| checkpoint.invariant.identity() == scope);
		if let Some(index) = position {
			if self.status() != PgUpgradeStatus::Building {
				return Err(DocumentCheckpointError::BuildingRequired.into());
			}
			let checkpoint = &self.document_conversion_checkpoints()[index];
			if !stage.matches_policy(&checkpoint.invariant) {
				return Err(DocumentCheckpointError::CompletionInvariantMismatch {
					scope: scope.clone(),
				}
				.into());
			}
			if !matches!(checkpoint.progress, DocumentScanProgress::Scanned { .. }) {
				return Err(DocumentCheckpointError::CompletionScanIncomplete {
					scope: scope.clone(),
				}
				.into());
			}
		} else {
			if verification_position.is_some() {
				return Err(DocumentCheckpointError::CompletionVerificationOrphan {
					scope: scope.clone(),
				}
				.into());
			}
			if !completion.schema_policy_unchanged() {
				return Err(DocumentCheckpointError::CompletionCheckpointMissing {
					scope: scope.clone(),
				}
				.into());
			}
		}
		if let Some(index) = verification_position {
			let verification = &self.document_verification_checkpoints()[index];
			if !stage.matches_policy(&verification.invariant) {
				return Err(DocumentCheckpointError::CompletionVerificationMismatch {
					scope: scope.clone(),
				}
				.into());
			}
			if !matches!(
				verification.progress(),
				DocumentVerificationProgress::Verified { .. }
			) {
				return Err(DocumentCheckpointError::CompletionVerificationIncomplete {
					scope: scope.clone(),
				}
				.into());
			}
		}
		let current = context.control.scope_policies();
		let components = context.control.component_policies();
		self.require_source_checkpoint_invariants(components, current)?;
		self.require_document_checkpoint_invariants(current)?;
		self.require_document_verification_invariants(current)?;
		self.require_payload_checkpoint_invariants(current)?;
		let prospective = completion.scopes();
		self.require_source_checkpoint_invariants(components, prospective)?;
		self.require_document_conversion_invariants(prospective, position)?;
		self.require_verification_invariants(prospective, verification_position)?;
		self.require_payload_checkpoint_invariants(prospective)?;
		let result = UpgradeWorkGenerationChange {
			expected: self.owned_generation(),
			next: self.next_build_generation()?,
		};
		let progress = self.building_progress_mut()?;
		progress.owned_generation = result.next;
		if let Some(index) = position {
			progress.document_conversion_checkpoints.remove(index);
		}
		if let Some(index) = verification_position {
			progress.document_verification_checkpoints.remove(index);
		}
		Ok(result)
	}
}
