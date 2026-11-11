//! Exact active-dedupe tuple and referenced logical job identity.
use super::{
	AuxiliaryRecord, AuxiliaryStateError, DedupeKey, IndexFields, JobId, RecordKey, RecordMetadata,
	TaskName, decode_json, encode_json, invalid,
};
use crate::queue::{JobStatus, state::NativeJobMetadata};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Eq, PartialEq)]
pub(in crate::queue) struct ActiveDedupeState {
	pub(in crate::queue) task_name: TaskName,
	pub(in crate::queue) dedupe_key: DedupeKey,
	pub(in crate::queue) job_id: JobId,
}
impl fmt::Debug for ActiveDedupeState {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("ActiveDedupeState { .. }")
	}
}
impl ActiveDedupeState {
	/// Pair consistency does not establish that both records were acquired atomically.
	pub(in crate::queue) fn check_job(
		&self,
		job: &NativeJobMetadata,
	) -> Result<(), AuxiliaryStateError> {
		if membership(job) != Some((&self.task_name, &self.dedupe_key, self.job_id)) {
			return Err(invalid("active dedupe mapping and job disagree"));
		}
		Ok(())
	}
}
impl AuxiliaryRecord for ActiveDedupeState {
	fn key(&self) -> Result<RecordKey, AuxiliaryStateError> {
		key(&self.task_name, &self.dedupe_key)
	}
	fn check_metadata(&self, _: RecordMetadata) -> Result<(), AuxiliaryStateError> {
		Ok(())
	}
	fn project(&self) -> IndexFields {
		IndexFields::new()
	}
}
/// Version one is injective over exact UTF8, with two unsigned 32-bit big-endian length frames.
pub(super) fn key(
	task_name: &TaskName,
	dedupe_key: &DedupeKey,
) -> Result<RecordKey, AuxiliaryStateError> {
	let task = task_name.as_str().as_bytes();
	let dedupe = dedupe_key.as_str().as_bytes();
	let task_len = u32::try_from(task.len())
		.map_err(|_| invalid("task exceeds the tuple key length domain"))?;
	let dedupe_len = u32::try_from(dedupe.len())
		.map_err(|_| invalid("dedupe exceeds the tuple key length domain"))?;
	let mut bytes = Vec::with_capacity(9 + task.len() + dedupe.len());
	bytes.push(1);
	bytes.extend_from_slice(&task_len.to_be_bytes());
	bytes.extend_from_slice(task);
	bytes.extend_from_slice(&dedupe_len.to_be_bytes());
	bytes.extend_from_slice(dedupe);
	Ok(RecordKey::new(bytes))
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Wire {
	task_name: String,
	dedupe_key: String,
	job_id: [u8; primitives_rs::id::time_sortable::SORTABLE_ID_LEN],
}
pub(super) fn decode(bytes: &[u8]) -> Result<ActiveDedupeState, AuxiliaryStateError> {
	let wire: Wire = decode_json(bytes)?;
	Ok(ActiveDedupeState {
		task_name: TaskName::new(wire.task_name).map_err(|_| invalid("task name is invalid"))?,
		dedupe_key: DedupeKey::new(wire.dedupe_key)
			.map_err(|_| invalid("dedupe key is invalid"))?,
		job_id: JobId::from_bytes(&wire.job_id)
			.map_err(|_| invalid("job identifier is invalid"))?,
	})
}
pub(super) fn encode(state: &ActiveDedupeState) -> Result<Vec<u8>, AuxiliaryStateError> {
	encode_json(&Wire {
		task_name: state.task_name.as_str().to_owned(),
		dedupe_key: state.dedupe_key.as_str().to_owned(),
		job_id: *state.job_id.as_bytes(),
	})
}

/// Current active membership is determined only by actual status and the optional dedupe field.
pub(in crate::queue) fn membership(
	job: &NativeJobMetadata,
) -> Option<(&TaskName, &DedupeKey, JobId)> {
	if !matches!(job.status(), JobStatus::Pending | JobStatus::Running) {
		return None;
	}
	job.dedupe_key
		.as_ref()
		.map(|dedupe| (&job.task_name, dedupe, job.id))
}
