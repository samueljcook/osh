//! Queue payload conversion progress under the installed writer barrier.

use super::{
	PgUpgradeStatus, UpgradeSourceOrigin, UpgradeWork, UpgradeWorkCheckpointContext,
	UpgradeWorkError, UpgradeWorkGenerationChange,
};
use crate::installation::contract::ComponentPolicy;
use crate::installation::control_state::InstallationControlState;
use crate::queue::{
	QueueJobComponent,
	upgrade::{QueueJobPayloadStage, QueueJobUpgradeError, policy_stage, writer_stage},
};
use crate::records::RecordKey;
use crate::upgrade::policy::InstallationUpgradeCandidate;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::upgrade) enum QueueJobScanProgress {
	Scanning { after_key: Option<RecordKey> },
	Scanned { last_key: Option<RecordKey> },
}

pub(in crate::upgrade) struct QueueJobConversionCheckpoint {
	pub(super) invariant: ComponentPolicy,
	pub(super) progress: QueueJobScanProgress,
}

impl QueueJobConversionCheckpoint {
	pub(crate) fn progress(&self) -> &QueueJobScanProgress {
		&self.progress
	}
	pub(crate) fn invariant(&self) -> &ComponentPolicy {
		&self.invariant
	}
	pub(super) fn matches(&self, other: &Self) -> bool {
		self.invariant.matches_policy(&other.invariant) && self.progress == other.progress
	}
}

pub(in crate::upgrade) struct QueueJobCheckpointContext<'a> {
	pub(crate) control: &'a InstallationControlState,
	pub(crate) candidate: &'a InstallationUpgradeCandidate,
	pub(crate) origin: UpgradeSourceOrigin,
	pub(crate) expected: &'a UpgradeWorkCheckpointContext,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum QueueJobCheckpointError {
	#[error("Queue job conversion policy is invalid")]
	Policy(#[from] QueueJobUpgradeError),
	#[error("Queue job conversion target or installed component is absent")]
	ComponentAbsent,
	#[error("Queue job conversion checkpoint differs from its writer barrier")]
	InvariantMismatch,
	#[error("Queue job conversion checkpoint is absent")]
	CheckpointAbsent,
	#[error("Queue job conversion scan has not completed")]
	ScanIncomplete,
	#[error("Queue job conversion cursor did not advance")]
	CursorDidNotAdvance,
	#[error("Queue job conversion progress is inconsistent with work status")]
	InvalidStatus,
}

impl UpgradeWork {
	pub(in crate::upgrade) fn queue_job_checkpoint(&self) -> Option<&QueueJobConversionCheckpoint> {
		self.progress()
			.and_then(|progress| progress.queue_job_checkpoint.as_deref())
	}

	pub(in crate::upgrade) fn queue_job_target(
		&self,
	) -> Result<&ComponentPolicy, QueueJobCheckpointError> {
		self.target
			.request()
			.policies()
			.iter()
			.find(|policy| {
				policy.component_identifier() == QueueJobComponent::component_identifier()
			})
			.ok_or(QueueJobCheckpointError::ComponentAbsent)
	}

	pub(in crate::upgrade) fn require_queue_job_checkpoint_invariant(
		&self,
		components: &[ComponentPolicy],
	) -> Result<(), UpgradeWorkError> {
		validate_checkpoint(self)?;
		if let Some(checkpoint) = self.queue_job_checkpoint()
			&& !components
				.iter()
				.any(|policy| policy.matches_policy(&checkpoint.invariant))
		{
			return Err(QueueJobCheckpointError::InvariantMismatch.into());
		}
		Ok(())
	}

