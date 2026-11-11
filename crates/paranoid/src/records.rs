//! Record state and mutation contracts, and their PostgreSQL implementation.
//!
//! A caller admits its transaction and selects a representation before locking a record.
//! Projection derivation receives the encoded representation, not the latest working value.

mod batch;
pub(crate) use batch::check_many_record_keys;
pub use batch::{MAX_MANY_RECORD_ENTRIES, ManyRecordRequestError};
pub(crate) mod v10_key;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::num::NonZeroU32;

pub use primitives_rs::db::records::{
	RecordIdentity, RecordIdentityError, RecordKey, RecordNamespace,
};

/// A signed Unix timestamp in microseconds.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct StateTimestampMicros(i64);

impl StateTimestampMicros {
	/// Constructs a timestamp from its signed Unix microsecond value.
	pub const fn new(value: i64) -> Self {
		Self(value)
	}

	/// Returns the signed Unix microsecond value.
	pub const fn as_i64(self) -> i64 {
		self.0
	}
}

/// A record's expiry and modification time, which Paranoid stores beside its value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordMetadata {
	/// The time at which the record expires, or `None` when it never expires. A record is live
	/// before this time.
	pub expires_at: Option<StateTimestampMicros>,
	/// The database time of the record's last write, `touch`, `set_ttl`, or `expire`.
	pub updated_at: StateTimestampMicros,
}

impl RecordMetadata {
	pub(crate) fn is_live_at(self, timestamp: StateTimestampMicros) -> bool {
		self.expires_at.is_none_or(|expiry| timestamp < expiry)
	}
}

/// A record's metadata and the database clock at one read, without the decoded value.
///
/// An expired record still has its metadata. An absent record and a reservation (a stored key
/// without a value) have no metadata, but the observation still has the database clock of the
/// read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordHeaderObservation {
	metadata: Option<RecordMetadata>,
	observed_at: StateTimestampMicros,
}

impl RecordHeaderObservation {
	pub(crate) fn new(metadata: Option<RecordMetadata>, observed_at: StateTimestampMicros) -> Self {
		Self {
			metadata,
			observed_at,
		}
	}

	/// The record's metadata, including an expired record's metadata, or `None` for an absent
	/// record or a reservation.
	pub fn metadata(&self) -> Option<RecordMetadata> {
		self.metadata
	}

	/// The database clock of the read, also for an absent record.
	pub fn observed_at(&self) -> StateTimestampMicros {
		self.observed_at
	}

	/// Whether the record was live at the read's clock. A live record can still hold a value
	/// that the codec cannot decode.
	pub fn is_live(&self) -> bool {
		self.metadata
			.is_some_and(|metadata| metadata.is_live_at(self.observed_at))
	}
}

/// A typed scalar projected from an encoded body and its authoritative envelope.
#[derive(Clone, Eq, PartialEq)]
pub enum IndexScalar {
	/// A Boolean field value.
	Boolean(bool),
	/// A signed 64-bit integer field value.
	SignedInteger(i64),
	/// A signed Unix microsecond timestamp field value.
	Timestamp(StateTimestampMicros),
	/// Text compared by its UTF-8 bytes.
	BytewiseText(String),
	/// Opaque bytes compared bytewise.
	Bytes(Vec<u8>),
}

/// Fields with code-owned names and scalar semantics. Missing fields have no projection.
pub(crate) type IndexFields = BTreeMap<String, IndexScalar>;

/// A parsed physical value. Its bytes can still require a schema-specific parser.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct StoredRecord {
	body: Vec<u8>,
	metadata: RecordMetadata,
	index_fields: IndexFields,
}

impl StoredRecord {
	pub(crate) fn from_storage(
		body: Vec<u8>,
		metadata: RecordMetadata,
		index_fields: IndexFields,
	) -> Self {
		Self {
			body,
			metadata,
			index_fields,
		}
	}

	pub(crate) fn body(&self) -> &[u8] {
		&self.body
	}

	pub(crate) fn metadata(&self) -> RecordMetadata {
		self.metadata
	}

