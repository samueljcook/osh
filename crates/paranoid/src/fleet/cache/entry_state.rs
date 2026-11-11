//! Native cache value identity and library envelope without application payload inference.

use crate::fleet::CoalescingCacheKey;
use crate::fleet::namespace_keys::validated_cache_key_parts;

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
	EncodedRecord, IndexFields, RecordIdentity, RecordMetadata, RecordNamespace, StoredRecord,
};

mod wire;

const SEMANTICS: &str = "paranoid.cache-entry.v1";
const NATIVE_REPRESENTATION_VERSION: u32 = 1;

pub(crate) struct CacheEntryIdentityFields {
	pub(crate) cache: CoalescingCacheKey,
	pub(crate) key_parts: Vec<String>,
}

/// The complete suffix tuple participates in resource identity, including an empty tuple.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct CacheEntryIdentity {
	cache: CoalescingCacheKey,
	key_parts: Vec<String>,
}
impl CacheEntryIdentity {
	pub(crate) fn new(fields: CacheEntryIdentityFields) -> Result<Self, crate::kv::KeyError> {
		Ok(Self {
			cache: fields.cache,
			key_parts: validated_cache_key_parts(fields.key_parts)?,
		})
	}
	pub(crate) fn cache(&self) -> &CoalescingCacheKey {
		&self.cache
	}
	pub(crate) fn key_parts(&self) -> &[String] {
		&self.key_parts
	}
}
impl std::fmt::Debug for CacheEntryIdentity {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("CacheValueIdentity")
			.field("suffix_count", &self.key_parts.len())
			.finish_non_exhaustive()
	}
}

pub(crate) struct NativeCacheEntryStateFields {
	pub(crate) identity: CacheEntryIdentity,
	pub(crate) epoch: i64,
	pub(crate) payload: FleetPayload,
}
pub(crate) struct NativeCacheEntryState {
	identity: CacheEntryIdentity,
	epoch: i64,
	payload: FleetPayload,
}
impl NativeCacheEntryState {
	pub(crate) fn new(fields: NativeCacheEntryStateFields) -> Self {
		Self {
			identity: fields.identity,
			epoch: fields.epoch,
			payload: fields.payload,
		}
	}
	pub(crate) fn identity(&self) -> &CacheEntryIdentity {
		&self.identity
	}
	pub(crate) fn epoch(&self) -> i64 {
		self.epoch
	}
	pub(crate) fn payload(&self) -> &FleetPayload {
		&self.payload
	}
}
impl std::fmt::Debug for NativeCacheEntryState {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("NativeCacheValueState")
			.finish_non_exhaustive()
	}
}

