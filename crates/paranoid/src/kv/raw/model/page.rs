//! Ordinary opaque value pages and contextual cursors, without snapshot or mutation authority.

mod key_page;
pub(in crate::kv) use key_page::RecordKeyPageFields;
pub use key_page::{RecordKeyEntry, RecordKeyPage};

use std::{fmt, num::NonZeroU32, sync::Arc};

use crate::db::PgQualifiedTableName;
use crate::records::{RecordKey, RecordMetadata, RecordNamespace, StateTimestampMicros};
use crate::runtime::admission::bindings::PgInstallationAdmissionBinding;
use crate::scope::ScopeIdentity;

use crate::kv::{Key, KeyError, KeyPrefix, MAX_PAGE_LIMIT, PageLimitError};

/// A nonempty key prefix. A request for the whole namespace uses `RecordKeySelection::All`.
#[derive(Clone, Eq, PartialEq)]
pub struct RecordKeyPrefix(Vec<u8>);
impl RecordKeyPrefix {
	/// Builds a key prefix from nonempty bytes.
	pub fn new(bytes: Vec<u8>) -> Result<Self, RecordPageRequestError> {
		if bytes.is_empty() {
			return Err(RecordPageRequestError::EmptyPrefix);
		}
		Ok(Self(bytes))
	}
	/// The prefix bytes.
	pub fn as_bytes(&self) -> &[u8] {
		&self.0
	}
	/// The smallest byte string above every key with this prefix, if one exists.
	pub(crate) fn exclusive_upper_bound(&self) -> Option<Vec<u8>> {
		primitives_rs::bytes::prefix_range::exclusive_upper_bound(&self.0)
	}
}

