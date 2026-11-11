//! Observable dead-letter state without worker ownership or private claim material.
use super::{
	AuxiliaryRecord, AuxiliaryStateError, DedupeKey, IndexFields, JobId, RecordKey, RecordMetadata,
	TaskName, decode_json, encode_json, id_key, invalid,
};
use crate::queue::state::{
	NativePayloadEncoding, TimeoutWire, check_payload_json, decode_timeout, encode_timeout,
	parse_i64,
};
use crate::queue::{DeadLetterJob, DeadLetterReason, StoredJobTimeout};
use crate::records::{IndexScalar, StateTimestampMicros};
use primitives_rs::serialization::json::deserialize_required_option;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Eq, PartialEq)]
pub(in crate::queue) struct NativeDeadLetterState {
	dead_letter: DeadLetterJob,
	payload_encoding: NativePayloadEncoding,
}
impl NativeDeadLetterState {
	/// All timestamps are supplied facts; construction generates no identity or authority.
	pub(in crate::queue) fn new(
		dead_letter: DeadLetterJob,
		payload_encoding: NativePayloadEncoding,
	) -> Result<Self, AuxiliaryStateError> {
		let state = Self {
			dead_letter,
			payload_encoding,
		};
		state.check_shape()?;
		Ok(state)
	}
	pub(in crate::queue) fn dead_letter(&self) -> &DeadLetterJob {
		&self.dead_letter
	}
	pub(in crate::queue) fn into_parts(self) -> (DeadLetterJob, NativePayloadEncoding) {
		(self.dead_letter, self.payload_encoding)
	}
	fn check_shape(&self) -> Result<(), AuxiliaryStateError> {
		self.payload_encoding.check()?;
		check_payload_json(&self.dead_letter.payload_json)?;
		if self.dead_letter.max_retries > crate::queue::MAX_RETRIES {
			return Err(invalid("dead-letter max_retries exceeds MAX_RETRIES"));
		}
		if self.dead_letter.retry_count > crate::queue::MAX_RETRIES + 1 {
			return Err(invalid("dead-letter retry_count exceeds MAX_RETRIES + 1"));
		}
		if matches!(self.dead_letter.timeout, StoredJobTimeout::ExpiresAfter(duration) if duration.is_zero())
		{
			return Err(invalid("a job-specific timeout must be positive"));
		}
		Ok(())
	}
}
impl fmt::Debug for NativeDeadLetterState {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("NativeDeadLetterState")
			.field("payload_len", &self.dead_letter.payload_json.len())
			.field("payload_encoding", &self.payload_encoding)
			.finish_non_exhaustive()
	}
}
impl AuxiliaryRecord for NativeDeadLetterState {
	fn key(&self) -> Result<RecordKey, AuxiliaryStateError> {
		Ok(id_key(self.dead_letter.id))
	}
	fn check_metadata(&self, metadata: RecordMetadata) -> Result<(), AuxiliaryStateError> {
		self.check_shape()?;
		if metadata.updated_at.as_i64() != self.dead_letter.updated_at_unix_micros {
			return Err(invalid(
				"dead letter and envelope modification timestamps differ",
			));
		}
		Ok(())
	}
	fn project(&self) -> IndexFields {
		let mut fields = IndexFields::new();
		fields.insert(
			"task_name".to_owned(),
			IndexScalar::BytewiseText(self.dead_letter.task_name.as_str().to_owned()),
		);
		fields.insert(
			"original_job_id".to_owned(),
			IndexScalar::Bytes(self.dead_letter.original_job_id.as_bytes().to_vec()),
		);
		fields.insert(
			"dead_lettered_at".to_owned(),
			IndexScalar::Timestamp(StateTimestampMicros::new(
				self.dead_letter.dead_lettered_at_unix_micros,
			)),
		);
		fields
	}
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Wire {
	id: [u8; primitives_rs::id::time_sortable::SORTABLE_ID_LEN],
	original_job_id: [u8; primitives_rs::id::time_sortable::SORTABLE_ID_LEN],
	task_name: String,
	payload_json: String,
	payload_encoding: NativePayloadEncoding,
	#[serde(rename = "last_error_message")]
	last_error_msg: String,
	retry_count: u32,
	max_retries: u32,
	timeout: TimeoutWire,
	#[serde(deserialize_with = "deserialize_required_option")]
	dedupe_key: Option<String>,
	reason: String,
	dead_lettered_at_unix_micros: String,
	created_at_unix_micros: String,
	updated_at_unix_micros: String,
}
pub(super) fn decode(bytes: &[u8]) -> Result<NativeDeadLetterState, AuxiliaryStateError> {
	let wire: Wire = decode_json(bytes)?;
	NativeDeadLetterState::new(
		DeadLetterJob {
			id: JobId::from_bytes(&wire.id)
				.map_err(|_| invalid("dead letter identifier is invalid"))?,
			original_job_id: JobId::from_bytes(&wire.original_job_id)
				.map_err(|_| invalid("original job identifier is invalid"))?,
			task_name: TaskName::new(wire.task_name)
				.map_err(|_| invalid("task name is invalid"))?,
			payload_json: wire.payload_json,
			last_error_msg: wire.last_error_msg,
			retry_count: wire.retry_count,
			max_retries: wire.max_retries,
			timeout: decode_timeout(wire.timeout)?,
			dedupe_key: wire
				.dedupe_key
				.map(|text| DedupeKey::new(text).map_err(|_| invalid("dedupe key is invalid")))
				.transpose()?,
			reason: DeadLetterReason::parse(&wire.reason)
				.map_err(|_| invalid("dead-letter reason is invalid"))?,
			dead_lettered_at_unix_micros: parse_i64(&wire.dead_lettered_at_unix_micros)?,
			created_at_unix_micros: parse_i64(&wire.created_at_unix_micros)?,
			updated_at_unix_micros: parse_i64(&wire.updated_at_unix_micros)?,
		},
		wire.payload_encoding,
	)
}
pub(super) fn encode(state: &NativeDeadLetterState) -> Result<Vec<u8>, AuxiliaryStateError> {
	state.check_shape()?;
	let dead_letter = state.dead_letter();
	encode_json(&Wire {
		id: *dead_letter.id.as_bytes(),
		original_job_id: *dead_letter.original_job_id.as_bytes(),
		task_name: dead_letter.task_name.as_str().to_owned(),
		payload_json: dead_letter.payload_json.clone(),
		payload_encoding: state.payload_encoding.clone(),
		last_error_msg: dead_letter.last_error_msg.clone(),
		retry_count: dead_letter.retry_count,
		max_retries: dead_letter.max_retries,
		timeout: encode_timeout(dead_letter.timeout),
		dedupe_key: dead_letter
			.dedupe_key
			.as_ref()
			.map(|key| key.as_str().to_owned()),
		reason: dead_letter.reason.as_str().to_owned(),
		dead_lettered_at_unix_micros: dead_letter.dead_lettered_at_unix_micros.to_string(),
		created_at_unix_micros: dead_letter.created_at_unix_micros.to_string(),
		updated_at_unix_micros: dead_letter.updated_at_unix_micros.to_string(),
	})
}