/// Local codec registration for the library-owned CacheEntry state.
pub(crate) struct CacheEntryComponentFields {
	/// Registers only readers, or both readers and writers, without choosing installed policy.
	pub(crate) code_access: NativePrimitiveCodeAccess,
}
/// Actual library-owned intrinsic codecs; construction grants no stored-state access.
pub(crate) struct CacheEntryComponent {
	schema: StateSchema<NativeCacheEntryState, CacheEntryCodecError>,
}
impl CacheEntryComponent {
	pub(crate) fn component_identifier() -> &'static str {
		"paranoid.cache-entry"
	}
	/// Registers the actual intrinsic codecs with the explicitly selected local access.
	pub(crate) fn new(fields: CacheEntryComponentFields) -> Self {
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
			.expect("CacheValue declares its actual initial library envelope codec"),
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
	pub(crate) fn schema(&self) -> &StateSchema<NativeCacheEntryState, CacheEntryCodecError> {
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
		.expect("CacheValue declares its actual library envelope functions")
	}
	pub(crate) fn bind_reader<'a>(
		&self,
		fields: CacheEntryReaderFields<'a>,
	) -> Result<CacheEntryReader<'a>, CacheEntryStateError> {
		if !fields.reader.matches_schema_declaration(&self.schema) {
			return Err(CacheEntryStateError::DifferentDeclaration);
		}
		Ok(CacheEntryReader {
			reader: fields.reader,
			expected: fields.expected,
			namespace: fields.namespace,
		})
	}
	pub(crate) fn bind_writer<'a>(
		&self,
		fields: CacheEntryWriterFields<'a>,
	) -> Result<CacheEntryWriter<'a>, CacheEntryStateError> {
		if !fields.writer.matches_schema_declaration(&self.schema) {
			return Err(CacheEntryStateError::DifferentDeclaration);
		}
		if fields.writer.policy().writer().envelope != StateStorageEnvelope::NativeJson {
			return Err(CacheEntryStateError::NativeWriterRequired);
		}
		Ok(CacheEntryWriter {
			writer: fields.writer,
			expected: fields.expected,
			namespace: fields.namespace,
		})
	}
}
pub(crate) struct CacheEntryReaderFields<'a> {
	pub(crate) reader: BoundSchemaReader<'a, NativeCacheEntryState, CacheEntryCodecError>,
	pub(crate) expected: &'a CacheEntryIdentity,
	pub(crate) namespace: &'a RecordNamespace,
}
pub(crate) struct CacheEntryReader<'a> {
	reader: BoundSchemaReader<'a, NativeCacheEntryState, CacheEntryCodecError>,
	expected: &'a CacheEntryIdentity,
	namespace: &'a RecordNamespace,
}
impl CacheEntryReader<'_> {
	pub(crate) fn decode_record(
		&self,
		identity: &RecordIdentity,
		record: &StoredRecord,
	) -> Result<NativeCacheEntryState, CacheEntryStateError> {
		require_identity(self.expected, self.namespace, identity)?;
		let state = decode_stored_record(&self.reader, record)?;
		require_resource(self.expected, &state)?;
		Ok(state)
	}
}

pub(in crate::fleet) struct CacheEntryDiscoveredReadFields<'a> {
	pub(in crate::fleet) namespace: &'a RecordNamespace,
	pub(in crate::fleet) identity: &'a RecordIdentity,
	pub(in crate::fleet) record: &'a StoredRecord,
}
/// Resolves the actual body identity once, then checks the complete existing framed lookup.
pub(in crate::fleet) fn read_discovered_record(
	reader: &BoundSchemaReader<'_, NativeCacheEntryState, CacheEntryCodecError>,
	fields: CacheEntryDiscoveredReadFields<'_>,
) -> Result<NativeCacheEntryState, CacheEntryStateError> {
	if fields.identity.namespace() != fields.namespace {
		return Err(CacheEntryStateError::RecordIdentityMismatch {
			expected: RecordIdentity::new(fields.namespace.clone(), fields.identity.key().clone()),
			actual: fields.identity.clone(),
		});
	}
	let state = decode_stored_record(reader, fields.record)?;
	require_identity(state.identity(), fields.namespace, fields.identity)?;
	Ok(state)
}
fn decode_stored_record(
	reader: &BoundSchemaReader<'_, NativeCacheEntryState, CacheEntryCodecError>,
	record: &StoredRecord,
) -> Result<NativeCacheEntryState, CacheEntryStateError> {
	if !record.index_fields().is_empty() {
		return Err(CacheEntryStateError::UnexpectedIndexFields);
	}
	let (_, state) = reader.decode_document(record.body())?;
	Ok(state)
}
pub(crate) struct CacheEntryWriterFields<'a> {
	pub(crate) writer: BoundStateSchema<'a, NativeCacheEntryState, CacheEntryCodecError>,
	pub(crate) expected: &'a CacheEntryIdentity,
	pub(crate) namespace: &'a RecordNamespace,
}
pub(crate) struct CacheEntryWriter<'a> {
	writer: BoundStateSchema<'a, NativeCacheEntryState, CacheEntryCodecError>,
	expected: &'a CacheEntryIdentity,
	namespace: &'a RecordNamespace,
}
impl CacheEntryWriter<'_> {
	/// Metadata passes through unchanged; the library wrapper does not refresh or remove TTL.
	pub(crate) fn encode_record(
		&self,
		identity: &RecordIdentity,
		state: &NativeCacheEntryState,
		metadata: RecordMetadata,
	) -> Result<EncodedRecord, CacheEntryStateError> {
		require_identity(self.expected, self.namespace, identity)?;
		require_resource(self.expected, state)?;
		let body = self.writer.encode(state)?;
		EncodedRecord::derive(body, metadata, |body, _metadata| {
			let (_, actual) = self.writer.decode_document(body)?;
			require_resource(self.expected, &actual)?;
			if actual.epoch != state.epoch || !actual.payload.matches_payload(&state.payload) {
				return Err(CacheEntryStateError::EncodedStateMismatch);
			}
			Ok(IndexFields::new())
		})
	}
}
fn require_identity(
	expected: &CacheEntryIdentity,
	namespace: &RecordNamespace,
	actual: &RecordIdentity,
) -> Result<(), CacheEntryStateError> {
	let identity = RecordIdentity::new(
		namespace.clone(),
		NativeFleetPayloadIdentity::CacheEntry(expected).record_key(),
	);
	if &identity != actual {
		return Err(CacheEntryStateError::RecordIdentityMismatch {
			expected: identity,
			actual: actual.clone(),
		});
	}
	Ok(())
}
fn require_resource(
	expected: &CacheEntryIdentity,
	state: &NativeCacheEntryState,
) -> Result<(), CacheEntryStateError> {
	if expected != state.identity() {
		return Err(CacheEntryStateError::ResourceMismatch);
	}
	Ok(())
}

