//! Pure native Throttler settings, activity, and tentative transition preparation.
//!
//! Activity expiry never deletes settings. Parsing and prepared mutations establish no
//! admission, ordinary-outcome anchor authentication, persistence, or delivered authority.

use std::collections::BTreeMap;
use std::time::Duration;

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

use super::arithmetic::semantic_settings::ThrottlerCircuitSettings;
use super::arithmetic::{
	add_duration_to_timestamp, compute_rate_limit_retry_after_duration, duration_to_micros_lossy,
};
use super::model::ThrottlerTaskOutcome;
use crate::fleet::{
	AcquisitionCapability, CircuitState, HolderId, MAX_SLOT_COUNT, RandomnessError, ThrottlerKey,
	ThrottlerStatus,
};

mod circuit;
use circuit::{CircuitActivity, CircuitOwnershipView};
mod acquisition;
pub(crate) mod outcome;
pub(crate) use acquisition::NativeThrottlerReservationKind;
use acquisition::{
	AcquisitionAuthority, PrivateAcquisitionIdentity, ThrottlerAcquisitionPredicates,
};
mod settings;
pub(super) mod wire;
pub use settings::{
	ThrottlerCircuitSettingsFields, ThrottlerConcurrencySettingsFields, ThrottlerSettings,
	ThrottlerSettingsError, ThrottlerSettingsFields,
};

const COMPONENT: &str = "paranoid.throttler";
const SEMANTICS: &str = "paranoid.throttler.v1";
const NATIVE_REPRESENTATION_VERSION: u32 = 1;

pub(crate) struct NativeThrottlerStateFields {
	pub(crate) key: ThrottlerKey,
	pub(crate) settings: ThrottlerSettings,
}
pub(crate) struct NativeThrottlerState {
	key: ThrottlerKey,
	settings: ThrottlerSettings,
	activity: Option<ThrottlerActivity>,
}
#[derive(Clone)]
struct ThrottlerActivity {
	expires_at: StateTimestampMicros,
	updated_at: StateTimestampMicros,
	tokens: f64,
	last_refill: StateTimestampMicros,
	slots: BTreeMap<u16, ConcurrencySlot>,
	failures: u32,
	circuit: CircuitActivity,
}
#[derive(Clone)]
struct ConcurrencySlot {
	holder: HolderId,
	capability: AcquisitionCapability,
	expires_at: StateTimestampMicros,
}
impl ConcurrencySlot {
	fn expiry(&self) -> StateTimestampMicros {
		self.expires_at
	}
}
#[derive(Clone)]
struct PrivateProbe {
	holder: HolderId,
	capability: AcquisitionCapability,
	expires_at: StateTimestampMicros,
}
impl NativeThrottlerState {
	pub(crate) fn new(fields: NativeThrottlerStateFields) -> Self {
		Self {
			key: fields.key,
			settings: fields.settings,
			activity: None,
		}
	}
	pub(crate) fn key(&self) -> &ThrottlerKey {
		&self.key
	}
	pub(crate) fn settings(&self) -> &ThrottlerSettings {
		&self.settings
	}
	fn live_activity(&self, now: StateTimestampMicros) -> Option<&ThrottlerActivity> {
		self.activity
			.as_ref()
			.filter(|activity| now < activity.expires_at)
	}
}
impl std::fmt::Debug for NativeThrottlerState {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("NativeThrottlerState")
			.finish_non_exhaustive()
	}
}

/// Local codec registration for the library-owned Throttler state.
pub struct ThrottlerComponentFields {
	/// Registers only readers, or both readers and writers, without choosing installed policy.
	pub code_access: NativePrimitiveCodeAccess,
}