	pub(crate) fn index_fields(&self) -> &IndexFields {
		&self.index_fields
	}

	pub(crate) fn into_parts(self) -> (Vec<u8>, RecordMetadata, IndexFields) {
		(self.body, self.metadata, self.index_fields)
	}
}

impl fmt::Debug for StoredRecord {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("StoredRecord")
			.field("body_len", &self.body.len())
			.field("metadata", &self.metadata)
			.field("index_field_count", &self.index_fields.len())
			.finish()
	}
}

/// A record and one authoritative time observation. Internal reservations appear absent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PointRecordRead {
	record: PointRecordState,
	observed_at: StateTimestampMicros,
}

/// The stored state of one identity at a read.
#[derive(Clone, Debug, Eq, PartialEq)]
enum PointRecordState {
	/// A stored value, including an expired value.
	Value(StoredRecord),
	/// No stored value.
	NoValue,
	/// No stored value. The acquisition that made this read inserted the identity's reservation
	/// row and holds it, and it must remove the row unless it writes a value there.
	InsertedReservation,
}

impl PointRecordRead {
	pub(crate) fn new(record: Option<StoredRecord>, observed_at: StateTimestampMicros) -> Self {
		Self {
			record: match record {
				Some(record) => PointRecordState::Value(record),
				None => PointRecordState::NoValue,
			},
			observed_at,
		}
	}

	/// A read of a held identity, which records whether its acquisition inserted the row.
	pub(crate) fn held(
		row: postgres::HeldNativeRow<StoredRecord>,
		observed_at: StateTimestampMicros,
	) -> Self {
		Self {
			record: match row {
				postgres::HeldNativeRow::Value(record) => PointRecordState::Value(record),
				postgres::HeldNativeRow::Reservation(
					postgres::ReservationOwnership::Preexisting,
				) => PointRecordState::NoValue,
				postgres::HeldNativeRow::Reservation(postgres::ReservationOwnership::Inserted) => {
					PointRecordState::InsertedReservation
				}
			},
			observed_at,
		}
	}

	/// Returns the physical value, including an expired value that a mutation can replace.
	pub(crate) fn record(&self) -> Option<&StoredRecord> {
		match &self.record {
			PointRecordState::Value(record) => Some(record),
			PointRecordState::NoValue | PointRecordState::InsertedReservation => None,
		}
	}

	/// Returns a value only when its envelope is live at this observation.
	pub(crate) fn live_record(&self) -> Option<&StoredRecord> {
		self.record()
			.filter(|record| record.metadata.is_live_at(self.observed_at))
	}

	/// Whether the acquisition that made this read inserted the identity's reservation row.
	pub(crate) fn holds_inserted_reservation(&self) -> bool {
		matches!(self.record, PointRecordState::InsertedReservation)
	}

	pub(crate) fn observed_at(&self) -> StateTimestampMicros {
		self.observed_at
	}
}

/// An explicit bounded physical lookup within one namespace, after an optional key.
/// `None` starts at the namespace's first key, including an empty key. A bound excludes
/// that exact key. Neither form grants mutation, retirement, or participant authority.
#[derive(Clone, Debug)]
pub(crate) struct NamespacePageRequest {
	namespace: RecordNamespace,
	after_key: Option<RecordKey>,
	limit: NonZeroU32,
}

impl NamespacePageRequest {
	pub(crate) fn new(
		namespace: RecordNamespace,
		after_key: Option<RecordKey>,
		limit: NonZeroU32,
	) -> Self {
		Self {
			namespace,
			after_key,
			limit,
		}
	}

	pub(crate) fn namespace(&self) -> &RecordNamespace {
		&self.namespace
	}

	pub(crate) fn after_key(&self) -> Option<&RecordKey> {
		self.after_key.as_ref()
	}

	pub(crate) fn limit(&self) -> NonZeroU32 {
		self.limit
	}
}

/// A physical value paired with the exact identity at which the backend read it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecordPageEntry {
	identity: RecordIdentity,
	record: StoredRecord,
}

