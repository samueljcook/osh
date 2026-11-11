//! Actual typed published-compatible dead-letter and pause readers.
//!
//! Physical observation, recorded layout/association, source-aware admission, and atomic
//! persistence belong to the owning adapter. These readers provide no phase or authority proof.

use super::source_bridge::{
	QueueSourceImportError, QueueSourceRetryField, QueueSourceTimestamp, QueueSourceTimestampField,
	parse_source_retry, parse_source_timeout,
};
use crate::installation::contract::{
	BoundComponentContract, ComponentPolicy, ComponentPolicyFields, ComponentSettings,
	ComponentSettingsRequirement, SemanticContractDef, StateContractError,
};
use crate::queue::auxiliary_state::{
	AuxiliaryStateError, NativeDeadLetterState, QueuePauseScope, QueuePauseState,
};
use crate::queue::state::NativePayloadEncoding;
use crate::queue::{
	DeadLetterJob, DeadLetterReason, DedupeKey, GLOBAL_PAUSE_KEY, JobId, TaskName, paused_task_key,
};
use crate::records::{RecordMetadata, StateTimestampMicros};
use std::fmt;

const COMPONENT: &str = "paranoid.queue.auxiliary.source.v10";
const SEMANTICS: &str = "paranoid.queue.auxiliary.source.v10.import.v1";

/// Exact parsed SQL facts; counts and retained source timestamps keep their source domains.
pub(in crate::queue) struct QueueSourceDeadLetter {
	pub(super) id: JobId,
	pub(super) original_job_id: JobId,
	pub(super) task_name: TaskName,
	pub(super) payload_json: String,
	pub(super) last_error_msg: String,
	pub(super) retry_count: i32,
	pub(super) max_retries: i32,
	pub(super) timeout_nanos: i64,
	pub(super) dedupe_key: Option<DedupeKey>,
	pub(super) reason: DeadLetterReason,
	pub(super) dead_lettered_at: QueueSourceTimestamp,
	pub(super) created_at: QueueSourceTimestamp,
	pub(super) updated_at: QueueSourceTimestamp,
}
impl fmt::Debug for QueueSourceDeadLetter {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("QueueSourceDeadLetter { .. }")
	}
}

/// Pause presence with its exact key/task association and observable modification time.
/// The unobserved paused_at column has no historical field in this input or native state.
pub(in crate::queue) struct QueueSourcePause {
	pub(super) key: String,
	pub(super) task_name: Option<TaskName>,
	pub(super) updated_at: QueueSourceTimestamp,
}
impl fmt::Debug for QueueSourcePause {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("QueueSourcePause { .. }")
	}
}

pub(super) struct QueueSourceDeadLetterRequest {
	pub(super) dead_letter_id: JobId,
	pub(super) source: QueueSourceDeadLetter,
}
pub(super) struct QueueSourcePauseRequest {
	pub(super) scope: QueuePauseScope,
	pub(super) source: QueueSourcePause,
}

/// Preserved working state and source modification metadata, without mutation or commit proof.
pub(super) struct QueueAuxiliarySourceImport<T> {
	pub(super) state: T,
	pub(super) metadata: RecordMetadata,
}
impl<T> fmt::Debug for QueueAuxiliarySourceImport<T> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("QueueAuxiliarySourceImport { .. }")
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum QueueAuxiliarySourceImportError {
	#[error("Queue auxiliary source reader has a different bound declaration or semantic contract")]
	ReaderBindingMismatch,
	#[error("Queue source dead letter belongs to another selected identity")]
	DeadLetterIdentityMismatch,
	#[error("Queue source pause key, task, and requested scope disagree")]
	PauseScopeMismatch,
	#[error("Queue source auxiliary field cannot be preserved in its domain")]
	Domain(#[from] QueueSourceImportError),
	#[error("Queue source auxiliary fields cannot form native working state")]
	State(#[from] AuxiliaryStateError),
}

type DeadLetterImportFn = fn(
	QueueSourceDeadLetterRequest,
) -> Result<
	QueueAuxiliarySourceImport<NativeDeadLetterState>,
	QueueAuxiliarySourceImportError,
>;
type PauseImportFn =
	fn(
		QueueSourcePauseRequest,
	) -> Result<QueueAuxiliarySourceImport<QueuePauseState>, QueueAuxiliarySourceImportError>;

/// Separate actual auxiliary readers; registration does not broaden job-only source support.
pub(crate) struct QueueAuxiliarySourceBridge {
	import_dead_letter_fn: DeadLetterImportFn,
	pause_fn: PauseImportFn,
}
pub(crate) struct BoundQueueAuxiliarySourceBridge<'a, 'p, 'd> {
	contract: &'a BoundComponentContract<'p, 'd, QueueAuxiliarySourceBridge>,
}

impl QueueAuxiliarySourceBridge {
	pub(crate) fn component_identifier() -> &'static str {
		COMPONENT
	}

	pub(crate) fn new() -> Self {
		Self {
			import_dead_letter_fn: import_dead_letter,
			pause_fn: import_pause,
		}
	}
	pub(in crate::queue) fn semantic_contract(&self) -> &'static str {
		SEMANTICS
	}

	/// The complete retained registry must bind this metadata and its paired job policy.
	pub(super) fn initial_policy(&self) -> Result<ComponentPolicy, StateContractError> {
		ComponentPolicy::new(ComponentPolicyFields {
			component_identifier: Self::component_identifier().to_owned(),
			semantic_contract: self.semantic_contract().to_owned(),
			schemas: Vec::new(),
			settings: ComponentSettings::Parameterless,
		})
	}
	pub(super) fn semantic_defs(&self) -> [SemanticContractDef<'_, Self>; 1] {
		[SemanticContractDef {
			identifier: SEMANTICS,
			transitions: self,
			settings: ComponentSettingsRequirement::Parameterless,
		}]
	}
	pub(crate) fn bind<'a, 'p, 'd>(
		&'a self,
		contract: &'a BoundComponentContract<'p, 'd, Self>,
	) -> Result<BoundQueueAuxiliarySourceBridge<'a, 'p, 'd>, QueueAuxiliarySourceImportError> {
		if !std::ptr::eq(self, contract.transitions())
			|| contract.policy().component_identifier() != Self::component_identifier()
			|| contract.policy().semantic_contract() != SEMANTICS
		{
			return Err(QueueAuxiliarySourceImportError::ReaderBindingMismatch);
		}
		Ok(BoundQueueAuxiliarySourceBridge { contract })
	}
}
impl BoundQueueAuxiliarySourceBridge<'_, '_, '_> {
	/// Only the owning physical adapter can establish that these are authenticated source facts.
	pub(super) fn import_dead_letter(
		&self,
		request: QueueSourceDeadLetterRequest,
	) -> Result<QueueAuxiliarySourceImport<NativeDeadLetterState>, QueueAuxiliarySourceImportError>
	{
		(self.contract.transitions().import_dead_letter_fn)(request)
	}
	pub(super) fn import_pause(
		&self,
		request: QueueSourcePauseRequest,
	) -> Result<QueueAuxiliarySourceImport<QueuePauseState>, QueueAuxiliarySourceImportError> {
		(self.contract.transitions().pause_fn)(request)
	}
}

