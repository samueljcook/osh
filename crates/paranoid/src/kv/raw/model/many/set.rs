//! Parsed owned batch replacement, without storage or application payload interpretation.

use crate::kv::Ttl;
use crate::kv::raw::state::RawWriteResult;
use crate::records::{
	MAX_MANY_RECORD_ENTRIES, ManyRecordRequestError, RecordKey, check_many_record_keys,
};

use super::RawManySetRequestFields;
/// A validated request that writes several records in one operation.
pub struct RawManySetRequest {
	keys: Vec<RecordKey>,
	values: Vec<Vec<u8>>,
	ttl: Ttl,
}
impl RawManySetRequest {
	/// Validates the request without database work. The keys must be unique, and their number
	/// must not exceed the largest supported batch.
	pub fn new(fields: RawManySetRequestFields) -> Result<Self, ManyRecordRequestError> {
		let (keys, values): (Vec<_>, Vec<_>) = fields
			.entries
			.into_iter()
			.map(|entry| (entry.key, entry.value))
			.unzip();
		check_many_record_keys(&keys, MAX_MANY_RECORD_ENTRIES)?;
		Ok(Self {
			keys,
			values,
			ttl: fields.ttl,
		})
	}
	/// The keys in the order of the request's entries.
	pub fn keys(&self) -> &[RecordKey] {
		&self.keys
	}
	/// Whether the request has no entries.
	pub fn is_empty(&self) -> bool {
		self.keys.is_empty()
	}
	/// The TTL of every written record.
	pub fn ttl(&self) -> Ttl {
		self.ttl
	}
	pub(crate) fn into_values(self) -> Vec<Vec<u8>> {
		self.values
	}
	pub(in crate::kv) fn source_texts(&self) -> Result<Vec<String>, super::RawManySourceKeyError> {
		super::source_texts(&self.keys)
	}
}
#[derive(Debug)]
/// The result of a many-record write.
pub enum RawManyWriteResult {
	/// The request had no entries, so Paranoid did no database work.
	Empty,
	/// Paranoid wrote every entry.
	Written {
		/// The number of written records.
		count: usize,
		/// The database time and the metadata that every written record shares.
		outcome: RawWriteResult,
	},
}
