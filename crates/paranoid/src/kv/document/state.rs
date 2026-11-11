//! Typed point operations on the same consumed storage authority used by opaque records.
//!
//! Runtime composition must bind scoped schema ownership and every installed maintenance
//! index before calling mutations. These kernels provide no admission or query readiness.

pub(crate) mod batch;
pub(crate) mod metadata;
pub(crate) mod preparation;
pub(crate) mod slot;
pub use batch::{
	DocumentManyObservation, DocumentManyReadEntry, DocumentManyReadRequest,
	DocumentManyReadRequestFields, DocumentManyReadResult, DocumentManySetRequest,
	DocumentManySetRequestFields, DocumentManyWriteResult, DocumentSetEntry,
};
pub use slot::{
	DocumentSlotRequest, DocumentSlotRequestFields, DocumentSlotResult, SlotRequestError,
};

use std::{convert::Infallible, fmt};

use crate::records::metadata_mutation::{
	PointRecordMetadataMutationLock, RecordMetadataMutationLock,
};
use metadata::{DocumentMetadataChangeFields, prepare_document_metadata_change};

use crate::scope::{
	BoundDocumentMaintenance, BoundDocumentProducer, BoundDocumentReader, ScopeError,
	ScopeReadError,
};

use crate::installation::contract::ContractStateReadError;
use crate::kv::Ttl;
use crate::kv::document::schema::DocumentProjectionError;
use crate::records::{
	EncodedRecord, PointRecordLock, PointRecordRead, RecordMetadata, StateTimestampMicros,
};
use preparation::{
	DocumentLiveOrInitFields, DocumentMutationPlan, DocumentMutationPlanError,
	DocumentMutationPlanFields, DocumentWriteExpiration, DocumentWriteMetadataFields,
	plan_document_mutation, prepare_document_write_metadata, prepare_live_or_initialized,
};

/// One live document and its metadata.
pub struct DocumentRead<T> {
	/// The decoded document.
	pub value: T,
	/// The stored record's metadata.
	pub metadata: RecordMetadata,
}

/// Scoped point reads invoke only their exact retained-reader binding.
pub(in crate::kv) fn decode_scoped_document<T, C, P>(
	snapshot: &PointRecordRead,
	reader: &BoundDocumentReader<'_, '_, '_, T, C, P>,
) -> Result<Option<DocumentRead<T>>, ScopeReadError<C>> {
	decode_live_document(snapshot, |body| {
		reader.decode_document(body).map(|(_, value)| value)
	})
}

fn decode_live_document<T, E>(
	snapshot: &PointRecordRead,
	decode_fn: impl FnOnce(&[u8]) -> Result<T, E>,
) -> Result<Option<DocumentRead<T>>, E> {
	snapshot
		.live_record()
		.map(|record| {
			Ok(DocumentRead {
				value: decode_fn(record.body())?,
				metadata: record.metadata(),
			})
		})
		.transpose()
}

/// What a mutation callback receives while the operation holds the key's lock.
pub struct DocumentMutationCurrent<'a, T> {
	/// The decoded live document and its metadata, or `None` when the document is absent or
	/// expired.
	pub live_document: Option<&'a DocumentRead<T>>,
	/// The database clock after the operation locked the key.
	pub observed_at: StateTimestampMicros,
}

/// What a mutation does with the key's document.
pub enum DocumentMutation<T> {
	/// Keeps the stored record unchanged. After `mutate_live_or_init` created the document,
	/// stores the created document with the initializer's TTL.
	KeepExisting,
	/// Deletes the stored document.
	Delete,
	/// Stores a new document and its index entries.
	SetValue {
		/// The new document.
		value: T,
		/// The document's TTL, which counts from the database clock of the write.
		ttl: Ttl,
	},
	/// Replaces a live document and keeps its expiry time. After `mutate_live_or_init` created
	/// the document, stores the new document with the initializer's TTL. When the document is
	/// absent or expired, `mutate` fails with the kind `NotLive`.
	///
	/// The document must be live at the database clock after the lock; it can expire before
	/// the write or the commit.
	SetValuePreservingExpiration {
		/// The new document.
		value: T,
	},
}

