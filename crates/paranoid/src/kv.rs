//! Key-value storage in the installation's records: byte values and typed documents.
//!
//! [`PgRaw`] reads and writes byte values without interpreting them, in the installation's core
//! Raw namespace ([`PgRuntime::core_raw`](crate::runtime::PgRuntime::core_raw)) or in one
//! declared Raw scope ([`PgRuntime::raw`](crate::runtime::PgRuntime::raw)). [`PgRaw::encrypted`]
//! returns a [`PgEncryptedRaw`] view that encrypts each value with associated data that contains
//! the handle's Raw domain (either the core Raw namespace or the declared
//! [`ScopeIdentity`](crate::scope::ScopeIdentity)) and the key. [`PgDocument`]
//! ([`PgRuntime::document`](crate::runtime::PgRuntime::document)) stores
//! the typed documents of one declared document scope, keeps their index entries, and runs index
//! queries.
//!
//! Every handle addresses a value by a [`RecordKey`] of bytes. While a namespace is still in v10
//! storage, and always for `PgEncryptedRaw`, the key must be the text of a valid v10 key. [`Key`]
//! builds that text from parts, and `RecordKey::from(&key)` is also the record key of a value
//! that published v10 stored under that key. A [`KeyPrefix`] builds the prefix of such keys.
//!
//! A [`Ttl`] counts from the database clock reading that a write takes after it locks its
//! records, and every time in a result comes from the database clock. An operation on a pool
//! runs in its own transaction. A change in the caller's transaction becomes durable only when
//! the caller commits.
//!
//! # Document code
//!
//! A [`DocumentCode`] holds a document's [`StateSchema`], its [`IndexContract`]s, and, for each
//! JSON stored format version, at most one [`IndexProjector`] for each index. A version can have
//! a reader and no encoder so that the code can still read values that an earlier release
//! stored. The installation's recorded policy, not the order of the versions, selects the
//! version that Paranoid writes and records when an index is ready for queries. Paranoid chooses
//! the stored field names and the database index names. A declaration grants no database access
//! and does not change an installation.

use std::time::Duration;

pub use crate::records::v10_key::{KEY_SEPARATOR, Key, KeyError, KeyPrefix, MAX_KEY_LEN};

/// Minimum accepted positive KV TTL.
pub const MIN_TTL: Duration = Duration::from_secs(1);

/// Maximum accepted number of rows deleted by one batch-delete statement.
pub const MAX_DELETE_BATCH_SIZE: u32 = 10_000;

/// The largest page limit of a Raw page request or a document query.
pub const MAX_PAGE_LIMIT: u32 = 10_000;

/// Maximum number of candidate keys accepted by one slot acquisition.
const MAX_ACQUIRE_SLOT_CANDIDATES: usize = 10_000;

/// Validated KV TTL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ttl {
	positive_duration: Option<Duration>,
}

/// A `KeyLifetime` is the remaining lifetime of a live KV key at the database clock reading of
/// the statement that read the key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyLifetime {
	/// The key has no expiration.
	NoExpiration,
	/// The key expires after this duration, which is the time from the statement's database
	/// clock reading to the key's stored expiration.
	ExpiresIn(Duration),
}

pub use crate::records::StateTimestampMicros;

/// Current live value presented to an atomic mutation decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AtomicMutationCurrent<'a> {
	live_value: Option<&'a [u8]>,
	observed_at: StateTimestampMicros,
}

/// Current live value presented to an existing-key atomic mutation decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AtomicLiveMutationCurrent<'a> {
	live_value: &'a [u8],
	observed_at: StateTimestampMicros,
}

/// Mutation to apply after a key has been locked.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AtomicMutation {
	/// Keep the loaded live value, or keep absence if no live value exists. After
	/// `mutate_live_or_init` initialized the value, Paranoid stores the initial value with the
	/// initial TTL.
	KeepExisting,
	/// Store replacement bytes with the supplied TTL.
	SetBytes {
		/// Replacement value bytes.
		value: Vec<u8>,
		/// Replacement TTL.
		ttl: Ttl,
	},
	/// Store replacement bytes while preserving the locked row's current expiration. After
	/// `mutate_live_or_init` initialized the value, Paranoid uses the initial TTL. When the
	/// record is absent or expired, `mutate` fails with the kind `NotLive`.
	SetBytesPreservingExpiration {
		/// Replacement value bytes.
		value: Vec<u8>,
	},
	/// Physically delete the key row, even if the existing row is expired.
	Delete,
}

/// Outcome of an atomic mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AtomicMutationOutcome {
	/// A live value existed and was kept.
	KeptLiveValue,
	/// No live value existed and absence was kept.
	KeptAbsent,
	/// Replacement bytes were stored.
	SetBytes,
	/// Replacement bytes were stored without changing the previous expiration.
	SetBytesPreservingExpiration,
	/// A live value existed and was deleted.
	DeletedLiveValue,
	/// No live value existed and the key was left absent.
	DeletedAbsent,
}

/// Result returned after an atomic mutation is applied.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct AtomicMutationResult {
	/// Live value observed while the key lock was held.
	pub previous_live_value: Option<Vec<u8>>,
	/// Mutation outcome.
	pub outcome: AtomicMutationOutcome,
}