impl RecordPageEntry {
	pub(crate) fn new(identity: RecordIdentity, record: StoredRecord) -> Self {
		Self { identity, record }
	}

	pub(crate) fn identity(&self) -> &RecordIdentity {
		&self.identity
	}

	pub(crate) fn record(&self) -> &StoredRecord {
		&self.record
	}
}

/// A backend page violates the exact requested namespace, bound, order, or row limit.
#[derive(Debug, thiserror::Error)]
pub(crate) enum InvalidRecordPageError {
	#[error("native record page exceeds its row limit")]
	LimitExceeded,
	#[error("native record page contains a different namespace")]
	NamespaceMismatch,
	#[error("native record page contains a key outside its exclusive lower bound")]
	LowerBoundViolation,
	#[error("native record page keys are not strictly increasing")]
	KeyOrderViolation,
}

/// One coherent physical page and authoritative clock, including expired physical values.
/// Reservations are excluded. Every entry must come from the same statement observation.
/// Pages are separate observations, not a stable snapshot: inserts before a later cursor can
/// be missed, and records may change between pages. Empty pages prove no durable completion.
/// Callers own durable progress and must separately pin/prove any controller prerequisites.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecordPageRead {
	entries: Vec<RecordPageEntry>,
	observed_at: StateTimestampMicros,
}

impl RecordPageRead {
	pub(crate) fn parse(
		request: &NamespacePageRequest,
		entries: Vec<RecordPageEntry>,
		observed_at: StateTimestampMicros,
	) -> Result<Self, InvalidRecordPageError> {
		if entries.len() > request.limit.get() as usize {
			return Err(InvalidRecordPageError::LimitExceeded);
		}
		let mut previous = request.after_key.as_ref();
		for entry in &entries {
			if entry.identity.namespace() != request.namespace() {
				return Err(InvalidRecordPageError::NamespaceMismatch);
			}
			let key = entry.identity.key();
			if request.after_key.as_ref().is_some_and(|bound| key <= bound) {
				return Err(InvalidRecordPageError::LowerBoundViolation);
			}
			if previous.is_some_and(|earlier| key <= earlier) {
				return Err(InvalidRecordPageError::KeyOrderViolation);
			}
			previous = Some(key);
		}
		Ok(Self {
			entries,
			observed_at,
		})
	}

	pub(crate) fn entries(&self) -> &[RecordPageEntry] {
		&self.entries
	}

	pub(crate) fn observed_at(&self) -> StateTimestampMicros {
		self.observed_at
	}
}

/// Body, envelope, and projections derived together for one selected representation.
pub(crate) struct EncodedRecord {
	record: StoredRecord,
}

impl EncodedRecord {
	/// Derives indexes from the exact bytes that the backend will persist.
	pub(crate) fn derive<E>(
		body: Vec<u8>,
		metadata: RecordMetadata,
		projection_fn: impl FnOnce(&[u8], RecordMetadata) -> Result<IndexFields, E>,
	) -> Result<Self, E> {
		let index_fields = projection_fn(&body, metadata)?;
		Ok(Self {
			record: StoredRecord::from_storage(body, metadata, index_fields),
		})
	}

	pub(crate) fn record(&self) -> &StoredRecord {
		&self.record
	}
	pub(crate) fn into_record(self) -> StoredRecord {
		self.record
	}
}

impl fmt::Debug for EncodedRecord {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("EncodedRecord")
			.field("record", &self.record)
			.finish()
	}
}

/// Metadata and indexes derived under held authority while preserving the stored body.
/// Semantic primitive code derives projections from the exact locked body or explicitly
/// selects an unindexed update for an opaque record. This is an internal
/// operation boundary, not an application API for independently editing projections.
pub(crate) struct EncodedMetadataUpdate {
	metadata: RecordMetadata,
	index_fields: IndexFields,
}

impl EncodedMetadataUpdate {
	/// Raw records have no body-dependent index projections.
	pub(crate) fn unindexed(metadata: RecordMetadata) -> Self {
		Self {
			metadata,
			index_fields: IndexFields::default(),
		}
	}

