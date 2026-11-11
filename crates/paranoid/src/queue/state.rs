//! Native Queue job schemas and projections, independent of physical storage.
//!
//! Payloads remain opaque JSON text. Private running state retains secret claim material;
//! retired published running state retains occupancy without authority. This parser never
//! generates authority or interprets an application's payload.

use super::{
	DedupeKey, Job, JobId, JobStatus, StoredJobLifecycleFields, StoredJobTimeout, TaskName,
	WORKER_CLAIM_TOKEN_LEN, WorkerClaimToken, WorkerOwnerId,
};
use crate::installation::contract::{
	BoundComponentContract, BoundStateSchema, ComponentDeclaration, ComponentDeclarationFields,
	ComponentPolicy, ComponentPolicyFields, ComponentSettingsBindingError,
	ComponentSettingsDeclaration, ComponentSettingsRequirement, ComponentSettingsSchema,
	ContractStateReadError, RepresentationPolicy, SchemaPolicy, SemanticContractDef,
	StateContractError, StateStorageEnvelope,
};
use crate::records::state_schema::{
	StateRepresentationFormat, StateSchema, StateSchemaError, StateVersion,
};
use crate::records::{
	EncodedRecord, IndexFields, IndexScalar, PointRecordRead, RecordMetadata, StateTimestampMicros,
};
use primitives_rs::encoding::decimal::parse_canonical_decimal;
use primitives_rs::serialization::json::{
	JsonDecodeFailureSummary, deserialize_required_option, exceeds_max_nesting_depth,
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::fmt;
use std::time::Duration;
use zeroize::Zeroize;

mod lifecycle;
pub(super) use lifecycle::{JobExecution, JobLifecycle, RunningJob, RunningJobClaim};
mod metadata;
pub(crate) mod upgrade;
pub(super) use metadata::NativeJobMetadata;
mod stored_record;
pub(super) use stored_record::{NativeJobPayloadRecord, NativeJobRecord};
mod operator;
pub(super) use operator::QueueJobOperatorPreparation;
pub(in crate::queue) use operator::{QueueJobOperatorAction, QueueJobOperatorOutcome};

const JOB_SCHEMA: &str = "paranoid.queue.job";
const JOB_REPRESENTATION_VERSION: u32 = 2;
const JOB_SEMANTICS: &str = "paranoid.queue.job.v1";
const MAX_PAYLOAD_JSON_NESTING_DEPTH: usize = 100;

/// Explicit transport meaning of an application's JSON payload.
#[derive(Clone, Eq, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum NativePayloadEncoding {
	/// Untyped JSON transport without an application schema or conversion contract.
	OpaqueJson {},
	/// JSON payload assigned explicitly to one application's registered source reader.
	SourceSchema {
		schema_identifier: String,
		representation_version: u32,
	},
	/// Declares an application schema/version/state document for typed decoding.
	NativeSchemaDocument {},
}

impl NativePayloadEncoding {
	pub(super) fn check(&self) -> Result<(), QueueStateError> {
		if let Self::SourceSchema {
			schema_identifier,
			representation_version,
		} = self
		{
			if schema_identifier.is_empty() {
				return Err(invalid(
					"source payload schema identifier must not be empty",
				));
			}
			if *representation_version == 0 {
				return Err(invalid(
					"source payload representation version must be positive",
				));
			}
		}
		Ok(())
	}
}

impl fmt::Debug for NativePayloadEncoding {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(match self {
			Self::OpaqueJson {} => "OpaqueJson",
			Self::SourceSchema { .. } => "SourceSchema",
			Self::NativeSchemaDocument {} => "NativeSchemaDocument",
		})
	}
}

/// A native job rejected during parsing, construction, or envelope interpretation.
#[derive(Debug, thiserror::Error)]
pub(crate) enum QueueStateError {
	#[error("queue job JSON is invalid: {summary}")]
	Json { summary: JsonDecodeFailureSummary },
	#[error("queue job state is invalid: {reason}")]
	Invalid { reason: &'static str },
}

/// All observable job fields and optional native private authority.
#[derive(Clone, Eq, PartialEq)]
pub(in crate::queue) struct NativeJob {
	job: NativeJobMetadata,
	payload: NativeJobPayload,
	payload_encoding: NativePayloadEncoding,
}

#[derive(Clone, Eq, PartialEq)]
pub(super) enum NativeJobPayload {
	InlineJson(String),
	SeparateRecord,
}

/// All native semantic fields transferred together through worker transitions.
pub(super) struct NativeJobParts {
	pub(super) job: NativeJobMetadata,
	pub(super) payload: NativeJobPayload,
	pub(super) payload_encoding: NativePayloadEncoding,
}

impl NativeJob {
	/// Parses a working value without creating claim material or guessing missing fields.
	pub(super) fn new(parts: NativeJobParts) -> Result<Self, QueueStateError> {
		let state = Self {
			job: parts.job,
			payload: parts.payload,
			payload_encoding: parts.payload_encoding,
		};
		state.check_shape()?;
		Ok(state)
	}

