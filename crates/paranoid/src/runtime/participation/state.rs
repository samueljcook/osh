//! Private runtime registration state and capabilities derived from code declarations.
//!
//! These deterministic operations require installation control and participant mutation
//! authority from their caller. A live registration authenticates a runtime; it does not
//! establish that an external task has stopped, admit a transaction, or authorize promotion.

use std::fmt;
use std::num::NonZeroI64;
use std::sync::Arc;

use primitives_rs::id::time_sortable::SortableId;
use primitives_rs::secret_bytes::SecretToken32;
use primitives_rs::serialization::json::JsonDecodeFailureSummary;

use crate::installation::contract::{
	BoundStateSchema, ComponentDeclarationFields, ComponentPolicyFields, ComponentSettings,
	ComponentSettingsDeclaration, ComponentSettingsRequirement, ContractStateReadError,
	RepresentationPolicy, SchemaPolicy, SemanticContractDef, StateContractError,
	StateStorageEnvelope,
};
use crate::queue::{QueueAuxiliarySourceBridge, QueueSourceBridge};
use crate::records::state_schema::{
	StateRepresentationFormat, StateSchema, StateSchemaError, StateVersion,
};
use crate::records::{
	EncodedRecord, IndexFields, IndexScalar, PointRecordRead, RecordIdentity, RecordMetadata,
	StateTimestampMicros, StoredRecord,
};
use crate::{
	fleet::lease::LeaseSourceBridge,
	installation::control_state::InstallationControlComponent,
	scope::{ScopeError, wire::ScopeWireError},
};

pub(crate) mod catalog;
mod wire;

pub(crate) use catalog::{ParticipantCapabilities, ParticipantComponentSupport};
pub(crate) use wire::ComponentSupportWire;
use wire::{decode_state, encode_state};

const PARTICIPANT_SCHEMA: &str = "paranoid.runtime-participant";
const PARTICIPANT_REPRESENTATION_VERSION: u32 = 1;
const PARTICIPANT_TOKEN_LEN: usize = 32;
const EXPIRY_INDEX: &str = "participation_expires_at";
const PARTICIPATION_COMPONENT: &str = "paranoid.runtime-participation";
const PARTICIPATION_SEMANTIC_CONTRACT: &str = "paranoid.runtime-participation.v1";

/// Secret authority for one registration, distinct from its public registration identifier.
#[derive(Clone, Debug)]
struct ParticipantToken(SecretToken32);

impl PartialEq for ParticipantToken {
	fn eq(&self, other: &Self) -> bool {
		use subtle::ConstantTimeEq;
		self.0.ct_eq(&other.0).into()
	}
}

impl Eq for ParticipantToken {}

/// Generated registration material. It becomes usable only after registration commits.
#[derive(Clone)]
pub(crate) struct ParticipantAuthority {
	installation_id: SortableId,
	registration_id: SortableId,
	token: ParticipantToken,
}

impl ParticipantAuthority {
	pub(crate) fn generate(
		installation_id: SortableId,
	) -> Result<Self, primitives_rs::random::secure_bytes::RandomError> {
		let registration_id = SortableId::generate()?;
		let token = ParticipantToken(SecretToken32::generate()?);
		Ok(Self {
			installation_id,
			registration_id,
			token,
		})
	}
	pub(crate) fn registration_id(&self) -> SortableId {
		self.registration_id
	}
	pub(crate) fn installation_id(&self) -> SortableId {
		self.installation_id
	}
}

impl fmt::Debug for ParticipantAuthority {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("ParticipantAuthority { .. }")
	}
}

/// Positive configured participation duration in database microseconds.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ParticipationDuration(NonZeroI64);

impl ParticipationDuration {
	pub(crate) fn from_micros(micros: i64) -> Result<Self, ParticipantStateError> {
		if micros <= 0 {
			return Err(ParticipantStateError::NonPositiveDuration);
		}
		Ok(Self(
			NonZeroI64::new(micros).expect("positive duration is nonzero"),
		))
	}
}