/// Actual library-owned intrinsic codecs; construction grants no stored-state access.
pub struct ThrottlerComponent {
	schema: StateSchema<NativeThrottlerState, ThrottlerCodecError>,
	outcome_schema:
		StateSchema<outcome::NativeThrottlerOutcomeAnchor, outcome::ThrottlerOutcomeCodecError>,
}
struct ThrottlerNativeSchemas<'a> {
	schema: BoundStateSchema<'a, NativeThrottlerState, ThrottlerCodecError>,
	outcome_schema: BoundStateSchema<
		'a,
		outcome::NativeThrottlerOutcomeAnchor,
		outcome::ThrottlerOutcomeCodecError,
	>,
}
impl ThrottlerComponent {
	/// Registers the actual intrinsic codecs with the explicitly selected local access.
	pub fn new(fields: ThrottlerComponentFields) -> Self {
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
			.expect("Throttler declares its actual initial native codec"),
			outcome_schema: outcome::schema(fields.code_access),
		}
	}
	pub(crate) fn component_identifier() -> &'static str {
		COMPONENT
	}
	/// Selects this actual owner's initial native writer and retained reader without resource
	/// authority.
	pub(crate) fn initial_policy(&self) -> Result<ComponentPolicy, ThrottlerStateError> {
		let writer = RepresentationPolicy {
			version: NATIVE_REPRESENTATION_VERSION,
			envelope: StateStorageEnvelope::NativeJson,
		};
		let policy = ComponentPolicy::new(ComponentPolicyFields {
			component_identifier: COMPONENT.to_owned(),
			semantic_contract: SEMANTICS.to_owned(),
			schemas: vec![
				SchemaPolicy::new(self.schema.identifier().to_owned(), writer, vec![writer])?,
				outcome::initial_schema_policy(&self.outcome_schema)?,
			],
			settings: ComponentSettings::Parameterless,
		})?;
		let declaration = self.declaration();
		let bound = declaration.bind(&policy)?;
		self.require_bound_native_writers(&bound)?;
		Ok(policy)
	}
	pub(crate) fn schema(&self) -> &StateSchema<NativeThrottlerState, ThrottlerCodecError> {
		&self.schema
	}
	pub(crate) fn declaration(&self) -> ComponentDeclaration<'_, Self> {
		ComponentDeclaration::new(ComponentDeclarationFields {
			identifier: Self::component_identifier(),
			fixed_schemas: vec![
				self.schema.capabilities(),
				self.outcome_schema.capabilities(),
			],
			settings: ComponentSettingsDeclaration::Parameterless,
			semantics: vec![SemanticContractDef {
				identifier: SEMANTICS,
				transitions: self,
				settings: ComponentSettingsRequirement::Parameterless,
			}],
		})
		.expect("Throttler declares actual code with settings retained per resource")
	}
	fn bind_native_schemas<'a>(
		&'a self,
		contract: &'a BoundComponentContract<'_, '_, Self>,
	) -> Result<ThrottlerNativeSchemas<'a>, ThrottlerStateError> {
		if !std::ptr::eq(self, contract.transitions())
			|| contract.policy().component_identifier() != COMPONENT
			|| contract.policy().semantic_contract() != SEMANTICS
		{
			return Err(ThrottlerStateError::TransitionInstanceMismatch);
		}
		let schema = contract.bind_schema(&self.schema)?;
		let outcome_schema = contract.bind_schema(&self.outcome_schema)?;
		if schema.policy().writer().envelope != StateStorageEnvelope::NativeJson
			|| outcome_schema.policy().writer().envelope != StateStorageEnvelope::NativeJson
		{
			return Err(ThrottlerStateError::NativeWriterRequired);
		}
		Ok(ThrottlerNativeSchemas {
			schema,
			outcome_schema,
		})
	}
	/// Checks the exact resource and accounting schema writers without a resource or grant.
	pub(crate) fn require_bound_native_writers(
		&self,
		contract: &BoundComponentContract<'_, '_, Self>,
	) -> Result<(), ThrottlerStateError> {
		self.bind_native_schemas(contract).map(|_| ())
	}
	pub(crate) fn bind_schema<'a>(
		&'a self,
		fields: ThrottlerBindingFields<'a, '_, '_>,
	) -> Result<ThrottlerSchemaBinding<'a>, ThrottlerStateError> {
		let schemas = self.bind_native_schemas(fields.contract)?;
		Ok(ThrottlerSchemaBinding {
			schema: schemas.schema,
			outcome_schema: schemas.outcome_schema,
			key: fields.key,
			settings: fields.settings,
		})
	}
}
pub(crate) struct ThrottlerBindingFields<'a, 'c, 'd> {
	pub(crate) contract: &'a BoundComponentContract<'c, 'd, ThrottlerComponent>,
	pub(crate) key: &'a ThrottlerKey,
	pub(crate) settings: &'a ThrottlerSettings,
}
pub(crate) struct ThrottlerSchemaBinding<'a> {
	schema: BoundStateSchema<'a, NativeThrottlerState, ThrottlerCodecError>,
	outcome_schema: BoundStateSchema<
		'a,
		outcome::NativeThrottlerOutcomeAnchor,
		outcome::ThrottlerOutcomeCodecError,
	>,
	key: &'a ThrottlerKey,
	settings: &'a ThrottlerSettings,
}

