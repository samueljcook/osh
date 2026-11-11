//! Queue-owned representation policies for payload separation.

use super::{
	BoundComponentContract, ComponentPolicy, ComponentPolicyFields, EncodedRecord,
	JOB_PAYLOAD_KEY_PREFIX, JOB_REPRESENTATION_VERSION, JOB_SCHEMA, JOB_SEMANTICS, JobId,
	NativeJobPayload, NativeJobPayloadRecord, QueueJobBindingError, QueueJobComponent,
	QueueJobSchemaBinding, QueueRepresentationError, RepresentationPolicy, SchemaPolicy,
	StateContractError, StateStorageEnvelope,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum QueueJobRepresentation {
	InlinePayload,
	SeparatePayload,
}

impl QueueJobRepresentation {
	pub(in crate::queue) fn version(self) -> u32 {
		match self {
			Self::InlinePayload => 1,
			Self::SeparatePayload => JOB_REPRESENTATION_VERSION,
		}
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum QueueJobPayloadStage {
	Inline,
	Separating,
	SeparateRecords,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum QueueJobUpgradeError {
	#[error("Queue job conversion found an invalid record key")]
	InvalidRecordKey,
	#[error("Queue job conversion found an orphan or conflicting payload")]
	ConflictingPayload,
	#[error("Queue job conversion representation binding failed")]
	Binding(#[from] QueueJobBindingError),
	#[error("Queue job conversion record is invalid")]
	Record(#[from] QueueRepresentationError),
	#[error("Queue job representation policy is unsupported")]
	UnsupportedPolicy,
	#[error("Queue job upgrade changes recovery settings or semantic meaning")]
	MeaningMismatch,
	#[error("Queue job representation cannot move to an older writer")]
	WriterDowngrade,
	#[error("Queue job upgrade policy construction failed")]
	Contract(#[from] StateContractError),
}

pub(crate) fn record_job_id(
	key: &crate::records::RecordKey,
) -> Result<JobId, QueueJobUpgradeError> {
	let bytes = key.as_bytes();
	let id = if bytes.len() == primitives_rs::id::time_sortable::SORTABLE_ID_LEN {
		bytes
	} else if bytes.len()
		== primitives_rs::id::time_sortable::SORTABLE_ID_LEN + JOB_PAYLOAD_KEY_PREFIX.len()
		&& bytes.starts_with(JOB_PAYLOAD_KEY_PREFIX)
	{
		&bytes[JOB_PAYLOAD_KEY_PREFIX.len()..]
	} else {
		return Err(QueueJobUpgradeError::InvalidRecordKey);
	};
	JobId::from_bytes(id).map_err(|_| QueueJobUpgradeError::InvalidRecordKey)
}

pub(crate) fn record_keys(job_id: JobId) -> [crate::records::RecordKey; 2] {
	[
		crate::records::RecordKey::new(job_id.as_bytes().to_vec()),
		QueueJobSchemaBinding::payload_record_key(job_id),
	]
}

pub(crate) struct QueueJobConversionRecords<'a> {
	pub(crate) job_id: JobId,
	pub(crate) job: Option<&'a crate::records::StoredRecord>,
	pub(crate) payload: Option<&'a crate::records::StoredRecord>,
}

impl QueueJobComponent {
	pub(crate) fn prepare_payload_separation(
		&self,
		contract: &BoundComponentContract<'_, '_, Self>,
		records: QueueJobConversionRecords<'_>,
	) -> Result<Vec<(crate::records::RecordKey, EncodedRecord)>, QueueJobUpgradeError> {
		let binding = self.bind_schema(contract, records.job_id)?;
		if !binding.writes_separate_payload() {
			return Err(QueueJobUpgradeError::UnsupportedPolicy);
		}
		let Some(record) = records.job else {
			return if records.payload.is_some() {
				Err(QueueJobUpgradeError::ConflictingPayload)
			} else {
				Ok(Vec::new())
			};
		};
		let state = binding.inspect_stored_record(record)?;
		match state.payload() {
			NativeJobPayload::SeparateRecord => {
				let payload = records
					.payload
					.ok_or(QueueJobUpgradeError::ConflictingPayload)?;
				let payload = binding.decode_payload_record(payload)?;
				if payload.created_at_unix_micros != state.job().created_at_unix_micros {
					return Err(QueueJobUpgradeError::ConflictingPayload);
				}
				Ok(Vec::new())
			}
			NativeJobPayload::InlineJson(payload_json) => {
				if records.payload.is_some() {
					return Err(QueueJobUpgradeError::ConflictingPayload);
				}
				let payload = binding.encode_payload_record(NativeJobPayloadRecord {
					job_id: records.job_id,
					created_at_unix_micros: state.job().created_at_unix_micros,
					payload_json: payload_json.clone(),
				})?;
				let job = binding.encode_record(&state)?;
				let [job_key, payload_key] = record_keys(records.job_id);
				Ok(vec![(job_key, job), (payload_key, payload)])
			}
		}
	}
}

pub(crate) fn policy_stage(
	policy: &ComponentPolicy,
) -> Result<QueueJobPayloadStage, QueueJobUpgradeError> {
	if policy.component_identifier() != QueueJobComponent::component_identifier()
		|| policy.semantic_contract() != JOB_SEMANTICS
		|| policy.schemas().len() != 1
	{
		return Err(QueueJobUpgradeError::UnsupportedPolicy);
	}
	let schema = &policy.schemas()[0];
	if schema.identifier() != JOB_SCHEMA
		|| schema.writer().envelope != StateStorageEnvelope::NativeJson
		|| schema
			.required_readers()
			.iter()
			.any(|reader| reader.envelope != StateStorageEnvelope::NativeJson)
	{
		return Err(QueueJobUpgradeError::UnsupportedPolicy);
	}
	let mut versions = schema
		.required_readers()
		.iter()
		.map(|reader| reader.version)
		.collect::<Vec<_>>();
	versions.sort_unstable();
	match (schema.writer().version, versions.as_slice()) {
		(1, [1]) => Ok(QueueJobPayloadStage::Inline),
		(2, [1, 2]) => Ok(QueueJobPayloadStage::Separating),
		(2, [2]) => Ok(QueueJobPayloadStage::SeparateRecords),
		_ => Err(QueueJobUpgradeError::UnsupportedPolicy),
	}
}

pub(crate) fn writer_stage(
	current: &ComponentPolicy,
	target: &ComponentPolicy,
) -> Result<ComponentPolicy, QueueJobUpgradeError> {
	policy_stage(current)?;
	if policy_stage(target)? != QueueJobPayloadStage::SeparateRecords {
		return Err(QueueJobUpgradeError::WriterDowngrade);
	}
	if !current.settings().matches_settings(target.settings()) {
		return Err(QueueJobUpgradeError::MeaningMismatch);
	}
	Ok(ComponentPolicy::new(ComponentPolicyFields {
		component_identifier: target.component_identifier().to_owned(),
		semantic_contract: target.semantic_contract().to_owned(),
		schemas: vec![SchemaPolicy::new(
			JOB_SCHEMA.to_owned(),
			RepresentationPolicy {
				version: 2,
				envelope: StateStorageEnvelope::NativeJson,
			},
			vec![
				RepresentationPolicy {
					version: 1,
					envelope: StateStorageEnvelope::NativeJson,
				},
				RepresentationPolicy {
					version: 2,
					envelope: StateStorageEnvelope::NativeJson,
				},
			],
		)?],
		settings: target.settings().clone(),
	})?)
}