/// Advertised support, active dependencies and one registration's private live authority.
pub(crate) struct ParticipantState {
	installation_id: SortableId,
	registration_id: SortableId,
	token: ParticipantToken,
	expires_at: StateTimestampMicros,
	capabilities: ParticipantCapabilities,
}

impl ParticipantState {
	pub(crate) fn register(
		authority: &ParticipantAuthority,
		capabilities: ParticipantCapabilities,
		duration: ParticipationDuration,
		observed_at: StateTimestampMicros,
	) -> Result<Self, ParticipantStateError> {
		(PARTICIPATION_SEMANTICS.require_capabilities_fn)(&capabilities)?;
		Self::register_state(authority, capabilities, duration, observed_at)
	}

	fn register_state(
		authority: &ParticipantAuthority,
		capabilities: ParticipantCapabilities,
		duration: ParticipationDuration,
		observed_at: StateTimestampMicros,
	) -> Result<Self, ParticipantStateError> {
		Ok(Self {
			installation_id: authority.installation_id,
			registration_id: authority.registration_id,
			token: authority.token.clone(),
			expires_at: expiry(observed_at, duration)?,
			capabilities,
		})
	}
	pub(crate) fn capabilities(&self) -> &ParticipantCapabilities {
		&self.capabilities
	}
	pub(crate) fn installation_id(&self) -> SortableId {
		self.installation_id
	}
	pub(crate) fn expires_at(&self) -> StateTimestampMicros {
		self.expires_at
	}
	pub(crate) fn is_live_at(&self, observed_at: StateTimestampMicros) -> bool {
		observed_at < self.expires_at
	}
	pub(crate) fn authenticate(
		&self,
		authority: &ParticipantAuthority,
		observed_at: StateTimestampMicros,
	) -> Result<(), ParticipantStateError> {
		self.authenticate_private_authority(authority)?;
		if !self.is_live_at(observed_at) {
			return Err(ParticipantStateError::AuthorityUnavailable);
		}
		Ok(())
	}
	/// Authorizes removal of this exact registration even after expiry, without extending
	/// authority.
	fn authenticate_private_authority(
		&self,
		authority: &ParticipantAuthority,
	) -> Result<(), ParticipantStateError> {
		if self.installation_id != authority.installation_id
			|| self.registration_id != authority.registration_id
			|| self.token != authority.token
		{
			return Err(ParticipantStateError::AuthorityUnavailable);
		}
		Ok(())
	}
	/// Renews matching live authority. Expired registrations cannot be revived.
	/// The token remains stable because the scoped runtime retains it across admitted transactions.
	pub(crate) fn renew(
		&mut self,
		authority: &ParticipantAuthority,
		duration: ParticipationDuration,
		observed_at: StateTimestampMicros,
	) -> Result<(), ParticipantStateError> {
		self.authenticate(authority, observed_at)?;
		(PARTICIPATION_SEMANTICS.require_capabilities_fn)(&self.capabilities)?;
		self.update_expiry(duration, observed_at)
	}

	fn update_expiry(
		&mut self,
		duration: ParticipationDuration,
		observed_at: StateTimestampMicros,
	) -> Result<(), ParticipantStateError> {
		let expires_at = expiry(observed_at, duration)?;
		self.expires_at = expires_at;
		Ok(())
	}

	fn withdraw_retired_source_uses(
		&mut self,
		authority: &ParticipantAuthority,
		observed_at: StateTimestampMicros,
	) -> Result<(), ParticipantStateError> {
		self.authenticate(authority, observed_at)?;
		withdraw_retired_source_capabilities(&mut self.capabilities)
	}
}

