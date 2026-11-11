//! Complete bounded raw key requests and caller-order observations, without transaction authority.

use std::fmt;

use crate::records::{RecordKey, StateTimestampMicros};

use crate::kv::raw::state::{ObservedRawValue, RawValueObservation};
pub(crate) use crate::kv::raw::state::{RawWritePreparationFields, prepare_raw_write};
use crate::kv::{Key, KeyError};
use crate::records::{MAX_MANY_RECORD_ENTRIES, ManyRecordRequestError, check_many_record_keys};

mod set;
pub use set::{RawManySetRequest, RawManyWriteResult};
/// One record of a many-record write.
pub struct RawSetEntry {
	/// The record's key. While the namespace is still in v10 storage, the key must be a
	/// valid v10 key.
	pub key: RecordKey,
	/// The bytes that replace the record's value.
	pub value: Vec<u8>,
}
/// The inputs of a many-record write.
pub struct RawManySetRequestFields {
	/// The records to write, with unique keys.
	pub entries: Vec<RawSetEntry>,
	/// The TTL of every written record.
	pub ttl: crate::kv::Ttl,
}
/// The inputs of a many-record read.
pub struct RawManyReadRequestFields {
	/// The keys to read, without duplicates, in the order of the result.
	pub keys: Vec<RecordKey>,
}
/// A validated request that reads several records in one statement.
pub struct RawManyReadRequest {
	keys: Vec<RecordKey>,
}
impl RawManyReadRequest {
	/// Validates the request without database work. The keys must be unique, and their number
	/// must not exceed the largest supported batch.
	pub fn new(fields: RawManyReadRequestFields) -> Result<Self, ManyRecordRequestError> {
		check_many_record_keys(&fields.keys, MAX_MANY_RECORD_ENTRIES)?;
		Ok(Self { keys: fields.keys })
	}
	/// The keys in request order.
	pub fn keys(&self) -> &[RecordKey] {
		&self.keys
	}
	/// Whether the request has no keys.
	pub fn is_empty(&self) -> bool {
		self.keys.is_empty()
	}
	pub(in crate::kv::raw) fn source_keys(
		&self,
	) -> Result<RawManySourceKeys<'_>, RawManySourceKeyError> {
		let texts = source_texts(&self.keys)?;
		Ok(RawManySourceKeys {
			request: self,
			texts,
		})
	}
	pub(crate) fn finish(
		&self,
		records: Vec<Option<crate::records::StoredRecord>>,
		observed_at: StateTimestampMicros,
	) -> RawManyReadResult {
		let entries = self
			.keys
			.iter()
			.cloned()
			.zip(records)
			.map(|(key, record)| RawManyReadEntry {
				key,
				value: RawValueObservation::from_owned_record(record, observed_at).into_value(),
			})
			.collect();
		RawManyReadResult::Observed(RawManyObservation {
			entries,
			observed_at,
		})
	}
}

/// Canonical source texts derived from, and borrowing, this exact complete byte-key request.
pub(crate) struct RawManySourceKeys<'a> {
	request: &'a RawManyReadRequest,
	texts: Vec<String>,
}
impl RawManySourceKeys<'_> {
	pub(crate) fn request(&self) -> &RawManyReadRequest {
		self.request
	}
	pub(crate) fn texts(&self) -> &[String] {
		&self.texts
	}
}
#[derive(Debug, thiserror::Error)]
pub(in crate::kv) enum RawManySourceKeyInnerError {
	#[error("requested source key is not UTF-8")]
	Utf8(#[source] std::str::Utf8Error),
	#[error("requested source key does not use the published canonical grammar")]
	Canonical(#[source] KeyError),
}
#[derive(Debug, thiserror::Error)]
#[error("raw multi-record source key at input position {input_index} is invalid")]
pub(in crate::kv) struct RawManySourceKeyError {
	input_index: usize,
	key: RecordKey,
	#[source]
	source: RawManySourceKeyInnerError,
}
impl RawManySourceKeyError {
	pub(crate) fn input_index(&self) -> usize {
		self.input_index
	}
	pub(crate) fn key(&self) -> &RecordKey {
		&self.key
	}
}

/// The result of a many-record read.
#[derive(Debug)]
pub enum RawManyReadResult {
	/// The request had no keys, so Paranoid did no database work.
	Empty,
	/// Paranoid read every requested key.
	Observed(RawManyObservation),
}

/// The records of a many-record read, one entry per requested key in request order. An
/// absent or expired record has an entry without a value.
#[derive(Debug)]
pub struct RawManyObservation {
	entries: Vec<RawManyReadEntry>,
	observed_at: StateTimestampMicros,
}
impl RawManyObservation {
	/// One entry per requested key, in request order.
	pub fn entries(&self) -> &[RawManyReadEntry] {
		&self.entries
	}
	/// The database time at which the statement read the records.
	pub fn observed_at(&self) -> StateTimestampMicros {
		self.observed_at
	}
	/// The entries and the database time.
	pub fn into_parts(self) -> (Vec<RawManyReadEntry>, StateTimestampMicros) {
		(self.entries, self.observed_at)
	}
}
/// One requested key and its live value, or `None` when the record is absent or expired.
pub struct RawManyReadEntry {
	key: RecordKey,
	value: Option<ObservedRawValue>,
}
impl RawManyReadEntry {
	/// The requested key.
	pub fn key(&self) -> &RecordKey {
		&self.key
	}
	/// The live value, or `None` when the record is absent or expired.
	pub fn value(&self) -> Option<&ObservedRawValue> {
		self.value.as_ref()
	}
	/// The key and the value.
	pub fn into_parts(self) -> (RecordKey, Option<ObservedRawValue>) {
		(self.key, self.value)
	}
}
impl fmt::Debug for RawManyReadEntry {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("RawManyReadEntry")
			.field("key", &self.key)
			.field("value", &self.value)
			.finish()
	}
}

fn source_texts(keys: &[RecordKey]) -> Result<Vec<String>, RawManySourceKeyError> {
	let mut texts = Vec::with_capacity(keys.len());
	for (input_index, key) in keys.iter().enumerate() {
		let text = std::str::from_utf8(key.as_bytes()).map_err(|error| RawManySourceKeyError {
			input_index,
			key: key.clone(),
			source: RawManySourceKeyInnerError::Utf8(error),
		})?;
		let parsed =
			Key::from_persisted_text(text.to_owned()).map_err(|error| RawManySourceKeyError {
				input_index,
				key: key.clone(),
				source: RawManySourceKeyInnerError::Canonical(error),
			})?;
		texts.push(parsed.into_string());
	}
	Ok(texts)
}
