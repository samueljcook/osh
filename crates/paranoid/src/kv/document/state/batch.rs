//! Requests and results of operations on several documents.

use crate::kv::Ttl;
use crate::kv::document::schema::DocumentProjectionError;
use crate::records::{
	EncodedRecord, MAX_MANY_RECORD_ENTRIES, ManyRecordRequestError, PointRecordRead,
	RecordIdentity, RecordKey, RecordMetadata, RecordNamespace, StateTimestampMicros,
	check_many_record_keys,
};
use crate::scope::{BoundDocumentProducer, BoundDocumentReader, ScopeReadError};

use super::{DocumentRead, decode_scoped_document};

/// The inputs of a request that reads several documents.
pub struct DocumentManyReadRequestFields {
	/// The keys, without duplicates, in the order of the results. An empty request does no
	/// database work.
	pub keys: Vec<RecordKey>,
}
/// A validated request that reads several documents.
pub struct DocumentManyReadRequest {
	keys: Vec<RecordKey>,
}
impl DocumentManyReadRequest {
	/// Validates the number and the uniqueness of the keys without database work.
	pub fn new(fields: DocumentManyReadRequestFields) -> Result<Self, ManyRecordRequestError> {
		check_many_record_keys(&fields.keys, MAX_MANY_RECORD_ENTRIES)?;
		Ok(Self { keys: fields.keys })
	}
	/// The keys, in the caller's order.
	pub fn keys(&self) -> &[RecordKey] {
		&self.keys
	}
	/// Whether the request has no keys.
	pub fn is_empty(&self) -> bool {
		self.keys.is_empty()
	}
}

/// One document to store and its key.
pub struct DocumentSetEntry<T> {
	/// The key in the handle's scope.
	pub key: RecordKey,
	/// The document.
	pub value: T,
}
/// The inputs of a request that stores several documents in one transaction.
pub struct DocumentManySetRequestFields<T> {
	/// The documents and their keys, without duplicate keys.
	pub entries: Vec<DocumentSetEntry<T>>,
	/// The TTL of every document, which counts from the database clock of the operation.
	pub ttl: Ttl,
}
/// A validated request that stores several documents.
pub struct DocumentManySetRequest<T> {
	keys: Vec<RecordKey>,
	values: Vec<T>,
	ttl: Ttl,
}
impl<T> DocumentManySetRequest<T> {
	/// Validates the number and the uniqueness of the keys without database work and without
	/// encoding the documents.
	pub fn new(fields: DocumentManySetRequestFields<T>) -> Result<Self, ManyRecordRequestError> {
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
	/// The keys, in the caller's order.
	pub fn keys(&self) -> &[RecordKey] {
		&self.keys
	}
	/// Whether the request has no documents.
	pub fn is_empty(&self) -> bool {
		self.keys.is_empty()
	}
	/// The TTL of every document.
	pub fn ttl(&self) -> Ttl {
		self.ttl
	}
}

/// The result of `PgDocument::get_many`.
pub enum DocumentManyReadResult<T> {
	/// The request had no keys, so Paranoid did no database work.
	Empty,
	/// Paranoid read every requested key at one database clock.
	Observed(DocumentManyObservation<T>),
}
/// The documents of every requested key, read at one database clock.
pub struct DocumentManyObservation<T> {
	entries: Vec<DocumentManyReadEntry<T>>,
	observed_at: StateTimestampMicros,
}
impl<T> DocumentManyObservation<T> {
	/// One entry for each requested key, in the caller's order, including absent and expired
	/// documents.
	pub fn entries(&self) -> &[DocumentManyReadEntry<T>] {
		&self.entries
	}
	/// The database clock of the read.
	pub fn observed_at(&self) -> StateTimestampMicros {
		self.observed_at
	}
	/// Returns the entries and the database clock.
	pub fn into_parts(self) -> (Vec<DocumentManyReadEntry<T>>, StateTimestampMicros) {
		(self.entries, self.observed_at)
	}
}
/// One requested key and its live document, if any.
pub struct DocumentManyReadEntry<T> {
	key: RecordKey,
	value: Option<DocumentRead<T>>,
}
impl<T> DocumentManyReadEntry<T> {
	/// The requested key.
	pub fn key(&self) -> &RecordKey {
		&self.key
	}
	/// The live document and its metadata, or `None` when the document is absent or expired.
	pub fn value(&self) -> Option<&DocumentRead<T>> {
		self.value.as_ref()
	}
	/// Returns the key and the document.
	pub fn into_parts(self) -> (RecordKey, Option<DocumentRead<T>>) {
		(self.key, self.value)
	}
}
/// The result of `set_many`. Inside a caller's transaction, the changes are durable only after
/// the caller commits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocumentManyWriteResult {
	/// The request had no documents, so Paranoid did no database work.
	Empty,
	/// Paranoid stored every document.
	Written {
		/// The number of stored documents.
		count: usize,
		/// The metadata of every stored document, from the database clock after the locks.
		metadata: RecordMetadata,
	},
}