/// Only exact physical-import Component references are withdrawn; schema roles remain independent.
fn withdraw_retired_source_capabilities(
	capabilities: &mut ParticipantCapabilities,
) -> Result<(), ParticipantStateError> {
	(PARTICIPATION_SEMANTICS.require_capabilities_fn)(capabilities)?;
	capabilities.active_uses.retain(|reference| match reference {
		catalog::ParticipantUseReference::Component { identifier } => {
			identifier != LeaseSourceBridge::component_identifier()
				&& identifier != QueueSourceBridge::component_identifier()
				&& identifier != QueueAuxiliarySourceBridge::component_identifier()
				&& identifier != crate::fleet::v10::scalar_source_bridge::ScalarFleetSourceBridge::component_identifier()
		}
		catalog::ParticipantUseReference::Document { .. }
		| catalog::ParticipantUseReference::Primitive { .. }
		| catalog::ParticipantUseReference::Payload { .. }
		| catalog::ParticipantUseReference::Raw { .. }
		| catalog::ParticipantUseReference::DocumentQuery { .. } => true,
	});
	Ok(())
}

/// Fixed private-authority transition for phase-obsolete source importer references.
type SourceUseWithdrawalFn = fn(
	&mut ParticipantState,
	&ParticipantAuthority,
	StateTimestampMicros,
) -> Result<(), ParticipantStateError>;

type SourceCapabilitiesWithdrawalFn =
	fn(&mut ParticipantCapabilities) -> Result<(), ParticipantStateError>;

/// Actual deterministic participation transitions for the selected semantic contract.
pub(crate) struct ParticipationSemantics {
	pub(crate) require_capabilities_fn:
		fn(&ParticipantCapabilities) -> Result<(), ParticipantStateError>,
	pub(crate) register_fn: fn(
		&ParticipantAuthority,
		ParticipantCapabilities,
		ParticipationDuration,
		StateTimestampMicros,
	) -> Result<ParticipantState, ParticipantStateError>,
	pub(crate) renew_fn: fn(
		&mut ParticipantState,
		&ParticipantAuthority,
		ParticipationDuration,
		StateTimestampMicros,
	) -> Result<(), ParticipantStateError>,
	pub(crate) authorize_removal_fn:
		fn(&ParticipantState, &ParticipantAuthority) -> Result<(), ParticipantStateError>,
	source_use_withdrawal_fn: SourceUseWithdrawalFn,
	source_capabilities_withdrawal_fn: SourceCapabilitiesWithdrawalFn,
}

impl ParticipationSemantics {
	/// The selected catalog semantic and its actual native writer withdraw exact obsolete
	/// source importer uses after private authentication.
	/// The owning backend must establish DataRetired through its actual pinned route first.
	pub(crate) fn reconcile_retired_source_uses(
		&self,
		state: &mut ParticipantState,
		authority: &ParticipantAuthority,
		writer: &BoundStateSchema<'_, ParticipantState, ParticipantStateError>,
		observed_at: StateTimestampMicros,
	) -> Result<(), ParticipantStateError> {
		if writer.policy().writer().envelope != StateStorageEnvelope::NativeJson {
			return Err(ParticipantStateError::NativeWriterRequired);
		}
		(self.source_use_withdrawal_fn)(state, authority, observed_at)
	}

	/// Normalizes only the four fixed obsolete adapter uses before initial compatibility checks.
	/// The backend must supply its freshly pinned NativeImported route before this pure step.
	pub(crate) fn normalize_retired_source_capabilities(
		&self,
		capabilities: &mut ParticipantCapabilities,
		writer: &BoundStateSchema<'_, ParticipantState, ParticipantStateError>,
	) -> Result<(), ParticipantStateError> {
		if writer.policy().writer().envelope != StateStorageEnvelope::NativeJson {
			return Err(ParticipantStateError::NativeWriterRequired);
		}
		(self.source_capabilities_withdrawal_fn)(capabilities)
	}
}

const PARTICIPATION_SEMANTICS: ParticipationSemantics = ParticipationSemantics {
	require_capabilities_fn: ParticipantCapabilities::require_active_administration,
	register_fn: ParticipantState::register,
	renew_fn: ParticipantState::renew,
	authorize_removal_fn: ParticipantState::authenticate_private_authority,
	source_use_withdrawal_fn: ParticipantState::withdraw_retired_source_uses,
	source_capabilities_withdrawal_fn: withdraw_retired_source_capabilities,
};

