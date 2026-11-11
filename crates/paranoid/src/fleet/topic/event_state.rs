//! Native topic event identity and library envelope without publication or payload inference.

use super::sequence::validate_nonnegative_topic_sequence;
use crate::fleet::{NegativeTopicSequenceError, TopicKey};

use crate::fleet::payload::identity::NativeFleetPayloadIdentity;
use crate::fleet::payload::state::{FleetPayload, FleetPayloadError};
use crate::fleet::primitive_scope::NativePrimitiveCodeAccess;
use crate::installation::contract::role_binding::BoundSchemaReader;
use crate::installation::contract::{
	BoundStateSchema, ComponentDeclaration, ComponentDeclarationFields, ComponentPolicy,
	ComponentPolicyFields, ComponentSettings, ComponentSettingsDeclaration,
	ComponentSettingsRequirement, ContractStateReadError, RepresentationPolicy, SchemaPolicy,
	SemanticContractDef, StateContractError, StateStorageEnvelope,
};
use crate::records::state_schema::{
	StateRepresentationFormat, StateSchema, StateSchemaError, StateVersion,
};
use crate::records::{
	EncodedRecord, IndexFields, RecordIdentity, RecordMetadata, RecordNamespace,
	StateTimestampMicros, StoredRecord,
};

mod wire;

const SEMANTICS: &str = "paranoid.topic-event.v1";
const NATIVE_REPRESENTATION_VERSION: u32 = 1;

pub(crate) struct TopicEventIdentityFields {
	pub(crate) topic: TopicKey,
	pub(crate) sequence: i64,
}

/// Retained sequence zero is valid; conversion must not allocate another sequence.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct TopicEventIdentity {
	topic: TopicKey,
	sequence: i64,
}
impl TopicEventIdentity {
	pub(crate) fn new(
		fields: TopicEventIdentityFields,
	) -> Result<Self, NegativeTopicSequenceError> {
		validate_nonnegative_topic_sequence(fields.sequence)?;
		Ok(Self {
			topic: fields.topic,
			sequence: fields.sequence,
		})
	}
	pub(crate) fn topic(&self) -> &TopicKey {
		&self.topic
	}
	pub(crate) fn sequence(&self) -> i64 {
		self.sequence
	}
}
impl std::fmt::Debug for TopicEventIdentity {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("TopicEventIdentity").finish_non_exhaustive()
	}
}

pub(crate) struct NativeTopicEventStateFields {
	pub(crate) identity: TopicEventIdentity,
	pub(crate) published_at: StateTimestampMicros,
	pub(crate) payload: FleetPayload,
}
pub(crate) struct NativeTopicEventState {
	identity: TopicEventIdentity,
	published_at: StateTimestampMicros,
	payload: FleetPayload,
}
impl NativeTopicEventState {
	pub(crate) fn new(fields: NativeTopicEventStateFields) -> Self {
		Self {
			identity: fields.identity,
			published_at: fields.published_at,
			payload: fields.payload,
		}
	}
	pub(crate) fn identity(&self) -> &TopicEventIdentity {
		&self.identity
	}
	pub(crate) fn published_at(&self) -> StateTimestampMicros {
		self.published_at
	}
	pub(crate) fn payload(&self) -> &FleetPayload {
		&self.payload
	}
}
impl std::fmt::Debug for NativeTopicEventState {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("NativeTopicEventState")
			.finish_non_exhaustive()
	}
}