/// The result of `set_if_absent`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocumentConditionalWriteResult {
	/// The document and its index entries were stored.
	Created {
		/// The stored metadata. Its `updated_at` is the database clock after the lock.
		metadata: RecordMetadata,
	},
	/// A live document had the key at the database clock after the lock, and the operation
	/// kept it without decoding it.
	Existing {
		/// The database clock at which the document was live.
		observed_at: StateTimestampMicros,
	},
}

/// What a mutation did with the key's document. Inside a caller's transaction, the change is
/// durable only after the caller commits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocumentMutationOutcome {
	/// The stored record was kept.
	Kept,
	/// The stored document was deleted.
	Deleted,
	/// A new document and its index entries were stored.
	Written {
		/// The stored metadata.
		metadata: RecordMetadata,
	},
}

/// The result of `mutate`.
pub struct DocumentMutationResult<T> {
	/// The live document that the callback received, or `None`.
	pub previous_live_value: Option<T>,
	/// What the mutation did.
	pub outcome: DocumentMutationOutcome,
}

/// The document that the callback of `mutate_live_or_init` receives, and where it came from.
/// `V` is `&T` in the callback's input and `T` in the result.
pub enum DocumentLiveOrInitValue<V> {
	/// A live document had the key.
	Existing {
		/// The live document.
		value: V,
		/// The live document's metadata.
		metadata: RecordMetadata,
	},
	/// No live document had the key, and the initializer created this document. Paranoid
	/// stores it only when the callback keeps it.
	Initialized {
		/// The created document.
		value: V,
	},
}

/// The result of `mutate_live_or_init`.
pub struct DocumentLiveOrInitMutationResult<T> {
	/// The live or created document that the callback received.
	pub value_seen: DocumentLiveOrInitValue<T>,
	/// What the mutation did.
	pub outcome: DocumentMutationOutcome,
}

/// What the callback of `mutate_live_or_init` receives while the operation holds the key's
/// lock.
pub struct DocumentLiveMutationCurrent<'a, T> {
	/// The live document, or the document that the initializer created.
	pub value: DocumentLiveOrInitValue<&'a T>,
	/// The database clock after the operation locked the key.
	pub observed_at: StateTimestampMicros,
}

/// Codec, projection, callback, storage, and rejection-cleanup causes remain distinguishable.
pub(in crate::kv) enum DocumentMutationError<C, P, F, S> {
	Scope(ScopeError),
	Read(ContractStateReadError<C>),
	Projection(DocumentProjectionError<C, P>),
	Callback(F),
	NotLive,
	ExpirationOverflow,
	Storage(S),
	CleanupFailed {
		operation_error: Box<Self>,
		cleanup_error: S,
	},
}

impl<C, P, F, S> DocumentMutationError<C, P, F, S> {
	/// Borrows the original failure independently from rejection-cleanup failures.
	pub(crate) fn primary_error(&self) -> &Self {
		match self {
			Self::CleanupFailed {
				operation_error, ..
			} => operation_error.primary_error(),
			_ => self,
		}
	}
	/// Borrows every retained cleanup failure, from outermost to innermost rejection.
	pub(in crate::kv) fn cleanup_errors(&self) -> impl Iterator<Item = &S> {
		std::iter::successors(Some(self), |error| match error {
			Self::CleanupFailed {
				operation_error, ..
			} => Some(operation_error.as_ref()),
			_ => None,
		})
		.filter_map(|error| match error {
			Self::CleanupFailed { cleanup_error, .. } => Some(cleanup_error),
			_ => None,
		})
	}
	pub(crate) fn read_error(&self) -> Option<&ContractStateReadError<C>> {
		match self.primary_error() {
			Self::Read(source) => Some(source),
			_ => None,
		}
	}
	pub(in crate::kv) fn projection_error(&self) -> Option<&DocumentProjectionError<C, P>> {
		match self.primary_error() {
			Self::Projection(source) => Some(source),
			_ => None,
		}
	}
	pub(crate) fn callback_error(&self) -> Option<&F> {
		match self.primary_error() {
			Self::Callback(source) => Some(source),
			_ => None,
		}
	}
	pub(in crate::kv) fn primary_storage_error(&self) -> Option<&S> {
		match self.primary_error() {
			Self::Storage(source) => Some(source),
			_ => None,
		}
	}
}

