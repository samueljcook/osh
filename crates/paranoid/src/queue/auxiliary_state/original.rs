//! Current dead-letter uniqueness by original job identity, without permanent lineage.
use super::{
	AuxiliaryRecord, AuxiliaryStateError, IndexFields, JobId, NativeDeadLetterState, RecordKey,
	RecordMetadata, decode_json, encode_json, id_key, invalid,
};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Eq, PartialEq)]
pub(in crate::queue) struct DeadLetterOriginalState {
	pub(in crate::queue) original_job_id: JobId,
	pub(in crate::queue) dead_letter_id: JobId,
}
impl fmt::Debug for DeadLetterOriginalState {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("DeadLetterOriginalState { .. }")
	}
}
impl DeadLetterOriginalState {
	/// Pair consistency is distinct from proof that both records were acquired atomically.
	pub(in crate::queue) fn check_dead_letter(
		&self,
		dead_letter: &NativeDeadLetterState,
	) -> Result<(), AuxiliaryStateError> {
		if *self != Self::from_dead_letter(dead_letter) {
			return Err(invalid("original mapping and dead letter disagree"));
		}
		Ok(())
	}
	pub(in crate::queue) fn from_dead_letter(dead_letter: &NativeDeadLetterState) -> Self {
		Self {
			original_job_id: dead_letter.dead_letter().original_job_id,
			dead_letter_id: dead_letter.dead_letter().id,
		}
	}
}
impl AuxiliaryRecord for DeadLetterOriginalState {
	fn key(&self) -> Result<RecordKey, AuxiliaryStateError> {
		Ok(id_key(self.original_job_id))
	}
	fn check_metadata(&self, _: RecordMetadata) -> Result<(), AuxiliaryStateError> {
		Ok(())
	}
	fn project(&self) -> IndexFields {
		IndexFields::new()
	}
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Wire {
	original_job_id: [u8; primitives_rs::id::time_sortable::SORTABLE_ID_LEN],
	dead_letter_id: [u8; primitives_rs::id::time_sortable::SORTABLE_ID_LEN],
}
pub(super) fn decode(bytes: &[u8]) -> Result<DeadLetterOriginalState, AuxiliaryStateError> {
	let wire: Wire = decode_json(bytes)?;
	Ok(DeadLetterOriginalState {
		original_job_id: JobId::from_bytes(&wire.original_job_id)
			.map_err(|_| invalid("original job identifier is invalid"))?,
		dead_letter_id: JobId::from_bytes(&wire.dead_letter_id)
			.map_err(|_| invalid("dead letter identifier is invalid"))?,
	})
}
pub(super) fn encode(state: &DeadLetterOriginalState) -> Result<Vec<u8>, AuxiliaryStateError> {
	encode_json(&Wire {
		original_job_id: *state.original_job_id.as_bytes(),
		dead_letter_id: *state.dead_letter_id.as_bytes(),
	})
}