impl fmt::Debug for ParticipantState {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("ParticipantState")
			.field("expires_at", &self.expires_at)
			.field(
				"advertised_component_count",
				&self.capabilities.advertised().components().len(),
			)
			.finish_non_exhaustive()
	}
}

/// One configured lookup identity and the installation that must own its registration.
pub(crate) struct ParticipantComponent {
	identity: RecordIdentity,
	installation_id: SortableId,
	registration_id: SortableId,
	participant_schema: Arc<ParticipantSchema>,
}

/// Actual administrative codec declaration independent of a registration lookup.
/// Controllers bind control policy before reading any registration page.
pub(crate) struct ParticipantSchema {
	schema: StateSchema<ParticipantState, ParticipantStateError>,
}

impl ParticipantSchema {
	pub(crate) fn new() -> Self {
		Self {
			schema: StateSchema::new(crate::records::state_schema::StateSchemaFields {
				identifier: PARTICIPANT_SCHEMA,
				versions: vec![StateVersion {
					version: PARTICIPANT_REPRESENTATION_VERSION,
					format: StateRepresentationFormat::Json,
					decoder: decode_state,
					encoder: Some(encode_state),
				}],
			})
			.expect("participant schema declares explicit positive native JSON versions"),
		}
	}

	pub(crate) fn schema(&self) -> &StateSchema<ParticipantState, ParticipantStateError> {
		&self.schema
	}

	/// The native administrative policy fields for this participant and the actual control owner.
	/// Backend adapters must append their own required schemas before complete binding.
	pub(crate) fn initial_policy_fields(
		&self,
		control: &InstallationControlComponent,
	) -> Result<ComponentPolicyFields, StateContractError> {
		let participant = RepresentationPolicy {
			version: PARTICIPANT_REPRESENTATION_VERSION,
			envelope: StateStorageEnvelope::NativeJson,
		};
		Ok(ComponentPolicyFields {
			component_identifier: PARTICIPATION_COMPONENT.to_owned(),
			semantic_contract: PARTICIPATION_SEMANTIC_CONTRACT.to_owned(),
			schemas: vec![
				SchemaPolicy::new(
					self.schema.identifier().to_owned(),
					participant,
					vec![participant],
				)?,
				control.initial_schema_policy()?,
			],
			settings: ComponentSettings::Parameterless,
		})
	}

	pub(crate) fn require_bound_writer(
		&self,
		bound: &BoundStateSchema<'_, ParticipantState, ParticipantStateError>,
	) -> Result<(), ParticipantStateError> {
		if !bound.matches_schema_declaration(self.schema()) {
			return Err(invalid(
				"participant writer belongs to another schema declaration",
			));
		}
		if bound.policy().writer().envelope != StateStorageEnvelope::NativeJson {
			return Err(ParticipantStateError::NativeWriterRequired);
		}
		Ok(())
	}

	pub(crate) fn declaration_fields<'a>(
		&'a self,
		control: &'a InstallationControlComponent,
	) -> ComponentDeclarationFields<'a, ParticipationSemantics> {
		ComponentDeclarationFields {
			identifier: PARTICIPATION_COMPONENT,
			fixed_schemas: vec![self.schema.capabilities(), control.schema().capabilities()],
			settings: ComponentSettingsDeclaration::Parameterless,
			semantics: vec![SemanticContractDef {
				identifier: PARTICIPATION_SEMANTIC_CONTRACT,
				transitions: &PARTICIPATION_SEMANTICS,
				settings: ComponentSettingsRequirement::Parameterless,
			}],
		}
	}
}

pub(crate) struct ParticipantComponentFields {
	pub(crate) identity: RecordIdentity,
	pub(crate) installation_id: SortableId,
	pub(crate) registration_id: SortableId,
}

