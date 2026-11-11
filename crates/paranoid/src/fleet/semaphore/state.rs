//! Pure native Semaphore state and private transitions, without admission or persistence.
//!
//! The immortal resource retains settings and all possible slot material. Tentative grants
//! require the owning executor's known commit before delivery. Source conversion grants none.

use crate::fleet::primitive_scope::NativePrimitiveCodeAccess;
use crate::installation::contract::role_binding::BoundSchemaReader;
use crate::installation::contract::{
	BoundComponentContract, BoundStateSchema, ComponentDeclaration, ComponentDeclarationFields,
	ComponentPolicy, ComponentPolicyFields, ComponentSettings, ComponentSettingsDeclaration,
	ComponentSettingsRequirement, ContractStateReadError, RepresentationPolicy, SchemaPolicy,
	SemanticContractDef, StateContractError, StateStorageEnvelope,
};
use crate::records::state_schema::{
	StateRepresentationFormat, StateSchema, StateSchemaError, StateVersion,
};
use crate::records::{
	EncodedRecord, IndexFields, RecordMetadata, StateTimestampMicros, StoredRecord,
};
use primitives_rs::serialization::json::JsonDecodeFailureSummary;

use crate::fleet::{
	AcquisitionCapability, HolderId, MAX_SLOT_COUNT, RandomnessError, SemaphoreClaim, SemaphoreKey,
	SemaphoreStatus,
};

mod settings;
mod undelivered;
use undelivered::SemaphoreReleaseAuthorization;
pub(crate) use undelivered::{
	SemaphoreUndeliveredReleaseInput, SemaphoreUndeliveredTarget, TentativeSemaphoreGrant,
};
pub(super) mod wire;

pub use settings::{SemaphoreSettings, SemaphoreSettingsError, SemaphoreSettingsFields};

const COMPONENT: &str = "paranoid.semaphore";
const SEMANTICS: &str = "paranoid.semaphore.v1";
const NATIVE_REPRESENTATION_VERSION: u32 = 1;
const SLOT_COUNT: usize = MAX_SLOT_COUNT as usize;

pub(crate) struct NativeSemaphoreStateFields {
	pub(crate) key: SemaphoreKey,
	pub(crate) settings: SemaphoreSettings,
}

/// Exact possible slots, including expired material and occupancy above configured capacity.
pub(crate) struct NativeSemaphoreState {
	key: SemaphoreKey,
	settings: SemaphoreSettings,
	slots: [SemaphoreSlotState; SLOT_COUNT],
}

enum SemaphoreSlotState {
	Absent,
	NativePrivate(HeldSemaphoreSlot),
}

struct HeldSemaphoreSlot {
	holder: HolderId,
	capability: AcquisitionCapability,
	metadata: SlotMetadata,
}

#[derive(Clone, Copy)]
struct SlotMetadata {
	expires_at: StateTimestampMicros,
	updated_at: StateTimestampMicros,
}

impl SemaphoreSlotState {
	fn occupied_at(&self, observed_at: StateTimestampMicros) -> bool {
		match self {
			Self::NativePrivate(held) => observed_at < held.metadata.expires_at,
			Self::Absent => false,
		}
	}
}

impl NativeSemaphoreState {
	pub(crate) fn new(fields: NativeSemaphoreStateFields) -> Self {
		Self {
			key: fields.key,
			settings: fields.settings,
			slots: std::array::from_fn(|_| SemaphoreSlotState::Absent),
		}
	}
	pub(crate) fn key(&self) -> &SemaphoreKey {
		&self.key
	}
	pub(crate) fn settings(&self) -> &SemaphoreSettings {
		&self.settings
	}
	fn live_count(&self, observed_at: StateTimestampMicros) -> u64 {
		self.slots
			.iter()
			.filter(|slot| slot.occupied_at(observed_at))
			.count() as u64
	}
}

impl std::fmt::Debug for NativeSemaphoreState {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("NativeSemaphoreState")
			.finish_non_exhaustive()
	}
}

/// Local codec registration for the library-owned Semaphore state.
pub struct SemaphoreComponentFields {
	/// Registers only readers, or both readers and writers, without choosing installed policy.
	pub code_access: NativePrimitiveCodeAccess,
}

/// Actual library-owned intrinsic codecs; construction grants no stored-state access.
pub struct SemaphoreComponent {
	schema: StateSchema<NativeSemaphoreState, SemaphoreCodecError>,
}

