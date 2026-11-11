//! Named atomic Work preparation for one effective final payload-reader policy.

use crate::upgrade::policy::payload_reader::PrimitivePayloadReaderCompletion;

use super::payload_checkpoint::{
	PayloadCheckpointContext, PayloadCheckpointError, PayloadScanProgress,
};
use super::{UpgradeWork, UpgradeWorkError, UpgradeWorkGenerationChange};

pub(in crate::upgrade) struct PayloadReaderRetirementWorkFields<'a, 'prepared, 'code> {
	pub(crate) context: PayloadCheckpointContext<'a>,
	pub(crate) completion: &'a PrimitivePayloadReaderCompletion<'prepared, 'code>,
}
impl UpgradeWork {
	/// The fixed executor must persist its final scope and this update in one effective write.
	/// Parsed Scanned metadata supplies no route, coverage, inspection or commit authority.
	pub(in crate::upgrade) fn complete_payload_reader_retirement(
		&mut self,
		fields: PayloadReaderRetirementWorkFields<'_, '_, '_>,
	) -> Result<UpgradeWorkGenerationChange, UpgradeWorkError> {
		let context = fields.context;
		let completion = fields.completion;
		let stage = completion.writer_stage();
		let scope = stage.policy().identity();
		self.require_payload_context(&context, stage)?;
		if !completion
			.current_scopes()
			.matches_policies(context.control.scope_policies())
		{
			return Err(PayloadCheckpointError::CompletionCurrentMismatch {
				scope: scope.clone(),
			}
			.into());
		}
		if !self
			.target
			.request()
			.scope_replacements()
			.and_then(|scopes| scopes.lookup(scope).ok())
			.is_some_and(|target| target.matches_policy(stage.final_policy()))
		{
			return Err(PayloadCheckpointError::CompletionTargetMismatch {
				scope: scope.clone(),
			}
			.into());
		}
		let position = self
			.payload_checkpoints()
			.iter()
			.position(|checkpoint| checkpoint.invariant.identity() == scope)
			.ok_or_else(|| PayloadCheckpointError::CompletionCheckpointMissing {
				scope: scope.clone(),
			})?;
		let checkpoint = &self.payload_checkpoints()[position];
		if !stage.matches_policy(&checkpoint.invariant) {
			return Err(PayloadCheckpointError::CompletionInvariantMismatch {
				scope: scope.clone(),
			}
			.into());
		}
		if !matches!(checkpoint.progress, PayloadScanProgress::Scanned { .. }) {
			return Err(PayloadCheckpointError::CompletionScanIncomplete {
				scope: scope.clone(),
			}
			.into());
		}
		let current = context.control.scope_policies();
		let components = context.control.component_policies();
		self.require_source_checkpoint_invariants(components, current)?;
		self.require_document_checkpoint_invariants(current)?;
		self.require_document_verification_invariants(current)?;
		self.require_payload_checkpoint_invariants(current)?;
		let prospective = completion.scopes();
		self.require_source_checkpoint_invariants(components, prospective)?;
		self.require_document_checkpoint_invariants(prospective)?;
		self.require_document_verification_invariants(prospective)?;
		self.require_payload_invariants(prospective, Some(position))?;
		let result = UpgradeWorkGenerationChange {
			expected: self.owned_generation(),
			next: self.next_build_generation()?,
		};
		let progress = self.building_progress_mut()?;
		progress.owned_generation = result.next;
		progress.payload_checkpoints.remove(position);
		Ok(result)
	}
}
