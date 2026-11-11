//! Typed import of recorded published-compatible Queue layouts without granting authority.
//!
//! The owning source adapter must establish coherent physical observation, exact layout,
//! source-aware admission, and retirement before persisting retained running state.

use crate::installation::contract::{
	BoundComponentContract, ComponentPolicy, ComponentPolicyFields, ComponentSettings,
	ComponentSettingsRequirement, SemanticContractDef, StateContractError,
};
use crate::queue::state::{
	JobLifecycle, NativeJob, NativeJobMetadata, NativeJobParts, NativeJobPayload,
	NativePayloadEncoding, QueueJobSchemaBinding, QueueStateError, RunningJobClaim,
};
use crate::queue::{
	DedupeKey, JobId, JobStatus, StoredJobLifecycleFields, StoredJobTimeout, TaskName,
	WorkerClaimToken, WorkerOwnerId,
};
use std::time::Duration;

const SOURCE_COMPONENT: &str = "paranoid.queue.job.source.v10";
const SOURCE_SEMANTICS: &str = "paranoid.queue.job.source.v10.import.v1";
const PREPARED_SOURCE_SEMANTICS: &str = "paranoid.queue.job.source.prepared.v2.import.v1";

/// Exact source TIMESTAMPTZ domain before native range checking; no floating-point conversion.
pub(super) enum QueueSourceTimestamp {
	NegativeInfinity,
	FiniteUnixMicros(i128),
	PositiveInfinity,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum QueueSourceTimestampField {
	RunAtOrAfter,
	ClaimedByWorkerAt,
	ExecutionStartedAt,
	ExecutionHeartbeatAt,
	FinishedAt,
	CreatedAt,
	UpdatedAt,
	DeadLetteredAt,
}

impl QueueSourceTimestamp {
	pub(super) fn into_native(
		self,
		field: QueueSourceTimestampField,
	) -> Result<i64, QueueSourceImportError> {
		match self {
			Self::FiniteUnixMicros(value) => i64::try_from(value)
				.map_err(|_| QueueSourceImportError::TimestampUnrepresentable { field }),
			Self::NegativeInfinity | Self::PositiveInfinity => {
				Err(QueueSourceImportError::TimestampUnrepresentable { field })
			}
		}
	}
}

/// Parsed published physical fields. Signed integer and timestamp domains remain exact.
/// This value carries neither private claim material nor proof of source retirement.
pub(in crate::queue) struct QueueSourceJob {
	pub(super) id: JobId,
	pub(super) task_name: TaskName,
	pub(super) payload_json: String,
	pub(super) status: JobStatus,
	pub(super) run_at_or_after: QueueSourceTimestamp,
	pub(super) last_error_msg: Option<String>,
	pub(super) retry_count: i32,
	pub(super) max_retries: i32,
	pub(super) timeout_nanos: i64,
	pub(super) dedupe_key: Option<DedupeKey>,
	pub(super) worker_owner_id: Option<WorkerOwnerId>,
	pub(super) claimed_by_worker_at: Option<QueueSourceTimestamp>,
	pub(super) execution_started_at: Option<QueueSourceTimestamp>,
	pub(super) execution_heartbeat_at: Option<QueueSourceTimestamp>,
	pub(super) finished_at: Option<QueueSourceTimestamp>,
	pub(super) created_at: QueueSourceTimestamp,
	pub(super) updated_at: QueueSourceTimestamp,
}

impl std::fmt::Debug for QueueSourceJob {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter
			.debug_struct("QueueSourceJob")
			.field("status", &self.status)
			.finish_non_exhaustive()
	}
}

/// Exact prepared-layout fields plus its nullable parsed private token column.
/// The physical adapter must select this reader from the recorded layout, not body resemblance.
pub(in crate::queue) struct PreparedQueueSourceJob {
	pub(super) job: QueueSourceJob,
	pub(super) claim_token: Option<WorkerClaimToken>,
}
impl std::fmt::Debug for PreparedQueueSourceJob {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter
			.debug_struct("PreparedQueueSourceJob")
			.field("status", &self.job.status)
			.finish_non_exhaustive()
	}
}

