//! CacheEntry intrinsic metadata access and separately declared typed replacement.

use super::{PayloadScopeError, PayloadScopeValueError, payload_policy, require_record_identity};
use crate::fleet::cache::entry_state::{
	CacheEntryComponent, CacheEntryIdentity, CacheEntryReaderFields, CacheEntryWriterFields,
	NativeCacheEntryState, NativeCacheEntryStateFields,
};
use crate::fleet::payload::identity::NativeFleetPayloadIdentity;
use crate::fleet::payload::state::{FleetPayload, require_payload_policy};
use crate::installation::contract::{
	BoundComponentContract, BoundComponentReader, ComponentDeclaration, ComponentPolicy,
	StateContractError, StateStorageEnvelope,
};
use crate::records::{EncodedRecord, RecordIdentity, RecordMetadata, StoredRecord};
use crate::scope::payload::{BoundPayloadProducer, BoundPayloadReader, PayloadSchemaDeclaration};
use crate::scope::{
	ComponentScopeDeclaration, ComponentScopeKind, ScopeError, ScopeIdentity, ScopePolicy,
};

/// Actual scope identity and borrowed CacheEntry envelope code.
pub struct CacheEntryScopeDeclarationFields<'scope, 'code> {
	/// Explicit logical storage and namespace identity.
	pub identity: &'scope ScopeIdentity,
	/// The actual library-owned envelope codec registry.
	pub component: &'code CacheEntryComponent,
}
/// A borrowed intrinsic CacheEntry declaration, independent from business payload codecs.
pub struct CacheEntryScopeDeclaration<'scope, 'code> {
	identity: &'scope ScopeIdentity,
	component: &'code CacheEntryComponent,
	declaration: ComponentDeclaration<'code, CacheEntryComponent>,
}
impl<'scope, 'code> CacheEntryScopeDeclaration<'scope, 'code> {
	/// Borrows the supplied identity and actual envelope code without accessing data.
	pub fn new(fields: CacheEntryScopeDeclarationFields<'scope, 'code>) -> Self {
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
	pub(crate) fn declaration(&self) -> &ComponentDeclaration<'code, CacheEntryComponent> {
		&self.declaration
	}
	pub(crate) fn record_identity(
		&self,
		cache_entry_identity: &CacheEntryIdentity,
	) -> RecordIdentity {
		RecordIdentity::new(
			self.identity.namespace().clone(),
			NativeFleetPayloadIdentity::CacheEntry(cache_entry_identity).record_key(),
		)
	}
	pub(crate) fn bind_reader<'a>(
		&'a self,
		actual_scope: &ScopeIdentity,
		policy: &'a ScopePolicy,
	) -> Result<BoundCacheEntryScopeReader<'a, 'code>, PayloadScopeError> {
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
		Ok(BoundCacheEntryScopeReader {
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
	) -> Result<BoundCacheEntryScopeWriter<'a, 'code>, PayloadScopeError> {
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
		Ok(BoundCacheEntryScopeWriter { reader, contract })
	}
}

pub(crate) struct CacheEntryScopeReadFields<'a> {
	pub(crate) identity: &'a RecordIdentity,
	pub(crate) cache_entry_identity: &'a CacheEntryIdentity,
	pub(crate) record: &'a StoredRecord,
}
pub(crate) struct CacheEntryScopeWriteFields<'a> {
	pub(crate) identity: &'a RecordIdentity,
	pub(crate) cache_entry_identity: &'a CacheEntryIdentity,
	pub(crate) epoch: i64,
	pub(crate) metadata: RecordMetadata,
}

pub(crate) struct CacheEntryPayloadPrepareFields<'a, 'scope, 'code, 'bound, T, C> {
	pub(crate) identity: &'a RecordIdentity,
	pub(crate) cache_entry_identity: &'a CacheEntryIdentity,
	pub(crate) captured_epoch: i64,
	pub(crate) declaration: &'a PayloadSchemaDeclaration<'scope, 'code, T, C>,
	pub(crate) producer: &'a BoundPayloadProducer<'bound, T, C>,
	pub(crate) value: &'a T,
}

/// Prepared business bytes retain their actual writer and accept only final library metadata.
pub(crate) struct PreparedCacheEntryPayload<'writer, 'binding, 'code> {
	writer: &'writer BoundCacheEntryScopeWriter<'binding, 'code>,
	identity: RecordIdentity,
	cache_entry_identity: CacheEntryIdentity,
	captured_epoch: i64,
	payload: FleetPayload,
}
impl PreparedCacheEntryPayload<'_, '_, '_> {
	/// Supplies no data-lock, clock, participant or private Lease authority.
	pub(crate) fn encode(
		self,
		metadata: RecordMetadata,
	) -> Result<EncodedRecord, PayloadScopeError> {
		self.writer.encode_record(
			CacheEntryScopeWriteFields {
				identity: &self.identity,
				cache_entry_identity: &self.cache_entry_identity,
				epoch: self.captured_epoch,
				metadata,
			},
			self.payload,
		)
	}
}
impl std::fmt::Debug for PreparedCacheEntryPayload<'_, '_, '_> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("PreparedCacheValuePayload { .. }")
	}
}