fn import_dead_letter(
	request: QueueSourceDeadLetterRequest,
) -> Result<QueueAuxiliarySourceImport<NativeDeadLetterState>, QueueAuxiliarySourceImportError> {
	let QueueSourceDeadLetterRequest {
		dead_letter_id,
		source,
	} = request;
	if source.id != dead_letter_id {
		return Err(QueueAuxiliarySourceImportError::DeadLetterIdentityMismatch);
	}
	let retry_count = parse_source_retry(source.retry_count, QueueSourceRetryField::RetryCount)?;
	let max_retries = parse_source_retry(source.max_retries, QueueSourceRetryField::MaxRetries)?;
	let timeout = parse_source_timeout(source.timeout_nanos)?;
	let updated_at = source
		.updated_at
		.into_native(QueueSourceTimestampField::UpdatedAt)?;
	let state = NativeDeadLetterState::new(
		DeadLetterJob {
			id: source.id,
			original_job_id: source.original_job_id,
			task_name: source.task_name,
			payload_json: source.payload_json,
			last_error_msg: source.last_error_msg,
			retry_count,
			max_retries,
			timeout,
			dedupe_key: source.dedupe_key,
			reason: source.reason,
			dead_lettered_at_unix_micros: source
				.dead_lettered_at
				.into_native(QueueSourceTimestampField::DeadLetteredAt)?,
			created_at_unix_micros: source
				.created_at
				.into_native(QueueSourceTimestampField::CreatedAt)?,
			updated_at_unix_micros: updated_at,
		},
		NativePayloadEncoding::OpaqueJson {},
	)?;
	Ok(QueueAuxiliarySourceImport {
		state,
		metadata: metadata(updated_at),
	})
}
fn import_pause(
	request: QueueSourcePauseRequest,
) -> Result<QueueAuxiliarySourceImport<QueuePauseState>, QueueAuxiliarySourceImportError> {
	let QueueSourcePauseRequest { scope, source } = request;
	if parse_source_pause_scope(&source.key, source.task_name.as_ref())? != scope {
		return Err(QueueAuxiliarySourceImportError::PauseScopeMismatch);
	}
	let updated_at = source
		.updated_at
		.into_native(QueueSourceTimestampField::UpdatedAt)?;
	Ok(QueueAuxiliarySourceImport {
		state: QueuePauseState { scope },
		metadata: metadata(updated_at),
	})
}

/// The exact published source identity; native pause keys use a separate representation.
pub(crate) fn source_pause_key(scope: &QueuePauseScope) -> String {
	match scope {
		QueuePauseScope::Global => GLOBAL_PAUSE_KEY.to_owned(),
		QueuePauseScope::Task(task) => paused_task_key(task),
	}
}

/// Parses the full source key/task association without inferring a scope from an unknown key.
pub(crate) fn parse_source_pause_scope(
	key: &str,
	task: Option<&TaskName>,
) -> Result<QueuePauseScope, QueueAuxiliarySourceImportError> {
	let scope = match task {
		None => QueuePauseScope::Global,
		Some(task) => QueuePauseScope::Task(task.clone()),
	};
	if key != source_pause_key(&scope) {
		return Err(QueueAuxiliarySourceImportError::PauseScopeMismatch);
	}
	Ok(scope)
}
fn metadata(updated_at: i64) -> RecordMetadata {
	RecordMetadata {
		expires_at: None,
		updated_at: StateTimestampMicros::new(updated_at),
	}
}
