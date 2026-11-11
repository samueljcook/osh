//! Body-free pages retain actual valued keys, absolute metadata, and contextual continuations.

use super::{RawPageContext, RawPageContinuationFields, RecordPageCursor, RecordPageRequest};
use crate::kv::raw::postgres::key_headers::KeyHeaderPage;
use crate::records::{RecordKey, RecordMetadata, StateTimestampMicros};

/// One record of a key page: its key and its metadata, without its value.
#[derive(Debug)]
pub struct RecordKeyEntry {
	key: RecordKey,
	metadata: RecordMetadata,
}
impl RecordKeyEntry {
	/// The record's key.
	pub fn key(&self) -> &RecordKey {
		&self.key
	}
	/// The record's expiry and modification time.
	pub fn metadata(&self) -> RecordMetadata {
		self.metadata
	}
	/// The key and the metadata.
	pub fn into_parts(self) -> (RecordKey, RecordMetadata) {
		(self.key, self.metadata)
	}
}

/// One page of keys in key order, read in one statement. Each page is a separate read, so
/// records can change between pages.
#[derive(Debug)]
pub struct RecordKeyPage {
	entries: Vec<RecordKeyEntry>,
	observed_at: StateTimestampMicros,
	continuation: Option<RecordPageCursor>,
}
pub(in crate::kv) struct RecordKeyPageFields<'request> {
	pub(in crate::kv) page: KeyHeaderPage,
	pub(in crate::kv) request: &'request RecordPageRequest,
	pub(in crate::kv) context: RawPageContext,
}
impl RecordKeyPage {
	pub(in crate::kv) fn from_headers(fields: RecordKeyPageFields<'_>) -> Self {
		let page = fields.page;
		let continuation = fields.request.continuation(RawPageContinuationFields {
			context: fields.context,
			returned_count: page.entries.len(),
			last_key: page.entries.last().map(|entry| &entry.key),
		});
		Self {
			entries: page
				.entries
				.into_iter()
				.map(|entry| RecordKeyEntry {
					key: entry.key,
					metadata: entry.metadata,
				})
				.collect(),
			observed_at: page.observed_at,
			continuation,
		}
	}
	/// The page's records in byte order of their keys.
	pub fn entries(&self) -> &[RecordKeyEntry] {
		&self.entries
	}
	/// The database time at which the page was read.
	pub fn observed_at(&self) -> StateTimestampMicros {
		self.observed_at
	}
	/// The cursor of the next page when this page is full, or `None` when the page holds
	/// fewer records than the limit.
	pub fn continuation(&self) -> Option<&RecordPageCursor> {
		self.continuation.as_ref()
	}
	/// The records, the database time, and the cursor.
	pub fn into_parts(
		self,
	) -> (
		Vec<RecordKeyEntry>,
		StateTimestampMicros,
		Option<RecordPageCursor>,
	) {
		(self.entries, self.observed_at, self.continuation)
	}
}
