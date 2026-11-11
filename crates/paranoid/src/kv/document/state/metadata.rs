//! The metadata that a touch, a TTL change, or an expiry gives a live document.

use crate::records::{RecordMetadata, StateTimestampMicros};

use super::DocumentMetadataChange;
use super::preparation::DocumentMutationPlanError;

pub(super) struct DocumentMetadataChangeFields {
	pub(super) current_metadata: RecordMetadata,
	pub(super) observed_at: StateTimestampMicros,
	pub(super) change: DocumentMetadataChange,
}

/// The fixed caller obtains the live metadata and clock from its actual held snapshot.
pub(super) fn prepare_document_metadata_change(
	fields: DocumentMetadataChangeFields,
) -> Result<RecordMetadata, DocumentMutationPlanError> {
	let updated_at = fields.observed_at;
	let expires_at = match fields.change {
		DocumentMetadataChange::Touch => fields.current_metadata.expires_at,
		DocumentMetadataChange::SetTtl(ttl) => ttl
			.expiration_at(updated_at)
			.map_err(|_| DocumentMutationPlanError::ExpirationOverflow)?,
		DocumentMetadataChange::Expire => Some(updated_at),
	};
	Ok(RecordMetadata {
		expires_at,
		updated_at,
	})
}