impl ParticipantComponent {
	pub(crate) fn new(fields: ParticipantComponentFields, schema: Arc<ParticipantSchema>) -> Self {
		Self {
			identity: fields.identity,
			installation_id: fields.installation_id,
			registration_id: fields.registration_id,
			participant_schema: schema,
		}
	}
	/// Parses the actual lookup key independently of its shared code registry.
	pub(crate) fn from_record_identity_with_schema(
		installation_id: SortableId,
		identity: RecordIdentity,
		schema: Arc<ParticipantSchema>,
	) -> Result<Self, ParticipantStateError> {
		let registration_id = SortableId::from_bytes(identity.key().as_bytes())
			.map_err(|_| invalid("participant record key is not a sortable identifier"))?;
		Ok(Self::new(
			ParticipantComponentFields {
				identity,
				installation_id,
				registration_id,
			},
			schema,
		))
	}
	pub(crate) fn participant_schema(&self) -> &Arc<ParticipantSchema> {
		&self.participant_schema
	}

	pub(crate) fn identity(&self) -> &RecordIdentity {
		&self.identity
	}
	pub(crate) fn schema(&self) -> &StateSchema<ParticipantState, ParticipantStateError> {
		&self.participant_schema.schema
	}
	pub(crate) fn require_bound_writer(
		&self,
		bound: &BoundStateSchema<'_, ParticipantState, ParticipantStateError>,
	) -> Result<(), ParticipantStateError> {
		self.participant_schema.require_bound_writer(bound)
	}

	/// Applies the administrative policy's retained reader after physical and lookup validation.
	pub(crate) fn decode_bound_record(
		&self,
		lookup_identity: &RecordIdentity,
		snapshot: &PointRecordRead,
		bound_schema: &BoundStateSchema<'_, ParticipantState, ParticipantStateError>,
	) -> Result<Option<ParticipantState>, ContractStateReadError<ParticipantStateError>> {
		self.require_lookup_identity(lookup_identity)
			.map_err(participant_read_error)?;
		let Some(record) = snapshot.record() else {
			return Ok(None);
		};
		self.decode_bound_stored_record(lookup_identity, record, bound_schema)
			.map(Some)
	}