impl fmt::Debug for RecordKeyPrefix {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("RecordKeyPrefix")
			.field("byte_len", &self.0.len())
			.finish()
	}
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// The keys that a request covers: the whole namespace or the keys with one prefix.
pub enum RecordKeySelection {
	/// Every key in the namespace.
	All,
	/// The keys that start with the prefix.
	Prefix(RecordKeyPrefix),
}
impl RecordKeySelection {
	/// Whether the selection covers `key`.
	pub fn contains(&self, key: &RecordKey) -> bool {
		match self {
			Self::All => true,
			Self::Prefix(prefix) => key.as_bytes().starts_with(prefix.as_bytes()),
		}
	}
	/// The selected prefix, or `None` for the whole namespace.
	pub fn prefix(&self) -> Option<&RecordKeyPrefix> {
		match self {
			Self::All => None,
			Self::Prefix(prefix) => Some(prefix),
		}
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Whether a page includes expired records.
pub enum RecordPageVisibility {
	/// The page includes only the records that are live at the page's database time.
	Live,
	/// The page includes every record that has a value, including expired records.
	AllValued,
}

#[derive(Debug, thiserror::Error)]
/// A page request that Paranoid rejected.
pub enum RecordPageRequestError {
	#[error("a raw byte prefix must not be empty; use All for a whole namespace")]
	/// The prefix is empty. A request for the whole namespace uses `RecordKeySelection::All`.
	EmptyPrefix,
	#[error("raw page row limit is invalid")]
	/// The page limit exceeds the largest supported page size.
	Limit(#[from] PageLimitError),
	#[error("raw page cursor came from another runtime, namespace, selection, or visibility")]
	/// The cursor came from a page that a handle of another runtime read, or from a page of
	/// another namespace, selection, or visibility. Each [`PgRuntime`](crate::runtime::PgRuntime)
	/// accepts only the cursors of its own handles, also when another runtime opened the same
	/// installation.
	CursorContextMismatch,
	#[error("raw page cursor is outside its requested prefix")]
	/// The cursor's key does not start with the requested prefix.
	CursorOutsidePrefix,
	#[error("raw page source-domain input is not UTF-8")]
	/// While the namespace is still in v10 storage, the prefix or cursor key is not UTF-8.
	SourceUtf8(#[from] std::str::Utf8Error),
	#[error("raw page source-domain input does not use the published key grammar")]
	/// While the namespace is still in v10 storage, the prefix or cursor key is not a
	/// valid v10 key.
	SourceKey(#[from] KeyError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::kv) enum RawPageOwner {
	Core,
	Scope(ScopeIdentity),
}

/// Existing placement identities are retained without cloning storage or admission authority.
#[derive(Clone)]
pub(in crate::kv) struct RawPageContext {
	pub(in crate::kv) installation: Arc<PgInstallationAdmissionBinding>,
	pub(in crate::kv) owner: RawPageOwner,
	pub(in crate::kv) table: PgQualifiedTableName,
	pub(in crate::kv) namespace: RecordNamespace,
}
impl RawPageContext {
	fn matches(&self, other: &Self) -> bool {
		Arc::ptr_eq(&self.installation, &other.installation)
			&& self.owner == other.owner
			&& self.table == other.table
			&& self.namespace == other.namespace
	}
}

/// The position after the last key of a full page. A request with the cursor reads the next
/// page of the same namespace, selection, and visibility, with a handle of the runtime that read
/// the cursor's page.
#[derive(Clone)]
pub struct RecordPageCursor {
	context: RawPageContext,
	selection: RecordKeySelection,
	visibility: RecordPageVisibility,
	after: RecordKey,
}
impl RecordPageCursor {
	/// The last key of the previous page; the next page starts after it.
	pub fn after_key(&self) -> &RecordKey {
		&self.after
	}
}
impl fmt::Debug for RecordPageCursor {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("RecordPageCursor")
			.field("visibility", &self.visibility)
			.finish_non_exhaustive()
	}
}
/// The inputs of a key page request.
pub struct RecordPageRequestFields {
	/// The whole namespace or a nonempty key prefix.
	pub selection: RecordKeySelection,
	/// Whether the page lists live records or every record that has a value.
	pub visibility: RecordPageVisibility,
	/// The cursor of the previous page, or `None` for the first page.
	pub cursor: Option<RecordPageCursor>,
	/// The maximum number of keys in the page. It must be positive.
	pub limit: NonZeroU32,
}

/// A validated request for one page of keys or values.
#[derive(Clone)]
pub struct RecordPageRequest {
	selection: RecordKeySelection,
	visibility: RecordPageVisibility,
	cursor: Option<RecordPageCursor>,
	limit: NonZeroU32,
}

/// Actual contextual ownership and the independently checked physical filter.
pub(in crate::kv) struct RawPageContextSelectionFields<'a> {
	pub(in crate::kv) context: &'a RawPageContext,
	pub(in crate::kv) effective_selection: &'a RecordKeySelection,
}

impl RecordPageRequest {
	/// Validates the request without database work. A cursor must come from a page with the
	/// same selection and visibility, and its key must be inside the selection.
	pub fn new(fields: RecordPageRequestFields) -> Result<Self, RecordPageRequestError> {
		if fields.limit.get() > MAX_PAGE_LIMIT {
			return Err(PageLimitError::TooLarge {
				actual: fields.limit.get(),
				max: MAX_PAGE_LIMIT,
			}
			.into());
		}
		if let Some(cursor) = &fields.cursor {
			if cursor.selection != fields.selection || cursor.visibility != fields.visibility {
				return Err(RecordPageRequestError::CursorContextMismatch);
			}
			if !fields.selection.contains(&cursor.after) {
				return Err(RecordPageRequestError::CursorOutsidePrefix);
			}
		}
		Ok(Self {
			selection: fields.selection,
			visibility: fields.visibility,
			cursor: fields.cursor,
			limit: fields.limit,
		})
	}
	/// The keys that the request covers.
	pub fn selection(&self) -> &RecordKeySelection {
		&self.selection
	}
	/// Whether the page includes expired records.
	pub fn visibility(&self) -> RecordPageVisibility {
		self.visibility
	}
	/// The key after which the page starts, or `None` for the first page.
	pub fn after_key(&self) -> Option<&RecordKey> {
		self.cursor.as_ref().map(RecordPageCursor::after_key)
	}
	/// Returns the largest number of keys in the page.
	pub fn limit(&self) -> NonZeroU32 {
		self.limit
	}
	pub(in crate::kv) fn require_context(
		&self,
		context: &RawPageContext,
	) -> Result<(), RecordPageRequestError> {
		self.require_context_with_selection(RawPageContextSelectionFields {
			context,
			effective_selection: &self.selection,
		})
	}
	/// Checks the actual cursor context without replacing its original requested selection.
	pub(in crate::kv) fn require_context_with_selection(
		&self,
		fields: RawPageContextSelectionFields<'_>,
	) -> Result<(), RecordPageRequestError> {
		if self.cursor.as_ref().is_some_and(|cursor| {
			!cursor.context.matches(fields.context)
				|| !fields.effective_selection.contains(&cursor.after)
		}) {
			return Err(RecordPageRequestError::CursorContextMismatch);
		}
		Ok(())
	}
	/// Source routes use the actual complete-key parser for both full keys and prefixes.
	pub(in crate::kv::raw) fn require_source_domain(&self) -> Result<(), RecordPageRequestError> {
		if let Some(prefix) = self.selection.prefix() {
			KeyPrefix::from_persisted_text(std::str::from_utf8(prefix.as_bytes())?.to_owned())?;
		}
		if let Some(after) = self.after_key() {
			Key::from_persisted_text(std::str::from_utf8(after.as_bytes())?.to_owned())?;
		}
		Ok(())
	}
	pub(in crate::kv::raw) fn finish(
		&self,
		context: RawPageContext,
		entries: Vec<RawValuePageEntry>,
		observed_at: StateTimestampMicros,
	) -> RawValuePage {
		let continuation = self.continuation(RawPageContinuationFields {
			context,
			returned_count: entries.len(),
			last_key: entries.last().map(RawValuePageEntry::key),
		});
		RawValuePage {
			entries,
			observed_at,
			continuation,
		}
	}
	/// Derives only a statement-local cursor from the actual returned count and final key.
	pub(in crate::kv) fn continuation(
		&self,
		fields: RawPageContinuationFields<'_>,
	) -> Option<RecordPageCursor> {
		if fields.returned_count != self.limit.get() as usize {
			return None;
		}
		fields.last_key.map(|key| RecordPageCursor {
			context: fields.context,
			selection: self.selection.clone(),
			visibility: self.visibility,
			after: key.clone(),
		})
	}
}

/// The concrete final row association; this carries no record body or admission proof.
pub(in crate::kv) struct RawPageContinuationFields<'a> {
	pub(in crate::kv) context: RawPageContext,
	pub(in crate::kv) returned_count: usize,
	pub(in crate::kv) last_key: Option<&'a RecordKey>,
}

/// One record of a value page: its key, its bytes, and its metadata.
pub struct RawValuePageEntry {
	key: RecordKey,
	bytes: Vec<u8>,
	metadata: RecordMetadata,
}
impl RawValuePageEntry {
	pub(crate) fn new(key: RecordKey, bytes: Vec<u8>, metadata: RecordMetadata) -> Self {
		Self {
			key,
			bytes,
			metadata,
		}
	}
	/// The record's key.
	pub fn key(&self) -> &RecordKey {
		&self.key
	}
	/// The stored bytes.
	pub fn bytes(&self) -> &[u8] {
		&self.bytes
	}
	/// The record's expiry and modification time.
	pub fn metadata(&self) -> RecordMetadata {
		self.metadata
	}
	/// The key, the bytes, and the metadata.
	pub fn into_parts(self) -> (RecordKey, Vec<u8>, RecordMetadata) {
		(self.key, self.bytes, self.metadata)
	}
}
impl fmt::Debug for RawValuePageEntry {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("RawValuePageEntry")
			.field("key", &self.key)
			.finish_non_exhaustive()
	}
}

/// One page of records in key order, read in one statement. Each page is a separate read, so
/// records can change between pages.
#[derive(Debug)]
pub struct RawValuePage {
	entries: Vec<RawValuePageEntry>,
	observed_at: StateTimestampMicros,
	continuation: Option<RecordPageCursor>,
}
impl RawValuePage {
	/// The page's records in key order.
	pub fn entries(&self) -> &[RawValuePageEntry] {
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
		Vec<RawValuePageEntry>,
		StateTimestampMicros,
		Option<RecordPageCursor>,
	) {
		(self.entries, self.observed_at, self.continuation)
	}
}