struct ThrottlerAcquireInput {
	rate_limit_cost: std::num::NonZeroU32,
	pub(crate) holder: Option<HolderId>,
	pub(crate) observed_at: StateTimestampMicros,
	outcome_id: Option<outcome::ThrottlerOutcomeId>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ThrottlerMutationDecision {
	Keep,
	Write,
}
pub(crate) enum ThrottlerAcquisitionResult {
	Acquired(TentativeThrottlerAcquisition),
	Throttled { retry_after: Option<Duration> },
	CircuitOpen,
}
pub(crate) struct ThrottlerAcquisitionDecision {
	pub(crate) result: ThrottlerAcquisitionResult,
	pub(crate) mutation: ThrottlerMutationDecision,
}

/// Non-Clone private acquisition data; it is not a durable outcome anchor.
pub(crate) struct NativeThrottlerPermit {
	predicates: ThrottlerAcquisitionPredicates,
}

impl NativeThrottlerPermit {
	pub(crate) fn reservation_kind(&self) -> NativeThrottlerReservationKind {
		self.predicates.reservation_kind()
	}
	/// Identifies the accounting prerequisite; it does not establish its authentication.
	pub(crate) fn ordinary_anchor_requirement(&self) -> Option<&Self> {
		matches!(
			self.predicates.authority,
			AcquisitionAuthority::OrdinaryCircuit { .. }
		)
		.then_some(self)
	}
}
impl std::fmt::Debug for NativeThrottlerPermit {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("NativeThrottlerPermit")
			.finish_non_exhaustive()
	}
}
pub(crate) struct TentativeThrottlerAcquisition {
	permit: NativeThrottlerPermit,
}
impl TentativeThrottlerAcquisition {
	pub(crate) fn ordinary_anchor_requirement(&self) -> Option<&NativeThrottlerPermit> {
		self.permit.ordinary_anchor_requirement()
	}
	/// Only the future owning executor can establish a known commit and required anchor.
	pub(crate) fn into_permit_after_known_commit(self) -> NativeThrottlerPermit {
		self.permit
	}
}
struct ThrottlerOutcomeInput {
	predicates: ThrottlerAcquisitionPredicates,
	pub(crate) outcome: ThrottlerTaskOutcome,
	pub(crate) observed_at: StateTimestampMicros,
}
struct ThrottlerReleasePreparation {
	key: ThrottlerKey,
	settings: ThrottlerSettings,
	activity: Option<ThrottlerActivity>,
	mutation: ThrottlerMutationDecision,
}
impl ThrottlerReleasePreparation {
	/// Applies tentative working state, without proving anchor authentication or persistence.
	pub(crate) fn into_state(self) -> (NativeThrottlerState, ThrottlerMutationDecision) {
		(
			NativeThrottlerState {
				key: self.key,
				settings: self.settings,
				activity: self.activity,
			},
			self.mutation,
		)
	}
}
pub(crate) struct ThrottlerProbeAuthority {
	key: ThrottlerKey,
	holder: HolderId,
	capability: AcquisitionCapability,
	epoch: AcquisitionCapability,
}
impl NativeThrottlerPermit {
	/// The duplicated material can only be passed to probe extension, never to outcome APIs.
	pub(crate) fn probe_extension_authority(&self) -> Option<ThrottlerProbeAuthority> {
		if !self.predicates.is_probe() {
			return None;
		}
		Some(ThrottlerProbeAuthority {
			key: self.predicates.key.clone(),
			holder: self.predicates.holder()?.clone(),
			capability: self.predicates.capability()?.clone(),
			epoch: self.predicates.epoch()?.clone(),
		})
	}
}
pub(crate) struct ThrottlerProbeExtensionInput<'a> {
	pub(crate) authority: &'a ThrottlerProbeAuthority,
	pub(crate) observed_at: StateTimestampMicros,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ThrottlerCircuitAction {
	Open,
	Close,
}
pub(crate) struct ThrottlerCircuitInput {
	pub(crate) action: ThrottlerCircuitAction,
	pub(crate) observed_at: StateTimestampMicros,
}

