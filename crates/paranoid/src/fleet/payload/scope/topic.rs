//! TopicEvent intrinsic metadata access and separately declared typed replacement.

use super::{PayloadScopeError, PayloadScopeValueError, payload_policy, require_record_identity};
use crate::fleet::payload::identity::NativeFleetPayloadIdentity;
use crate::fleet::payload::state::{FleetPayload, require_payload_policy};
use crate::fleet::topic::event_state::{
	NativeTopicEventState, NativeTopicEventStateFields, TopicEventComponent, TopicEventIdentity,
	TopicEventReaderFields, TopicEventWriterFields,
};
use crate::installation::contract::{
	BoundComponentContract, BoundComponentReader, ComponentDeclaration, ComponentPolicy,
	StateContractError, StateStorageEnvelope,
};
use crate::records::{
	EncodedRecord, RecordIdentity, RecordMetadata, StateTimestampMicros, StoredRecord,
};
use crate::scope::payload::{BoundPayloadProducer, BoundPayloadReader};
use crate::scope::{
	ComponentScopeDeclaration, ComponentScopeKind, ScopeError, ScopeIdentity, ScopePolicy,
};

/// Actual scope identity and borrowed TopicEvent envelope code.
pub struct TopicEventScopeDeclarationFields<'scope, 'code> {
	/// Explicit logical storage and namespace identity.
	pub identity: &'scope ScopeIdentity,
	/// The actual library-owned envelope codec registry.
	pub component: &'code TopicEventComponent,
}
/// A borrowed intrinsic TopicEvent declaration, independent from business payload codecs.
pub struct TopicEventScopeDeclaration<'scope, 'code> {
	identity: &'scope ScopeIdentity,
	component: &'code TopicEventComponent,
	declaration: ComponentDeclaration<'code, TopicEventComponent>,
}
impl<'scope, 'code> TopicEventScopeDeclaration<'scope, 'code> {
	/// Borrows the supplied identity and actual envelope code without accessing data.
	pub fn new(fields: TopicEventScopeDeclarationFields<'scope, 'code>) -> Self {
		Self {
			identity: fields.identity,
			component: fields.component,
			declaration: fields.component.declaration(),
		}
	}
	/// Borrows the exact supplied scope identity.
	pub fn identity(&self) -> &'scope ScopeIdentity {
		self.identity
	}
	pub(crate) fn initial_intrinsic_policy(&self) -> Result<ComponentPolicy, StateContractError> {
		self.component.initial_policy()
	}
	pub(crate) fn declaration(&self) -> &ComponentDeclaration<'code, TopicEventComponent> {
		&self.declaration
	}
	pub(crate) fn bind_reader<'a>(
		&'a self,
		actual_scope: &ScopeIdentity,
		policy: &'a ScopePolicy,
	) -> Result<BoundTopicEventScopeReader<'a, 'code>, PayloadScopeError> {
		let contract = ComponentScopeDeclaration::new(
			self.identity,
			&self.declaration,
			ComponentScopeKind::Primitive,
		)
		.bind_reader(actual_scope, policy)?;
		contract
			.bind_schema(self.component.schema())
			.map_err(ScopeError::from)?;
		payload_policy(policy)?;
		Ok(BoundTopicEventScopeReader {
			identity: self.identity,
			component: self.component,
			contract,
			policy,
		})
	}
	pub(crate) fn bind_writer<'a>(
		&'a self,
		actual_scope: &ScopeIdentity,
		policy: &'a ScopePolicy,
	) -> Result<BoundTopicEventScopeWriter<'a, 'code>, PayloadScopeError> {
		let contract = ComponentScopeDeclaration::new(
			self.identity,
			&self.declaration,
			ComponentScopeKind::Primitive,
		)
		.bind(actual_scope, policy)?;
		let writer = contract
			.bind_schema(self.component.schema())
			.map_err(ScopeError::from)?;
		if writer.policy().writer().envelope != StateStorageEnvelope::NativeJson {
			return Err(ScopeError::NativeWriterRequired.into());
		}
		let reader = self.bind_reader(actual_scope, policy)?;
		Ok(BoundTopicEventScopeWriter { reader, contract })
	}
}

pub(crate) struct TopicEventScopeReadFields<'a> {
	pub(crate) identity: &'a RecordIdentity,
	pub(crate) topic_event_identity: &'a TopicEventIdentity,
	pub(crate) record: &'a StoredRecord,
}
pub(crate) struct TopicEventScopeWriteFields<'a> {
	pub(crate) identity: &'a RecordIdentity,
	pub(crate) topic_event_identity: &'a TopicEventIdentity,
	pub(crate) published_at: StateTimestampMicros,
	pub(crate) metadata: RecordMetadata,
}

