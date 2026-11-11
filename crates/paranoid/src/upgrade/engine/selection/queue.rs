//! Queue representation steps follow source retirement and retain old readers during conversion.

use super::{
	InstallationStorageRoute, NativeUpgradeSelection, NativeUpgradeSelectionError,
	NativeUpgradeSelectionFields, PgUpgradeStep, UpgradeWorkError,
};
use crate::queue::{
	QueueJobComponent,
	upgrade::{QueueJobPayloadStage, policy_stage, writer_stage},
};
use crate::upgrade::work::queue_checkpoint::{QueueJobCheckpointError, QueueJobScanProgress};

pub(super) fn collect(
	selection: &mut NativeUpgradeSelection,
	fields: &NativeUpgradeSelectionFields<'_>,
) -> Result<(), NativeUpgradeSelectionError> {
	if !matches!(
		fields.observation.route(),
		InstallationStorageRoute::NativeFresh | InstallationStorageRoute::NativeImported
	) {
		return Ok(());
	}
	let work = fields
		.observation
		.work()
		.ok_or(NativeUpgradeSelectionError::WorkAbsent)?;
	let target = work.queue_job_target().map_err(UpgradeWorkError::from)?;
	let current = fields
		.observation
		.control()
		.component(QueueJobComponent::component_identifier())
		.ok_or(UpgradeWorkError::from(
			QueueJobCheckpointError::ComponentAbsent,
		))?;
	if current.matches_policy(target) && work.queue_job_checkpoint().is_none() {
		return Ok(());
	}
	writer_stage(current, target)?;
	match (policy_stage(current)?, work.queue_job_checkpoint()) {
		(QueueJobPayloadStage::Inline, _) | (QueueJobPayloadStage::Separating, None) => {
			selection.steps.push(PgUpgradeStep::PromoteQueueJobWriter)
		}
		(QueueJobPayloadStage::Separating, Some(checkpoint)) => {
			selection.steps.push(match checkpoint.progress() {
				QueueJobScanProgress::Scanning { .. } => PgUpgradeStep::ConvertQueueJobs,
				QueueJobScanProgress::Scanned { .. } => PgUpgradeStep::FinalizeQueueJobReaders,
			})
		}
		_ => return Err(crate::queue::upgrade::QueueJobUpgradeError::UnsupportedPolicy.into()),
	}
	Ok(())
}

pub(super) fn is_final(fields: &NativeUpgradeSelectionFields<'_>) -> bool {
	fields
		.observation
		.work()
		.and_then(|work| work.queue_job_target().ok())
		.is_some_and(|target| {
			fields
				.observation
				.control()
				.component(QueueJobComponent::component_identifier())
				.is_some_and(|current| current.matches_policy(target))
		})
}