impl<C, P, F, S> fmt::Debug for DocumentMutationError<C, P, F, S> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(match self {
			Self::Scope(_) => "DocumentScopeBindingFailed",
			Self::Read(_) => "DocumentReadFailed",
			Self::Projection(_) => "DocumentProjectionFailed",
			Self::Callback(_) => "DocumentCallbackFailed",
			Self::NotLive => "DocumentNotLive",
			Self::ExpirationOverflow => "DocumentExpirationOverflow",
			Self::Storage(_) => "DocumentStorageFailed",
			Self::CleanupFailed { .. } => "DocumentCleanupFailed",
		})
	}
}

impl<C, P, F, S> fmt::Display for DocumentMutationError<C, P, F, S> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(match self {
			Self::Scope(_) => "document mutation roles use different actual declarations",
			Self::Read(_) => "document retained reader rejected its live value",
			Self::Projection(_) => "document encoding or projection failed",
			Self::Callback(_) => "document mutation decision failed",
			Self::NotLive => "document mutation requires a preexisting live value",
			Self::ExpirationOverflow => "document expiry exceeds the finite timestamp domain",
			Self::Storage(_) => "document storage operation failed",
			Self::CleanupFailed { .. } => "document operation and rejection cleanup both failed",
		})
	}
}

impl<C, P, F, S> std::error::Error for DocumentMutationError<C, P, F, S>
where
	C: std::error::Error + 'static,
	P: std::error::Error + 'static,
	F: std::error::Error + 'static,
	S: std::error::Error + 'static,
{
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Scope(error) => Some(error),
			Self::Read(error) => Some(error),
			Self::Projection(error) => Some(error),
			Self::Callback(error) => Some(error),
			Self::Storage(error) => Some(error),
			Self::CleanupFailed {
				operation_error, ..
			} => Some(operation_error.as_ref()),
			Self::NotLive | Self::ExpirationOverflow => None,
		}
	}
}