impl SemaphoreComponent {
	/// Registers the actual intrinsic codecs with the explicitly selected local access.
	pub fn new(fields: SemaphoreComponentFields) -> Self {
		Self {
			schema: StateSchema::new(crate::records::state_schema::StateSchemaFields {
				identifier: COMPONENT,
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
			.expect("Semaphore declares its native JSON codecs"),
		}
	}
	pub(crate) fn component_identifier() -> &'static str {
		COMPONENT
	}
	/// Selects this actual owner's initial native writer and retained reader without resource
	/// authority.
	pub(crate) fn initial_policy(&self) -> Result<ComponentPolicy, SemaphoreStateError> {
		let writer = RepresentationPolicy {
			version: NATIVE_REPRESENTATION_VERSION,
			envelope: StateStorageEnvelope::NativeJson,
		};
		let policy = ComponentPolicy::new(ComponentPolicyFields {
			component_identifier: COMPONENT.to_owned(),
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
		self.require_bound_native_writer(&bound)?;
		Ok(policy)
	}
	pub(crate) fn schema(&self) -> &StateSchema<NativeSemaphoreState, SemaphoreCodecError> {
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
		.expect("Semaphore declares actual code and per-resource settings ownership")
	}
	fn bind_native_schema<'a>(
		&'a self,
		contract: &'a BoundComponentContract<'_, '_, Self>,
	) -> Result<BoundStateSchema<'a, NativeSemaphoreState, SemaphoreCodecError>, SemaphoreStateError>
	{
		if !std::ptr::eq(self, contract.transitions())
			|| contract.policy().component_identifier() != COMPONENT
			|| contract.policy().semantic_contract() != SEMANTICS
		{
			return Err(SemaphoreStateError::TransitionInstanceMismatch);
		}
		let schema = contract.bind_schema(&self.schema)?;
		if schema.policy().writer().envelope != StateStorageEnvelope::NativeJson {
			return Err(SemaphoreStateError::NativeWriterRequired);
		}
		Ok(schema)
	}
	/// Checks actual native writer code without constructing a resource or settings.
	pub(crate) fn require_bound_native_writer(
		&self,
		contract: &BoundComponentContract<'_, '_, Self>,
	) -> Result<(), SemaphoreStateError> {
		self.bind_native_schema(contract).map(|_| ())
	}
	pub(crate) fn bind_schema<'a>(
		&'a self,
		fields: SemaphoreBindingFields<'a, '_, '_>,
	) -> Result<SemaphoreSchemaBinding<'a>, SemaphoreStateError> {
		Ok(SemaphoreSchemaBinding {
			schema: self.bind_native_schema(fields.contract)?,
			key: fields.key,
			settings: fields.settings,
		})
	}
}

pub(crate) struct SemaphoreBindingFields<'a, 'c, 'd> {
	pub(crate) contract: &'a BoundComponentContract<'c, 'd, SemaphoreComponent>,
	pub(crate) key: &'a SemaphoreKey,
	pub(crate) settings: &'a SemaphoreSettings,
}

pub(crate) struct SemaphoreSchemaBinding<'a> {
	schema: BoundStateSchema<'a, NativeSemaphoreState, SemaphoreCodecError>,
	key: &'a SemaphoreKey,
	settings: &'a SemaphoreSettings,
}

pub(crate) struct SemaphoreAcquireInput {
	pub(crate) holder: HolderId,
	pub(crate) observed_at: StateTimestampMicros,
}

pub(crate) struct SemaphoreReleaseInput<'a> {
	pub(crate) claim: &'a SemaphoreClaim,
	pub(crate) observed_at: StateTimestampMicros,
}

/// Private tentative acquisition; neither point persistence nor parsing confirms delivery.
pub(crate) struct SemaphoreAcquisition {
	claim: SemaphoreClaim,
}

impl SemaphoreAcquisition {
	/// Retain the actual target before persistence; decomposition grants no delivery proof.
	pub(crate) fn into_parts(self) -> (TentativeSemaphoreGrant, SemaphoreUndeliveredTarget) {
		undelivered::decompose(self)
	}
}

impl std::fmt::Debug for SemaphoreAcquisition {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("SemaphoreAcquisition")
			.finish_non_exhaustive()
	}
}