	pub(in crate::upgrade) fn promote_queue_job_writer(
		&mut self,
		context: QueueJobCheckpointContext<'_>,
		stage: &ComponentPolicy,
	) -> Result<UpgradeWorkGenerationChange, UpgradeWorkError> {
		let current = context
			.control
			.component(QueueJobComponent::component_identifier())
			.ok_or(QueueJobCheckpointError::ComponentAbsent)?;
		if !writer_stage(current, self.queue_job_target()?)
			.map_err(QueueJobCheckpointError::from)?
			.matches_policy(stage)
		{
			return Err(QueueJobCheckpointError::InvariantMismatch.into());
		}
		if self.queue_job_checkpoint().is_some() {
			return Err(QueueJobCheckpointError::InvariantMismatch.into());
		}
		let change = self.advance_effective_build(
			context.control,
			context.candidate,
			context.origin,
			context.expected,
		)?;
		self.building_progress_mut()?.queue_job_checkpoint =
			Some(Box::new(QueueJobConversionCheckpoint {
				invariant: stage.clone(),
				progress: QueueJobScanProgress::Scanning { after_key: None },
			}));
		Ok(change)
	}

	pub(in crate::upgrade) fn record_queue_job_scan(
		&mut self,
		context: QueueJobCheckpointContext<'_>,
		progress: QueueJobScanProgress,
	) -> Result<(), UpgradeWorkError> {
		self.require_checkpoint_context(
			context.control,
			context.candidate,
			context.origin,
			context.expected,
		)?;
		self.require_queue_job_checkpoint_invariant(context.control.component_policies())?;
		if !self.is_unfinished() {
			return Err(UpgradeWorkError::TerminalWork);
		}
		let checkpoint = self
			.queue_job_checkpoint()
			.ok_or(QueueJobCheckpointError::CheckpointAbsent)?;
		let QueueJobScanProgress::Scanning {
			after_key: previous,
		} = &checkpoint.progress
		else {
			return Err(QueueJobCheckpointError::CursorDidNotAdvance.into());
		};
		let (next, complete) = match &progress {
			QueueJobScanProgress::Scanning { after_key } => (after_key, false),
			QueueJobScanProgress::Scanned { last_key } => (last_key, true),
		};
		if next < previous || (!complete && (next.is_none() || next == previous)) {
			return Err(QueueJobCheckpointError::CursorDidNotAdvance.into());
		}
		self.building_progress_mut()?
			.queue_job_checkpoint
			.as_mut()
			.expect("Queue checkpoint was checked")
			.progress = progress;
		Ok(())
	}

	pub(in crate::upgrade) fn finalize_queue_job_readers(
		&mut self,
		context: QueueJobCheckpointContext<'_>,
	) -> Result<UpgradeWorkGenerationChange, UpgradeWorkError> {
		self.require_queue_job_checkpoint_invariant(context.control.component_policies())?;
		let checkpoint = self
			.queue_job_checkpoint()
			.ok_or(QueueJobCheckpointError::CheckpointAbsent)?;
		if !matches!(checkpoint.progress, QueueJobScanProgress::Scanned { .. }) {
			return Err(QueueJobCheckpointError::ScanIncomplete.into());
		}
		let change = self.advance_effective_build(
			context.control,
			context.candidate,
			context.origin,
			context.expected,
		)?;
		self.building_progress_mut()?.queue_job_checkpoint = None;
		Ok(change)
	}
}

pub(super) fn validate_checkpoint(work: &UpgradeWork) -> Result<(), QueueJobCheckpointError> {
	let Some(checkpoint) = work.queue_job_checkpoint() else {
		return Ok(());
	};
	if matches!(
		work.status(),
		PgUpgradeStatus::Queued | PgUpgradeStatus::Completed
	) {
		return Err(QueueJobCheckpointError::InvalidStatus);
	}
	if policy_stage(&checkpoint.invariant)? != QueueJobPayloadStage::Separating
		|| !writer_stage(&checkpoint.invariant, work.queue_job_target()?)?
			.matches_policy(&checkpoint.invariant)
	{
		return Err(QueueJobCheckpointError::InvariantMismatch);
	}
	Ok(())
}