	pub(super) fn job(&self) -> &NativeJobMetadata {
		&self.job
	}

	pub(super) fn public_job(&self, payload_json: String) -> Job {
		self.job.with_payload(payload_json)
	}

	pub(super) fn payload(&self) -> &NativeJobPayload {
		&self.payload
	}

	pub(super) fn claim_token(&self) -> Option<&WorkerClaimToken> {
		match &self.job.lifecycle.running()?.claim {
			RunningJobClaim::NativePrivate(token) => Some(token),
			RunningJobClaim::RetiredPublished => None,
		}
	}

	fn ownership_wire(&self) -> OwnershipWire {
		match self.job.lifecycle.running().map(|running| &running.claim) {
			Some(RunningJobClaim::NativePrivate(_)) => OwnershipWire::NativePrivate {},
			Some(RunningJobClaim::RetiredPublished) => OwnershipWire::RetiredPublished {},
			None => OwnershipWire::Inactive {},
		}
	}

	pub(super) fn into_parts(self) -> NativeJobParts {
		NativeJobParts {
			job: self.job,
			payload: self.payload,
			payload_encoding: self.payload_encoding,
		}
	}

	fn check_shape(&self) -> Result<(), QueueStateError> {
		let job = &self.job;
		self.payload_encoding.check()?;
		if let NativeJobPayload::InlineJson(json) = &self.payload {
			check_payload_json(json)?;
		}
		if job.max_retries > super::MAX_RETRIES {
			return Err(invalid("job max_retries exceeds MAX_RETRIES"));
		}
		let max_retry_count = match job.status() {
			JobStatus::Failed => super::MAX_RETRIES + 1,
			JobStatus::Pending | JobStatus::Running | JobStatus::Completed => super::MAX_RETRIES,
		};
		if job.retry_count > max_retry_count {
			return Err(invalid("job retry_count exceeds the bound for its status"));
		}
		if matches!(job.timeout, StoredJobTimeout::ExpiresAfter(duration) if duration.is_zero()) {
			return Err(invalid("a job-specific timeout must be positive"));
		}
		Ok(())
	}
}

impl fmt::Debug for NativeJob {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("NativeJob")
			.field("status", &self.job.status())
			.field(
				"inline_payload_len",
				&match &self.payload {
					NativeJobPayload::InlineJson(json) => Some(json.len()),
					NativeJobPayload::SeparateRecord => None,
				},
			)
			.field(
				"ownership",
				&match self.ownership_wire() {
					OwnershipWire::Inactive {} => "Inactive",
					OwnershipWire::NativePrivate {} => "NativePrivate",
					OwnershipWire::RetiredPublished {} => "RetiredPublished",
				},
			)
			.field("payload_encoding", &self.payload_encoding)
			.finish()
	}
}

/// One shared immutable job registry and its actual transition implementation.
pub(crate) struct QueueJobComponent {
	schema: StateSchema<NativeJobRecord, QueueStateError>,
	recovery_settings: ComponentSettingsSchema<
		super::record_transitions::QueueRecoveryPolicy,
		super::recovery_settings::QueueRecoverySettingsError,
	>,
}

pub(crate) struct QueueJobInitialPolicyFields<'a> {
	pub(crate) recovery: &'a super::record_transitions::QueueRecoveryPolicy,
	pub(crate) representation: upgrade::QueueJobRepresentation,
}