	pub(crate) fn derive<E>(
		body: &[u8],
		metadata: RecordMetadata,
		projection_fn: impl FnOnce(&[u8], RecordMetadata) -> Result<IndexFields, E>,
	) -> Result<Self, E> {
		Ok(Self {
			metadata,
			index_fields: projection_fn(body, metadata)?,
		})
	}

	pub(crate) fn metadata(&self) -> RecordMetadata {
		self.metadata
	}

	pub(crate) fn index_fields(&self) -> &IndexFields {
		&self.index_fields
	}
}

impl fmt::Debug for EncodedMetadataUpdate {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("EncodedMetadataUpdate")
			.field("metadata", &self.metadata)
			.field("index_field_count", &self.index_fields.len())
			.finish()
	}
}

/// Mutation authority retained by the exact transaction that acquired a point identity.
///
/// The snapshot's time must be observed after the acquisition's required waits. Finishing one of
/// these operations releases the Rust borrow, not the database transaction's locks. Acquisition
/// must track unresolved mutation before its first backend await. Failed or abandoned acquisition,
/// a dropped or forgotten lock, and failed or abandoned completion must prevent transaction commit
/// and require rollback. Successful `write`, `update_metadata`, `delete`, or rejection cleanup
/// through `keep` completes that mutation; completion must not clear another mutation's rollback
/// requirement. Backends must enforce this without asynchronous destructor work.
///
/// A multi-record operation needs a group contract, not independent mutable borrows of one
/// transaction. Admission, selected writer, and primitive transitions belong to the caller.
pub(crate) trait PointRecordLock: Sized + Send {
	type Error: std::error::Error + Send + Sync + 'static;

	fn snapshot(&self) -> &PointRecordRead;

	/// Observes fresh authoritative time without changing the snapshot or finishing authority.
	/// Primitive code chooses timestamps after callbacks with this observation. The timestamp
	/// is an explicit code-selected value, not the later write statement's execution time.
	/// An observation error or cancellation must require rollback even if the guard is
	/// subsequently finished. The observation needs its own unresolved-mutation tracker.
	fn observe_time(
		&mut self,
	) -> impl Future<Output = Result<StateTimestampMicros, Self::Error>> + Send;

	/// Updates metadata and derived indexes of an existing value without rewriting its body.
	/// The primitive must derive the update from this guard's exact locked body.
	fn update_metadata(
		self,
		update: EncodedMetadataUpdate,
	) -> impl Future<Output = Result<(), Self::Error>> + Send;

	fn write(self, record: EncodedRecord) -> impl Future<Output = Result<(), Self::Error>> + Send;

	/// Deletes a physical value or reservation and returns whether a value was present.
	fn delete(self) -> impl Future<Output = Result<bool, Self::Error>> + Send;

	/// Keeps a value, removing an absent reservation that this acquisition inserted.
	fn keep(self) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// All acquired identities at one authoritative time observed after every required wait.
#[derive(Debug)]
pub(crate) struct RecordGroupRead {
	snapshots: BTreeMap<RecordIdentity, PointRecordRead>,
	observed_at: StateTimestampMicros,
}

impl RecordGroupRead {
	pub(crate) fn new(
		records: BTreeMap<RecordIdentity, postgres::HeldNativeRow<StoredRecord>>,
		observed_at: StateTimestampMicros,
	) -> Self {
		Self {
			snapshots: records
				.into_iter()
				.map(|(identity, row)| (identity, PointRecordRead::held(row, observed_at)))
				.collect(),
			observed_at,
		}
	}

	pub(crate) fn snapshots(&self) -> &BTreeMap<RecordIdentity, PointRecordRead> {
		&self.snapshots
	}

	/// The identities for which the group's acquisition inserted a reservation row.
	pub(crate) fn inserted_reservations(&self) -> BTreeSet<RecordIdentity> {
		self.snapshots
			.iter()
			.filter(|(_, snapshot)| snapshot.holds_inserted_reservation())
			.map(|(identity, _)| identity.clone())
			.collect()
	}