impl SemaphoreSchemaBinding<'_> {
	fn require_state(&self, state: &NativeSemaphoreState) -> Result<(), SemaphoreStateError> {
		require_resource(state, self.key, self.settings)
	}
	pub(crate) fn decode_record(
		&self,
		record: &StoredRecord,
	) -> Result<NativeSemaphoreState, SemaphoreStateError> {
		decode_resource(record, self.key, self.settings, |body| {
			self.schema.decode_document(body)
		})
	}
	pub(crate) fn encode_record(
		&self,
		state: &NativeSemaphoreState,
		metadata: RecordMetadata,
	) -> Result<EncodedRecord, SemaphoreStateError> {
		require_permanent(metadata)?;
		self.require_state(state)?;
		let body = self.schema.encode(state)?;
		EncodedRecord::derive(body, metadata, |body, actual_metadata| {
			require_permanent(actual_metadata)?;
			let (_, exact) = self.schema.decode_document(body)?;
			self.require_state(&exact)?;
			Ok(IndexFields::new())
		})
	}
	/// Refuses capacity exhaustion before RNG; every fallible preparation precedes mutation.
	pub(crate) fn acquire(
		&self,
		state: &mut NativeSemaphoreState,
		input: SemaphoreAcquireInput,
	) -> Result<Option<SemaphoreAcquisition>, SemaphoreStateError> {
		self.require_state(state)?;
		if state.live_count(input.observed_at) >= u64::from(self.settings.capacity()) {
			return Ok(None);
		}
		let candidate = state.slots[..usize::from(self.settings.capacity())]
			.iter()
			.position(|slot| !slot.occupied_at(input.observed_at));
		let Some(index) = candidate else {
			return Ok(None);
		};
		let expires_at = StateTimestampMicros::new(
			input
				.observed_at
				.as_i64()
				.checked_add(self.settings.max_hold_micros())
				.ok_or(SemaphoreStateError::ExpirationOverflow)?,
		);
		let capability =
			AcquisitionCapability::generate().map_err(SemaphoreStateError::Randomness)?;
		let claim = SemaphoreClaim {
			semaphore_key: self.key.clone(),
			slot_suffix: (index + 1).to_string(),
			holder_id: input.holder.clone(),
			acquisition_capability: capability.clone(),
		};
		state.slots[index] = SemaphoreSlotState::NativePrivate(HeldSemaphoreSlot {
			holder: input.holder,
			capability,
			metadata: SlotMetadata {
				expires_at,
				updated_at: input.observed_at,
			},
		});
		Ok(Some(SemaphoreAcquisition { claim }))
	}
	pub(crate) fn release(
		&self,
		state: &mut NativeSemaphoreState,
		input: SemaphoreReleaseInput<'_>,
	) -> Result<bool, SemaphoreStateError> {
		self.release_exact(
			state,
			SemaphoreReleaseAuthorization::Claimed(input.claim),
			input.observed_at,
		)
	}
	/// Exact cleanup predicates require originating physical ordering and fresh admission in the
	/// executor.
	pub(crate) fn release_undelivered(
		&self,
		state: &mut NativeSemaphoreState,
		input: SemaphoreUndeliveredReleaseInput<'_>,
	) -> Result<bool, SemaphoreStateError> {
		self.release_exact(
			state,
			SemaphoreReleaseAuthorization::Undelivered(input.target),
			input.observed_at,
		)
	}
	fn release_exact(
		&self,
		state: &mut NativeSemaphoreState,
		authorization: SemaphoreReleaseAuthorization<'_>,
		observed_at: StateTimestampMicros,
	) -> Result<bool, SemaphoreStateError> {
		let predicates = authorization.predicates();
		self.require_state(state)?;
		if predicates.key != self.key {
			return Err(SemaphoreStateError::ResourceMismatch);
		}
		let slot_number = predicates
			.slot_suffix
			.parse::<u16>()
			.ok()
			.filter(|slot| {
				(1..=MAX_SLOT_COUNT).contains(slot) && slot.to_string() == predicates.slot_suffix
			})
			.ok_or(SemaphoreStateError::ClaimSlotInvalid)?;
		let index = usize::from(slot_number - 1);
		let matches = match &state.slots[index] {
			SemaphoreSlotState::NativePrivate(held) => {
				observed_at < held.metadata.expires_at
					&& &held.holder == predicates.holder
					&& &held.capability == predicates.capability
			}
			SemaphoreSlotState::Absent => false,
		};
		if matches {
			state.slots[index] = SemaphoreSlotState::Absent;
		}
		Ok(matches)
	}
	/// Clears all retained activity without changing resource settings or stopping tasks.
	pub(crate) fn reset(
		&self,
		state: &mut NativeSemaphoreState,
	) -> Result<u64, SemaphoreStateError> {
		self.require_state(state)?;
		let count = state
			.slots
			.iter()
			.filter(|slot| !matches!(slot, SemaphoreSlotState::Absent))
			.count() as u64;
		state.slots = std::array::from_fn(|_| SemaphoreSlotState::Absent);
		Ok(count)
	}
}