/// Scoped read-modify-write needs both roles of the exact same local declaration.
/// A binding mismatch is cleaned up before any document decoder or callback runs.
pub(in crate::kv) async fn mutate_scoped_document<L, T, C, P, F>(
	lock: L,
	reader: &BoundDocumentReader<'_, '_, '_, T, C, P>,
	producer: &BoundDocumentProducer<'_, '_, '_, T, C, P>,
	decide_mutation_fn: impl FnOnce(DocumentMutationCurrent<'_, T>) -> Result<DocumentMutation<T>, F>,
) -> Result<DocumentMutationResult<T>, DocumentMutationError<C, P, F, L::Error>>
where
	L: PointRecordLock,
{
	if let Err(error) = reader.require_matching_producer(producer) {
		return Err(reject(
			PointRecordMetadataMutationLock::new(lock),
			DocumentMutationError::Scope(error),
		)
		.await);
	}
	mutate_with_codecs(
		lock,
		|body| {
			reader
				.decode_document(body)
				.map(|(_, value)| value)
				.map_err(|error| error.0)
		},
		|value, metadata| producer.encode_record(value, metadata),
		decide_mutation_fn,
	)
	.await
}

/// Initializes only absence or expiry; an unreadable live document remains a typed failure.
/// The selected producer encodes the final body and complete projection set exactly once.
pub(in crate::kv) async fn mutate_scoped_document_live_or_init<L, T, C, P, F>(
	lock: L,
	reader: &BoundDocumentReader<'_, '_, '_, T, C, P>,
	producer: &BoundDocumentProducer<'_, '_, '_, T, C, P>,
	initialize_value_fn: impl FnOnce(StateTimestampMicros) -> Result<(T, Ttl), F>,
	decide_mutation_fn: impl FnOnce(
		DocumentLiveMutationCurrent<'_, T>,
	) -> Result<DocumentMutation<T>, F>,
) -> Result<DocumentLiveOrInitMutationResult<T>, DocumentMutationError<C, P, F, L::Error>>
where
	L: PointRecordLock,
{
	if let Err(error) = reader.require_matching_producer(producer) {
		return Err(reject(
			PointRecordMetadataMutationLock::new(lock),
			DocumentMutationError::Scope(error),
		)
		.await);
	}
	let current_result = decode_scoped_document(lock.snapshot(), reader)
		.map_err(|error| DocumentMutationError::Read(error.0));
	mutate_prepared_document_live_or_init(DocumentPreparedLiveOrInitFields {
		lock: PointRecordMetadataMutationLock::new(lock),
		current_result,
		encode_fn: |value: &T, metadata| {
			producer
				.encode_record(value, metadata)
				.map_err(DocumentMutationError::Projection)
		},
		initialize_value_fn,
		decide_mutation_fn,
	})
	.await
}

/// Blind typed replacement does not decode or return the previous body.
/// Its selected producer derives the complete installed projection set.
pub(in crate::kv) async fn set_scoped_document<L, T, C, P>(
	lock: L,
	producer: &BoundDocumentProducer<'_, '_, '_, T, C, P>,
	value: T,
	ttl: Ttl,
) -> Result<DocumentMutationOutcome, DocumentMutationError<C, P, Infallible, L::Error>>
where
	L: RecordMetadataMutationLock,
{
	apply_mutation(
		lock,
		DocumentMutation::SetValue { value, ttl },
		|value, metadata| {
			producer
				.encode_record(value, metadata)
				.map_err(DocumentMutationError::Projection)
		},
	)
	.await
}

/// A live header is sufficient to keep an existing value; no Reader is required or invoked.
pub(in crate::kv) async fn set_scoped_document_if_absent<L, T, C, P>(
	lock: L,
	producer: &BoundDocumentProducer<'_, '_, '_, T, C, P>,
	value: T,
	ttl: Ttl,
) -> Result<DocumentConditionalWriteResult, DocumentMutationError<C, P, Infallible, L::Error>>
where
	L: RecordMetadataMutationLock,
{
	set_document_if_absent(
		lock,
		DocumentFixedStoreFields {
			value: &value,
			ttl,
			encode_fn: |value: &T, metadata| {
				producer
					.encode_record(value, metadata)
					.map_err(DocumentMutationError::Projection)
			},
		},
	)
	.await
}

/// Fixed writes borrow the actual value and derive time only from the held lock.
pub(in crate::kv) struct DocumentFixedStoreFields<'value, T, E> {
	pub(in crate::kv) value: &'value T,
	pub(in crate::kv) ttl: Ttl,
	pub(in crate::kv) encode_fn: E,
}

/// A live value is kept before TTL arithmetic or the actual encoder runs.
pub(in crate::kv) async fn set_document_if_absent<L, T, C, P, E>(
	lock: L,
	fields: DocumentFixedStoreFields<'_, T, E>,
) -> Result<DocumentConditionalWriteResult, DocumentMutationError<C, P, Infallible, L::Error>>
where
	L: RecordMetadataMutationLock,
	E: FnOnce(
		&T,
		RecordMetadata,
	) -> Result<EncodedRecord, DocumentMutationError<C, P, Infallible, L::Error>>,
{
	let current = lock.current();
	let observed_at = current.observed_at;
	if current.live_metadata.is_some() {
		lock.keep().await.map_err(DocumentMutationError::Storage)?;
		return Ok(DocumentConditionalWriteResult::Existing { observed_at });
	}
	let outcome = write_value_at(
		lock,
		fields.value,
		fields.ttl,
		observed_at,
		fields.encode_fn,
	)
	.await?;
	let DocumentMutationOutcome::Written { metadata } = outcome else {
		unreachable!("conditional insertion prepares exactly one document write")
	};
	Ok(DocumentConditionalWriteResult::Created { metadata })
}

/// Fixed metadata changes do not replace or convert the stored document representation.
#[derive(Clone, Copy)]
pub(in crate::kv) enum DocumentMetadataChange {
	Touch,
	SetTtl(Ttl),
	Expire,
}