/// Local codec registration for the library-owned TopicEvent state.
pub(crate) struct TopicEventComponentFields {
	/// Registers only readers, or both readers and writers, without choosing installed policy.
	pub(crate) code_access: NativePrimitiveCodeAccess,
}
/// Actual library-owned intrinsic codecs; construction grants no stored-state access.
pub(crate) struct TopicEventComponent {
	schema: StateSchema<NativeTopicEventState, TopicEventCodecError>,
}
impl TopicEventComponent {
	pub(crate) fn component_identifier() -> &'static str {
		"paranoid.topic-event"
	}
	/// Registers the actual intrinsic codecs with the explicitly selected local access.
	pub(crate) fn new(fields: TopicEventComponentFields) -> Self {
		Self {
			schema: StateSchema::new(crate::records::state_schema::StateSchemaFields {
				identifier: Self::component_identifier(),
				versions: vec![StateVersion {
					version: NATIVE_REPRESENTATION_VERSION,
					format: StateRepresentationFormat::Json,
					decoder: wire::decode,
					encoder: match fields.code_access {
						NativePrimitiveCodeAccess::ReaderOnly => None,
						NativePrimitiveCodeAccess::ReaderWriter => Some(wire::encode),
					},
				}],
			})
			.expect("TopicEvent declares its actual initial library envelope codec"),
		}
	}
	/// Selects this actual owner's initial native writer and retained reader without resource
	/// authority.
	pub(crate) fn initial_policy(&self) -> Result<ComponentPolicy, StateContractError> {
		let writer = RepresentationPolicy {
			version: NATIVE_REPRESENTATION_VERSION,
			envelope: StateStorageEnvelope::NativeJson,
		};
		let policy = ComponentPolicy::new(ComponentPolicyFields {
			component_identifier: Self::component_identifier().to_owned(),
			semantic_contract: SEMANTICS.to_owned(),
			schemas: vec![SchemaPolicy::new(
				self.schema.identifier().to_owned(),
				writer,
				vec![writer],
			)?],
			settings: ComponentSettings::Parameterless,
		})?;
		let declaration = self.declaration();
		let bound = declaration.bind(&policy)?;
		bound.bind_schema(&self.schema)?;
		Ok(policy)
	}
	pub(crate) fn schema(&self) -> &StateSchema<NativeTopicEventState, TopicEventCodecError> {
		&self.schema
	}
	pub(crate) fn declaration(&self) -> ComponentDeclaration<'_, Self> {
		ComponentDeclaration::new(ComponentDeclarationFields {
			identifier: Self::component_identifier(),
			fixed_schemas: vec![self.schema.capabilities()],
			settings: ComponentSettingsDeclaration::Parameterless,
			semantics: vec![SemanticContractDef {
				identifier: SEMANTICS,
				transitions: self,
				settings: ComponentSettingsRequirement::Parameterless,
			}],
		})
		.expect("TopicEvent declares its actual library envelope functions")
	}
	pub(crate) fn bind_reader<'a>(
		&self,
		fields: TopicEventReaderFields<'a>,
	) -> Result<TopicEventReader<'a>, TopicEventStateError> {
		if !fields.reader.matches_schema_declaration(&self.schema) {
			return Err(TopicEventStateError::DifferentDeclaration);
		}
		Ok(TopicEventReader {
			reader: fields.reader,
			expected: fields.expected,
			namespace: fields.namespace,
		})
	}
	pub(crate) fn bind_writer<'a>(
		&self,
		fields: TopicEventWriterFields<'a>,
	) -> Result<TopicEventWriter<'a>, TopicEventStateError> {
		if !fields.writer.matches_schema_declaration(&self.schema) {
			return Err(TopicEventStateError::DifferentDeclaration);
		}
		if fields.writer.policy().writer().envelope != StateStorageEnvelope::NativeJson {
			return Err(TopicEventStateError::NativeWriterRequired);
		}
		Ok(TopicEventWriter {
			writer: fields.writer,
			expected: fields.expected,
			namespace: fields.namespace,
		})
	}
}
pub(crate) struct TopicEventReaderFields<'a> {
	pub(crate) reader: BoundSchemaReader<'a, NativeTopicEventState, TopicEventCodecError>,
	pub(crate) expected: &'a TopicEventIdentity,
	pub(crate) namespace: &'a RecordNamespace,
}
pub(crate) struct TopicEventReader<'a> {
	reader: BoundSchemaReader<'a, NativeTopicEventState, TopicEventCodecError>,
	expected: &'a TopicEventIdentity,
	namespace: &'a RecordNamespace,
}
impl TopicEventReader<'_> {
	pub(crate) fn decode_record(
		&self,
		identity: &RecordIdentity,
		record: &StoredRecord,
	) -> Result<NativeTopicEventState, TopicEventStateError> {
		require_identity(self.expected, self.namespace, identity)?;
		let state = decode_stored_record(&self.reader, record)?;
		require_resource(self.expected, &state)?;
		Ok(state)
	}
}