impl ThrottlerSchemaBinding<'_> {
	fn require_state(&self, state: &NativeThrottlerState) -> Result<(), ThrottlerStateError> {
		require_resource(state, self.key, self.settings)
	}
	pub(crate) fn decode_record(
		&self,
		record: &StoredRecord,
	) -> Result<NativeThrottlerState, ThrottlerStateError> {
		decode_resource(record, self.key, self.settings, |body| {
			self.schema.decode_document(body)
		})
	}
	pub(crate) fn encode_record(
		&self,
		state: &NativeThrottlerState,
		metadata: RecordMetadata,
	) -> Result<EncodedRecord, ThrottlerStateError> {
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
	fn acquire(
		&self,
		state: &mut NativeThrottlerState,
		input: ThrottlerAcquireInput,
	) -> Result<ThrottlerAcquisitionDecision, ThrottlerStateError> {
		self.require_state(state)?;
		if input.outcome_id.is_some() != self.settings.circuit().is_some() {
			return Err(ThrottlerStateError::OutcomeProposalRequired);
		}
		if (self.settings.concurrency().is_some() || self.settings.circuit().is_some())
			&& input.holder.is_none()
		{
			return Err(ThrottlerStateError::HolderRequired);
		}
		let now = input.observed_at;
		let mut activity = state
			.live_activity(now)
			.cloned()
			.unwrap_or_else(|| initial_activity(*self.settings, now));
		let probe = match circuit_gate(*self.settings, &activity, now) {
			CircuitGate::Block => {
				return Ok(ThrottlerAcquisitionDecision {
					result: ThrottlerAcquisitionResult::CircuitOpen,
					mutation: ThrottlerMutationDecision::Keep,
				});
			}
			CircuitGate::Probe => true,
			CircuitGate::Normal => false,
		};
		let before = activity.slots.len();
		let mut slot = None;
		if let Some(concurrency) = self.settings.concurrency() {
			activity.slots.retain(|_, entry| now < entry.expiry());
			if activity.slots.len() >= usize::from(concurrency.capacity()) {
				let mutation = if activity.slots.len() != before {
					refresh_activity(&mut activity, *self.settings, now)?;
					state.activity = Some(activity);
					ThrottlerMutationDecision::Write
				} else {
					ThrottlerMutationDecision::Keep
				};
				return Ok(ThrottlerAcquisitionDecision {
					result: ThrottlerAcquisitionResult::Throttled { retry_after: None },
					mutation,
				});
			}
			slot = (1..=concurrency.capacity()).find(|number| !activity.slots.contains_key(number));
		}
		if let Some(rate) = self.settings.rate() {
			activity.tokens = refilled_tokens(activity.tokens, activity.last_refill, now, rate);
			activity.last_refill = now;
			if activity.tokens < f64::from(input.rate_limit_cost.get()) {
				let retry_after = compute_rate_limit_retry_after_duration(
					super::arithmetic::RateLimitRetryInput {
						current_tokens: activity.tokens,
						refill_rate_per_sec: rate.refill_rate_per_sec(),
						required_tokens: input.rate_limit_cost,
					},
				);
				refresh_activity(&mut activity, *self.settings, now)?;
				state.activity = Some(activity);
				return Ok(ThrottlerAcquisitionDecision {
					result: ThrottlerAcquisitionResult::Throttled {
						retry_after: Some(retry_after),
					},
					mutation: ThrottlerMutationDecision::Write,
				});
			}
			activity.tokens -= f64::from(input.rate_limit_cost.get());
		}
		let slot_expiry = self
			.settings
			.concurrency()
			.map(|control| expiration(now, control.max_hold_duration()))
			.transpose()?;
		let probe_expiry = if probe {
			self.settings
				.circuit()
				.map(|control| expiration(now, control.probe_window()))
				.transpose()?
		} else {
			None
		};
		refresh_activity(&mut activity, *self.settings, now)?;
		let capability =
			if self.settings.concurrency().is_some() || self.settings.circuit().is_some() {
				Some(AcquisitionCapability::generate().map_err(ThrottlerStateError::Randomness)?)
			} else {
				None
			};
		if let Some(number) = slot {
			activity.slots.insert(
				number,
				ConcurrencySlot {
					holder: input
						.holder
						.as_ref()
						.expect("required holder was checked")
						.clone(),
					capability: capability
						.as_ref()
						.expect("private profile generated a capability")
						.clone(),
					expires_at: slot_expiry.expect("concurrency profile prepared its expiration"),
				},
			);
		}
		let epoch = if self.settings.circuit().is_some() {
			let epoch = if probe {
				capability
					.as_ref()
					.expect("probe capability exists")
					.clone()
			} else {
				activity
					.circuit
					.native_epoch()
					.unwrap_or_else(|| capability.as_ref().expect("circuit capability exists"))
					.clone()
			};
			let reservation = if probe {
				Some(PrivateProbe {
					holder: input.holder.as_ref().expect("probe holder exists").clone(),
					capability: capability
						.as_ref()
						.expect("probe capability exists")
						.clone(),
					expires_at: probe_expiry.expect("probe expiry was prepared"),
				})
			} else {
				None
			};
			match reservation {
				Some(probe) => activity.circuit.reserve_probe(epoch.clone(), probe),
				None => activity.circuit.retain_native_epoch(epoch.clone()),
			}
			Some(epoch)
		} else {
			None
		};
		let authority = if let Some(epoch) = epoch {
			let identity = PrivateAcquisitionIdentity {
				holder: input.holder.expect("circuit profile requires a holder"),
				capability: capability.expect("circuit profile generated its capability"),
			};
			if probe {
				AcquisitionAuthority::Probe {
					identity,
					slot,
					epoch,
				}
			} else {
				AcquisitionAuthority::OrdinaryCircuit {
					identity,
					slot,
					epoch,
					outcome_id: input
						.outcome_id
						.expect("ordinary circuit acquisition proposed its outcome"),
				}
			}
		} else if let Some(slot) = slot {
			AcquisitionAuthority::Concurrency {
				identity: PrivateAcquisitionIdentity {
					holder: input.holder.expect("concurrency profile requires a holder"),
					capability: capability.expect("concurrency profile generated its capability"),
				},
				slot,
			}
		} else {
			AcquisitionAuthority::RateOnly
		};
		state.activity = Some(activity);
		Ok(ThrottlerAcquisitionDecision {
			result: ThrottlerAcquisitionResult::Acquired(TentativeThrottlerAcquisition {
				permit: NativeThrottlerPermit {
					predicates: ThrottlerAcquisitionPredicates {
						key: self.key.clone(),
						authority,
					},
				},
			}),
			mutation: ThrottlerMutationDecision::Write,
		})
	}
	fn prepare_release(
		&self,
		state: &NativeThrottlerState,
		input: ThrottlerOutcomeInput,
	) -> Result<ThrottlerReleasePreparation, ThrottlerStateError> {
		self.require_state(state)?;
		if &input.predicates.key != self.key {
			return Err(ThrottlerStateError::ResourceMismatch);
		}
		let mut activity = state.activity.clone();
		// Whether the release removed a slot, updated the circuit, or released a probe.
		let mut changed = false;
		if let Some(live) = activity
			.as_mut()
			.filter(|activity| input.observed_at < activity.expires_at)
		{
			if self.settings.concurrency().is_some()
				&& let Some(number) = input.predicates.slot()
			{
				let matched = live.slots.get(&number).is_some_and(|entry| {
					Some(&entry.holder) == input.predicates.holder()
						&& Some(&entry.capability) == input.predicates.capability()
				});
				if matched {
					live.slots.remove(&number);
					changed = true;
				}
			}
			if let Some(control) = self.settings.circuit()
				&& let Some(epoch) = live.circuit.native_epoch().cloned()
			{
				let epoch_matches = Some(&epoch) == input.predicates.epoch();
				let probe_matches = live.circuit.native_probe().is_some_and(|(_, probe)| {
					Some(&probe.holder) == input.predicates.holder()
						&& Some(&probe.capability) == input.predicates.capability()
				});
				if epoch_matches && (!input.predicates.is_probe() || probe_matches) {
					let prior_state = live.circuit.state();
					let circuit_state_updated = match input.outcome {
						ThrottlerTaskOutcome::NotExecuted => false,
						ThrottlerTaskOutcome::Failed => {
							live.failures = live.failures.saturating_add(1);
							if live.circuit.state() == CircuitState::Open
								|| live.failures >= control.failure_threshold()
							{
								live.circuit.open(input.observed_at);
							}
							true
						}
						ThrottlerTaskOutcome::Succeeded => {
							let updated =
								live.failures != 0 || live.circuit.state() != CircuitState::Closed;
							live.failures = 0;
							live.circuit.close();
							updated
						}
					};
					let probe_released = input.predicates.is_probe() && probe_matches;
					let starts_era = circuit_state_updated
						&& (input.predicates.is_probe() || prior_state != live.circuit.state());
					if starts_era {
						let next = AcquisitionCapability::generate()
							.map_err(ThrottlerStateError::Randomness)?;
						live.circuit.retain_native_epoch(next);
					} else if probe_released {
						live.circuit.retain_native_epoch(epoch);
					}
					changed |= circuit_state_updated || probe_released;
				}
			}
			if changed {
				refresh_activity(live, *self.settings, input.observed_at)?;
			}
		}
		let mutation = if changed {
			ThrottlerMutationDecision::Write
		} else {
			ThrottlerMutationDecision::Keep
		};
		Ok(ThrottlerReleasePreparation {
			key: self.key.clone(),
			settings: *self.settings,
			activity,
			mutation,
		})
	}
	pub(crate) fn extend_probe(
		&self,
		state: &mut NativeThrottlerState,
		input: ThrottlerProbeExtensionInput<'_>,
	) -> Result<bool, ThrottlerStateError> {
		self.require_state(state)?;
		if &input.authority.key != self.key {
			return Err(ThrottlerStateError::ResourceMismatch);
		}
		let Some(control) = self.settings.circuit() else {
			return Ok(false);
		};
		let Some(activity) = state.live_activity(input.observed_at) else {
			return Ok(false);
		};
		if activity.circuit.state() != CircuitState::Open {
			return Ok(false);
		}
		let matched = activity
			.circuit
			.native_probe()
			.is_some_and(|(epoch, probe)| {
				epoch == &input.authority.epoch
					&& probe.holder == input.authority.holder
					&& probe.capability == input.authority.capability
					&& input.observed_at < probe.expires_at
			});
		if !matched {
			return Ok(false);
		}
		let expires_at = expiration(input.observed_at, control.probe_window())?;
		let next_activity_expiry = activity_expiration(*self.settings, input.observed_at)?;
		let activity = state.activity.as_mut().expect("matching activity exists");
		activity
			.circuit
			.native_probe_mut()
			.expect("matched native probe remains retained")
			.expires_at = expires_at;
		activity.expires_at = next_activity_expiry;
		activity.updated_at = input.observed_at;
		Ok(true)
	}
	/// Opens or closes the circuit. `_circuit` is the circuit of the bound settings, which only a
	/// Throttler with a circuit has, so this transition has no case without a circuit.
	pub(crate) fn set_circuit(
		&self,
		state: &mut NativeThrottlerState,
		_circuit: ThrottlerCircuitSettings,
		input: ThrottlerCircuitInput,
	) -> Result<ThrottlerMutationDecision, ThrottlerStateError> {
		self.require_state(state)?;
		let mut activity = state
			.live_activity(input.observed_at)
			.cloned()
			.unwrap_or_else(|| initial_activity(*self.settings, input.observed_at));
		refresh_activity(&mut activity, *self.settings, input.observed_at)?;
		let epoch = AcquisitionCapability::generate().map_err(ThrottlerStateError::Randomness)?;
		match input.action {
			ThrottlerCircuitAction::Open => {
				activity.circuit.open(input.observed_at);
			}
			ThrottlerCircuitAction::Close => {
				activity.circuit.close();
				activity.failures = 0;
			}
		}
		activity.circuit.retain_native_epoch(epoch);
		state.activity = Some(activity);
		Ok(ThrottlerMutationDecision::Write)
	}
	pub(crate) fn reset(
		&self,
		state: &mut NativeThrottlerState,
	) -> Result<ThrottlerMutationDecision, ThrottlerStateError> {
		self.require_state(state)?;
		let decision = if state.activity.is_some() {
			ThrottlerMutationDecision::Write
		} else {
			ThrottlerMutationDecision::Keep
		};
		state.activity = None;
		Ok(decision)
	}
}

