//! Exact ordinary raw aggregates, without enumeration or snapshot authority.

use super::page::RecordKeySelection;
use crate::kv::{KeyError, KeyPrefix};
use crate::records::StateTimestampMicros;

/// The number of live records in a selection, counted in one statement.
#[derive(Clone, Copy, Debug)]
pub struct RecordLiveCount {
	count: u64,
	observed_at: StateTimestampMicros,
}
impl RecordLiveCount {
	pub(crate) fn new(count: u64, observed_at: StateTimestampMicros) -> Self {
		Self { count, observed_at }
	}
	/// The number of live records.
	pub fn count(&self) -> u64 {
		self.count
	}
	/// The database time at which the statement counted the records.
	pub fn observed_at(&self) -> StateTimestampMicros {
		self.observed_at
	}
}
#[derive(Debug, thiserror::Error)]
pub(in crate::kv) enum RawAggregateInputError {
	#[error("raw aggregate source prefix is not UTF-8")]
	Utf8(#[from] std::str::Utf8Error),
	#[error("raw aggregate source prefix is not canonical")]
	Prefix(#[from] KeyError),
}
pub(in crate::kv::raw) fn require_source_selection(
	selection: &RecordKeySelection,
) -> Result<(), RawAggregateInputError> {
	if let Some(prefix) = selection.prefix() {
		KeyPrefix::from_persisted_text(std::str::from_utf8(prefix.as_bytes())?.to_owned())?;
	}
	Ok(())
}
