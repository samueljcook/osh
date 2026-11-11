//! Shared pure point decisions and metadata, without record or transaction authority.

use crate::kv::Ttl;
use crate::records::{RecordMetadata, StateTimestampMicros};

use super::{
	DocumentLiveMutationCurrent, DocumentLiveOrInitMutationResult, DocumentLiveOrInitValue,
	DocumentMutation, DocumentMutationOutcome, DocumentRead,
};

pub(super) enum DocumentWriteExpiration {
	Relative(Ttl),
	Absolute(Option<StateTimestampMicros>),
}

pub(super) enum DocumentMutationPlan<'a, T> {
	Keep,
	Delete,
	Write {
		value: &'a T,
		expiration: DocumentWriteExpiration,
	},
}

pub(super) enum DocumentMutationPlanError {
	NotLive,
	ExpirationOverflow,
}

pub(super) struct DocumentMutationPlanFields<'a, T> {
	pub(super) mutation: &'a DocumentMutation<T>,
	pub(super) current_metadata: Option<RecordMetadata>,
}

pub(super) fn plan_document_mutation<T>(
	fields: DocumentMutationPlanFields<'_, T>,
) -> Result<DocumentMutationPlan<'_, T>, DocumentMutationPlanError> {
	match fields.mutation {
		DocumentMutation::KeepExisting => Ok(DocumentMutationPlan::Keep),
		DocumentMutation::Delete => Ok(DocumentMutationPlan::Delete),
		DocumentMutation::SetValue { value, ttl } => Ok(DocumentMutationPlan::Write {
			value,
			expiration: DocumentWriteExpiration::Relative(*ttl),
		}),
		DocumentMutation::SetValuePreservingExpiration { value } => {
			let metadata = fields
				.current_metadata
				.ok_or(DocumentMutationPlanError::NotLive)?;
			Ok(DocumentMutationPlan::Write {
				value,
				expiration: DocumentWriteExpiration::Absolute(metadata.expires_at),
			})
		}
	}
}

pub(super) struct DocumentWriteMetadataFields {
	pub(super) expiration: DocumentWriteExpiration,
	pub(super) updated_at: StateTimestampMicros,
}

pub(super) fn prepare_document_write_metadata(
	fields: DocumentWriteMetadataFields,
) -> Result<RecordMetadata, DocumentMutationPlanError> {
	let expires_at = match fields.expiration {
		DocumentWriteExpiration::Relative(ttl) => ttl
			.expiration_at(fields.updated_at)
			.map_err(|_| DocumentMutationPlanError::ExpirationOverflow)?,
		DocumentWriteExpiration::Absolute(expires_at) => expires_at,
	};
	Ok(RecordMetadata {
		expires_at,
		updated_at: fields.updated_at,
	})
}

pub(super) enum DocumentLiveOrInitialized<T> {
	Live(DocumentRead<T>),
	Initialized { value: T, ttl: Ttl },
}

pub(super) struct DocumentLiveOrInitFields<T, I> {
	pub(super) current: Option<DocumentRead<T>>,
	pub(super) observed_at: StateTimestampMicros,
	pub(super) initialize_value_fn: I,
}

pub(super) fn prepare_live_or_initialized<T, F>(
	fields: DocumentLiveOrInitFields<T, impl FnOnce(StateTimestampMicros) -> Result<(T, Ttl), F>>,
) -> Result<DocumentLiveOrInitialized<T>, F> {
	match fields.current {
		Some(current) => Ok(DocumentLiveOrInitialized::Live(current)),
		None => {
			let (value, ttl) = (fields.initialize_value_fn)(fields.observed_at)?;
			Ok(DocumentLiveOrInitialized::Initialized { value, ttl })
		}
	}
}

impl<T> DocumentLiveOrInitialized<T> {
	pub(super) fn current(
		&self,
		observed_at: StateTimestampMicros,
	) -> DocumentLiveMutationCurrent<'_, T> {
		DocumentLiveMutationCurrent {
			value: match self {
				Self::Live(current) => DocumentLiveOrInitValue::Existing {
					value: &current.value,
					metadata: current.metadata,
				},
				Self::Initialized { value, .. } => DocumentLiveOrInitValue::Initialized { value },
			},
			observed_at,
		}
	}

	/// After initialization, `KeepExisting` stores the created document and
	/// `SetValuePreservingExpiration` stores the new document, both with the initializer's TTL.
	pub(super) fn plan<'a>(
		&'a self,
		mutation: &'a DocumentMutation<T>,
	) -> Result<DocumentMutationPlan<'a, T>, DocumentMutationPlanError> {
		match (self, mutation) {
			(Self::Initialized { value, ttl }, DocumentMutation::KeepExisting)
			| (
				Self::Initialized { ttl, .. },
				DocumentMutation::SetValuePreservingExpiration { value },
			) => Ok(DocumentMutationPlan::Write {
				value,
				expiration: DocumentWriteExpiration::Relative(*ttl),
			}),
			_ => plan_document_mutation(DocumentMutationPlanFields {
				mutation,
				current_metadata: match self {
					Self::Live(current) => Some(current.metadata),
					Self::Initialized { .. } => None,
				},
			}),
		}
	}

	pub(super) fn into_result(
		self,
		outcome: DocumentMutationOutcome,
	) -> DocumentLiveOrInitMutationResult<T> {
		DocumentLiveOrInitMutationResult {
			value_seen: match self {
				Self::Live(current) => DocumentLiveOrInitValue::Existing {
					value: current.value,
					metadata: current.metadata,
				},
				Self::Initialized { value, .. } => DocumentLiveOrInitValue::Initialized { value },
			},
			outcome,
		}
	}
}
