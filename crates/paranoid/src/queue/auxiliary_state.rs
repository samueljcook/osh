//! Native Queue auxiliary representations and exact logical identities.
//!
//! These codecs carry no mutation, admission, cross-record consistency, or commit proof.
//! Group operations must maintain the dedupe and original-job associations atomically.

use super::state::QueueStateError;
use super::{DedupeKey, JobId, TaskName};
use crate::installation::contract::{
	BoundComponentContract, BoundStateSchema, ContractStateReadError, RepresentationPolicy,
	SchemaPolicy, StateContractError, StateStorageEnvelope,
};
use crate::records::state_schema::{
	StateRepresentationFormat, StateSchema, StateSchemaError, StateVersion,
};
use crate::records::{
	EncodedRecord, IndexFields, PointRecordRead, RecordIdentity, RecordKey, RecordMetadata,
	RecordNamespace, StoredRecord,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

mod active_dedupe;
mod key;
pub(in crate::queue) use key::{parse_active_dedupe_key, parse_pause_key};
mod dead_letter;
mod original;
mod pause;

pub(super) use active_dedupe::ActiveDedupeState;
pub(in crate::queue) use active_dedupe::membership;
pub(super) use dead_letter::NativeDeadLetterState;
pub(super) use original::DeadLetterOriginalState;
pub(crate) use pause::QueuePauseScope;
pub(super) use pause::QueuePauseState;

const AUXILIARY_REPRESENTATION_VERSION: u32 = 1;

/// Actual registries; semantic support must come from the owning group implementation.
pub(in crate::queue) struct QueueAuxiliarySchemas {
	active_dedupe: StateSchema<ActiveDedupeState, AuxiliaryStateError>,
	dead_letter: StateSchema<NativeDeadLetterState, AuxiliaryStateError>,
	original: StateSchema<DeadLetterOriginalState, AuxiliaryStateError>,
	pause: StateSchema<QueuePauseState, AuxiliaryStateError>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AuxiliaryStateError {
	#[error("queue auxiliary JSON is invalid: {category:?} error at line {line}, column {column}")]
	Json {
		category: serde_json::error::Category,
		line: usize,
		column: usize,
	},
	#[error("queue auxiliary state is invalid: {reason}")]
	Invalid { reason: &'static str },
	#[error("queue auxiliary field is outside its shared queue domain")]
	Field(#[from] QueueStateError),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AuxiliaryRepresentationError {
	#[error("queue auxiliary schema policy binding failed")]
	Contract(#[from] StateContractError),
	#[error("queue auxiliary native writer is required")]
	NativeWriterRequired,
	#[error("queue auxiliary state cannot be read under its bound policy")]
	Read(#[from] ContractStateReadError<AuxiliaryStateError>),
	#[error("queue auxiliary state cannot encode its selected representation")]
	Encode(#[from] StateSchemaError<AuxiliaryStateError>),
	#[error("queue auxiliary identity, state, or metadata is invalid")]
	State(#[from] AuxiliaryStateError),
}

/// A codec bound to one exact namespace/key and one actual immutable schema instance.
pub(super) struct AuxiliarySchemaBinding<'a, T> {
	identity: RecordIdentity,
	schema: BoundStateSchema<'a, T, AuxiliaryStateError>,
}

/// Closed internal behavior of the four auxiliary working types.
pub(super) trait AuxiliaryRecord {
	fn key(&self) -> Result<RecordKey, AuxiliaryStateError>;
	fn check_metadata(&self, metadata: RecordMetadata) -> Result<(), AuxiliaryStateError>;
	fn project(&self) -> IndexFields;
}

impl QueueAuxiliarySchemas {
	pub(crate) fn new() -> Self {
		Self {
			active_dedupe: schema(
				"paranoid.queue.active-dedupe",
				active_dedupe::decode,
				active_dedupe::encode,
			),
			dead_letter: schema(
				"paranoid.queue.dead-letter",
				dead_letter::decode,
				dead_letter::encode,
			),
			original: schema(
				"paranoid.queue.dead-letter-original",
				original::decode,
				original::encode,
			),
			pause: schema("paranoid.queue.pause", pause::decode, pause::encode),
		}
	}
	pub(in crate::queue) fn active_dedupe_schema(
		&self,
	) -> &StateSchema<ActiveDedupeState, AuxiliaryStateError> {
		&self.active_dedupe
	}
	pub(in crate::queue) fn dead_letter_schema(
		&self,
	) -> &StateSchema<NativeDeadLetterState, AuxiliaryStateError> {
		&self.dead_letter
	}
	pub(in crate::queue) fn original_schema(
		&self,
	) -> &StateSchema<DeadLetterOriginalState, AuxiliaryStateError> {
		&self.original
	}
	pub(in crate::queue) fn pause_schema(
		&self,
	) -> &StateSchema<QueuePauseState, AuxiliaryStateError> {
		&self.pause
	}

	/// Fixed initial native writers and readers for these actual four auxiliary registries.
	pub(in crate::queue) fn initial_schema_policies(
		&self,
	) -> Result<Vec<SchemaPolicy>, StateContractError> {
		let writer = RepresentationPolicy {
			version: AUXILIARY_REPRESENTATION_VERSION,
			envelope: StateStorageEnvelope::NativeJson,
		};
		[
			self.active_dedupe.identifier(),
			self.dead_letter.identifier(),
			self.original.identifier(),
			self.pause.identifier(),
		]
		.into_iter()
		.map(|identifier| SchemaPolicy::new(identifier.to_owned(), writer, vec![writer]))
		.collect()
	}

	pub(super) fn bind_active_dedupe<'a, C>(
		&'a self,
		component: &'a BoundComponentContract<'_, '_, C>,
		namespace: RecordNamespace,
		task_name: &TaskName,
		dedupe_key: &DedupeKey,
	) -> Result<AuxiliarySchemaBinding<'a, ActiveDedupeState>, AuxiliaryRepresentationError> {
		self.bind(
			component,
			&self.active_dedupe,
			namespace,
			active_dedupe::key(task_name, dedupe_key)?,
		)
	}
	pub(super) fn bind_dead_letter<'a, C>(
		&'a self,
		component: &'a BoundComponentContract<'_, '_, C>,
		namespace: RecordNamespace,
		dead_letter_id: JobId,
	) -> Result<AuxiliarySchemaBinding<'a, NativeDeadLetterState>, AuxiliaryRepresentationError> {
		self.bind(
			component,
			&self.dead_letter,
			namespace,
			id_key(dead_letter_id),
		)
	}
	pub(super) fn bind_original<'a, C>(
		&'a self,
		component: &'a BoundComponentContract<'_, '_, C>,
		namespace: RecordNamespace,
		original_job_id: JobId,
	) -> Result<AuxiliarySchemaBinding<'a, DeadLetterOriginalState>, AuxiliaryRepresentationError>
	{
		self.bind(
			component,
			&self.original,
			namespace,
			id_key(original_job_id),
		)
	}
	pub(super) fn bind_pause<'a, C>(
		&'a self,
		component: &'a BoundComponentContract<'_, '_, C>,
		namespace: RecordNamespace,
		scope: &QueuePauseScope,
	) -> Result<AuxiliarySchemaBinding<'a, QueuePauseState>, AuxiliaryRepresentationError> {
		self.bind(component, &self.pause, namespace, scope.key())
	}
	fn bind<'a, C, T>(
		&'a self,
		component: &'a BoundComponentContract<'_, '_, C>,
		schema: &'a StateSchema<T, AuxiliaryStateError>,
		namespace: RecordNamespace,
		key: RecordKey,
	) -> Result<AuxiliarySchemaBinding<'a, T>, AuxiliaryRepresentationError> {
		Ok(AuxiliarySchemaBinding {
			identity: RecordIdentity::new(namespace, key),
			schema: component.bind_schema(schema)?,
		})
	}
}