/// Exact job identity and representation policy bound to the selected transition declaration.
pub(in crate::queue) struct QueueJobSchemaBinding<'a> {
	component: &'a QueueJobComponent,
	job_id: JobId,
	schema: BoundStateSchema<'a, NativeJobRecord, QueueStateError>,
	recovery_policy: &'a super::record_transitions::QueueRecoveryPolicy,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum QueueJobBindingError {
	#[error("queue recovery settings do not match the selected declaration")]
	Settings(#[from] ComponentSettingsBindingError),
	#[error("queue job schema policy binding failed")]
	Contract(#[from] StateContractError),
	#[error("queue job schema and selected transitions belong to different instances")]
	TransitionInstanceMismatch,
	#[error("native queue state requires a native JSON writer")]
	NativeWriterRequired,
	#[error("queue job selected semantic contract differs from its declaration")]
	SemanticContractMismatch,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum QueueRepresentationError {
	#[error("queue job stored projections differ from its decoded state")]
	ProjectionMismatch,
	#[error("queue state writer binding failed")]
	Binding(#[from] QueueJobBindingError),
	#[error("queue state cannot be read under its bound representation policy")]
	Read(#[from] ContractStateReadError<QueueStateError>),
	#[error("queue state cannot encode its selected representation")]
	Encode(#[from] StateSchemaError<QueueStateError>),
	#[error("queue state or physical metadata is invalid")]
	State(#[from] QueueStateError),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum BoundJobTransitionError {
	#[error("queue transition has a different selected job identity")]
	Identity(#[source] QueueStateError),
	#[error("queue transition rejected its state or arithmetic")]
	Transition(#[from] super::record_transitions::JobTransitionError),
}

impl QueueJobComponent {
	pub(super) fn separate_payload_key_prefix(
		&self,
		component: &BoundComponentContract<'_, '_, Self>,
	) -> Result<Option<&'static [u8]>, QueueJobBindingError> {
		let schema = self.bind_actual_schema(component)?;
		Ok(separate_payload_key_prefix(schema.policy()))
	}

	pub(crate) fn component_identifier() -> &'static str {
		JOB_SCHEMA
	}

	pub(crate) fn new() -> Self {
		Self {
			schema: StateSchema::new(crate::records::state_schema::StateSchemaFields {
				identifier: JOB_SCHEMA,
				versions: vec![
					StateVersion {
						version: 1,
						format: StateRepresentationFormat::Json,
						decoder: stored_record::decode_version_one,
						encoder: Some(stored_record::encode_version_one),
					},
					StateVersion {
						version: 2,
						format: StateRepresentationFormat::Json,
						decoder: stored_record::decode_version_two,
						encoder: Some(stored_record::encode_version_two),
					},
				],
			})
			.expect("the job schema declares its explicit native representation"),
			recovery_settings: super::recovery_settings::recovery_settings_schema(),
		}
	}

	pub(crate) fn declaration(&self) -> ComponentDeclaration<'_, Self> {
		ComponentDeclaration::new(ComponentDeclarationFields {
			identifier: Self::component_identifier(),
			fixed_schemas: vec![self.schema.capabilities()],
			settings: ComponentSettingsDeclaration::Schema(&self.recovery_settings),
			semantics: vec![SemanticContractDef {
				identifier: JOB_SEMANTICS,
				transitions: self,
				settings: ComponentSettingsRequirement::Required,
			}],
		})
		.expect("queue jobs declare one schema and one actual semantic implementation")
	}

	/// Selects the actual initial native writer and explicit Required recovery settings.
	pub(crate) fn initial_policy(
		&self,
		fields: QueueJobInitialPolicyFields<'_>,
	) -> Result<ComponentPolicy, QueueJobBindingError> {
		let writer = RepresentationPolicy {
			version: fields.representation.version(),
			envelope: StateStorageEnvelope::NativeJson,
		};
		let settings = super::recovery_settings::initial_recovery_settings(
			super::recovery_settings::QueueInitialRecoverySettingsFields {
				schema: &self.recovery_settings,
				recovery: fields.recovery,
			},
		)?;
		let policy = ComponentPolicy::new(ComponentPolicyFields {
			component_identifier: Self::component_identifier().to_owned(),
			semantic_contract: JOB_SEMANTICS.to_owned(),
			schemas: vec![SchemaPolicy::new(
				self.schema.identifier().to_owned(),
				writer,
				vec![writer],
			)?],
			settings,
		})?;
		let declaration = self.declaration();
		self.require_bound_policy(&declaration.bind(&policy)?)?;
		Ok(policy)
	}

	pub(in crate::queue) fn bind_schema<'a>(
		&'a self,
		component: &'a BoundComponentContract<'_, '_, Self>,
		job_id: JobId,
	) -> Result<QueueJobSchemaBinding<'a>, QueueJobBindingError> {
		let schema = self.bind_actual_schema(component)?;
		Ok(QueueJobSchemaBinding {
			component: component.transitions(),
			job_id,
			schema,
			recovery_policy: component.settings(&self.recovery_settings)?,
		})
	}

	/// Returns the recovery settings that the installed policy records.
	pub(crate) fn installed_recovery_policy(
		&self,
		component: &BoundComponentContract<'_, '_, Self>,
	) -> Result<super::record_transitions::QueueRecoveryPolicy, QueueJobBindingError> {
		Ok(component.settings(&self.recovery_settings)?.clone())
	}

	/// Checks actual semantic/reader compatibility and the selected native writer without a key.
	pub(crate) fn require_bound_policy(
		&self,
		component: &BoundComponentContract<'_, '_, Self>,
	) -> Result<(), QueueJobBindingError> {
		let schema = self.bind_actual_schema(component)?;
		component.settings(&self.recovery_settings)?;
		require_native_job_writer(&schema)
	}

	fn bind_actual_schema<'a>(
		&'a self,
		component: &'a BoundComponentContract<'_, '_, Self>,
	) -> Result<BoundStateSchema<'a, NativeJobRecord, QueueStateError>, QueueJobBindingError> {
		if !std::ptr::eq(self, component.transitions()) {
			return Err(QueueJobBindingError::TransitionInstanceMismatch);
		}
		if component.policy().semantic_contract() != JOB_SEMANTICS {
			return Err(QueueJobBindingError::SemanticContractMismatch);
		}
		Ok(component.bind_schema(&self.schema)?)
	}

	fn claim_pending(
		&self,
		state: NativeJob,
		acquisition: super::record_transitions::JobAcquisition,
		observed_at: StateTimestampMicros,
	) -> Result<Option<NativeJob>, super::record_transitions::JobTransitionError> {
		super::record_transitions::claim_pending(state, acquisition, observed_at)
	}
	fn apply_owned_mutation(
		&self,
		state: NativeJob,
		authority: &super::record_transitions::JobClaimCredentials<'_>,
		mutation: super::record_transitions::OwnedJobMutation,
		observed_at: StateTimestampMicros,
	) -> Result<Option<NativeJob>, super::record_transitions::JobTransitionError> {
		super::record_transitions::apply_owned_mutation(state, authority, mutation, observed_at)
	}

	fn recover_stale_claim(
		&self,
		state: NativeJob,
		policy: &super::record_transitions::QueueRecoveryPolicy,
		observed_at: StateTimestampMicros,
	) -> Result<
		Option<super::record_transitions::StaleClaimRecoveryChange>,
		super::record_transitions::JobTransitionError,
	> {
		super::record_transitions::recover_stale_claim(state, policy, observed_at)
	}
}

const JOB_PAYLOAD_KEY_PREFIX: &[u8] = &[1];

fn separate_payload_key_prefix(policy: &SchemaPolicy) -> Option<&'static [u8]> {
	policy
		.required_readers()
		.iter()
		.any(|reader| reader.version == 2)
		.then_some(JOB_PAYLOAD_KEY_PREFIX)
}

