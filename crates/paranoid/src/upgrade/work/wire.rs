//! Strict complete targets and controller status/generation metadata.

use primitives_rs::encoding::decimal::parse_canonical_decimal;
use primitives_rs::id::time_sortable::SORTABLE_ID_LEN;
use primitives_rs::serialization::json::JsonDecodeFailureSummary;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::installation::contract::policy_wire::ComponentPolicyWire;

use super::super::policy::request::{PolicyUpgradeRequest, ScopesWire};
use super::{CompleteUpgradeTarget, PgUpgradeStatus, SourceGoal, UpgradeWork, UpgradeWorkError};

mod document;
mod payload;
mod queue;
mod v10;
use queue::QueueJobCheckpointWire;
mod verification;
use document::DocumentCheckpointWire;
use payload::PayloadCheckpointWire;
use v10::SourceCheckpointWire;
use verification::DocumentVerificationWire;

#[cfg(test)]
mod tests;

const TARGET_VERSION: u32 = 1;
const WORK_VERSION: u32 = 1;

#[derive(Deserialize)]
struct VersionHeader {
	version: u32,
}

#[derive(Deserialize)]
struct WorkTargetHeader {
	target: VersionHeader,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum TargetKind {
	CompletePolicyTarget,
}

/// The stored names of the source goals, independent of the Rust names.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum SourceGoalWire {
	Unchanged,
	WritersRetired,
	DataRetired,
}

impl SourceGoalWire {
	fn from_goal(goal: SourceGoal) -> Self {
		match goal {
			SourceGoal::Unchanged => Self::Unchanged,
			SourceGoal::WritersRetired => Self::WritersRetired,
			SourceGoal::DataRetired => Self::DataRetired,
		}
	}
	fn into_goal(self) -> SourceGoal {
		match self {
			Self::Unchanged => SourceGoal::Unchanged,
			Self::WritersRetired => SourceGoal::WritersRetired,
			Self::DataRetired => SourceGoal::DataRetired,
		}
	}
}

/// The stored names of the upgrade statuses, independent of the public names.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum StatusWire {
	Queued,
	Building,
	Completed,
	Cancelled,
}

impl StatusWire {
	fn from_status(status: PgUpgradeStatus) -> Self {
		match status {
			PgUpgradeStatus::Queued => Self::Queued,
			PgUpgradeStatus::Building => Self::Building,
			PgUpgradeStatus::Completed => Self::Completed,
			PgUpgradeStatus::Cancelled => Self::Cancelled,
		}
	}
	fn into_status(self) -> PgUpgradeStatus {
		match self {
			Self::Queued => PgUpgradeStatus::Queued,
			Self::Building => PgUpgradeStatus::Building,
			Self::Completed => PgUpgradeStatus::Completed,
			Self::Cancelled => PgUpgradeStatus::Cancelled,
		}
	}
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TargetWire<'a> {
	kind: TargetKind,
	version: u32,
	request_id: [u8; SORTABLE_ID_LEN],
	installation_id: [u8; SORTABLE_ID_LEN],
	base_generation: String,
	policies: Vec<ComponentPolicyWire<'a>>,
	scopes: ScopesWire<'a>,
	source_goal: SourceGoalWire,
}

impl<'a> TargetWire<'a> {
	fn from_target(target: &'a CompleteUpgradeTarget) -> Self {
		let request = &target.request;
		Self {
			kind: TargetKind::CompletePolicyTarget,
			version: TARGET_VERSION,
			request_id: *request.identity().request_id.as_bytes(),
			installation_id: *request.identity().installation_id.as_bytes(),
			base_generation: request.identity().base_generation.get().to_string(),
			policies: request
				.policies()
				.iter()
				.map(ComponentPolicyWire::from_policy)
				.collect(),
			scopes: ScopesWire::from_request(request.scopes()),
			source_goal: SourceGoalWire::from_goal(target.source_goal),
		}
	}
}

impl TargetWire<'static> {
	fn into_target(self) -> Result<CompleteUpgradeTarget, UpgradeWorkError> {
		require_target_version(self.version)?;
		let scopes = self.scopes.into_request()?;
		Ok(CompleteUpgradeTarget {
			request: PolicyUpgradeRequest::from_wire_parts(
				self.request_id,
				self.installation_id,
				self.base_generation,
				self.policies,
				scopes,
			)?,
			source_goal: self.source_goal.into_goal(),
		})
	}
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum WorkKind {
	InstallationUpgradeWork,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkWire<'a> {
	kind: WorkKind,
	version: u32,
	target: TargetWire<'a>,
	owned_generation: String,
	status: StatusWire,
	document_conversion_checkpoints: Vec<DocumentCheckpointWire<'a>>,
	document_verification_checkpoints: Vec<DocumentVerificationWire<'a>>,
	payload_checkpoints: Vec<PayloadCheckpointWire<'a>>,
	source_checkpoints: Vec<SourceCheckpointWire<'a>>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	queue_job_checkpoint: Option<QueueJobCheckpointWire<'a>>,
}