pub(crate) mod document;
mod error;
mod keys;
mod mutation;
pub(crate) mod raw;
mod validation;

pub use error::{PageLimitError, TtlError};

pub use crate::kv::raw::postgres::slot::RawRejectionSettlement;
pub use crate::records::state_schema::{
	StateDecoder, StateEncoder, StateRepresentationFormat, StateSchema, StateSchemaError,
	StateSchemaFields, StateVersion,
};
pub use crate::records::{
	IndexScalar, MAX_MANY_RECORD_ENTRIES, ManyRecordRequestError, RecordHeaderObservation,
	RecordIdentity, RecordIdentityError, RecordKey, RecordMetadata, RecordNamespace,
};
pub use crate::runtime::{PgScopeHandleError, PgScopeHandleErrorKind};
pub use document::handle::{
	DocumentSelectFields, DocumentSelectionEntry, DocumentSelectionPage, PgDocument,
	PgDocumentError, PgDocumentErrorKind, PgDocumentManyError, PgDocumentManyErrorKind,
	PgDocumentNamespaceError, PgDocumentNamespaceErrorKind, PgDocumentProjectionReason,
	PgDocumentSelectionError, PgDocumentSelectionErrorKind, PgDocumentSelectionProjectionReason,
};
pub use document::query::{
	DocumentQueryCursor, DocumentQueryDirection, DocumentQueryError, DocumentQueryErrorKind,
	DocumentQueryFields, DocumentQueryRange, DocumentQuerySelectionFields,
	ScopedDocumentIndexQuery,
};
pub use document::schema::owned::{
	DocumentCode, DocumentCodeFields, IndexProjectorEntry, RepresentationProjectors,
};
pub use document::schema::{
	DocumentDeclarationError, IndexContract, IndexContractIdentity, IndexFieldDef, IndexProjection,
	IndexProjector, IndexScalarType,
};
pub use document::state::{
	DocumentConditionalWriteResult, DocumentLiveMutationCurrent, DocumentLiveOrInitMutationResult,
	DocumentLiveOrInitValue, DocumentManyObservation, DocumentManyReadEntry,
	DocumentManyReadRequest, DocumentManyReadRequestFields, DocumentManyReadResult,
	DocumentManySetRequest, DocumentManySetRequestFields, DocumentManyWriteResult,
	DocumentMutation, DocumentMutationCurrent, DocumentMutationOutcome, DocumentMutationResult,
	DocumentRead, DocumentSetEntry, DocumentSlotRequest, DocumentSlotRequestFields,
	DocumentSlotResult, SlotRequestError,
};
pub use raw::handle::{
	PgEncryptedRaw, PgEncryptedRawError, PgEncryptedRawErrorKind, PgEncryptedRawFields,
	PgEncryptedRawMutationError, PgEncryptedRawMutationErrorKind, PgRaw, PgRawError,
	PgRawErrorKind, PgRawFields,
};
pub use raw::model::aggregate::RecordLiveCount;
pub use raw::model::cleanup::{
	RawCleanupTimingError, RawExpiredCleanupCommitted, RawExpiredCleanupRunRequest,
	RawExpiredCleanupRunRequestFields, RawExpiredCleanupRunResult,
};
pub use raw::model::delete::{
	RecordDeleteBatchLimits, RecordDeleteBatchLimitsFields, RecordDeleteBatchRequest,
	RecordDeleteBatchRequestFields, RecordDeleteBatchResult, RecordDeleteContention,
	RecordDeleteKind, RecordDeleteRequestError, RecordDeleteRetained, RecordDeleteRetainedReason,
};
pub use raw::model::encryption::{
	DecryptedRawLiveMutationCurrent, DecryptedRawLiveOrInitMutationResult,
	DecryptedRawMutationCurrent, DecryptedRawMutationResult, DecryptedRawObservation,
	DecryptedRawValue, EncryptedRawMutation,
};
pub use raw::model::many::{
	RawManyObservation, RawManyReadEntry, RawManyReadRequest, RawManyReadRequestFields,
	RawManyReadResult, RawManySetRequest, RawManySetRequestFields, RawManyWriteResult, RawSetEntry,
};
pub use raw::model::namespace::{
	RecordSelectionDeleteRequest, RecordSelectionDeleteRequestFields, RecordSelectionDeleteResult,
};
pub use raw::model::page::{
	RawValuePage, RawValuePageEntry, RecordKeyEntry, RecordKeyPage, RecordKeyPrefix,
	RecordKeySelection, RecordPageCursor, RecordPageRequest, RecordPageRequestError,
	RecordPageRequestFields, RecordPageVisibility,
};
pub use raw::model::slot::{
	RawSlotRequest, RawSlotRequestFields, RawSlotResult, RawSlotSelectionRequest,
	RawSlotSelectionRequestFields,
};
pub use raw::model::slot_preparation::{
	EncryptedRawSlotInputError, EncryptedRawSlotRequest, EncryptedRawSlotRequestFields,
	RawSlotAssociatedData,
};
pub use raw::state::{
	ConditionalRawWriteResult, ObservedRawValue, RawLiveOrInitMutationResult, RawLiveOrInitValue,
	RawValueObservation, RawWriteResult,
};
use validation::*;
