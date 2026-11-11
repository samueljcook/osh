//! Pure native payload conversion progress bound to complete current Primitive writer stages.

use super::{
	CompleteUpgradeTarget, PgUpgradeStatus, UpgradeSourceOrigin, UpgradeWork,
	UpgradeWorkCheckpointContext, UpgradeWorkError,
};
use crate::installation::control_state::InstallationControlState;
use crate::records::RecordKey;
use crate::scope::{
	PrimitivePayloadPolicy, ScopeIdentity, ScopeOwner, ScopePolicy, ScopePolicySet,
};
use crate::upgrade::policy::InstallationUpgradeCandidate;
use crate::upgrade::policy::payload_stage::PrimitivePayloadWriterStage;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::upgrade) enum PayloadCheckpointProcedure {
	PrimitivePayloadSelectedWriterConversionV1,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::upgrade) enum PayloadScanProgress {
	Scanning { after_key: Option<RecordKey> },
	Scanned { last_key: Option<RecordKey> },
}
pub(in crate::upgrade) enum PayloadScanUpdate {
	Advance { after_key: RecordKey },
	Scanned { last_key: Option<RecordKey> },
}
pub(in crate::upgrade) struct PayloadConversionCheckpoint {
	pub(super) invariant: ScopePolicy,
	pub(super) procedure: PayloadCheckpointProcedure,
	pub(super) progress: PayloadScanProgress,
}
impl PayloadConversionCheckpoint {
	pub(crate) fn invariant(&self) -> &ScopePolicy {
		&self.invariant
	}
	pub(crate) fn procedure(&self) -> PayloadCheckpointProcedure {
		self.procedure
	}
	pub(crate) fn progress(&self) -> &PayloadScanProgress {
		&self.progress
	}
}
impl std::fmt::Debug for PayloadConversionCheckpoint {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("PayloadConversionCheckpoint")
			.field("procedure", &self.procedure)
			.field("progress", &self.progress)
			.finish_non_exhaustive()
	}
}
/// These pure observations grant no transaction, writer barrier, visited page or coverage.
pub(in crate::upgrade) struct PayloadCheckpointContext<'a> {
	pub(crate) control: &'a InstallationControlState,
	pub(crate) candidate: &'a InstallationUpgradeCandidate,
	pub(crate) origin: UpgradeSourceOrigin,
	pub(crate) expected: &'a UpgradeWorkCheckpointContext,
}
impl UpgradeWork {
	pub(in crate::upgrade) fn payload_checkpoints(&self) -> &[PayloadConversionCheckpoint] {
		self.progress()
			.map_or(&[], |progress| progress.payload_checkpoints.as_slice())
	}
	pub(in crate::upgrade) fn payload_checkpoint(
		&self,
		scope: &ScopeIdentity,
	) -> Option<&PayloadConversionCheckpoint> {
		self.payload_checkpoints()
			.iter()
			.find(|checkpoint| checkpoint.invariant.identity() == scope)
	}
	pub(super) fn require_payload_context(
		&self,
		context: &PayloadCheckpointContext<'_>,
		stage: &PrimitivePayloadWriterStage<'_>,
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
		if !context
			.control
			.scope_policies()
			.lookup(stage.policy().identity())
			.is_ok_and(|actual| stage.matches_policy(actual))
		{
			return Err(PayloadCheckpointError::CurrentInvariantMismatch {
				scope: stage.policy().identity().clone(),
			}
			.into());
		}
		require_target_closure(&self.target, stage.policy())?;
		Ok(())
	}
	/// The fixed executor must commit actual conversion and this cursor together.
	pub(in crate::upgrade) fn record_payload_scan(
		&mut self,
		context: PayloadCheckpointContext<'_>,
		stage: &PrimitivePayloadWriterStage<'_>,
		update: PayloadScanUpdate,
	) -> Result<(), UpgradeWorkError> {
		self.require_payload_context(&context, stage)?;
		let position = self
			.payload_checkpoints()
			.iter()
			.position(|checkpoint| checkpoint.invariant.identity() == stage.policy().identity());
		let previous = match position {
			None => None,
			Some(index) => {
				let checkpoint = &self.payload_checkpoints()[index];
				if !stage.matches_policy(&checkpoint.invariant) {
					return Err(PayloadCheckpointError::StoredInvariantMismatch.into());
				}
				let PayloadScanProgress::Scanning { after_key } = &checkpoint.progress else {
					return Err(PayloadCheckpointError::AlreadyScanned.into());
				};
				after_key.as_ref()
			}
		};
		let next = match update {
			PayloadScanUpdate::Advance { after_key } => {
				if previous.is_some_and(|prior| after_key <= *prior) {
					return Err(PayloadCheckpointError::CursorNotAdvanced.into());
				}
				PayloadScanProgress::Scanning {
					after_key: Some(after_key),
				}
			}
			PayloadScanUpdate::Scanned { last_key } => {
				if previous.is_some_and(|prior| last_key.as_ref().is_none_or(|last| last < prior)) {
					return Err(PayloadCheckpointError::CursorRewind.into());
				}
				PayloadScanProgress::Scanned { last_key }
			}
		};
		let progress = self.building_progress_mut()?;
		match position {
			Some(index) => progress.payload_checkpoints[index].progress = next,
			None => progress
				.payload_checkpoints
				.push(PayloadConversionCheckpoint {
					invariant: stage.policy().clone(),
					procedure:
						PayloadCheckpointProcedure::PrimitivePayloadSelectedWriterConversionV1,
					progress: next,
				}),
		}
		Ok(())
	}
	pub(in crate::upgrade) fn require_payload_checkpoint_invariants(
		&self,
		current: &ScopePolicySet,
	) -> Result<(), UpgradeWorkError> {
		self.require_payload_invariants(current, None)
	}
	pub(super) fn require_payload_invariants(
		&self,
		current: &ScopePolicySet,
		excluded: Option<usize>,
	) -> Result<(), UpgradeWorkError> {
		for (index, checkpoint) in self.payload_checkpoints().iter().enumerate() {
			if excluded == Some(index) {
				continue;
			}
			if !current
				.lookup(checkpoint.invariant.identity())
				.is_ok_and(|actual| checkpoint.invariant.matches_policy(actual))
			{
				return Err(PayloadCheckpointError::CurrentInvariantMismatch {
					scope: checkpoint.invariant.identity().clone(),
				}
				.into());
			}
		}
		Ok(())
	}
}
pub(super) struct PayloadCheckpointComparisonFields<'a> {
	pub(super) left: &'a [PayloadConversionCheckpoint],
	pub(super) right: &'a [PayloadConversionCheckpoint],
}
pub(super) fn matches_checkpoints(fields: PayloadCheckpointComparisonFields<'_>) -> bool {
	let PayloadCheckpointComparisonFields { left, right } = fields;
	left.len() == right.len()
		&& left.iter().all(|checkpoint| {
			right.iter().any(|other| {
				checkpoint.procedure == other.procedure
					&& checkpoint.progress == other.progress
					&& checkpoint.invariant.matches_policy(&other.invariant)
			})
		})
}
pub(super) fn validate_checkpoints(
	target: &CompleteUpgradeTarget,
	status: PgUpgradeStatus,
	checkpoints: &[PayloadConversionCheckpoint],
) -> Result<(), PayloadCheckpointError> {
	if !checkpoints.is_empty() {
		match status {
			PgUpgradeStatus::Queued => return Err(PayloadCheckpointError::QueuedProgress),
			PgUpgradeStatus::Completed => return Err(PayloadCheckpointError::CompletedProgress),
			PgUpgradeStatus::Building | PgUpgradeStatus::Cancelled => {}
		}
	}
	for (index, checkpoint) in checkpoints.iter().enumerate() {
		if checkpoints[..index]
			.iter()
			.any(|prior| prior.invariant.identity() == checkpoint.invariant.identity())
		{
			return Err(PayloadCheckpointError::DuplicateScope { index });
		}
		require_target_closure(target, &checkpoint.invariant)?;
	}
	Ok(())
}
fn require_target_closure(
	target: &CompleteUpgradeTarget,
	invariant: &ScopePolicy,
) -> Result<(), PayloadCheckpointError> {
	let fail = || PayloadCheckpointError::TargetMeaningMismatch {
		scope: invariant.identity().clone(),
	};
	let proposed = target
		.request()
		.scope_replacements()
		.and_then(|scopes| scopes.lookup(invariant.identity()).ok())
		.ok_or_else(fail)?;
	let (ScopeOwner::Primitive(current), ScopeOwner::Primitive(proposed)) =
		(invariant.owner(), proposed.owner())
	else {
		return Err(fail());
	};
	let family = current.intrinsic().component_identifier();
	if (family != crate::fleet::cache::entry_state::CacheEntryComponent::component_identifier()
		&& family != crate::fleet::topic::event_state::TopicEventComponent::component_identifier())
		|| !current.intrinsic().matches_policy(proposed.intrinsic())
	{
		return Err(fail());
	}
	let (PrimitivePayloadPolicy::Schema(current), PrimitivePayloadPolicy::Schema(proposed)) =
		(current.payload(), proposed.payload())
	else {
		return Err(fail());
	};
	if current.identifier() != proposed.identifier()
		|| current.writer() != proposed.writer()
		|| proposed
			.required_readers()
			.iter()
			.any(|required| !current.required_readers().contains(required))
	{
		return Err(fail());
	}
	Ok(())
}
#[derive(Debug, thiserror::Error)]
pub(crate) enum PayloadCheckpointError {
	#[error("payload reader completion prepared current scope set differs from control")]
	CompletionCurrentMismatch { scope: ScopeIdentity },
	#[error("payload reader completion final scope differs from the immutable Work target")]
	CompletionTargetMismatch { scope: ScopeIdentity },
	#[error("payload reader completion requires this scope's conversion checkpoint")]
	CompletionCheckpointMissing { scope: ScopeIdentity },
	#[error("payload reader completion requires this scope's current conversion invariant")]
	CompletionInvariantMismatch { scope: ScopeIdentity },
	#[error("payload reader completion requires this scope's scanned conversion checkpoint")]
	CompletionScanIncomplete { scope: ScopeIdentity },
	#[error("payload checkpoint current complete scope invariant differs")]
	CurrentInvariantMismatch { scope: ScopeIdentity },
	#[error("payload checkpoint scope is not closed over the final target meaning")]
	TargetMeaningMismatch { scope: ScopeIdentity },
	#[error("payload checkpoint stored invariant differs from the selected current stage")]
	StoredInvariantMismatch,
	#[error("payload conversion scan is already marked scanned")]
	AlreadyScanned,
	#[error("payload conversion cursor must advance strictly bytewise")]
	CursorNotAdvanced,
	#[error("payload conversion terminal cursor cannot rewind or discard a prior key")]
	CursorRewind,
	#[error("queued work cannot contain payload progress")]
	QueuedProgress,
	#[error("completed work cannot contain payload progress")]
	CompletedProgress,
	#[error("payload checkpoint repeats a scope")]
	DuplicateScope { index: usize },
}