fn require_native_job_writer(
	schema: &BoundStateSchema<'_, NativeJobRecord, QueueStateError>,
) -> Result<(), QueueJobBindingError> {
	if schema.policy().writer().envelope != StateStorageEnvelope::NativeJson {
		return Err(QueueJobBindingError::NativeWriterRequired);
	}
	Ok(())
}

impl QueueJobSchemaBinding<'_> {
	pub(super) fn writes_separate_payload(&self) -> bool {
		self.schema.policy().writer().version == 2
	}
	pub(super) fn separate_payload_key_prefix(&self) -> Option<&'static [u8]> {
		separate_payload_key_prefix(self.schema.policy())
	}

	pub(super) fn payload_record_key(job_id: JobId) -> crate::records::RecordKey {
		let mut bytes = Vec::with_capacity(1 + job_id.as_bytes().len());
		bytes.extend_from_slice(JOB_PAYLOAD_KEY_PREFIX);
		bytes.extend_from_slice(job_id.as_bytes());
		crate::records::RecordKey::new(bytes)
	}

	pub(super) fn public_job(
		&self,
		state: &NativeJob,
		payload_record: Option<&crate::records::StoredRecord>,
	) -> Result<Job, QueueRepresentationError> {
		self.check_identity(state)?;
		let json = match state.payload() {
			NativeJobPayload::InlineJson(json) => json.clone(),
			NativeJobPayload::SeparateRecord => {
				let record =
					payload_record.ok_or_else(|| invalid("job payload record is missing"))?;
				let payload = self.decode_payload_record(record)?;
				if payload.created_at_unix_micros != state.job().created_at_unix_micros {
					return Err(invalid("job and payload creation timestamps differ").into());
				}
				payload.payload_json
			}
		};
		Ok(state.public_job(json))
	}

	pub(super) fn encode_payload_record(
		&self,
		payload: NativeJobPayloadRecord,
	) -> Result<EncodedRecord, QueueRepresentationError> {
		if !self.writes_separate_payload() || payload.job_id != self.job_id {
			return Err(invalid("payload writer or job identity differs from its binding").into());
		}
		let metadata = RecordMetadata {
			expires_at: None,
			updated_at: StateTimestampMicros::new(payload.created_at_unix_micros),
		};
		let body = self.schema.encode(&NativeJobRecord::Payload(payload))?;
		EncodedRecord::derive(body, metadata, |encoded, metadata| {
			self.decode_payload(encoded, metadata)?;
			Ok(IndexFields::default())
		})
	}

	pub(super) fn decode_payload_record(
		&self,
		record: &crate::records::StoredRecord,
	) -> Result<NativeJobPayloadRecord, QueueRepresentationError> {
		if record.index_fields() != &IndexFields::default() {
			return Err(QueueRepresentationError::ProjectionMismatch);
		}
		self.decode_payload(record.body(), record.metadata())
	}

	fn decode_payload(
		&self,
		body: &[u8],
		metadata: RecordMetadata,
	) -> Result<NativeJobPayloadRecord, QueueRepresentationError> {
		let (_, state) = self.schema.decode_document(body)?;
		let NativeJobRecord::Payload(payload) = state else {
			return Err(invalid("a payload key contains job metadata").into());
		};
		if payload.job_id != self.job_id
			|| metadata.expires_at.is_some()
			|| payload.created_at_unix_micros != metadata.updated_at.as_i64()
		{
			return Err(
				invalid("payload identity or metadata differs from its bound record").into(),
			);
		}
		Ok(payload)
	}

	pub(super) fn job_id(&self) -> JobId {
		self.job_id
	}
	/// Requires the actual selected native writer; reads bind retained readers separately.
	pub(super) fn require_native_writer(&self) -> Result<(), QueueJobBindingError> {
		require_native_job_writer(&self.schema)
	}

	/// Dispatches the actual retained-state transition after exact semantic and job binding.
	pub(super) fn prepare_stale_recovery(
		&self,
		state: NativeJob,
		observed_at: StateTimestampMicros,
	) -> Result<Option<super::record_transitions::StaleClaimRecoveryChange>, BoundJobTransitionError>
	{
		self.check_identity(&state)
			.map_err(BoundJobTransitionError::Identity)?;
		Ok(self
			.component
			.recover_stale_claim(state, self.recovery_policy, observed_at)?)
	}

	fn check_identity(&self, state: &NativeJob) -> Result<(), QueueStateError> {
		if state.job.id == self.job_id {
			Ok(())
		} else {
			Err(invalid(
				"job body does not belong to the selected record identity",
			))
		}
	}
	/// Returns absence only for a missing physical value; an expiring job envelope is rejected.
	pub(super) fn decode_record(
		&self,
		snapshot: &PointRecordRead,
	) -> Result<Option<NativeJob>, QueueRepresentationError> {
		let Some(record) = snapshot.record() else {
			return Ok(None);
		};
		self.decode_stored(record.body(), record.metadata())
			.map(Some)
	}

	/// Inspects the whole valued record, including expired claims, without creating authority.
	pub(super) fn inspect_stored_record(
		&self,
		record: &crate::records::StoredRecord,
	) -> Result<NativeJob, QueueRepresentationError> {
		let state = self.decode_stored(record.body(), record.metadata())?;
		if record.index_fields() != &project_job(state.job()) {
			return Err(QueueRepresentationError::ProjectionMismatch);
		}
		Ok(state)
	}

	/// Encodes every semantic field and derives projections from those exact encoded bytes.
	pub(super) fn encode_record(
		&self,
		state: &NativeJob,
	) -> Result<EncodedRecord, QueueRepresentationError> {
		self.require_native_writer()?;
		self.check_identity(state)?;
		let body = self
			.schema
			.encode(&NativeJobRecord::Job(Box::new(state.clone())))?;
		let metadata = RecordMetadata {
			expires_at: None,
			updated_at: StateTimestampMicros::new(state.job.updated_at_unix_micros),
		};
		EncodedRecord::derive(body, metadata, |encoded, metadata| {
			let state = self.decode_stored(encoded, metadata)?;
			Ok(project_job(&state.job))
		})
	}

	fn decode_stored(
		&self,
		encoded: &[u8],
		metadata: RecordMetadata,
	) -> Result<NativeJob, QueueRepresentationError> {
		if metadata.expires_at.is_some() {
			return Err(QueueRepresentationError::State(invalid(
				"job envelopes must not expire",
			)));
		}
		let (_, stored) = self.schema.decode_document(encoded)?;
		let NativeJobRecord::Job(state) = stored else {
			return Err(QueueRepresentationError::State(invalid(
				"a job key contains a payload record",
			)));
		};
		self.check_identity(&state)?;
		if metadata.updated_at.as_i64() != state.job.updated_at_unix_micros {
			return Err(QueueRepresentationError::State(invalid(
				"job body and envelope modification timestamps differ",
			)));
		}
		Ok(*state)
	}

	/// Checks exact private claim through the selected job implementation before preparing a write.
	pub(super) fn prepare_claim(
		&self,
		state: NativeJob,
		acquisition: super::record_transitions::JobAcquisition,
		observed_at: StateTimestampMicros,
	) -> Result<Option<NativeJob>, BoundJobTransitionError> {
		self.check_identity(&state)
			.map_err(BoundJobTransitionError::Identity)?;
		Ok(self
			.component
			.claim_pending(state, acquisition, observed_at)?)
	}
	pub(super) fn prepare_owned_mutation(
		&self,
		state: NativeJob,
		authority: &super::record_transitions::JobClaimCredentials<'_>,
		mutation: super::record_transitions::OwnedJobMutation,
		observed_at: StateTimestampMicros,
	) -> Result<Option<NativeJob>, BoundJobTransitionError> {
		self.check_identity(&state)
			.map_err(BoundJobTransitionError::Identity)?;
		Ok(self
			.component
			.apply_owned_mutation(state, authority, mutation, observed_at)?)
	}
}