/// Preserved working state includes the exact source modification time. The native job
/// encoder derives envelope metadata from that state; Keep preserves the source row intact.
/// No import result can construct a ClaimedJob or authenticate a mutation.
pub(in crate::queue) struct QueueSourceImport {
	pub(super) state: NativeJob,
}
impl std::fmt::Debug for QueueSourceImport {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter
			.debug_struct("QueueSourceImport")
			.finish_non_exhaustive()
	}
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum QueueSourceRetryField {
	RetryCount,
	MaxRetries,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum QueueSourceImportError {
	#[error("queue source reader differs from its bound physical-layout semantic contract")]
	ReaderContractMismatch,
	#[error("queue inactive prepared source row has unexpected private claim material")]
	InactivePrivateToken,
	#[error("queue source row belongs to another selected job")]
	IdentityMismatch,
	#[error("queue source retry value is outside the nonnegative signed source domain: {field:?}")]
	NegativeRetry { field: QueueSourceRetryField },
	#[error("queue source timeout is outside its published sentinel or positive domain")]
	InvalidTimeout,
	#[error("queue source timestamp cannot be preserved in the native domain: {field:?}")]
	TimestampUnrepresentable { field: QueueSourceTimestampField },
	#[error("queue source fields cannot form native state")]
	State(#[from] QueueStateError),
}

type QueueSourceImportFn =
	fn(JobId, QueueSourceJob) -> Result<QueueSourceImport, QueueSourceImportError>;
type PreparedQueueSourceImportFn =
	fn(JobId, PreparedQueueSourceJob) -> Result<QueueSourceImport, QueueSourceImportError>;

/// The v10 job readers of the published and prepared layouts. Every v10 payload is opaque
/// JSON.
pub(crate) struct QueueSourceBridge {
	import_fn: QueueSourceImportFn,
	prepared_import_fn: PreparedQueueSourceImportFn,
}
impl QueueSourceBridge {
	pub(crate) fn component_identifier() -> &'static str {
		SOURCE_COMPONENT
	}

	pub(in crate::queue) fn require_bound_layout(
		&self,
		contract: &BoundComponentContract<'_, '_, Self>,
		layout: crate::queue::PublishedV10QueueLayout,
	) -> Result<(), QueueSourceImportError> {
		if !std::ptr::eq(self, contract.transitions())
			|| contract.policy().component_identifier() != Self::component_identifier()
		{
			return Err(QueueSourceImportError::ReaderContractMismatch);
		}
		require_reader_contract(contract, self.semantic_contract(layout))
	}

	pub(crate) fn new() -> Self {
		Self {
			import_fn: import_published_job,
			prepared_import_fn: import_prepared_job,
		}
	}

	pub(in crate::queue) fn semantic_contract(
		&self,
		layout: crate::queue::PublishedV10QueueLayout,
	) -> &'static str {
		match layout {
			crate::queue::PublishedV10QueueLayout::PublishedV1 => SOURCE_SEMANTICS,
			crate::queue::PublishedV10QueueLayout::PreparedV2 => PREPARED_SOURCE_SEMANTICS,
		}
	}

	/// The complete retained registry must bind this metadata and its paired auxiliary policy.
	pub(super) fn initial_policy(
		&self,
		layout: crate::queue::PublishedV10QueueLayout,
	) -> Result<ComponentPolicy, StateContractError> {
		ComponentPolicy::new(ComponentPolicyFields {
			component_identifier: Self::component_identifier().to_owned(),
			semantic_contract: self.semantic_contract(layout).to_owned(),
			schemas: Vec::new(),
			settings: ComponentSettings::Parameterless,
		})
	}

	pub(super) fn semantic_defs(&self) -> [SemanticContractDef<'_, Self>; 2] {
		[
			SemanticContractDef {
				identifier: SOURCE_SEMANTICS,
				transitions: self,
				settings: ComponentSettingsRequirement::Parameterless,
			},
			SemanticContractDef {
				identifier: PREPARED_SOURCE_SEMANTICS,
				transitions: self,
				settings: ComponentSettingsRequirement::Parameterless,
			},
		]
	}
}

impl QueueJobSchemaBinding<'_> {
	/// Uses the exact native job binding while invoking the actual layout-bound importer.
	/// Source admission and physical coherence remain owning adapter responsibilities.
	pub(super) fn import_published_source(
		&self,
		bridge: &BoundComponentContract<'_, '_, QueueSourceBridge>,
		source: QueueSourceJob,
	) -> Result<QueueSourceImport, QueueSourceImportError> {
		let actual = bridge.transitions();
		actual.require_bound_layout(bridge, crate::queue::PublishedV10QueueLayout::PublishedV1)?;
		(actual.import_fn)(self.job_id(), source)
	}

	/// Preserves real prepared private material as state without constructing a worker grant.
	pub(super) fn import_prepared_source(
		&self,
		bridge: &BoundComponentContract<'_, '_, QueueSourceBridge>,
		source: PreparedQueueSourceJob,
	) -> Result<QueueSourceImport, QueueSourceImportError> {
		let actual = bridge.transitions();
		actual.require_bound_layout(bridge, crate::queue::PublishedV10QueueLayout::PreparedV2)?;
		(actual.prepared_import_fn)(self.job_id(), source)
	}
}