pub(crate) struct BoundCacheEntryScopeReader<'a, 'code> {
	identity: &'a ScopeIdentity,
	component: &'code CacheEntryComponent,
	contract: BoundComponentReader<'a, 'code, CacheEntryComponent>,
	policy: &'a ScopePolicy,
}
impl BoundCacheEntryScopeReader<'_, '_> {
	/// Inspects a discovered native body without an application decoder or caller-invented key.
	pub(crate) fn decode_discovered_record(
		&self,
		fields: super::PayloadScopeRecordFields<'_>,
	) -> Result<NativeCacheEntryState, PayloadScopeError> {
		let reader = self
			.contract
			.bind_schema(self.component.schema())
			.map_err(ScopeError::from)?;
		let state = crate::fleet::cache::entry_state::read_discovered_record(
			&reader,
			crate::fleet::cache::entry_state::CacheEntryDiscoveredReadFields {
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
		fields: CacheEntryScopeReadFields<'_>,
	) -> Result<NativeCacheEntryState, PayloadScopeError> {
		let reader = self
			.contract
			.bind_schema(self.component.schema())
			.map_err(ScopeError::from)?;
		let state = self
			.component
			.bind_reader(CacheEntryReaderFields {
				reader,
				expected: fields.cache_entry_identity,
				namespace: self.identity.namespace(),
			})?
			.decode_record(fields.identity, fields.record)?;
		require_payload_policy(state.payload(), payload_policy(self.policy)?)?;
		Ok(state)
	}
}

pub(crate) struct BoundCacheEntryScopeWriter<'a, 'code> {
	reader: BoundCacheEntryScopeReader<'a, 'code>,
	contract: BoundComponentContract<'a, 'code, CacheEntryComponent>,
}
impl<'binding, 'code> BoundCacheEntryScopeWriter<'binding, 'code> {
	/// Encodes through the expected actual Producer before the caller acquires data locks.
	pub(crate) fn prepare_payload<'writer, T, C>(
		&'writer self,
		fields: CacheEntryPayloadPrepareFields<'_, '_, '_, '_, T, C>,
	) -> Result<PreparedCacheEntryPayload<'writer, 'binding, 'code>, PayloadScopeValueError<C>> {
		require_record_identity(
			self.reader.identity,
			NativeFleetPayloadIdentity::CacheEntry(fields.cache_entry_identity),
			fields.identity,
		)?;
		fields
			.producer
			.require_declaration(fields.declaration)
			.map_err(PayloadScopeError::from)?;
		let payload = self.prepare_payload_value(fields.producer, fields.value)?;
		Ok(PreparedCacheEntryPayload {
			writer: self,
			identity: fields.identity.clone(),
			cache_entry_identity: fields.cache_entry_identity.clone(),
			captured_epoch: fields.captured_epoch,
			payload,
		})
	}

	fn prepare_payload_value<T, C>(
		&self,
		producer: &BoundPayloadProducer<'_, T, C>,
		value: &T,
	) -> Result<FleetPayload, PayloadScopeValueError<C>> {
		producer
			.require_scope_policy(self.reader.identity, self.reader.policy)
			.map_err(PayloadScopeError::from)?;
		Ok(producer.encode(value)?)
	}

	pub(super) fn encode_record(
		&self,
		fields: CacheEntryScopeWriteFields<'_>,
		payload: FleetPayload,
	) -> Result<EncodedRecord, PayloadScopeError> {
		let state = NativeCacheEntryState::new(NativeCacheEntryStateFields {
			identity: fields.cache_entry_identity.clone(),
			epoch: fields.epoch,
			payload,
		});
		self.prepare_record(
			fields.identity,
			fields.cache_entry_identity,
			&state,
			fields.metadata,
		)
	}
	fn prepare_record(
		&self,
		identity: &RecordIdentity,
		cache_entry_identity: &CacheEntryIdentity,
		state: &NativeCacheEntryState,
		metadata: RecordMetadata,
	) -> Result<EncodedRecord, PayloadScopeError> {
		require_record_identity(
			self.reader.identity,
			NativeFleetPayloadIdentity::CacheEntry(cache_entry_identity),
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
			.bind_writer(CacheEntryWriterFields {
				writer,
				expected: cache_entry_identity,
				namespace: self.reader.identity.namespace(),
			})?
			.encode_record(identity, state, metadata)?)
	}
}
