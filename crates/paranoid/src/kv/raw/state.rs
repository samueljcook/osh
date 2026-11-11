//! Raw KV transitions over backend-independent record authority.

use crate::kv::{
	AtomicLiveMutationCurrent, AtomicMutation, AtomicMutationCurrent, AtomicMutationOutcome,
	AtomicMutationResult, KeyLifetime, Ttl,
};
use crate::records::metadata_mutation::{RecordMetadataMutationLock, RecordMetadataUpdateLock};
use crate::records::{
	EncodedMetadataUpdate, EncodedRecord, IndexFields, PointRecordLock, PointRecordRead,
	RecordHeaderObservation, RecordMetadata, RelocatingPointRecordLock, RelocationRead,
	RelocationRecord, SourcePointRecord, StateTimestampMicros,
};
use std::convert::Infallible;
use std::future::Future;
use std::time::Duration;

/// A live record's bytes and metadata.
pub struct ObservedRawValue {
	bytes: Vec<u8>,
	metadata: RecordMetadata,
}
impl ObservedRawValue {
	/// The stored bytes.
	pub fn bytes(&self) -> &[u8] {
		&self.bytes
	}
	/// The record's expiry and modification time.
	pub fn metadata(&self) -> RecordMetadata {
		self.metadata
	}
	/// The bytes and the metadata.
	pub fn into_parts(self) -> (Vec<u8>, RecordMetadata) {
		(self.bytes, self.metadata)
	}
}
impl std::fmt::Debug for ObservedRawValue {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter
			.debug_struct("ObservedRawValue")
			.field("byte_len", &self.bytes.len())
			.field("metadata", &self.metadata)
			.finish()
	}
}

/// The result of a read of one record: its live value, if any, and the database time of the
/// read, which the result reports also when the record is absent or expired.
#[derive(Debug)]
pub struct RawValueObservation {
	value: Option<ObservedRawValue>,
	observed_at: StateTimestampMicros,
}
impl RawValueObservation {
	pub(super) fn into_value(self) -> Option<ObservedRawValue> {
		self.value
	}
	/// Consumes an exact physical record and preserves the ordinary live-only observation meaning.
	pub(super) fn from_owned_record(
		record: Option<crate::records::StoredRecord>,
		observed_at: StateTimestampMicros,
	) -> Self {
		let value = record
			.filter(|record| record.metadata().is_live_at(observed_at))
			.map(|record| {
				let (bytes, metadata, _) = record.into_parts();
				ObservedRawValue { bytes, metadata }
			});
		Self { value, observed_at }
	}
	/// The live value, or `None` when the record is absent or expired.
	pub fn value(&self) -> Option<&ObservedRawValue> {
		self.value.as_ref()
	}
	/// The database time of the read.
	pub fn observed_at(&self) -> StateTimestampMicros {
		self.observed_at
	}
	/// The value and the database time.
	pub fn into_parts(self) -> (Option<ObservedRawValue>, StateTimestampMicros) {
		(self.value, self.observed_at)
	}
}

pub(super) fn observe_live_bytes(snapshot: &PointRecordRead) -> RawValueObservation {
	RawValueObservation {
		value: snapshot.live_record().map(|record| ObservedRawValue {
			bytes: record.body().to_vec(),
			metadata: record.metadata(),
		}),
		observed_at: snapshot.observed_at(),
	}
}

/// Retains live source/native bytes and metadata from the same relocation observation.
pub(super) fn observe_relocated_bytes(snapshot: &RelocationRead) -> RawValueObservation {
	let current = relocation_current(snapshot);
	RawValueObservation {
		value: current.live.map(|live| ObservedRawValue {
			bytes: live.body.to_vec(),
			metadata: live.metadata,
		}),
		observed_at: current.observed_at,
	}
}