/// The actual snapshot clock determines liveness and new metadata before decoder dispatch.
pub(in crate::kv) async fn update_scoped_document_metadata<L, T, C, P>(
	lock: L,
	maintenance: &BoundDocumentMaintenance<'_, '_, T, C, P>,
	change: DocumentMetadataChange,
) -> Result<RecordMetadata, DocumentMutationError<C, P, Infallible, L::Error>>
where
	L: PointRecordLock,
{
	let snapshot = lock.snapshot();
	let Some(record) = snapshot.live_record() else {
		return Err(reject(
			PointRecordMetadataMutationLock::new(lock),
			DocumentMutationError::NotLive,
		)
		.await);
	};
	let metadata = match prepare_document_metadata_change(DocumentMetadataChangeFields {
		current_metadata: record.metadata(),
		observed_at: snapshot.observed_at(),
		change,
	}) {
		Ok(metadata) => metadata,
		Err(source) => {
			return Err(reject(
				PointRecordMetadataMutationLock::new(lock),
				preparation_error(source),
			)
			.await);
		}
	};
	let prepared = match maintenance.prepare_metadata_update(record, metadata) {
		Ok(prepared) => prepared,
		Err(source) => {
			return Err(reject(
				PointRecordMetadataMutationLock::new(lock),
				DocumentMutationError::Projection(source),
			)
			.await);
		}
	};
	lock.update_metadata(prepared)
		.await
		.map_err(DocumentMutationError::Storage)?;
	Ok(metadata)
}

type DocumentPreparedCurrent<T, C, P, F, S> =
	Result<Option<DocumentRead<T>>, DocumentMutationError<C, P, F, S>>;

/// The fixed reader's owned result and actual consumed completion lock.
pub(in crate::kv) struct DocumentPreparedMutationFields<L, T, C, P, F, E, D>
where
	L: RecordMetadataMutationLock,
{
	pub(crate) lock: L,
	pub(crate) current_result: DocumentPreparedCurrent<T, C, P, F, L::Error>,
	pub(crate) encode_fn: E,
	pub(crate) decide_mutation_fn: D,
}

/// Owns the decoded current value while the decision borrows it, then completes once.
pub(in crate::kv) async fn mutate_prepared_document<L, T, C, P, F, E, D>(
	fields: DocumentPreparedMutationFields<L, T, C, P, F, E, D>,
) -> Result<DocumentMutationResult<T>, DocumentMutationError<C, P, F, L::Error>>
where
	L: RecordMetadataMutationLock,
	E: FnOnce(
		&T,
		RecordMetadata,
	) -> Result<EncodedRecord, DocumentMutationError<C, P, F, L::Error>>,
	D: for<'current> FnOnce(DocumentMutationCurrent<'current, T>) -> Result<DocumentMutation<T>, F>,
{
	let DocumentPreparedMutationFields {
		lock,
		current_result,
		encode_fn,
		decide_mutation_fn,
	} = fields;
	let current = match current_result {
		Ok(current) => current,
		Err(error) => return Err(reject(lock, error).await),
	};
	let mutation = decide_mutation_fn(DocumentMutationCurrent {
		live_document: current.as_ref(),
		observed_at: lock.current().observed_at,
	});
	let mutation = match mutation {
		Ok(mutation) => mutation,
		Err(error) => return Err(reject(lock, DocumentMutationError::Callback(error)).await),
	};
	let outcome = apply_mutation(lock, mutation, encode_fn).await?;
	Ok(DocumentMutationResult {
		previous_live_value: current.map(|current| current.value),
		outcome,
	})
}

/// Owns fixed reader preparation and caller initialization/decision without extra bounds.
pub(in crate::kv) struct DocumentPreparedLiveOrInitFields<L, T, C, P, F, E, I, D>
where
	L: RecordMetadataMutationLock,
{
	pub(crate) lock: L,
	pub(crate) current_result: DocumentPreparedCurrent<T, C, P, F, L::Error>,
	pub(crate) encode_fn: E,
	pub(crate) initialize_value_fn: I,
	pub(crate) decide_mutation_fn: D,
}