pub(super) fn encode_work(work: &UpgradeWork) -> Result<Box<RawValue>, UpgradeWorkError> {
	super::queue_checkpoint::validate_checkpoint(work)?;
	super::payload_checkpoint::validate_checkpoints(
		&work.target,
		work.status(),
		work.payload_checkpoints(),
	)?;
	super::document_verification::validate_checkpoints(
		&work.target,
		work.status(),
		work.document_verification_checkpoints(),
		work.document_conversion_checkpoints(),
	)?;
	super::document_checkpoint::validate_checkpoints(
		&work.target,
		work.status(),
		work.document_conversion_checkpoints(),
	)?;
	super::v10::source_checkpoint::validate_checkpoints(
		&work.target,
		work.status(),
		work.source_checkpoints(),
	)?;
	serde_json::value::to_raw_value(&WorkWire {
		kind: WorkKind::InstallationUpgradeWork,
		version: if work.queue_job_checkpoint().is_some() {
			2
		} else {
			WORK_VERSION
		},
		queue_job_checkpoint: work
			.queue_job_checkpoint()
			.map(QueueJobCheckpointWire::from_checkpoint),
		target: TargetWire::from_target(&work.target),
		owned_generation: work.owned_generation().get().to_string(),
		status: StatusWire::from_status(work.status()),
		document_conversion_checkpoints: work
			.document_conversion_checkpoints()
			.iter()
			.map(DocumentCheckpointWire::from_checkpoint)
			.collect(),
		document_verification_checkpoints: work
			.document_verification_checkpoints()
			.iter()
			.map(DocumentVerificationWire::from_checkpoint)
			.collect(),
		payload_checkpoints: work
			.payload_checkpoints()
			.iter()
			.map(PayloadCheckpointWire::from_checkpoint)
			.collect(),
		source_checkpoints: work
			.source_checkpoints()
			.iter()
			.map(SourceCheckpointWire::from_checkpoint)
			.collect(),
	})
	.map_err(json_error)
}

pub(super) fn decode_work(json: &RawValue) -> Result<UpgradeWork, UpgradeWorkError> {
	let version = work_version(json)?;
	if !matches!(version, WORK_VERSION | 2) {
		return Err(UpgradeWorkError::UnsupportedVersion { version });
	}
	let target_header: WorkTargetHeader = serde_json::from_str(json.get()).map_err(json_error)?;
	require_target_version(target_header.target.version)?;
	let wire: WorkWire<'static> = serde_json::from_str(json.get()).map_err(json_error)?;
	if (version == 2) != wire.queue_job_checkpoint.is_some() {
		return Err(UpgradeWorkError::InvalidGenerationState);
	}
	let queue_job_checkpoint = wire
		.queue_job_checkpoint
		.map(QueueJobCheckpointWire::into_checkpoint)
		.transpose()?
		.map(Box::new);
	let target = wire.target.into_target()?;
	let encoded_generation = wire.owned_generation;
	let status = wire.status.into_status();
	let document_conversion_checkpoints = wire
		.document_conversion_checkpoints
		.into_iter()
		.map(DocumentCheckpointWire::into_checkpoint)
		.collect::<Result<Vec<_>, _>>()?;
	let document_verification_checkpoints = wire
		.document_verification_checkpoints
		.into_iter()
		.map(DocumentVerificationWire::into_checkpoint)
		.collect::<Result<Vec<_>, _>>()?;
	let source_checkpoints = wire
		.source_checkpoints
		.into_iter()
		.map(SourceCheckpointWire::into_checkpoint)
		.collect::<Result<Vec<_>, _>>()?;
	let payload_checkpoints = wire
		.payload_checkpoints
		.into_iter()
		.map(PayloadCheckpointWire::into_checkpoint)
		.collect::<Result<Vec<_>, _>>()?;
	let value = parse_canonical_decimal::<u64>(&encoded_generation)
		.map_err(|_| UpgradeWorkError::InvalidOwnedGeneration)?;
	let owned_generation =
		std::num::NonZeroU64::new(value).ok_or(UpgradeWorkError::InvalidOwnedGeneration)?;
	if owned_generation < target.request.identity().base_generation
		|| (status == PgUpgradeStatus::Queued
			&& owned_generation != target.request.identity().base_generation)
	{
		return Err(UpgradeWorkError::InvalidGenerationState);
	}
	super::document_checkpoint::validate_checkpoints(
		&target,
		status,
		&document_conversion_checkpoints,
	)?;
	super::document_verification::validate_checkpoints(
		&target,
		status,
		&document_verification_checkpoints,
		&document_conversion_checkpoints,
	)?;
	super::v10::source_checkpoint::validate_checkpoints(&target, status, &source_checkpoints)?;
	super::payload_checkpoint::validate_checkpoints(&target, status, &payload_checkpoints)?;
	if matches!(status, PgUpgradeStatus::Queued | PgUpgradeStatus::Completed)
		&& queue_job_checkpoint.is_some()
	{
		return Err(super::queue_checkpoint::QueueJobCheckpointError::InvalidStatus.into());
	}
	let progress = super::UpgradeWorkProgress {
		owned_generation,
		document_conversion_checkpoints,
		document_verification_checkpoints,
		payload_checkpoints,
		source_checkpoints,
		queue_job_checkpoint,
	};
	let state = match status {
		PgUpgradeStatus::Queued => super::UpgradeWorkState::Queued,
		PgUpgradeStatus::Building => super::UpgradeWorkState::Building(progress),
		PgUpgradeStatus::Completed => super::UpgradeWorkState::Completed { owned_generation },
		PgUpgradeStatus::Cancelled => super::UpgradeWorkState::Cancelled(progress),
	};
	let work = UpgradeWork { target, state };
	super::queue_checkpoint::validate_checkpoint(&work)?;
	Ok(work)
}

fn work_version(json: &RawValue) -> Result<u32, UpgradeWorkError> {
	let header: VersionHeader = serde_json::from_str(json.get()).map_err(json_error)?;
	Ok(header.version)
}
fn require_target_version(version: u32) -> Result<(), UpgradeWorkError> {
	if version != TARGET_VERSION {
		return Err(UpgradeWorkError::UnsupportedVersion { version });
	}
	Ok(())
}

fn json_error(error: serde_json::Error) -> UpgradeWorkError {
	let summary = JsonDecodeFailureSummary::from_serde_error(&error);
	UpgradeWorkError::Json {
		category: summary.category(),
		line: summary.line(),
		column: summary.column(),
	}
}