fn require_reader_contract(
	bridge: &BoundComponentContract<'_, '_, QueueSourceBridge>,
	expected: &str,
) -> Result<(), QueueSourceImportError> {
	if bridge.policy().semantic_contract() == expected {
		Ok(())
	} else {
		Err(QueueSourceImportError::ReaderContractMismatch)
	}
}

fn import_published_job(
	requested_id: JobId,
	source: QueueSourceJob,
) -> Result<QueueSourceImport, QueueSourceImportError> {
	let claim = if source.status == JobStatus::Running {
		Some(RunningJobClaim::RetiredPublished)
	} else {
		None
	};
	import_source_fields(requested_id, source, claim)
}

fn import_prepared_job(
	requested_id: JobId,
	source: PreparedQueueSourceJob,
) -> Result<QueueSourceImport, QueueSourceImportError> {
	let claim = match (source.job.status, source.claim_token) {
		(JobStatus::Running, Some(token)) => Some(RunningJobClaim::NativePrivate(token)),
		(JobStatus::Running, None) => Some(RunningJobClaim::RetiredPublished),
		(_, Some(_)) => return Err(QueueSourceImportError::InactivePrivateToken),
		(_, None) => None,
	};
	import_source_fields(requested_id, source.job, claim)
}

fn import_source_fields(
	requested_id: JobId,
	source: QueueSourceJob,
	claim: Option<RunningJobClaim>,
) -> Result<QueueSourceImport, QueueSourceImportError> {
	if source.id != requested_id {
		return Err(QueueSourceImportError::IdentityMismatch);
	}
	let retry_count = parse_source_retry(source.retry_count, QueueSourceRetryField::RetryCount)?;
	let max_retries = parse_source_retry(source.max_retries, QueueSourceRetryField::MaxRetries)?;
	let timeout = parse_source_timeout(source.timeout_nanos)?;
	let updated_at = source
		.updated_at
		.into_native(QueueSourceTimestampField::UpdatedAt)?;
	let run_at_or_after = source
		.run_at_or_after
		.into_native(QueueSourceTimestampField::RunAtOrAfter)?;
	let lifecycle_fields = StoredJobLifecycleFields {
		status: source.status,
		worker_owner_id: source.worker_owner_id,
		claimed_at: optional_time(
			source.claimed_by_worker_at,
			QueueSourceTimestampField::ClaimedByWorkerAt,
		)?,
		started_at: optional_time(
			source.execution_started_at,
			QueueSourceTimestampField::ExecutionStartedAt,
		)?,
		heartbeat_at: optional_time(
			source.execution_heartbeat_at,
			QueueSourceTimestampField::ExecutionHeartbeatAt,
		)?,
		finished_at: optional_time(source.finished_at, QueueSourceTimestampField::FinishedAt)?,
	};
	let created_at = source
		.created_at
		.into_native(QueueSourceTimestampField::CreatedAt)?;
	let state = NativeJob::new(NativeJobParts {
		job: NativeJobMetadata {
			id: source.id,
			task_name: source.task_name,
			lifecycle: JobLifecycle::parse(lifecycle_fields, claim)?,
			run_at_or_after_unix_micros: run_at_or_after,
			last_error_msg: source.last_error_msg,
			retry_count,
			max_retries,
			timeout,
			dedupe_key: source.dedupe_key,
			created_at_unix_micros: created_at,
			updated_at_unix_micros: updated_at,
		},
		payload: NativeJobPayload::InlineJson(source.payload_json),
		payload_encoding: NativePayloadEncoding::OpaqueJson {},
	})?;
	Ok(QueueSourceImport { state })
}

fn optional_time(
	time: Option<QueueSourceTimestamp>,
	field: QueueSourceTimestampField,
) -> Result<Option<i64>, QueueSourceImportError> {
	time.map(|time| time.into_native(field)).transpose()
}

/// Preserves the exact nonnegative signed source count without imposing retry ordering.
pub(super) fn parse_source_retry(
	value: i32,
	field: QueueSourceRetryField,
) -> Result<u32, QueueSourceImportError> {
	u32::try_from(value).map_err(|_| QueueSourceImportError::NegativeRetry { field })
}

/// Published sentinels and positive nanoseconds retain their exact stored timeout meaning.
pub(super) fn parse_source_timeout(
	timeout_nanos: i64,
) -> Result<StoredJobTimeout, QueueSourceImportError> {
	match timeout_nanos {
		0 => Ok(StoredJobTimeout::RegisteredDef),
		-1 => Ok(StoredJobTimeout::NoTimeout),
		positive if positive > 0 => Ok(StoredJobTimeout::ExpiresAfter(Duration::from_nanos(
			positive as u64,
		))),
		_ => Err(QueueSourceImportError::InvalidTimeout),
	}
}