/// Rejects unreadable live input before initialization and completes the prepared plan once.
pub(in crate::kv) async fn mutate_prepared_document_live_or_init<L, T, C, P, F, E, I, D>(
	fields: DocumentPreparedLiveOrInitFields<L, T, C, P, F, E, I, D>,
) -> Result<DocumentLiveOrInitMutationResult<T>, DocumentMutationError<C, P, F, L::Error>>
where
	L: RecordMetadataMutationLock,
	E: FnOnce(
		&T,
		RecordMetadata,
	) -> Result<EncodedRecord, DocumentMutationError<C, P, F, L::Error>>,
	I: FnOnce(StateTimestampMicros) -> Result<(T, Ttl), F>,
	D: for<'current> FnOnce(
		DocumentLiveMutationCurrent<'current, T>,
	) -> Result<DocumentMutation<T>, F>,
{
	let DocumentPreparedLiveOrInitFields {
		lock,
		current_result,
		encode_fn,
		initialize_value_fn,
		decide_mutation_fn,
	} = fields;
	let current = match current_result {
		Ok(current) => current,
		Err(error) => return Err(reject(lock, error).await),
	};
	let observed_at = lock.current().observed_at;
	let current = match prepare_live_or_initialized(DocumentLiveOrInitFields {
		current,
		observed_at,
		initialize_value_fn,
	}) {
		Ok(current) => current,
		Err(error) => return Err(reject(lock, DocumentMutationError::Callback(error)).await),
	};
	let mutation = match decide_mutation_fn(current.current(observed_at)) {
		Ok(mutation) => mutation,
		Err(error) => return Err(reject(lock, DocumentMutationError::Callback(error)).await),
	};
	let plan = match current.plan(&mutation) {
		Ok(plan) => plan,
		Err(error) => return Err(reject(lock, preparation_error(error)).await),
	};
	let outcome = apply_mutation_plan(lock, plan, encode_fn).await?;
	Ok(current.into_result(outcome))
}