fn initial_activity(settings: ThrottlerSettings, now: StateTimestampMicros) -> ThrottlerActivity {
	ThrottlerActivity {
		expires_at: now,
		updated_at: now,
		tokens: settings.rate().map_or(0.0, |rate| f64::from(rate.capacity)),
		last_refill: now,
		slots: BTreeMap::new(),
		failures: 0,
		circuit: CircuitActivity::closed_uninitialized(),
	}
}
fn expiration(
	now: StateTimestampMicros,
	duration: Duration,
) -> Result<StateTimestampMicros, ThrottlerStateError> {
	add_duration_to_timestamp(now.as_i64(), duration)
		.map(StateTimestampMicros::new)
		.ok_or(ThrottlerStateError::ExpirationOverflow)
}
fn activity_expiration(
	settings: ThrottlerSettings,
	now: StateTimestampMicros,
) -> Result<StateTimestampMicros, ThrottlerStateError> {
	now.as_i64()
		.checked_add(settings.activity_ttl_micros())
		.map(StateTimestampMicros::new)
		.ok_or(ThrottlerStateError::ExpirationOverflow)
}
fn refresh_activity(
	activity: &mut ThrottlerActivity,
	settings: ThrottlerSettings,
	now: StateTimestampMicros,
) -> Result<(), ThrottlerStateError> {
	let expiry = activity_expiration(settings, now)?;
	activity.expires_at = expiry;
	activity.updated_at = now;
	Ok(())
}
fn refilled_tokens(
	tokens: f64,
	last: StateTimestampMicros,
	now: StateTimestampMicros,
	rate: super::model::ResolvedThrottlerRateLimit,
) -> f64 {
	(tokens
		+ (now.as_i64().saturating_sub(last.as_i64()).max(0) as f64 / 1_000_000.0)
			* rate.refill_rate_per_sec())
	.min(f64::from(rate.capacity))
}
enum CircuitGate {
	Normal,
	Probe,
	Block,
}
fn circuit_gate(
	settings: ThrottlerSettings,
	activity: &ThrottlerActivity,
	now: StateTimestampMicros,
) -> CircuitGate {
	let Some(control) = settings.circuit() else {
		return CircuitGate::Normal;
	};
	if activity.circuit.state() != CircuitState::Open {
		return CircuitGate::Normal;
	}
	let reserved = activity
		.circuit
		.reserved_until()
		.is_some_and(|until| now < until);
	if reserved {
		return CircuitGate::Block;
	}
	let Some(opened_at) = activity.circuit.opening() else {
		return CircuitGate::Probe;
	};
	if now.as_i64().saturating_sub(opened_at.as_i64())
		< duration_to_micros_lossy(control.recovery_timeout())
	{
		return CircuitGate::Block;
	}
	CircuitGate::Probe
}

