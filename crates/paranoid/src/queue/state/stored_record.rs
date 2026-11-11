//! Versioned job metadata and payload records within the Queue-owned job namespace.

use super::{
	Deserialize, JobId, JobWire, NativeJob, NativeJobPayload, OwnershipWire, QueueStateError,
	Serialize, check_payload_json, decode_job, decode_job_wire, encode_job, invalid, job_wire,
	json_error, parse_i64,
};

#[derive(Clone)]
pub(in crate::queue) enum NativeJobRecord {
	Job(Box<NativeJob>),
	Payload(NativeJobPayloadRecord),
}

#[derive(Clone)]
pub(in crate::queue) struct NativeJobPayloadRecord {
	pub(in crate::queue) job_id: JobId,
	pub(in crate::queue) created_at_unix_micros: i64,
	pub(in crate::queue) payload_json: String,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum VersionTwoWire {
	JobMetadata {
		ownership: OwnershipWire,
		job: Box<JobWire>,
	},
	JobPayload {
		job_id: [u8; primitives_rs::id::time_sortable::SORTABLE_ID_LEN],
		created_at_unix_micros: String,
		payload_json: String,
	},
}

pub(super) fn decode_version_one(bytes: &[u8]) -> Result<NativeJobRecord, QueueStateError> {
	decode_job(bytes).map(|job| NativeJobRecord::Job(Box::new(job)))
}

pub(super) fn encode_version_one(record: &NativeJobRecord) -> Result<Vec<u8>, QueueStateError> {
	match record {
		NativeJobRecord::Job(job) if matches!(job.payload(), NativeJobPayload::InlineJson(_)) => {
			encode_job(job)
		}
		_ => Err(invalid("version-one jobs require an inline payload")),
	}
}

pub(super) fn decode_version_two(bytes: &[u8]) -> Result<NativeJobRecord, QueueStateError> {
	match serde_json::from_slice(bytes).map_err(json_error)? {
		VersionTwoWire::JobMetadata { ownership, job } => {
			decode_job_wire(*job, ownership, 2).map(|job| NativeJobRecord::Job(Box::new(job)))
		}
		VersionTwoWire::JobPayload {
			job_id,
			created_at_unix_micros,
			payload_json,
		} => {
			check_payload_json(&payload_json)?;
			Ok(NativeJobRecord::Payload(NativeJobPayloadRecord {
				job_id: JobId::from_bytes(&job_id)
					.map_err(|_| invalid("payload job identifier is invalid"))?,
				created_at_unix_micros: parse_i64(&created_at_unix_micros)?,
				payload_json,
			}))
		}
	}
}

pub(super) fn encode_version_two(record: &NativeJobRecord) -> Result<Vec<u8>, QueueStateError> {
	let wire = match record {
		NativeJobRecord::Job(state) => {
			state.check_shape()?;
			let ownership = state.ownership_wire();
			let mut job = job_wire(state);
			job.payload_json = None;
			VersionTwoWire::JobMetadata {
				ownership,
				job: Box::new(job),
			}
		}
		NativeJobRecord::Payload(payload) => {
			check_payload_json(&payload.payload_json)?;
			VersionTwoWire::JobPayload {
				job_id: *payload.job_id.as_bytes(),
				created_at_unix_micros: payload.created_at_unix_micros.to_string(),
				payload_json: payload.payload_json.clone(),
			}
		}
	};
	serde_json::to_vec(&wire).map_err(json_error)
}