async fn mutate_with_codecs<L, T, C, P, F>(
	lock: L,
	decode_fn: impl FnOnce(&[u8]) -> Result<T, ContractStateReadError<C>>,
	encode_fn: impl FnOnce(&T, RecordMetadata) -> Result<EncodedRecord, DocumentProjectionError<C, P>>,
	decide_mutation_fn: impl FnOnce(DocumentMutationCurrent<'_, T>) -> Result<DocumentMutation<T>, F>,
) -> Result<DocumentMutationResult<T>, DocumentMutationError<C, P, F, L::Error>>
where
	L: PointRecordLock,
{
	let current_result =
		decode_live_document(lock.snapshot(), decode_fn).map_err(DocumentMutationError::Read);
	mutate_prepared_document(DocumentPreparedMutationFields {
		lock: PointRecordMetadataMutationLock::new(lock),
		current_result,
		encode_fn: |value: &T, metadata| {
			encode_fn(value, metadata).map_err(DocumentMutationError::Projection)
		},
		decide_mutation_fn,
	})
	.await
}

/// The write's time is the time that the lock observed after it acquired the record, which is
/// also the time that a mutation callback receives as the current time.
async fn write_value<L, T, C, P, F>(
	lock: L,
	value: &T,
	ttl: Ttl,
	encode_fn: impl FnOnce(
		&T,
		RecordMetadata,
	) -> Result<EncodedRecord, DocumentMutationError<C, P, F, L::Error>>,
) -> Result<DocumentMutationOutcome, DocumentMutationError<C, P, F, L::Error>>
where
	L: RecordMetadataMutationLock,
{
	let updated_at = lock.current().observed_at;
	write_value_at(lock, value, ttl, updated_at, encode_fn).await
}

async fn write_value_at<L, T, C, P, F>(
	lock: L,
	value: &T,
	ttl: Ttl,
	updated_at: StateTimestampMicros,
	encode_fn: impl FnOnce(
		&T,
		RecordMetadata,
	) -> Result<EncodedRecord, DocumentMutationError<C, P, F, L::Error>>,
) -> Result<DocumentMutationOutcome, DocumentMutationError<C, P, F, L::Error>>
where
	L: RecordMetadataMutationLock,
{
	let metadata = match prepare_document_write_metadata(DocumentWriteMetadataFields {
		expiration: DocumentWriteExpiration::Relative(ttl),
		updated_at,
	}) {
		Ok(metadata) => metadata,
		Err(error) => return Err(reject(lock, preparation_error(error)).await),
	};
	write_with_metadata(lock, value, metadata, encode_fn).await
}

async fn write_with_metadata<L, T, C, P, F>(
	lock: L,
	value: &T,
	metadata: RecordMetadata,
	encode_fn: impl FnOnce(
		&T,
		RecordMetadata,
	) -> Result<EncodedRecord, DocumentMutationError<C, P, F, L::Error>>,
) -> Result<DocumentMutationOutcome, DocumentMutationError<C, P, F, L::Error>>
where
	L: RecordMetadataMutationLock,
{
	let record = match encode_fn(value, metadata) {
		Ok(record) => record,
		Err(error) => return Err(reject(lock, error).await),
	};
	lock.write(record)
		.await
		.map_err(DocumentMutationError::Storage)?;
	Ok(DocumentMutationOutcome::Written { metadata })
}

async fn apply_mutation<L, T, C, P, F>(
	lock: L,
	mutation: DocumentMutation<T>,
	encode_fn: impl FnOnce(
		&T,
		RecordMetadata,
	) -> Result<EncodedRecord, DocumentMutationError<C, P, F, L::Error>>,
) -> Result<DocumentMutationOutcome, DocumentMutationError<C, P, F, L::Error>>
where
	L: RecordMetadataMutationLock,
{
	let plan = match plan_document_mutation(DocumentMutationPlanFields {
		mutation: &mutation,
		current_metadata: lock.current().live_metadata,
	}) {
		Ok(plan) => plan,
		Err(error) => return Err(reject(lock, preparation_error(error)).await),
	};
	apply_mutation_plan(lock, plan, encode_fn).await
}

async fn apply_mutation_plan<L, T, C, P, F>(
	lock: L,
	plan: DocumentMutationPlan<'_, T>,
	encode_fn: impl FnOnce(
		&T,
		RecordMetadata,
	) -> Result<EncodedRecord, DocumentMutationError<C, P, F, L::Error>>,
) -> Result<DocumentMutationOutcome, DocumentMutationError<C, P, F, L::Error>>
where
	L: RecordMetadataMutationLock,
{
	match plan {
		DocumentMutationPlan::Keep => {
			lock.keep().await.map_err(DocumentMutationError::Storage)?;
			Ok(DocumentMutationOutcome::Kept)
		}
		DocumentMutationPlan::Delete => {
			lock.delete()
				.await
				.map_err(DocumentMutationError::Storage)?;
			Ok(DocumentMutationOutcome::Deleted)
		}
		DocumentMutationPlan::Write {
			value,
			expiration: DocumentWriteExpiration::Relative(ttl),
		} => write_value(lock, value, ttl, encode_fn).await,
		DocumentMutationPlan::Write {
			value,
			expiration: DocumentWriteExpiration::Absolute(expires_at),
		} => {
			let updated_at = lock.current().observed_at;
			let metadata = match prepare_document_write_metadata(DocumentWriteMetadataFields {
				expiration: DocumentWriteExpiration::Absolute(expires_at),
				updated_at,
			}) {
				Ok(metadata) => metadata,
				Err(error) => return Err(reject(lock, preparation_error(error)).await),
			};
			write_with_metadata(lock, value, metadata, encode_fn).await
		}
	}
}

fn preparation_error<C, P, F, S>(
	error: DocumentMutationPlanError,
) -> DocumentMutationError<C, P, F, S> {
	match error {
		DocumentMutationPlanError::NotLive => DocumentMutationError::NotLive,
		DocumentMutationPlanError::ExpirationOverflow => DocumentMutationError::ExpirationOverflow,
	}
}

async fn reject<L: RecordMetadataMutationLock, C, P, F>(
	lock: L,
	operation_error: DocumentMutationError<C, P, F, L::Error>,
) -> DocumentMutationError<C, P, F, L::Error> {
	match lock.keep().await {
		Ok(()) => operation_error,
		Err(cleanup_error) => DocumentMutationError::CleanupFailed {
			operation_error: Box::new(operation_error),
			cleanup_error,
		},
	}
}