pub(in crate::kv) struct DocumentBatchReadFailure<C> {
	pub(in crate::kv) input_index: usize,
	pub(in crate::kv) source: ScopeReadError<C>,
}
pub(in crate::kv) fn read_scoped_documents<T, C, P>(
	reader: &BoundDocumentReader<'_, '_, '_, T, C, P>,
	keys: &[RecordKey],
	snapshots: Vec<PointRecordRead>,
	observed_at: StateTimestampMicros,
) -> Result<DocumentManyReadResult<T>, DocumentBatchReadFailure<C>> {
	let mut entries = Vec::with_capacity(keys.len());
	for (input_index, (key, snapshot)) in keys.iter().zip(snapshots).enumerate() {
		let value = decode_scoped_document(&snapshot, reader).map_err(|source| {
			DocumentBatchReadFailure {
				input_index,
				source,
			}
		})?;
		entries.push(DocumentManyReadEntry {
			key: key.clone(),
			value,
		});
	}
	Ok(DocumentManyReadResult::Observed(DocumentManyObservation {
		entries,
		observed_at,
	}))
}

pub(in crate::kv) struct DocumentSetPreparationFields<'a, 'b, 's, 'd, T, C, P> {
	pub(in crate::kv) request: DocumentManySetRequest<T>,
	pub(in crate::kv) producer: &'a BoundDocumentProducer<'b, 's, 'd, T, C, P>,
	pub(in crate::kv) namespace: &'a RecordNamespace,
	pub(in crate::kv) observed_at: StateTimestampMicros,
}
pub(in crate::kv) struct PreparedDocumentSet {
	pub(in crate::kv) records: Vec<EncodedRecord>,
	pub(in crate::kv) outcome: DocumentManyWriteResult,
}
pub(in crate::kv) enum DocumentBatchPreparationError<C, P> {
	ExpirationOverflow,
	Projection {
		input_index: usize,
		record: RecordIdentity,
		source: Box<DocumentProjectionError<C, P>>,
	},
}
pub(in crate::kv) fn prepare_scoped_document_set<T, C, P>(
	fields: DocumentSetPreparationFields<'_, '_, '_, '_, T, C, P>,
) -> Result<PreparedDocumentSet, DocumentBatchPreparationError<C, P>> {
	let metadata = prepare_document_batch_metadata(DocumentBatchMetadataFields {
		ttl: fields.request.ttl,
		observed_at: fields.observed_at,
	})?;
	let count = fields.request.keys.len();
	let mut records = Vec::with_capacity(count);
	for (input_index, (key, value)) in fields
		.request
		.keys
		.into_iter()
		.zip(fields.request.values)
		.enumerate()
	{
		let record = RecordIdentity::new(fields.namespace.clone(), key);
		let encoded = fields
			.producer
			.encode_record(&value, metadata)
			.map_err(|error| DocumentBatchPreparationError::Projection {
				input_index,
				record: record.clone(),
				source: Box::new(error),
			})?;
		records.push(encoded);
	}
	Ok(PreparedDocumentSet {
		records,
		outcome: DocumentManyWriteResult::Written { count, metadata },
	})
}

struct DocumentBatchMetadataFields {
	ttl: Ttl,
	observed_at: StateTimestampMicros,
}
fn prepare_document_batch_metadata<C, P>(
	fields: DocumentBatchMetadataFields,
) -> Result<RecordMetadata, DocumentBatchPreparationError<C, P>> {
	Ok(RecordMetadata {
		expires_at: fields
			.ttl
			.expiration_at(fields.observed_at)
			.map_err(|_| DocumentBatchPreparationError::ExpirationOverflow)?,
		updated_at: fields.observed_at,
	})
}