	pub(crate) fn observed_at(&self) -> StateTimestampMicros {
		self.observed_at
	}
	/// An owning backend refreshes the clock after all group/source waits while retaining
	/// authority. Stored bodies and lifetime metadata are preserved without copying their bytes.
	pub(in crate::records) fn refresh_observed_at(&mut self, observed_at: StateTimestampMicros) {
		self.observed_at = observed_at;
		for snapshot in self.snapshots.values_mut() {
			snapshot.observed_at = observed_at;
		}
	}
}

/// A final action on one held identity. A write never changes that identity.
#[derive(Debug)]
pub(crate) enum RecordMutation {
	Write(EncodedRecord),
	Delete,
	Keep,
}

/// A complete plan cannot address an unheld identity or act twice on one identity.
#[derive(Debug, thiserror::Error)]
pub(crate) enum InvalidGroupMutationError {
	#[error("native mutation addresses an unheld record {identity:?}")]
	UnknownIdentity { identity: RecordIdentity },
	#[error("native mutation acts twice on record {identity:?}")]
	DuplicateAction { identity: RecordIdentity },
}

/// Actions parsed against an acquired group. Omitted identities mean `Keep`.
pub(crate) struct RecordMutationPlan {
	actions: BTreeMap<RecordIdentity, RecordMutation>,
}

impl RecordMutationPlan {
	pub(crate) fn prepare(
		snapshot: &RecordGroupRead,
		actions: Vec<(RecordIdentity, RecordMutation)>,
	) -> Result<Self, InvalidGroupMutationError> {
		Self::prepare_records(snapshot.snapshots(), actions)
	}

	/// Parses complete held identity membership without requiring business body observations.
	pub(crate) fn prepare_records<R>(
		records: &BTreeMap<RecordIdentity, R>,
		actions: Vec<(RecordIdentity, RecordMutation)>,
	) -> Result<Self, InvalidGroupMutationError> {
		let mut parsed = BTreeMap::new();
		for (identity, action) in actions {
			if !records.contains_key(&identity) {
				return Err(InvalidGroupMutationError::UnknownIdentity { identity });
			}
			if parsed.contains_key(&identity) {
				return Err(InvalidGroupMutationError::DuplicateAction { identity });
			}
			parsed.insert(identity, action);
		}
		Ok(Self { actions: parsed })
	}

