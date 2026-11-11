//! Non-generic data work after actual application bytes have been prepared once.

use super::publication_lock::{
	PgTopicPublicationLock, TopicEventTarget, TopicPublicationLockFields, TopicPublicationRecords,
	TopicPublicationRejectionError, TopicPublicationStorageError, TopicSequenceTarget,
};
use crate::db::WriteTx;
use crate::fleet::TopicKey;
use crate::fleet::payload::scope::PayloadScopeError;
use crate::fleet::payload::scope::topic::{PreparedTopicPayload, TopicEventScopeWriteFields};
use crate::fleet::scalar_codec::{ScalarSchemaBinding, ScalarStateError};
use crate::kv::Ttl;
use crate::records::postgres::group_attempt::PgRecordGroupAttemptBudget;
use crate::records::postgres::record_set::PgPlacedRecordIdentity;
use crate::records::{RecordMetadata, StateTimestampMicros};

pub(crate) struct TopicPublication {
	pub(crate) sequence: i64,
	pub(crate) published_at: StateTimestampMicros,
}
impl std::fmt::Debug for TopicPublication {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("TopicPublication").finish_non_exhaustive()
	}
}

pub(super) struct PublicationFields<'a, 'tx, 'policy, 'bound, 'code> {
	pub(super) tx: &'a mut WriteTx<'tx>,
	pub(super) sequence: TopicSequenceTarget<'a, 'policy>,
	pub(super) sequence_schema: &'policy ScalarSchemaBinding<'policy, i64>,
	pub(super) events: TopicEventTarget<'a>,
	pub(super) topic: &'a TopicKey,
	pub(super) budget: PgRecordGroupAttemptBudget,
	pub(super) ttl: Ttl,
	pub(super) payload: PreparedTopicPayload<'policy, 'bound, 'code>,
}

pub(super) async fn publish(
	fields: PublicationFields<'_, '_, '_, '_, '_>,
) -> Result<TopicPublication, TopicPublicationError> {
	let lock = match PgTopicPublicationLock::acquire(TopicPublicationLockFields {
		tx: fields.tx,
		sequence: fields.sequence,
		events: fields.events,
		topic: fields.topic,
		budget: fields.budget,
	})
	.await
	.map_err(TopicPublicationError::Storage)?
	{
		Ok(lock) => lock,
		Err(rejection) => return Err(TopicPublicationError::Rejection(rejection)),
	};
	let observed_at = lock.observed_at();
	let prepared = prepare(&lock, fields.sequence_schema, fields.payload, fields.ttl);
	let records = match prepared {
		Ok(prepared) => prepared,
		Err(operation) => {
			return match lock.reject().await {
				Ok(()) => Err(operation),
				Err(cleanup) => Err(TopicPublicationError::PreparationCleanup {
					operation_error: Box::new(operation),
					cleanup_error: Box::new(cleanup),
				}),
			};
		}
	};
	let sequence = lock.topic_event_identity().sequence();
	lock.apply(records)
		.await
		.map_err(TopicPublicationError::Storage)?
		.map_err(TopicPublicationError::Rejection)?;
	Ok(TopicPublication {
		sequence,
		published_at: observed_at,
	})
}

fn prepare(
	lock: &PgTopicPublicationLock<'_, '_>,
	schema: &ScalarSchemaBinding<'_, i64>,
	payload: PreparedTopicPayload<'_, '_, '_>,
	ttl: Ttl,
) -> Result<TopicPublicationRecords, TopicPublicationError> {
	let observed_at = lock.observed_at();
	let event_metadata = RecordMetadata {
		expires_at: ttl.expiration_at(observed_at).map_err(|_| {
			TopicPublicationError::ExpirationOverflow.with_identity(lock.event_identity())
		})?,
		updated_at: observed_at,
	};
	let sequence = schema
		.encode_record(
			&lock.topic_event_identity().sequence(),
			RecordMetadata {
				expires_at: None,
				updated_at: observed_at,
			},
		)
		.map_err(|error| {
			TopicPublicationError::Sequence(error).with_identity(lock.sequence_identity())
		})?;
	let event = payload
		.encode_event(TopicEventScopeWriteFields {
			identity: lock.event_identity().record(),
			topic_event_identity: lock.topic_event_identity(),
			published_at: observed_at,
			metadata: event_metadata,
		})
		.map_err(|error| {
			TopicPublicationError::Envelope(error).with_identity(lock.event_identity())
		})?;
	Ok(TopicPublicationRecords { sequence, event })
}

#[derive(Debug, thiserror::Error)]
pub(in crate::fleet::topic) enum TopicPublicationError {
	#[error("native Topic selected sequence encoding failed")]
	Sequence(#[source] ScalarStateError),
	#[error("native Topic selected event encoding failed")]
	Envelope(#[source] PayloadScopeError),
	#[error("native Topic event expiry exceeds its timestamp domain")]
	ExpirationOverflow,
	#[error("native Topic publication was rejected under canonical authority")]
	Rejection(#[source] TopicPublicationRejectionError),
	#[error("native Topic canonical storage work failed")]
	Storage(#[source] TopicPublicationStorageError),
	#[error("native Topic preparation failed and rejection cleanup also failed")]
	PreparationCleanup {
		#[source]
		operation_error: Box<Self>,
		cleanup_error: Box<TopicPublicationStorageError>,
	},
	#[error("native Topic preparation failed for an identified placed record")]
	Record {
		identity: Box<PgPlacedRecordIdentity>,
		#[source]
		source: Box<Self>,
	},
}
impl TopicPublicationError {
	fn with_identity(self, identity: &PgPlacedRecordIdentity) -> Self {
		Self::Record {
			identity: Box::new(identity.clone()),
			source: Box::new(self),
		}
	}
	pub(crate) fn database_error(&self) -> Option<&crate::db::DatabaseError> {
		match self {
			Self::Record { source, .. } => source.database_error(),
			Self::Storage(source) => source.database_error(),
			Self::PreparationCleanup {
				operation_error,
				cleanup_error,
			} => operation_error
				.database_error()
				.or_else(|| cleanup_error.database_error()),
			Self::Sequence(_)
			| Self::Envelope(_)
			| Self::ExpirationOverflow
			| Self::Rejection(_) => None,
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Record { source, .. } => source.commit_outcome_is_unknown(),
			Self::Storage(source) => source.commit_outcome_is_unknown(),
			Self::PreparationCleanup {
				operation_error,
				cleanup_error,
			} => {
				operation_error.commit_outcome_is_unknown()
					|| cleanup_error.commit_outcome_is_unknown()
			}
			Self::Sequence(_)
			| Self::Envelope(_)
			| Self::ExpirationOverflow
			| Self::Rejection(_) => false,
		}
	}
}