pub(in crate::fleet) struct TopicEventDiscoveredReadFields<'a> {
	pub(in crate::fleet) namespace: &'a RecordNamespace,
	pub(in crate::fleet) identity: &'a RecordIdentity,
	pub(in crate::fleet) record: &'a StoredRecord,
}
/// Resolves the actual body identity once, then checks the complete existing framed lookup.
pub(in crate::fleet) fn read_discovered_record(
	reader: &BoundSchemaReader<'_, NativeTopicEventState, TopicEventCodecError>,
	fields: TopicEventDiscoveredReadFields<'_>,
) -> Result<NativeTopicEventState, TopicEventStateError> {
	if fields.identity.namespace() != fields.namespace {
		return Err(TopicEventStateError::RecordIdentityMismatch {
			expected: RecordIdentity::new(fields.namespace.clone(), fields.identity.key().clone()),
			actual: fields.identity.clone(),
		});
	}
	let state = decode_stored_record(reader, fields.record)?;
	require_identity(state.identity(), fields.namespace, fields.identity)?;
	Ok(state)
}
fn decode_stored_record(
	reader: &BoundSchemaReader<'_, NativeTopicEventState, TopicEventCodecError>,
	record: &StoredRecord,
) -> Result<NativeTopicEventState, TopicEventStateError> {
	if !record.index_fields().is_empty() {
		return Err(TopicEventStateError::UnexpectedIndexFields);
	}
	let (_, state) = reader.decode_document(record.body())?;
	Ok(state)
}
pub(crate) struct TopicEventWriterFields<'a> {
	pub(crate) writer: BoundStateSchema<'a, NativeTopicEventState, TopicEventCodecError>,
	pub(crate) expected: &'a TopicEventIdentity,
	pub(crate) namespace: &'a RecordNamespace,
}
pub(crate) struct TopicEventWriter<'a> {
	writer: BoundStateSchema<'a, NativeTopicEventState, TopicEventCodecError>,
	expected: &'a TopicEventIdentity,
	namespace: &'a RecordNamespace,
}
impl TopicEventWriter<'_> {
	/// Metadata passes through unchanged; the library wrapper does not refresh or remove TTL.
	pub(crate) fn encode_record(
		&self,
		identity: &RecordIdentity,
		state: &NativeTopicEventState,
		metadata: RecordMetadata,
	) -> Result<EncodedRecord, TopicEventStateError> {
		require_identity(self.expected, self.namespace, identity)?;
		require_resource(self.expected, state)?;
		let body = self.writer.encode(state)?;
		EncodedRecord::derive(body, metadata, |body, _metadata| {
			let (_, actual) = self.writer.decode_document(body)?;
			require_resource(self.expected, &actual)?;
			if actual.published_at != state.published_at
				|| !actual.payload.matches_payload(&state.payload)
			{
				return Err(TopicEventStateError::EncodedStateMismatch);
			}
			Ok(IndexFields::new())
		})
	}
}
fn require_identity(
	expected: &TopicEventIdentity,
	namespace: &RecordNamespace,
	actual: &RecordIdentity,
) -> Result<(), TopicEventStateError> {
	let identity = RecordIdentity::new(
		namespace.clone(),
		NativeFleetPayloadIdentity::TopicEvent(expected).record_key(),
	);
	if &identity != actual {
		return Err(TopicEventStateError::RecordIdentityMismatch {
			expected: identity,
			actual: actual.clone(),
		});
	}
	Ok(())
}
fn require_resource(
	expected: &TopicEventIdentity,
	state: &NativeTopicEventState,
) -> Result<(), TopicEventStateError> {
	if expected != state.identity() {
		return Err(TopicEventStateError::ResourceMismatch);
	}
	Ok(())
}

use primitives_rs::serialization::json::JsonDecodeFailureSummary;

#[derive(thiserror::Error)]
pub(crate) enum TopicEventCodecError {
	#[error("TopicEvent JSON is invalid: {summary}")]
	Json { summary: JsonDecodeFailureSummary },
	#[error("TopicEvent JSON exceeds the maximum nesting depth")]
	MaxNestingDepth,
	#[error("topic event topic name is invalid")]
	TopicKey(#[source] crate::kv::KeyError),
	#[error("topic event sequence must not be negative")]
	Sequence(#[source] NegativeTopicSequenceError),
	#[error("TopicEvent integer is invalid")]
	Integer(#[source] crate::fleet::scalar_codec::ValueError),
	#[error("TopicEvent payload transport is invalid")]
	Payload(#[from] FleetPayloadError),
}
impl std::fmt::Debug for TopicEventCodecError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		std::fmt::Display::fmt(self, f)
	}
}
#[derive(thiserror::Error)]
pub(crate) enum TopicEventStateError {
	#[error("TopicEvent binding does not use its schema declaration")]
	DifferentDeclaration,
	#[error("TopicEvent writer must select its native JSON representation")]
	NativeWriterRequired,
	#[error("TopicEvent lookup differs from the complete framed resource identity")]
	RecordIdentityMismatch {
		expected: RecordIdentity,
		actual: RecordIdentity,
	},
	#[error("TopicEvent body belongs to another complete resource identity")]
	ResourceMismatch,
	#[error("TopicEvent has stored fields not derived by this family")]
	UnexpectedIndexFields,
	#[error("TopicEvent encoded state differs from its prepared semantic value")]
	EncodedStateMismatch,
	#[error("TopicEvent body is not readable under its selected policy")]
	Read(#[from] ContractStateReadError<TopicEventCodecError>),
	#[error("TopicEvent body cannot encode its selected writer")]
	Encode(#[from] StateSchemaError<TopicEventCodecError>),
}
impl std::fmt::Debug for TopicEventStateError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		std::fmt::Display::fmt(self, f)
	}
}