/// Reads the same native resource without requiring a selected encoder.
pub(in crate::fleet) fn read_resource(
	reader: &BoundSchemaReader<'_, NativeThrottlerState, ThrottlerCodecError>,
	record: &StoredRecord,
	key: &ThrottlerKey,
	settings: &ThrottlerSettings,
) -> Result<NativeThrottlerState, ThrottlerStateError> {
	decode_resource(record, key, settings, |body| reader.decode_document(body))
}
fn decode_resource(
	record: &StoredRecord,
	key: &ThrottlerKey,
	settings: &ThrottlerSettings,
	decode_fn: impl FnOnce(
		&[u8],
	) -> Result<
		(u32, NativeThrottlerState),
		ContractStateReadError<ThrottlerCodecError>,
	>,
) -> Result<NativeThrottlerState, ThrottlerStateError> {
	let state = decode_native_resource(record, decode_fn)?;
	require_resource(&state, key, settings)?;
	Ok(state)
}
fn decode_native_resource(
	record: &StoredRecord,
	decode_fn: impl FnOnce(
		&[u8],
	) -> Result<
		(u32, NativeThrottlerState),
		ContractStateReadError<ThrottlerCodecError>,
	>,
) -> Result<NativeThrottlerState, ThrottlerStateError> {
	require_permanent(record.metadata())?;
	Ok(decode_fn(record.body())?.1)
}
pub(super) fn require_resource(
	state: &NativeThrottlerState,
	key: &ThrottlerKey,
	settings: &ThrottlerSettings,
) -> Result<(), ThrottlerStateError> {
	if key != state.key() {
		return Err(ThrottlerStateError::ResourceMismatch);
	}
	if settings != state.settings() {
		return Err(ThrottlerStateError::SettingsMismatch);
	}
	Ok(())
}
pub(in crate::fleet) fn resource_status(
	state: &NativeThrottlerState,
	observed_at: StateTimestampMicros,
) -> ThrottlerStatus {
	let mut status = ThrottlerStatus {
		available_tokens: state
			.settings
			.rate()
			.map_or(0.0, |rate| f64::from(rate.capacity)),
		rate_capacity: state
			.settings
			.rate()
			.map_or(0.0, |rate| f64::from(rate.capacity)),
		current_concurrency: 0,
		concurrency_capacity: state
			.settings
			.concurrency()
			.map_or(0, |control| control.capacity()),
		circuit_state: CircuitState::Closed,
		consecutive_failures: 0,
	};
	if let Some(activity) = state.live_activity(observed_at) {
		if let Some(rate) = state.settings.rate() {
			status.available_tokens =
				refilled_tokens(activity.tokens, activity.last_refill, observed_at, rate);
		}
		if state.settings.concurrency().is_some() {
			status.current_concurrency = activity
				.slots
				.values()
				.filter(|entry| observed_at < entry.expiry())
				.count() as u16;
		}
		if state.settings.circuit().is_some() {
			status.circuit_state = activity.circuit.state();
			status.consecutive_failures = activity.failures;
		}
	}
	status
}