impl<T: AuxiliaryRecord> AuxiliarySchemaBinding<'_, T> {
	pub(super) fn identity(&self) -> &RecordIdentity {
		&self.identity
	}

	/// The supplied paired identity must match even when its physical value is absent.
	pub(super) fn decode_record(
		&self,
		identity: &RecordIdentity,
		snapshot: &PointRecordRead,
	) -> Result<Option<T>, AuxiliaryRepresentationError> {
		self.require_identity(identity)?;
		let Some(record) = snapshot.record() else {
			return Ok(None);
		};
		self.decode_valued_record(record).map(Some)
	}

	pub(in crate::queue) fn decode_stored_record(
		&self,
		identity: &RecordIdentity,
		record: &StoredRecord,
	) -> Result<T, AuxiliaryRepresentationError> {
		self.require_identity(identity)?;
		self.decode_valued_record(record)
	}
	fn require_identity(
		&self,
		identity: &RecordIdentity,
	) -> Result<(), AuxiliaryRepresentationError> {
		if identity != &self.identity {
			return Err(invalid("selected record identity differs from the binding").into());
		}
		Ok(())
	}
	fn decode_valued_record(
		&self,
		record: &StoredRecord,
	) -> Result<T, AuxiliaryRepresentationError> {
		let state = self.decode_stored(record.body(), record.metadata())?;
		if record.index_fields() != &state.project() {
			return Err(invalid("stored projections differ from the encoded state").into());
		}
		Ok(state)
	}

	/// Modification time is explicit; map/pause bodies do not duplicate envelope timestamps.
	pub(super) fn encode_record(
		&self,
		state: &T,
		metadata: RecordMetadata,
	) -> Result<EncodedRecord, AuxiliaryRepresentationError> {
		if self.schema.policy().writer().envelope != StateStorageEnvelope::NativeJson {
			return Err(AuxiliaryRepresentationError::NativeWriterRequired);
		}
		self.check(state, metadata)?;
		let body = self.schema.encode(state)?;
		EncodedRecord::derive(body, metadata, |encoded, metadata| {
			Ok(self.decode_stored(encoded, metadata)?.project())
		})
	}
	fn decode_stored(
		&self,
		bytes: &[u8],
		metadata: RecordMetadata,
	) -> Result<T, AuxiliaryRepresentationError> {
		let (_, state) = self.schema.decode_document(bytes)?;
		self.check(&state, metadata)?;
		Ok(state)
	}
	fn check(&self, state: &T, metadata: RecordMetadata) -> Result<(), AuxiliaryStateError> {
		if metadata.expires_at.is_some() {
			return Err(invalid("auxiliary envelopes must not expire"));
		}
		if state.key()? != *self.identity.key() {
			return Err(invalid("body does not belong to the selected record key"));
		}
		state.check_metadata(metadata)
	}
}