	/// Borrows a page entry's body without copying private registration material.
	pub(crate) fn decode_bound_stored_record(
		&self,
		lookup_identity: &RecordIdentity,
		record: &StoredRecord,
		bound_schema: &BoundStateSchema<'_, ParticipantState, ParticipantStateError>,
	) -> Result<ParticipantState, ContractStateReadError<ParticipantStateError>> {
		self.require_lookup_identity(lookup_identity)
			.map_err(participant_read_error)?;
		if !bound_schema.matches_schema_declaration(self.schema()) {
			return Err(participant_read_error(invalid(
				"participant reader belongs to a different codec declaration",
			)));
		}
		require_metadata(record.metadata()).map_err(participant_read_error)?;
		let (_, state) = bound_schema.decode_document(record.body())?;
		self.require_identity(&state)
			.map_err(participant_read_error)?;
		Ok(state)
	}
	pub(crate) fn encode_record(
		&self,
		state: &ParticipantState,
		writer_version: u32,
		updated_at: StateTimestampMicros,
	) -> Result<EncodedRecord, StateSchemaError<ParticipantStateError>> {
		self.require_identity(state)
			.map_err(StateSchemaError::Codec)?;
		let body = self.schema().encode_document(writer_version, state)?;
		EncodedRecord::derive(
			body,
			RecordMetadata {
				expires_at: None,
				updated_at,
			},
			|body, metadata| {
				require_metadata(metadata).map_err(StateSchemaError::Codec)?;
				let (_, encoded) = self.schema().decode_document(body)?;
				let mut indexes = IndexFields::new();
				indexes.insert(
					EXPIRY_INDEX.to_owned(),
					IndexScalar::Timestamp(encoded.expires_at),
				);
				Ok(indexes)
			},
		)
	}
	fn require_lookup_identity(
		&self,
		lookup_identity: &RecordIdentity,
	) -> Result<(), ParticipantStateError> {
		if lookup_identity != &self.identity {
			return Err(ParticipantStateError::WrongRecordIdentity);
		}
		Ok(())
	}
	fn require_identity(&self, state: &ParticipantState) -> Result<(), ParticipantStateError> {
		if state.installation_id != self.installation_id
			|| state.registration_id != self.registration_id
		{
			return Err(ParticipantStateError::WrongStoredIdentity);
		}
		Ok(())
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ParticipantStateError {
	#[error("native participant state requires its native JSON writer")]
	NativeWriterRequired,
	#[error("participant does not support the selected component settings meaning")]
	UnsupportedSettings,
	#[error("participant settings representation capability is unsupported")]
	SettingsCapability(#[source] Box<ParticipantStateError>),
	#[error("mandatory administration is derived by the registration backend")]
	ReservedAdministrativeUse,
	#[error("participant JSON is invalid: {0}")]
	Json(JsonDecodeFailureSummary),
	#[error("participant state is invalid: {reason}")]
	Invalid { reason: &'static str },
	#[error("participation duration must be positive")]
	NonPositiveDuration,
	#[error("participation expiration exceeds the signed timestamp domain")]
	ExpirationOverflow,
	#[error("participant registration has no matching live private authority")]
	AuthorityUnavailable,
	#[error("participant lookup uses another record identity")]
	WrongRecordIdentity,
	#[error("participant state belongs to another installation or registration")]
	WrongStoredIdentity,
	#[error("participant component {component_index} repeats an identifier")]
	DuplicateComponent { component_index: usize },
	#[error("participant component {component_index} has no installed policy")]
	ComponentNotInstalled { component_index: usize },
	#[error("participant has no mandatory administrative capability declaration")]
	AdministrativeSupportAbsent,
	#[error("installation has no mandatory administrative component policy")]
	AdministrativePolicyAbsent,
	#[error("participant support belongs to another component")]
	WrongComponent,
	#[error("participant does not support the semantic contract")]
	UnsupportedSemanticContract,
	#[error("participant does not support schema policy {schema_index}")]
	UnsupportedSchema { schema_index: usize },
	#[error("participant schema {schema_index} has no target policy")]
	MissingPolicySchema { schema_index: usize },
	#[error("participant does not support schema policy {schema_index} writer {version}")]
	UnsupportedWriter { schema_index: usize, version: u32 },
	#[error("participant does not support schema policy {schema_index} reader {version}")]
	UnsupportedReader { schema_index: usize, version: u32 },
	#[error("participant active reference {use_index} repeats an identity")]
	DuplicateActiveUse { use_index: usize },
	#[error("participant active reference {use_index} has no matching advertised declaration")]
	ActiveUseNotAdvertised { use_index: usize },
	#[error("participant scoped role was rejected")]
	Scope(#[from] ScopeError),
	#[error("participant payload scope policy was rejected")]
	PayloadScope {
		scope: crate::scope::ScopeIdentity,
		#[source]
		source: Box<ScopeError>,
	},
	#[error("participant scoped role wire was rejected")]
	ScopeWire(#[from] ScopeWireError),
}

fn require_metadata(metadata: RecordMetadata) -> Result<(), ParticipantStateError> {
	if metadata.expires_at.is_some() {
		return Err(invalid(
			"participant expiration must be embedded state, not an outer TTL",
		));
	}
	Ok(())
}
fn participant_read_error(
	error: ParticipantStateError,
) -> ContractStateReadError<ParticipantStateError> {
	ContractStateReadError::Representation(StateSchemaError::Codec(error))
}
fn expiry(
	observed_at: StateTimestampMicros,
	duration: ParticipationDuration,
) -> Result<StateTimestampMicros, ParticipantStateError> {
	observed_at
		.as_i64()
		.checked_add(duration.0.get())
		.map(StateTimestampMicros::new)
		.ok_or(ParticipantStateError::ExpirationOverflow)
}
fn invalid(reason: &'static str) -> ParticipantStateError {
	ParticipantStateError::Invalid { reason }
}
fn json_error(error: serde_json::Error) -> ParticipantStateError {
	ParticipantStateError::Json(JsonDecodeFailureSummary::from(&error))
}