	pub(crate) fn into_actions(self) -> BTreeMap<RecordIdentity, RecordMutation> {
		self.actions
	}
}

/// Rejection preserves its reason even when reservation cleanup also fails.
#[derive(Debug, thiserror::Error)]
pub(crate) enum GroupMutationError<E: std::error::Error + 'static> {
	#[error("native group mutation database work failed")]
	Backend(#[source] E),
	#[error("native group mutation plan was rejected")]
	Rejected(#[source] InvalidGroupMutationError),
	#[error("native group mutation plan was rejected and reservation cleanup failed")]
	RejectionCleanup {
		rejection_error: InvalidGroupMutationError,
		#[source]
		cleanup_error: E,
	},
}

/// Mutation authority for a whole, fixed identity set in one exact transaction.
///
/// Acquisition deduplicates and sorts identities by bytewise namespace, then key, before
/// acquiring each identity once. Snapshots share one time observed after all lock waits.
/// There is no incremental acquisition. Callers must include every uniqueness and record
/// identity needed by their operation before acquiring the group.
///
/// `apply` checks the whole plan and prepares every projection before changing records.
/// Omitted identities mean `Keep`. Rejection and `keep` remove only new reservations and
/// preserve preexisting reservations. Writes, deletes, and cleanup remain atomic in the
/// caller's transaction; finishing releases the Rust borrow, not transaction locks.
///
/// One outer unresolved-mutation tracker starts before the first backend await and finishes
/// only after every action and cleanup succeeds. Errors, cancellation, abandonment, and
/// forgotten guards must prevent commit and require rollback even after some actions
/// succeeded. Pure rejection may finish the tracker only after all new reservations are
/// cleaned. Backends must enforce this without asynchronous destructor work.
pub(crate) trait RecordMutationGroup: Sized + Send {
	type Error: std::error::Error + Send + Sync + 'static;

	fn snapshot(&self) -> &RecordGroupRead;

	fn apply(
		self,
		actions: Vec<(RecordIdentity, RecordMutation)>,
	) -> impl Future<Output = Result<(), GroupMutationError<Self::Error>>> + Send;

	fn keep(self) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// Opaque bytes and physical metadata from a retired source representation.
pub(crate) struct SourcePointRecord {
	body: Vec<u8>,
	metadata: RecordMetadata,
}

impl SourcePointRecord {
	pub(crate) fn new(body: Vec<u8>, metadata: RecordMetadata) -> Self {
		Self { body, metadata }
	}
	pub(crate) fn body(&self) -> &[u8] {
		&self.body
	}
	pub(crate) fn metadata(&self) -> RecordMetadata {
		self.metadata
	}
}

impl fmt::Debug for SourcePointRecord {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("SourcePointRecord")
			.field("body_len", &self.body.len())
			.field("metadata", &self.metadata)
			.finish()
	}
}

/// Physical origin selects a codec explicitly; malformed native records never select source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PointRecordOrigin {
	Source,
	Native,
	Absent,
}

#[derive(Debug)]
pub(crate) enum RelocationRecord {
	Source(SourcePointRecord),
	Native(StoredRecord),
	Absent,
}

impl RelocationRecord {
	pub(crate) fn origin(&self) -> PointRecordOrigin {
		match self {
			Self::Source(_) => PointRecordOrigin::Source,
			Self::Native(_) => PointRecordOrigin::Native,
			Self::Absent => PointRecordOrigin::Absent,
		}
	}
	pub(crate) fn metadata(&self) -> Option<RecordMetadata> {
		match self {
			Self::Source(record) => Some(record.metadata()),
			Self::Native(record) => Some(record.metadata()),
			Self::Absent => None,
		}
	}
}

/// A coherent physical-origin lookup with one authoritative post-wait clock observation.
#[derive(Debug)]
pub(crate) struct RelocationRead {
	record: RelocationRecord,
	observed_at: StateTimestampMicros,
}

impl RelocationRead {
	pub(crate) fn new(record: RelocationRecord, observed_at: StateTimestampMicros) -> Self {
		Self {
			record,
			observed_at,
		}
	}
	pub(crate) fn record(&self) -> &RelocationRecord {
		&self.record
	}
	pub(crate) fn live_record(&self) -> Option<&RelocationRecord> {
		self.record
			.metadata()
			.filter(|metadata| metadata.is_live_at(self.observed_at))
			.map(|_| &self.record)
	}
	pub(crate) fn observed_at(&self) -> StateTimestampMicros {
		self.observed_at
	}
}

/// Point authority covering a retired source and its canonical native identity atomically.
///
/// This contract MUST only run under post-retirement admission that excludes source writers.
/// The adapter does not itself install or enforce that admission. Canonical native locking
/// precedes source locking; one outer tracker covers both representations through completion.
/// Source bytes select no codec implicitly. Conversion code must preserve the supplied absolute
/// expiry and modification time when promoting without a semantic mutation. A write persists
/// one explicitly encoded native record and removes the source in the same transaction.
/// `keep` preserves the source and preexisting native reservations, cleaning only a new native
/// reservation. Failed, cancelled, dropped, or forgotten work requires rollback, including
/// after a native write has succeeded. No asynchronous destructor performs backend work.
pub(crate) trait RelocatingPointRecordLock: Sized + Send {
	type Error: std::error::Error + Send + Sync + 'static;
	fn snapshot(&self) -> &RelocationRead;
	fn write(self, record: EncodedRecord) -> impl Future<Output = Result<(), Self::Error>> + Send;
	fn delete(self) -> impl Future<Output = Result<bool, Self::Error>> + Send;
	fn keep(self) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

#[derive(Debug, thiserror::Error)]
#[error("KV expiration exceeds the timestamp domain")]
pub(crate) struct RecordExpirationOverflowError;

pub(crate) mod metadata_mutation;
pub(crate) mod postgres;
pub(crate) mod state_schema;
