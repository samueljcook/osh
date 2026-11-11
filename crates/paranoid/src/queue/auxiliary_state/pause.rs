//! Presence-based Queue pause state with explicit global or task scope.
use super::{
	AuxiliaryRecord, AuxiliaryStateError, IndexFields, RecordKey, RecordMetadata, TaskName,
	decode_json, encode_json, invalid,
};
use crate::records::IndexScalar;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Eq, PartialEq)]
pub(crate) enum QueuePauseScope {
	Global,
	Task(TaskName),
}
impl fmt::Debug for QueuePauseScope {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::Global => "Global",
			Self::Task(_) => "Task { .. }",
		})
	}
}
impl QueuePauseScope {
	pub(in crate::queue) fn key(&self) -> RecordKey {
		let mut bytes = vec![
			1,
			match self {
				Self::Global => 0,
				Self::Task(_) => 1,
			},
		];
		if let Self::Task(task) = self {
			bytes.extend_from_slice(task.as_str().as_bytes());
		}
		RecordKey::new(bytes)
	}
}
#[derive(Clone, Eq, PartialEq)]
pub(in crate::queue) struct QueuePauseState {
	pub(in crate::queue) scope: QueuePauseScope,
}
impl fmt::Debug for QueuePauseState {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("QueuePauseState")
			.field("scope", &self.scope)
			.finish()
	}
}
impl AuxiliaryRecord for QueuePauseState {
	fn key(&self) -> Result<RecordKey, AuxiliaryStateError> {
		Ok(self.scope.key())
	}
	fn check_metadata(&self, _: RecordMetadata) -> Result<(), AuxiliaryStateError> {
		Ok(())
	}
	fn project(&self) -> IndexFields {
		let mut fields = IndexFields::new();
		if let QueuePauseScope::Task(task) = &self.scope {
			fields.insert(
				"task_name".to_owned(),
				IndexScalar::BytewiseText(task.as_str().to_owned()),
			);
		}
		fields
	}
}
#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Wire {
	Global {},
	Task { task: String },
}
pub(super) fn decode(bytes: &[u8]) -> Result<QueuePauseState, AuxiliaryStateError> {
	Ok(QueuePauseState {
		scope: match decode_json(bytes)? {
			Wire::Global {} => QueuePauseScope::Global,
			Wire::Task { task } => QueuePauseScope::Task(
				TaskName::new(task).map_err(|_| invalid("task name is invalid"))?,
			),
		},
	})
}
pub(super) fn encode(state: &QueuePauseState) -> Result<Vec<u8>, AuxiliaryStateError> {
	encode_json(&match &state.scope {
		QueuePauseScope::Global => Wire::Global {},
		QueuePauseScope::Task(task) => Wire::Task {
			task: task.as_str().to_owned(),
		},
	})
}