pub(crate) struct BoundTopicEventScopeReader<'a, 'code> {
	identity: &'a ScopeIdentity,
	component: &'code TopicEventComponent,
	contract: BoundComponentReader<'a, 'code, TopicEventComponent>,
	policy: &'a ScopePolicy,
}
impl BoundTopicEventScopeReader<'_, '_> {
	/// Inspects a discovered native body without an application decoder or caller-invented key.
	pub(crate) fn decode_discovered_record(
		&self,
		fields: super::PayloadScopeRecordFields<'_>,
	) -> Result<NativeTopicEventState, PayloadScopeError> {
		let reader = self
			.contract
			.bind_schema(self.component.schema())
			.map_err(ScopeError::from)?;
		let state = crate::fleet::topic::event_state::read_discovered_record(
			&reader,
			crate::fleet::topic::event_state::TopicEventDiscoveredReadFields {
				namespace: self.identity.namespace(),
				identity: fields.identity,
				record: fields.record,
			},
		)?;
		require_payload_policy(state.payload(), payload_policy(self.policy)?)?;
		Ok(state)
	}
	pub(crate) fn require_payload_reader<T, C>(
		&self,
		reader: &BoundPayloadReader<'_, T, C>,
	) -> Result<(), PayloadScopeError> {
		Ok(reader.require_scope_policy(self.identity, self.policy)?)
	}
	pub(crate) fn decode_record(
		&self,
		fields: TopicEventScopeReadFields<'_>,
	) -> Result<NativeTopicEventState, PayloadScopeError> {
		let reader = self
			.contract
			.bind_schema(self.component.schema())
			.map_err(ScopeError::from)?;
		let state = self
			.component
			.bind_reader(TopicEventReaderFields {
				reader,
				expected: fields.topic_event_identity,
				namespace: self.identity.namespace(),
			})?
			.decode_record(fields.identity, fields.record)?;
		require_payload_policy(state.payload(), payload_policy(self.policy)?)?;
		Ok(state)
	}
}

pub(crate) struct BoundTopicEventScopeWriter<'a, 'code> {
	reader: BoundTopicEventScopeReader<'a, 'code>,
	contract: BoundComponentContract<'a, 'code, TopicEventComponent>,
}
impl<'policy, 'code> BoundTopicEventScopeWriter<'policy, 'code> {
	/// Produces only owned library transport; application values and codec owners are not retained.
	pub(crate) fn prepare_payload_with_producer<'writer, T, C>(
		&'writer self,
		producer: &BoundPayloadProducer<'_, T, C>,
		value: &T,
	) -> Result<PreparedTopicPayload<'writer, 'policy, 'code>, PayloadScopeValueError<C>> {
		producer
			.require_scope_policy(self.reader.identity, self.reader.policy)
			.map_err(PayloadScopeError::from)?;
		let payload = producer.encode(value)?;
		require_payload_policy(&payload, payload_policy(self.reader.policy)?)
			.map_err(PayloadScopeError::from)?;
		Ok(PreparedTopicPayload {
			writer: self,
			payload,
		})
	}

	pub(super) fn encode_record(
		&self,
		fields: TopicEventScopeWriteFields<'_>,
		payload: FleetPayload,
	) -> Result<EncodedRecord, PayloadScopeError> {
		let state = NativeTopicEventState::new(NativeTopicEventStateFields {
			identity: fields.topic_event_identity.clone(),
			published_at: fields.published_at,
			payload,
		});
		self.prepare_record(
			fields.identity,
			fields.topic_event_identity,
			&state,
			fields.metadata,
		)
	}
	fn prepare_record(
		&self,
		identity: &RecordIdentity,
		topic_event_identity: &TopicEventIdentity,
		state: &NativeTopicEventState,
		metadata: RecordMetadata,
	) -> Result<EncodedRecord, PayloadScopeError> {
		require_record_identity(
			self.reader.identity,
			NativeFleetPayloadIdentity::TopicEvent(topic_event_identity),
			identity,
		)?;
		require_payload_policy(state.payload(), payload_policy(self.reader.policy)?)?;
		let writer = self
			.contract
			.bind_schema(self.reader.component.schema())
			.map_err(ScopeError::from)?;
		Ok(self
			.reader
			.component
			.bind_writer(TopicEventWriterFields {
				writer,
				expected: topic_event_identity,
				namespace: self.reader.identity.namespace(),
			})?
			.encode_record(identity, state, metadata)?)
	}
}

/// Preparation remains bound to the actual immutable event writer, with no independent payload
/// injection.
pub(crate) struct PreparedTopicPayload<'writer, 'policy, 'code> {
	writer: &'writer BoundTopicEventScopeWriter<'policy, 'code>,
	payload: FleetPayload,
}
impl PreparedTopicPayload<'_, '_, '_> {
	pub(crate) fn encode_event(
		self,
		fields: TopicEventScopeWriteFields<'_>,
	) -> Result<EncodedRecord, PayloadScopeError> {
		self.writer.encode_record(fields, self.payload)
	}
}