/// The result of a write: the database time of the write and the metadata that it stored. A
/// write inside the caller's transaction becomes durable only when the caller commits.
#[derive(Clone, Copy, Debug)]
pub struct RawWriteResult {
	observed_at: StateTimestampMicros,
	metadata: RecordMetadata,
}
impl RawWriteResult {
	/// The database time at which the write decided its result and computed the expiry.
	pub fn observed_at(&self) -> StateTimestampMicros {
		self.observed_at
	}
	/// The expiry and modification time that the write stored.
	pub fn metadata(&self) -> RecordMetadata {
		self.metadata
	}
}

/// The result of a write that happens only when the record is absent or expired.
#[derive(Clone, Copy, Debug)]
pub enum ConditionalRawWriteResult {
	/// The record was absent or expired, so the operation wrote the value.
	Created(RawWriteResult),
	/// The record was live, so the operation kept it. The result does not include the live
	/// record's value or metadata.
	Existing {
		/// The database time at which the record was live.
		observed_at: StateTimestampMicros,
	},
}

/// The value that the decision of `mutate_live_or_init` received, and where it came from.
/// `V` is `Vec<u8>` for [`PgRaw`](crate::kv::PgRaw) and a buffer that Paranoid zeroes for
/// [`PgEncryptedRaw`](crate::kv::PgEncryptedRaw). Formatting the value omits its bytes.
pub enum RawLiveOrInitValue<V> {
	/// The record was live, and the decision received its value.
	Existing {
		/// The record's value.
		value: V,
	},
	/// The record was absent or expired, and the decision received the value that the
	/// initializer returned. Paranoid stores that value only when the decision keeps it.
	Initialized {
		/// The initializer's value.
		value: V,
	},
}
impl<V> std::fmt::Debug for RawLiveOrInitValue<V> {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter
			.debug_struct(match self {
				Self::Existing { .. } => "Existing",
				Self::Initialized { .. } => "Initialized",
			})
			.finish_non_exhaustive()
	}
}

/// The result of `mutate_live_or_init`.
#[derive(Debug)]
pub struct RawLiveOrInitMutationResult {
	/// The bytes that the decision received: the live value or the initialized value.
	pub value_seen: RawLiveOrInitValue<Vec<u8>>,
	/// The change that the decision made.
	pub outcome: AtomicMutationOutcome,
}

/// A raw KV transition, its callback, or its reservation cleanup failed.
#[derive(Debug)]
pub(crate) enum RecordMutationError<C, S> {
	Callback(C),
	KeyNotFound,
	ExpirationOverflow,
	Storage(S),
	CleanupFailed {
		operation_error: Box<RecordMutationError<C, S>>,
		cleanup_error: S,
	},
}

impl<C, S> std::fmt::Display for RecordMutationError<C, S> {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str(match self {
			Self::Callback(_) => "KV mutation callback failed",
			Self::KeyNotFound => "KV key is absent or expired",
			Self::ExpirationOverflow => "KV expiration exceeds the timestamp domain",
			Self::Storage(_) => "KV storage operation failed",
			Self::CleanupFailed { .. } => "KV transition failed and its reservation cleanup failed",
		})
	}
}

impl<C, S> std::error::Error for RecordMutationError<C, S>
where
	C: std::error::Error + 'static,
	S: std::error::Error + 'static,
{
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Callback(error) => Some(error),
			Self::Storage(error) => Some(error),
			Self::CleanupFailed {
				operation_error, ..
			} => Some(operation_error.as_ref()),
			Self::KeyNotFound | Self::ExpirationOverflow => None,
		}
	}
}

/// Returns opaque live bytes without interpreting an application representation.
pub(super) fn get_bytes(snapshot: &PointRecordRead) -> Option<&[u8]> {
	snapshot.live_record().map(|record| record.body())
}

/// Reads raw bytes from the identified physical origin without interpreting their contents.
pub(super) fn get_relocated_bytes(snapshot: &RelocationRead) -> Option<&[u8]> {
	match snapshot.live_record()? {
		RelocationRecord::Source(record) => Some(record.body()),
		RelocationRecord::Native(record) => Some(record.body()),
		RelocationRecord::Absent => None,
	}
}