fn require_permanent(metadata: RecordMetadata) -> Result<(), ThrottlerStateError> {
	if metadata.expires_at.is_some() {
		Err(ThrottlerStateError::OuterExpiration)
	} else {
		Ok(())
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ThrottlerCodecError {
	#[error("Throttler JSON is invalid: {summary}")]
	Json { summary: JsonDecodeFailureSummary },
	#[error("Throttler durable state is invalid: {reason}")]
	Invalid { reason: &'static str },
	#[error("Throttler durable settings are invalid")]
	Settings(#[from] ThrottlerSettingsError),
	#[error("Throttler private capability wire is invalid")]
	Private(#[from] crate::fleet::private_capability_wire::PrivateCapabilityWireError),
}
#[derive(Debug, thiserror::Error)]
pub(crate) enum ThrottlerStateError {
	#[error("Throttler policy does not bind its code")]
	Contract(#[from] StateContractError),
	#[error("Throttler binding belongs to a different declaration")]
	TransitionInstanceMismatch,
	#[error("Throttler storage requires its explicit native JSON writer")]
	NativeWriterRequired,
	#[error("Throttler resource must not have an outer expiration")]
	OuterExpiration,
	#[error("Throttler state belongs to another resource")]
	ResourceMismatch,
	#[error("Throttler resource settings differ from its required settings")]
	SettingsMismatch,
	#[error("Throttler circuit acquisition requires a proposed outcome locator")]
	OutcomeProposalRequired,
	#[error("Throttler private control requires a parsed holder")]
	HolderRequired,
	#[error("Throttler deadline exceeds the signed timestamp domain")]
	ExpirationOverflow,
	#[error("Throttler private capability generation failed")]
	Randomness(#[source] RandomnessError),
	#[error("Throttler state is not readable under its bound policy")]
	Read(#[from] ContractStateReadError<ThrottlerCodecError>),
	#[error("Throttler state cannot encode its selected representation")]
	Encode(#[from] StateSchemaError<ThrottlerCodecError>),
}