fn project_job(job: &NativeJobMetadata) -> IndexFields {
	let mut fields = IndexFields::new();
	fields.insert(
		"status".to_owned(),
		IndexScalar::BytewiseText(job.status().as_str().to_owned()),
	);
	fields.insert(
		"task_name".to_owned(),
		IndexScalar::BytewiseText(job.task_name.as_str().to_owned()),
	);
	if job.status() == JobStatus::Pending {
		fields.insert(
			"run_at_or_after".to_owned(),
			IndexScalar::Timestamp(StateTimestampMicros::new(job.run_at_or_after_unix_micros)),
		);
	}
	if let Some(owner) = job.worker_owner_id() {
		fields.insert(
			"worker_owner_id".to_owned(),
			IndexScalar::BytewiseText(owner.as_str().to_owned()),
		);
		fields.insert(
			"execution_started".to_owned(),
			IndexScalar::Boolean(job.execution_started_at_unix_micros().is_some()),
		);
	}
	if let Some(heartbeat) = job.execution_heartbeat_at_unix_micros() {
		fields.insert(
			"execution_heartbeat_at".to_owned(),
			IndexScalar::Timestamp(StateTimestampMicros::new(heartbeat)),
		);
	}
	if let Some(finished) = job.finished_at_unix_micros() {
		fields.insert(
			"finished_at".to_owned(),
			IndexScalar::Timestamp(StateTimestampMicros::new(finished)),
		);
	}
	if matches!(job.status(), JobStatus::Pending | JobStatus::Running)
		&& let Some(dedupe) = &job.dedupe_key
	{
		fields.insert(
			"active_dedupe_key".to_owned(),
			IndexScalar::BytewiseText(dedupe.as_str().to_owned()),
		);
	}
	fields
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct JobWire {
	id: [u8; primitives_rs::id::time_sortable::SORTABLE_ID_LEN],
	task_name: String,
	#[serde(deserialize_with = "deserialize_required_option")]
	payload_json: Option<String>,
	payload_encoding: NativePayloadEncoding,
	status: String,
	run_at_or_after_unix_micros: String,
	#[serde(deserialize_with = "deserialize_required_option")]
	#[serde(rename = "last_error_message")]
	last_error_msg: Option<String>,
	retry_count: u32,
	max_retries: u32,
	timeout: TimeoutWire,
	#[serde(deserialize_with = "deserialize_required_option")]
	dedupe_key: Option<String>,
	#[serde(deserialize_with = "deserialize_required_option")]
	worker_owner_id: Option<String>,
	#[serde(deserialize_with = "deserialize_required_option")]
	worker_claim_token: Option<TokenWire>,
	#[serde(deserialize_with = "deserialize_required_option")]
	claimed_by_worker_at_unix_micros: Option<String>,
	#[serde(deserialize_with = "deserialize_required_option")]
	execution_started_at_unix_micros: Option<String>,
	#[serde(deserialize_with = "deserialize_required_option")]
	execution_heartbeat_at_unix_micros: Option<String>,
	#[serde(deserialize_with = "deserialize_required_option")]
	finished_at_unix_micros: Option<String>,
	created_at_unix_micros: String,
	updated_at_unix_micros: String,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum TimeoutWire {
	RegisteredDef {},
	NoTimeout {},
	ExpiresAfter { secs: String, subsec_nanos: u32 },
}

#[derive(Deserialize, Serialize)]
#[serde(transparent)]
struct TokenWire([u8; WORKER_CLAIM_TOKEN_LEN]);

impl Drop for TokenWire {
	fn drop(&mut self) {
		self.0.zeroize();
	}
}

pub(in crate::queue) fn check_payload_json(json: &str) -> Result<(), QueueStateError> {
	if exceeds_max_nesting_depth(json.as_bytes(), MAX_PAYLOAD_JSON_NESTING_DEPTH) {
		return Err(invalid(
			"job payload JSON exceeds the maximum nesting depth",
		));
	}
	serde_json::from_str::<&RawValue>(json).map_err(json_error)?;
	Ok(())
}

fn decode_job_wire(
	wire: JobWire,
	ownership: OwnershipWire,
	version: u32,
) -> Result<NativeJob, QueueStateError> {
	let payload = match (version, wire.payload_json) {
		(1, Some(json)) => NativeJobPayload::InlineJson(json),
		(2, None) => NativeJobPayload::SeparateRecord,
		_ => {
			return Err(invalid(
				"job payload placement differs from its representation version",
			));
		}
	};
	let claim = match (ownership, wire.worker_claim_token) {
		(OwnershipWire::NativePrivate {}, Some(token)) => Some(RunningJobClaim::NativePrivate(
			WorkerClaimToken::from_persisted_bytes(token.0),
		)),
		(OwnershipWire::Inactive {}, None) => None,
		(OwnershipWire::RetiredPublished {}, None) => Some(RunningJobClaim::RetiredPublished),
		_ => {
			return Err(invalid(
				"ownership kind and private token presence disagree",
			));
		}
	};
	let job = NativeJobMetadata {
		id: JobId::from_bytes(&wire.id).map_err(|_| invalid("job identifier is invalid"))?,
		task_name: TaskName::new(wire.task_name).map_err(|_| invalid("task name is invalid"))?,
		lifecycle: JobLifecycle::parse(
			StoredJobLifecycleFields {
				status: JobStatus::parse(&wire.status)
					.map_err(|_| invalid("job status is invalid"))?,
				worker_owner_id: wire
					.worker_owner_id
					.map(|owner| {
						WorkerOwnerId::from_validated_text(owner)
							.map_err(|_| invalid("worker owner label is invalid"))
					})
					.transpose()?,
				claimed_at: parse_optional_timestamp(wire.claimed_by_worker_at_unix_micros)?,
				started_at: parse_optional_timestamp(wire.execution_started_at_unix_micros)?,
				heartbeat_at: parse_optional_timestamp(wire.execution_heartbeat_at_unix_micros)?,
				finished_at: parse_optional_timestamp(wire.finished_at_unix_micros)?,
			},
			claim,
		)?,
		run_at_or_after_unix_micros: parse_i64(&wire.run_at_or_after_unix_micros)?,
		last_error_msg: wire.last_error_msg,
		retry_count: wire.retry_count,
		max_retries: wire.max_retries,
		timeout: decode_timeout(wire.timeout)?,
		dedupe_key: wire
			.dedupe_key
			.map(|key| DedupeKey::new(key).map_err(|_| invalid("dedupe key is invalid")))
			.transpose()?,
		created_at_unix_micros: parse_i64(&wire.created_at_unix_micros)?,
		updated_at_unix_micros: parse_i64(&wire.updated_at_unix_micros)?,
	};

	NativeJob::new(NativeJobParts {
		job,
		payload,
		payload_encoding: wire.payload_encoding,
	})
}

fn job_wire(state: &NativeJob) -> JobWire {
	let job = &state.job;
	JobWire {
		id: *job.id.as_bytes(),
		task_name: job.task_name.as_str().to_owned(),
		payload_json: match &state.payload {
			NativeJobPayload::InlineJson(json) => Some(json.clone()),
			NativeJobPayload::SeparateRecord => None,
		},
		payload_encoding: state.payload_encoding.clone(),
		status: job.status().as_str().to_owned(),
		run_at_or_after_unix_micros: job.run_at_or_after_unix_micros.to_string(),
		last_error_msg: job.last_error_msg.clone(),
		retry_count: job.retry_count,
		max_retries: job.max_retries,
		timeout: encode_timeout(job.timeout),
		dedupe_key: job.dedupe_key.as_ref().map(|key| key.as_str().to_owned()),
		worker_owner_id: job.worker_owner_id().map(|owner| owner.as_str().to_owned()),
		worker_claim_token: state
			.claim_token()
			.map(|token| TokenWire(*token.as_bytes())),
		claimed_by_worker_at_unix_micros: timestamp_text(job.claimed_by_worker_at_unix_micros()),
		execution_started_at_unix_micros: timestamp_text(job.execution_started_at_unix_micros()),
		execution_heartbeat_at_unix_micros: timestamp_text(
			job.execution_heartbeat_at_unix_micros(),
		),
		finished_at_unix_micros: timestamp_text(job.finished_at_unix_micros()),
		created_at_unix_micros: job.created_at_unix_micros.to_string(),
		updated_at_unix_micros: job.updated_at_unix_micros.to_string(),
	}
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum OwnershipWire {
	Inactive {},
	NativePrivate {},
	RetiredPublished {},
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct NativeJobWire {
	ownership: OwnershipWire,
	job: JobWire,
}

fn decode_job(bytes: &[u8]) -> Result<NativeJob, QueueStateError> {
	let wire: NativeJobWire = serde_json::from_slice(bytes).map_err(json_error)?;
	decode_job_wire(wire.job, wire.ownership, 1)
}

fn encode_job(state: &NativeJob) -> Result<Vec<u8>, QueueStateError> {
	state.check_shape()?;
	let ownership = state.ownership_wire();
	serde_json::to_vec(&NativeJobWire {
		ownership,
		job: job_wire(state),
	})
	.map_err(json_error)
}

pub(super) fn decode_timeout(wire: TimeoutWire) -> Result<StoredJobTimeout, QueueStateError> {
	match wire {
		TimeoutWire::RegisteredDef {} => Ok(StoredJobTimeout::RegisteredDef),
		TimeoutWire::NoTimeout {} => Ok(StoredJobTimeout::NoTimeout),
		TimeoutWire::ExpiresAfter { secs, subsec_nanos } => {
			let secs = parse_u64(&secs)?;
			if subsec_nanos >= 1_000_000_000 {
				return Err(invalid("timeout nanoseconds must be less than one second"));
			}
			let duration = Duration::new(secs, subsec_nanos);
			if duration.is_zero() {
				return Err(invalid("a job-specific timeout must be positive"));
			}
			Ok(StoredJobTimeout::ExpiresAfter(duration))
		}
	}
}

pub(super) fn encode_timeout(timeout: StoredJobTimeout) -> TimeoutWire {
	match timeout {
		StoredJobTimeout::RegisteredDef => TimeoutWire::RegisteredDef {},
		StoredJobTimeout::NoTimeout => TimeoutWire::NoTimeout {},
		StoredJobTimeout::ExpiresAfter(duration) => TimeoutWire::ExpiresAfter {
			secs: duration.as_secs().to_string(),
			subsec_nanos: duration.subsec_nanos(),
		},
	}
}

fn timestamp_text(value: Option<i64>) -> Option<String> {
	value.map(|value| value.to_string())
}

fn parse_optional_timestamp(value: Option<String>) -> Result<Option<i64>, QueueStateError> {
	value.map(|value| parse_i64(&value)).transpose()
}

pub(super) fn parse_i64(text: &str) -> Result<i64, QueueStateError> {
	parse_canonical_decimal(text)
		.map_err(|_| invalid("timestamp text is not a canonical signed 64-bit decimal integer"))
}

fn parse_u64(text: &str) -> Result<u64, QueueStateError> {
	parse_canonical_decimal(text).map_err(|_| {
		invalid("duration seconds are not a canonical unsigned 64-bit decimal integer")
	})
}

fn invalid(reason: &'static str) -> QueueStateError {
	QueueStateError::Invalid { reason }
}

fn json_error(error: serde_json::Error) -> QueueStateError {
	QueueStateError::Json {
		summary: JsonDecodeFailureSummary::from(&error),
	}
}

#[cfg(test)]
mod tests;