fn schema<T>(
	identifier: &'static str,
	decoder: fn(&[u8]) -> Result<T, AuxiliaryStateError>,
	encoder: fn(&T) -> Result<Vec<u8>, AuxiliaryStateError>,
) -> StateSchema<T, AuxiliaryStateError> {
	StateSchema::new(crate::records::state_schema::StateSchemaFields {
		identifier,
		versions: vec![StateVersion {
			version: AUXILIARY_REPRESENTATION_VERSION,
			format: StateRepresentationFormat::Json,
			decoder,
			encoder: Some(encoder),
		}],
	})
	.expect("queue auxiliary schemas declare one positive representation")
}
fn id_key(id: JobId) -> RecordKey {
	RecordKey::new(id.as_bytes().to_vec())
}
fn invalid(reason: &'static str) -> AuxiliaryStateError {
	AuxiliaryStateError::Invalid { reason }
}
fn decode_json<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, AuxiliaryStateError> {
	serde_json::from_slice(bytes).map_err(json_error)
}
fn encode_json<T: Serialize>(value: &T) -> Result<Vec<u8>, AuxiliaryStateError> {
	serde_json::to_vec(value).map_err(json_error)
}
fn json_error(error: serde_json::Error) -> AuxiliaryStateError {
	AuxiliaryStateError::Json {
		category: error.classify(),
		line: error.line(),
		column: error.column(),
	}
}