/// Converts opaque source bytes while retaining absolute expiry and modification time.
pub(super) fn convert_source_record(record: &SourcePointRecord) -> EncodedRecord {
	let metadata = record.metadata();
	raw_record(
		record.body().to_vec(),
		metadata.updated_at,
		metadata.expires_at,
	)
}

/// Returns lifetime relative to this snapshot's authoritative time.
pub(super) fn lifetime(snapshot: &RecordHeaderObservation) -> Option<KeyLifetime> {
	let metadata = snapshot.metadata().filter(|_| snapshot.is_live())?;
	Some(lifetime_at(metadata, snapshot.observed_at()))
}

fn lifetime_at(metadata: RecordMetadata, observed_at: StateTimestampMicros) -> KeyLifetime {
	match metadata.expires_at {
		None => KeyLifetime::NoExpiration,
		Some(expiration) => {
			let micros = i128::from(expiration.as_i64()) - i128::from(observed_at.as_i64());
			KeyLifetime::ExpiresIn(Duration::from_micros(
				u64::try_from(micros).expect("a live signed timestamp difference fits u64"),
			))
		}
	}
}

/// Runs the decision once under mutation authority and applies its raw-byte transition.
pub(super) async fn mutate<L: PointRecordLock, C>(
	lock: L,
	decide_mutation_fn: impl FnOnce(AtomicMutationCurrent<'_>) -> Result<AtomicMutation, C>,
) -> Result<AtomicMutationResult, RecordMutationError<C, L::Error>> {
	mutate_raw(PointMutationLock(lock), decide_mutation_fn).await
}

/// Replaces opaque bytes without exposing or copying the previous value.
pub(super) async fn set_bytes<L: RecordMetadataMutationLock>(
	lock: L,
	value: Vec<u8>,
	ttl: Ttl,
) -> Result<RawWriteResult, RecordMutationError<Infallible, L::Error>> {
	let outcome = match prepare_raw_write(RawWritePreparationFields {
		observed_at: lock.current().observed_at,
		ttl,
	}) {
		Ok(outcome) => outcome,
		Err(_) => {
			return Err(clean_after_rejection(
				RecordMutationError::ExpirationOverflow,
				lock.keep(),
			)
			.await);
		}
	};
	lock.write(outcome.encode(value))
		.await
		.map_err(RecordMutationError::Storage)?;
	Ok(outcome)
}

/// Conditionally creates a live value from the header observed after acquisition.
pub(super) async fn set_bytes_if_absent<L: RecordMetadataMutationLock>(
	lock: L,
	value: Vec<u8>,
	ttl: Ttl,
) -> Result<ConditionalRawWriteResult, RecordMutationError<Infallible, L::Error>> {
	let current = lock.current();
	if current.live_metadata.is_some() {
		lock.keep().await.map_err(RecordMutationError::Storage)?;
		return Ok(ConditionalRawWriteResult::Existing {
			observed_at: current.observed_at,
		});
	}
	set_bytes(lock, value, ttl)
		.await
		.map(ConditionalRawWriteResult::Created)
}

/// Deletes only a live value and retains cleanup failures after a rejected deletion.
pub(super) async fn delete_live<L: RecordMetadataMutationLock>(
	lock: L,
) -> Result<(), RecordMutationError<Infallible, L::Error>> {
	if lock.current().live_metadata.is_none() {
		return Err(clean_after_rejection(RecordMutationError::KeyNotFound, lock.keep()).await);
	}
	lock.delete()
		.await
		.map(|_| ())
		.map_err(RecordMutationError::Storage)
}

/// Applies one opaque-byte decision under explicit source/native relocation authority.
/// The caller must admit this transaction after retirement of source writers.
pub(super) async fn mutate_relocated<L: RelocatingPointRecordLock, C>(
	lock: L,
	decide_mutation_fn: impl FnOnce(AtomicMutationCurrent<'_>) -> Result<AtomicMutation, C>,
) -> Result<AtomicMutationResult, RecordMutationError<C, L::Error>> {
	mutate_raw(RelocationMutationLock(lock), decide_mutation_fn).await
}

/// Requires a live value before invoking the decision exactly once.
pub(super) async fn mutate_live<L: PointRecordLock, C>(
	lock: L,
	decide_mutation_fn: impl FnOnce(AtomicLiveMutationCurrent<'_>) -> Result<AtomicMutation, C>,
) -> Result<AtomicMutationResult, RecordMutationError<C, L::Error>> {
	mutate_live_raw(PointMutationLock(lock), decide_mutation_fn).await
}

/// Initializes absent or expired state in memory, then decides once under the same authority.
/// Initial bytes are persisted only if the final decision requires them.
pub(super) async fn mutate_live_or_init<L: PointRecordLock, C>(
	lock: L,
	initialize_value_fn: impl FnOnce(StateTimestampMicros) -> Result<(Vec<u8>, Ttl), C>,
	decide_mutation_fn: impl FnOnce(AtomicLiveMutationCurrent<'_>) -> Result<AtomicMutation, C>,
) -> Result<RawLiveOrInitMutationResult, RecordMutationError<C, L::Error>> {
	mutate_live_or_init_raw(
		PointMutationLock(lock),
		initialize_value_fn,
		decide_mutation_fn,
	)
	.await
}
pub(super) async fn mutate_live_or_init_relocated<L: RelocatingPointRecordLock, C>(
	lock: L,
	initialize_value_fn: impl FnOnce(StateTimestampMicros) -> Result<(Vec<u8>, Ttl), C>,
	decide_mutation_fn: impl FnOnce(AtomicLiveMutationCurrent<'_>) -> Result<AtomicMutation, C>,
) -> Result<RawLiveOrInitMutationResult, RecordMutationError<C, L::Error>> {
	mutate_live_or_init_raw(
		RelocationMutationLock(lock),
		initialize_value_fn,
		decide_mutation_fn,
	)
	.await
}
async fn mutate_live_or_init_raw<L: RawMutationLock, C>(
	lock: L,
	initialize_value_fn: impl FnOnce(StateTimestampMicros) -> Result<(Vec<u8>, Ttl), C>,
	decide_mutation_fn: impl FnOnce(AtomicLiveMutationCurrent<'_>) -> Result<AtomicMutation, C>,
) -> Result<RawLiveOrInitMutationResult, RecordMutationError<C, L::Error>> {
	let current = lock.current();
	let decision = match current.live {
		Some(live) => decide_mutation_fn(AtomicLiveMutationCurrent {
			live_value: live.body,
			observed_at: current.observed_at,
		})
		.map(LiveOrInitDecision::Existing),
		None => initialize_value_fn(current.observed_at).and_then(|(value_seen, ttl)| {
			let mutation = decide_mutation_fn(AtomicLiveMutationCurrent {
				live_value: &value_seen,
				observed_at: current.observed_at,
			})?;
			let mutation = match mutation {
				AtomicMutation::KeepExisting => AtomicMutation::SetBytes {
					value: value_seen.clone(),
					ttl,
				},
				AtomicMutation::SetBytesPreservingExpiration { value } => {
					AtomicMutation::SetBytes { value, ttl }
				}
				mutation => mutation,
			};
			Ok(LiveOrInitDecision::Initialized {
				value_seen,
				mutation,
			})
		}),
	};
	let decision = match decision {
		Ok(decision) => decision,
		Err(error) => {
			return Err(
				clean_after_rejection(RecordMutationError::Callback(error), lock.keep()).await,
			);
		}
	};
	match decision {
		LiveOrInitDecision::Existing(mutation) => {
			let result = apply_mutation(lock, mutation).await?;
			Ok(RawLiveOrInitMutationResult {
				value_seen: RawLiveOrInitValue::Existing {
					value: result.previous_live_value.expect(
						"an existing-value decision retains its live input until storage completes",
					),
				},
				outcome: result.outcome,
			})
		}
		LiveOrInitDecision::Initialized {
			value_seen,
			mutation,
		} => {
			let result = apply_mutation(lock, mutation).await?;
			Ok(RawLiveOrInitMutationResult {
				value_seen: RawLiveOrInitValue::Initialized { value: value_seen },
				outcome: result.outcome,
			})
		}
	}
}

enum LiveOrInitDecision {
	Existing(AtomicMutation),
	Initialized {
		value_seen: Vec<u8>,
		mutation: AtomicMutation,
	},
}

/// Requires a live source/native value before deciding one opaque-byte relocation mutation.
/// Reading source state does not itself convert or remove it.
/// The caller must admit this transaction after retirement of source writers.
pub(super) async fn mutate_live_relocated<L: RelocatingPointRecordLock, C>(
	lock: L,
	decide_mutation_fn: impl FnOnce(AtomicLiveMutationCurrent<'_>) -> Result<AtomicMutation, C>,
) -> Result<AtomicMutationResult, RecordMutationError<C, L::Error>> {
	mutate_live_raw(RelocationMutationLock(lock), decide_mutation_fn).await
}

async fn mutate_raw<L: RawMutationLock, C>(
	lock: L,
	decide_mutation_fn: impl FnOnce(AtomicMutationCurrent<'_>) -> Result<AtomicMutation, C>,
) -> Result<AtomicMutationResult, RecordMutationError<C, L::Error>> {
	let current = lock.current();
	let mutation = decide_mutation_fn(AtomicMutationCurrent {
		live_value: current.live.map(|live| live.body),
		observed_at: current.observed_at,
	});
	match mutation {
		Ok(mutation) => apply_mutation(lock, mutation).await,
		Err(error) => {
			Err(clean_after_rejection(RecordMutationError::Callback(error), lock.keep()).await)
		}
	}
}

async fn mutate_live_raw<L: RawMutationLock, C>(
	lock: L,
	decide_mutation_fn: impl FnOnce(AtomicLiveMutationCurrent<'_>) -> Result<AtomicMutation, C>,
) -> Result<AtomicMutationResult, RecordMutationError<C, L::Error>> {
	let current = lock.current();
	let mutation = match current.live {
		Some(live) => decide_mutation_fn(AtomicLiveMutationCurrent {
			live_value: live.body,
			observed_at: current.observed_at,
		})
		.map_err(RecordMutationError::Callback),
		None => Err(RecordMutationError::KeyNotFound),
	};
	match mutation {
		Ok(mutation) => apply_mutation(lock, mutation).await,
		Err(error) => Err(clean_after_rejection(error, lock.keep()).await),
	}
}

async fn apply_mutation<L: RawMutationLock, C>(
	lock: L,
	mutation: AtomicMutation,
) -> Result<AtomicMutationResult, RecordMutationError<C, L::Error>> {
	let (lock, prepared) = prepare_or_reject(lock, mutation).await?;
	let previous_live_value = lock.current().live.map(|live| live.body.to_vec());
	let had_live_value = previous_live_value.is_some();
	let outcome = apply_prepared(lock, prepared, had_live_value).await?;
	Ok(AtomicMutationResult {
		previous_live_value,
		outcome,
	})
}

async fn prepare_or_reject<L: RawMutationLock, C>(
	lock: L,
	mutation: AtomicMutation,
) -> Result<(L, PreparedRawMutation), RecordMutationError<C, L::Error>> {
	match prepare_mutation(&lock, mutation) {
		Ok(prepared) => Ok((lock, prepared)),
		Err(error) => Err(clean_after_rejection(error, lock.keep()).await),
	}
}

async fn apply_prepared<L: RawMutationLock, C>(
	lock: L,
	prepared: PreparedRawMutation,
	had_live_value: bool,
) -> Result<AtomicMutationOutcome, RecordMutationError<C, L::Error>> {
	let outcome = match prepared {
		PreparedRawMutation::Keep => {
			lock.keep().await.map_err(RecordMutationError::Storage)?;
			if had_live_value {
				AtomicMutationOutcome::KeptLiveValue
			} else {
				AtomicMutationOutcome::KeptAbsent
			}
		}
		PreparedRawMutation::Delete => {
			lock.delete().await.map_err(RecordMutationError::Storage)?;
			if had_live_value {
				AtomicMutationOutcome::DeletedLiveValue
			} else {
				AtomicMutationOutcome::DeletedAbsent
			}
		}
		PreparedRawMutation::Write { record, outcome } => {
			lock.write(record)
				.await
				.map_err(RecordMutationError::Storage)?;
			outcome
		}
	};
	Ok(outcome)
}

enum PreparedRawMutation {
	Keep,
	Delete,
	Write {
		record: EncodedRecord,
		outcome: AtomicMutationOutcome,
	},
}

/// Prepares all semantic rejection before copying a previous value for a success result. A
/// write uses the database time that the lock observed after it acquired the record, which is
/// also the time that the decision received, for its modification time and its expiry.
fn prepare_mutation<L: RawMutationLock, C>(
	lock: &L,
	mutation: AtomicMutation,
) -> Result<PreparedRawMutation, RecordMutationError<C, L::Error>> {
	let current = lock.current();
	match mutation {
		AtomicMutation::KeepExisting => Ok(PreparedRawMutation::Keep),
		AtomicMutation::Delete => Ok(PreparedRawMutation::Delete),
		AtomicMutation::SetBytes { value, ttl } => {
			let outcome = prepare_raw_write(RawWritePreparationFields {
				observed_at: current.observed_at,
				ttl,
			})
			.map_err(|_| RecordMutationError::ExpirationOverflow)?;
			Ok(PreparedRawMutation::Write {
				record: outcome.encode(value),
				outcome: AtomicMutationOutcome::SetBytes,
			})
		}
		AtomicMutation::SetBytesPreservingExpiration { value } => {
			let expires_at = current
				.live
				.ok_or(RecordMutationError::KeyNotFound)?
				.metadata
				.expires_at;
			Ok(PreparedRawMutation::Write {
				record: raw_record(value, current.observed_at, expires_at),
				outcome: AtomicMutationOutcome::SetBytesPreservingExpiration,
			})
		}
	}
}

/// Updates the modification time of a live value without changing its bytes or expiry.
pub(super) async fn touch<L: RecordMetadataUpdateLock>(
	lock: L,
) -> Result<(), RecordMutationError<Infallible, L::Error>> {
	update_metadata(lock, MetadataMutation::Touch).await
}

/// Replaces a live value's expiry relative to the post-lock authoritative time.
pub(super) async fn set_ttl<L: RecordMetadataUpdateLock>(
	lock: L,
	ttl: Ttl,
) -> Result<(), RecordMutationError<Infallible, L::Error>> {
	update_metadata(lock, MetadataMutation::SetTtl(ttl)).await
}

/// Expires a live value at the post-lock authoritative time.
pub(super) async fn expire<L: RecordMetadataUpdateLock>(
	lock: L,
) -> Result<(), RecordMutationError<Infallible, L::Error>> {
	update_metadata(lock, MetadataMutation::Expire).await
}

enum MetadataMutation {
	Touch,
	SetTtl(Ttl),
	Expire,
}

async fn update_metadata<L: RecordMetadataUpdateLock>(
	lock: L,
	mutation: MetadataMutation,
) -> Result<(), RecordMutationError<Infallible, L::Error>> {
	let snapshot = lock.current();
	let Some(metadata) = snapshot.live_metadata else {
		return Err(clean_after_rejection(RecordMutationError::KeyNotFound, lock.keep()).await);
	};
	let metadata = match prepare_metadata(metadata, snapshot.observed_at, mutation) {
		Ok(metadata) => metadata,
		Err(error) => return Err(clean_after_rejection(error, lock.keep()).await),
	};
	let replacement = EncodedMetadataUpdate::unindexed(metadata);
	lock.update_metadata(replacement)
		.await
		.map_err(RecordMutationError::Storage)
}

fn prepare_metadata<C, S>(
	original: RecordMetadata,
	observed_at: StateTimestampMicros,
	mutation: MetadataMutation,
) -> Result<RecordMetadata, RecordMutationError<C, S>> {
	let expires_at = match mutation {
		MetadataMutation::Touch => original.expires_at,
		MetadataMutation::Expire => Some(observed_at),
		MetadataMutation::SetTtl(ttl) => expiration(observed_at, ttl)?,
	};
	Ok(RecordMetadata {
		expires_at,
		updated_at: observed_at,
	})
}

fn expiration<C, S>(
	observed_at: StateTimestampMicros,
	ttl: Ttl,
) -> Result<Option<StateTimestampMicros>, RecordMutationError<C, S>> {
	ttl.expiration_at(observed_at)
		.map_err(|_| RecordMutationError::ExpirationOverflow)
}

fn raw_record(
	body: Vec<u8>,
	updated_at: StateTimestampMicros,
	expires_at: Option<StateTimestampMicros>,
) -> EncodedRecord {
	EncodedRecord::derive(
		body,
		RecordMetadata {
			expires_at,
			updated_at,
		},
		no_index_fields,
	)
	.expect("raw KV has no derived index fields")
}

fn no_index_fields(_: &[u8], _: RecordMetadata) -> Result<IndexFields, Infallible> {
	Ok(IndexFields::new())
}

async fn clean_after_rejection<C, S>(
	operation_error: RecordMutationError<C, S>,
	cleanup: impl Future<Output = Result<(), S>>,
) -> RecordMutationError<C, S> {
	match cleanup.await {
		Ok(()) => operation_error,
		Err(cleanup_error) => RecordMutationError::CleanupFailed {
			operation_error: Box::new(operation_error),
			cleanup_error,
		},
	}
}

/// A borrowed raw-byte view; it does not copy a source or native snapshot.
struct RawMutationCurrent<'a> {
	live: Option<RawLiveValue<'a>>,
	observed_at: StateTimestampMicros,
}

struct RawLiveValue<'a> {
	body: &'a [u8],
	metadata: RecordMetadata,
}

/// The same raw transitions use either native point authority or relocation authority.
/// This private boundary has no schema inference or metadata-update operation.
trait RawMutationLock: Sized + Send {
	type Error: std::error::Error + Send + Sync + 'static;
	fn current(&self) -> RawMutationCurrent<'_>;
	fn write(self, record: EncodedRecord) -> impl Future<Output = Result<(), Self::Error>> + Send;
	fn delete(self) -> impl Future<Output = Result<bool, Self::Error>> + Send;
	fn keep(self) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

struct PointMutationLock<L>(L);
struct RelocationMutationLock<L>(L);

impl<L: PointRecordLock> RawMutationLock for PointMutationLock<L> {
	type Error = L::Error;
	fn current(&self) -> RawMutationCurrent<'_> {
		let snapshot = self.0.snapshot();
		RawMutationCurrent {
			live: snapshot.live_record().map(|record| RawLiveValue {
				body: record.body(),
				metadata: record.metadata(),
			}),
			observed_at: snapshot.observed_at(),
		}
	}
	fn write(self, record: EncodedRecord) -> impl Future<Output = Result<(), Self::Error>> + Send {
		self.0.write(record)
	}
	fn delete(self) -> impl Future<Output = Result<bool, Self::Error>> + Send {
		self.0.delete()
	}
	fn keep(self) -> impl Future<Output = Result<(), Self::Error>> + Send {
		self.0.keep()
	}
}

fn relocation_current(snapshot: &RelocationRead) -> RawMutationCurrent<'_> {
	let live = snapshot.live_record().and_then(|record| match record {
		RelocationRecord::Source(record) => Some(RawLiveValue {
			body: record.body(),
			metadata: record.metadata(),
		}),
		RelocationRecord::Native(record) => Some(RawLiveValue {
			body: record.body(),
			metadata: record.metadata(),
		}),
		RelocationRecord::Absent => None,
	});
	RawMutationCurrent {
		live,
		observed_at: snapshot.observed_at(),
	}
}

impl<L: RelocatingPointRecordLock> RawMutationLock for RelocationMutationLock<L> {
	type Error = L::Error;
	fn current(&self) -> RawMutationCurrent<'_> {
		relocation_current(self.0.snapshot())
	}
	fn write(self, record: EncodedRecord) -> impl Future<Output = Result<(), Self::Error>> + Send {
		self.0.write(record)
	}
	fn delete(self) -> impl Future<Output = Result<bool, Self::Error>> + Send {
		self.0.delete()
	}
	fn keep(self) -> impl Future<Output = Result<(), Self::Error>> + Send {
		self.0.keep()
	}
}

/// Shared checked metadata and opaque-body encoder for point and batch replacement.
pub(crate) struct RawWritePreparationFields {
	pub(crate) observed_at: StateTimestampMicros,
	pub(crate) ttl: Ttl,
}
pub(crate) use crate::records::RecordExpirationOverflowError as RawWriteExpirationOverflowError;
pub(crate) fn prepare_raw_write(
	fields: RawWritePreparationFields,
) -> Result<RawWriteResult, RawWriteExpirationOverflowError> {
	let expires_at = fields
		.ttl
		.expiration_at(fields.observed_at)
		.map_err(|_| RawWriteExpirationOverflowError)?;
	Ok(RawWriteResult {
		observed_at: fields.observed_at,
		metadata: RecordMetadata {
			expires_at,
			updated_at: fields.observed_at,
		},
	})
}
impl RawWriteResult {
	pub(crate) fn encode(&self, body: Vec<u8>) -> EncodedRecord {
		raw_record(body, self.metadata.updated_at, self.metadata.expires_at)
	}
}

/// Immutable metadata for an intended empty valued slot, retained across actual restores.
#[derive(Clone, Copy)]
pub(crate) struct RawSlotSeedIntent {
	metadata: RecordMetadata,
}
#[derive(Debug, thiserror::Error)]
#[error("raw slot neutral seed expiry exceeds the timestamp domain")]
pub(crate) struct RawSlotSeedOverflowError;
pub(crate) fn prepare_raw_slot_seed(
	seed_at: StateTimestampMicros,
) -> Result<RawSlotSeedIntent, RawSlotSeedOverflowError> {
	const NEUTRAL_SLOT_AGE_MICROS: i64 = 1_000_000;
	let expires_at = seed_at
		.as_i64()
		.checked_sub(NEUTRAL_SLOT_AGE_MICROS)
		.ok_or(RawSlotSeedOverflowError)?;
	Ok(RawSlotSeedIntent {
		metadata: RecordMetadata {
			expires_at: Some(StateTimestampMicros::new(expires_at)),
			updated_at: seed_at,
		},
	})
}
impl RawSlotSeedIntent {
	pub(crate) fn metadata(self) -> RecordMetadata {
		self.metadata
	}
	pub(crate) fn encode(self) -> EncodedRecord {
		raw_record(
			Vec::new(),
			self.metadata.updated_at,
			self.metadata.expires_at,
		)
	}
}