use primitives_rs::serialization::json::JsonDecodeFailureSummary;

#[derive(thiserror::Error)]
pub(crate) enum CacheEntryCodecError {
	#[error("CacheValue JSON is invalid: {summary}")]
	Json { summary: JsonDecodeFailureSummary },
	#[error("CacheValue JSON exceeds the maximum nesting depth")]
	MaxNestingDepth,
	#[error("cache value cache name is invalid")]
	CacheKey(#[source] crate::fleet::KeyError),
	#[error("cache value suffix tuple is invalid")]
	KeyParts(#[source] crate::kv::KeyError),
	#[error("CacheValue integer is invalid")]
	Integer(#[source] crate::fleet::scalar_codec::ValueError),
	#[error("CacheValue payload transport is invalid")]
	Payload(#[from] FleetPayloadError),
}
impl std::fmt::Debug for CacheEntryCodecError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		std::fmt::Display::fmt(self, f)
	}
}
#[derive(thiserror::Error)]
pub(crate) enum CacheEntryStateError {
	#[error("CacheValue binding does not use its schema declaration")]
	DifferentDeclaration,
	#[error("CacheValue writer must select its native JSON representation")]
	NativeWriterRequired,
	#[error("CacheValue lookup differs from the complete framed resource identity")]
	RecordIdentityMismatch {
		expected: RecordIdentity,
		actual: RecordIdentity,
	},
	#[error("CacheValue body belongs to another complete resource identity")]
	ResourceMismatch,
	#[error("CacheValue has stored fields not derived by this family")]
	UnexpectedIndexFields,
	#[error("CacheValue encoded state differs from its prepared semantic value")]
	EncodedStateMismatch,
	#[error("CacheValue body is not readable under its selected policy")]
	Read(#[from] ContractStateReadError<CacheEntryCodecError>),
	#[error("CacheValue body cannot encode its selected writer")]
	Encode(#[from] StateSchemaError<CacheEntryCodecError>),
}
impl std::fmt::Debug for CacheEntryStateError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		std::fmt::Display::fmt(self, f)
	}
}
