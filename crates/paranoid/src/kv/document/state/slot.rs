//! Slot requests of document scopes.

use crate::kv::Ttl;
use crate::kv::document::schema::DocumentProjectionError;
pub use crate::kv::raw::model::slot::SlotRequestError;
use crate::kv::raw::model::slot::{
	RawSlotCandidates, RawSlotSelectionRequest, RawSlotSelectionRequestFields,
	RawSlotSelectionRequestParts,
};
use crate::records::{EncodedRecord, RecordKey, RecordMetadata, StateTimestampMicros};
use crate::scope::BoundDocumentProducer;

/// The inputs of a slot request.
pub struct DocumentSlotRequestFields<'value, T> {
	/// The candidate keys, without duplicates. Paranoid tries the keys in byte order, not
	/// request order.
	pub candidates: Vec<RecordKey>,
	/// The document to store under the selected key. Paranoid encodes it only when a key is
	/// selected.
	pub value: &'value T,
	/// The document's TTL, which must be positive and expiring. It counts from the database
	/// clock after the lock.
	pub ttl: Ttl,
}

/// A validated slot request.
pub struct DocumentSlotRequest<'value, T> {
	selection: RawSlotSelectionRequestParts,
	value: &'value T,
}
impl<'value, T> DocumentSlotRequest<'value, T> {
	/// Validates the candidates and the TTL without database work. An invalid TTL fails even
	/// when there are no candidates.
	pub fn new(fields: DocumentSlotRequestFields<'value, T>) -> Result<Self, SlotRequestError> {
		let selection = RawSlotSelectionRequest::new(RawSlotSelectionRequestFields {
			candidates: fields.candidates,
			ttl: fields.ttl,
		})?
		.into_parts();
		Ok(Self {
			selection,
			value: fields.value,
		})
	}
	/// Whether the request has no candidates.
	pub fn is_empty(&self) -> bool {
		self.selection.candidates.keys().is_empty()
	}
	pub(crate) fn candidates(&self) -> &RawSlotCandidates {
		&self.selection.candidates
	}
	pub(in crate::kv) fn keys_in_input_order(&self) -> Vec<RecordKey> {
		self.selection.candidates.keys_in_input_order()
	}
}

/// The result of a slot request.
#[derive(Debug)]
pub enum DocumentSlotResult {
	/// The request had no candidates, so Paranoid did no database work.
	Empty,
	/// Every candidate held a live document or was locked by another transaction, so Paranoid
	/// wrote nothing. A later attempt can succeed.
	Unavailable {
		/// The database clock of the attempt.
		observed_at: StateTimestampMicros,
	},
	/// Paranoid stored the document and its index entries under one candidate key. Inside a
	/// caller's transaction, the change is durable only after the caller commits.
	Written {
		/// The selected key.
		key: RecordKey,
		/// The stored metadata, from the database clock after the lock.
		metadata: RecordMetadata,
	},
}

pub(in crate::kv) struct DocumentSlotPreparationFields<'a, 'value, 'bound, 'scope, 'code, T, C, P> {
	pub(in crate::kv) request: &'a DocumentSlotRequest<'value, T>,
	pub(in crate::kv) producer: &'a BoundDocumentProducer<'bound, 'scope, 'code, T, C, P>,
	pub(in crate::kv) observed_at: StateTimestampMicros,
}
pub(in crate::kv) enum DocumentSlotPreparationError<C, P> {
	ExpirationOverflow,
	Projection(Box<DocumentProjectionError<C, P>>),
}

pub(in crate::kv) fn prepare_document_slot<T, C, P>(
	fields: DocumentSlotPreparationFields<'_, '_, '_, '_, '_, T, C, P>,
) -> Result<EncodedRecord, DocumentSlotPreparationError<C, P>> {
	let metadata = RecordMetadata {
		expires_at: fields
			.request
			.selection
			.ttl
			.expiration_at(fields.observed_at)
			.map_err(|_| DocumentSlotPreparationError::ExpirationOverflow)?,
		updated_at: fields.observed_at,
	};
	fields
		.producer
		.encode_record(fields.request.value, metadata)
		.map_err(|error| DocumentSlotPreparationError::Projection(Box::new(error)))
}