/// Reads the same native resource without requiring a selected encoder.
pub(in crate::fleet) fn read_resource(
	reader: &BoundSchemaReader<'_, NativeSemaphoreState, SemaphoreCodecError>,
	record: &StoredRecord,
	key: &SemaphoreKey,
	settings: &SemaphoreSettings,
) -> Result<NativeSemaphoreState, SemaphoreStateError> {
	decode_resource(record, key, settings, |body| reader.decode_document(body))
}
fn decode_resource(
	record: &StoredRecord,
	key: &SemaphoreKey,
	settings: &SemaphoreSettings,
	decode_fn: impl FnOnce(
		&[u8],
	) -> Result<
		(u32, NativeSemaphoreState),
		ContractStateReadError<SemaphoreCodecError>,
	>,
) -> Result<NativeSemaphoreState, SemaphoreStateError> {
	let state = decode_native_resource(record, decode_fn)?;
	require_resource(&state, key, settings)?;
	Ok(state)
}
fn decode_native_resource(
	record: &StoredRecord,
	decode_fn: impl FnOnce(
		&[u8],
	) -> Result<
		(u32, NativeSemaphoreState),
		ContractStateReadError<SemaphoreCodecError>,
	>,
) -> Result<NativeSemaphoreState, SemaphoreStateError> {
	require_permanent(record.metadata())?;
	Ok(decode_fn(record.body())?.1)
}
pub(super) fn require_resource(
	state: &NativeSemaphoreState,
	key: &SemaphoreKey,
	settings: &SemaphoreSettings,
) -> Result<(), SemaphoreStateError> {
	if key != state.key() {
		return Err(SemaphoreStateError::ResourceMismatch);
	}
	if settings != state.settings() {
		return Err(SemaphoreStateError::SettingsMismatch);
	}
	Ok(())
}
pub(in crate::fleet) fn resource_status(
	state: &NativeSemaphoreState,
	observed_at: StateTimestampMicros,
) -> SemaphoreStatus {
	SemaphoreStatus {
		current_count: state.live_count(observed_at),
		capacity: state.settings.capacity(),
	}
}

fn require_permanent(metadata: RecordMetadata) -> Result<(), SemaphoreStateError> {
	if metadata.expires_at.is_some() {
		return Err(SemaphoreStateError::OuterExpiration);
	}
	Ok(())
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SemaphoreCodecError {
	#[error("Semaphore private capability encoding is invalid")]
	PrivateCapability(#[from] crate::fleet::private_capability_wire::PrivateCapabilityWireError),
	#[error("Semaphore JSON is invalid: {summary}")]
	Json { summary: JsonDecodeFailureSummary },
	#[error("Semaphore durable state is invalid: {reason}")]
	Invalid { reason: &'static str },
	#[error("Semaphore durable settings are invalid")]
	Settings(#[from] SemaphoreSettingsError),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SemaphoreStateError {
	#[error("Semaphore policy does not bind its code")]
	Contract(#[from] StateContractError),
	#[error("Semaphore transition differs from its schema owner")]
	TransitionInstanceMismatch,
	#[error("Semaphore storage requires its explicitly selected native JSON writer")]
	NativeWriterRequired,
	#[error("Semaphore resource must not have an outer expiration")]
	OuterExpiration,
	#[error("Semaphore state belongs to another resource")]
	ResourceMismatch,
	#[error("Semaphore resource settings differ from the required settings")]
	SettingsMismatch,
	#[error("Semaphore claim has an invalid slot suffix")]
	ClaimSlotInvalid,
	#[error("Semaphore expiration exceeds its signed timestamp domain")]
	ExpirationOverflow,
	#[error("Semaphore private capability generation failed")]
	Randomness(#[source] RandomnessError),
	#[error("Semaphore state is not readable under its installed policy")]
	Read(#[from] ContractStateReadError<SemaphoreCodecError>),
	#[error("Semaphore cannot encode its selected representation")]
	Encode(#[from] StateSchemaError<SemaphoreCodecError>),
}
