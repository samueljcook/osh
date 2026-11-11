//! Native installation control data, independent of transaction admission and upgrade decisions.
//!
//! Active policies, required opaque backend registration, and an opaque pending request are
//! separate fields. This neutral parser establishes only JSON syntax for backend registration
//! and pending metadata; their owning adapters interpret them.

use std::{fmt, num::NonZeroU64};

use primitives_rs::encoding::decimal::parse_canonical_decimal;
use primitives_rs::id::time_sortable::{SORTABLE_ID_LEN, SortableId};
use primitives_rs::serialization::json::{
	JsonDecodeFailureSummary, JsonFailureCategory, deserialize_required_option,
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::installation::contract::policy_wire::ComponentPolicyWire;
use crate::installation::contract::{
	BoundStateSchema, ComponentPolicy, ContractStateReadError, RepresentationPolicy, SchemaPolicy,
	StateContractError, StateStorageEnvelope,
};
use crate::records::state_schema::{
	StateRepresentationFormat, StateSchema, StateSchemaError, StateVersion,
};
use crate::records::{EncodedRecord, IndexFields, PointRecordRead, RecordIdentity, RecordMetadata};
use crate::scope::ScopePolicySet;
use crate::scope::wire::{ScopePolicySetWire, ScopeWireError};

const CONTROL_SCHEMA: &str = "paranoid.installation-control";
const CONTROL_REPRESENTATION_VERSION: u32 = 1;

/// Exact storage phase supplied by the owning adapter's checked control interpretation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InstallationStorageRoute {
	NativeFresh,
	SourceCoexistence,
	SourceRelocation,
	NativeImported,
}

/// Parsed active policy and optional opaque target-request metadata.
pub(crate) struct InstallationControlState {
	installation_id: SortableId,
	generation: NonZeroU64,
	component_policies: Vec<ComponentPolicy>,
	scope_policies: ScopePolicySet,
	backend_registration: Box<RawValue>,
	pending_upgrade: Option<Box<RawValue>>,
}

/// Complete control state supplied together without choosing a generation implicitly.
pub(crate) struct InstallationControlFields {
	pub(crate) installation_id: SortableId,
	pub(crate) generation: NonZeroU64,
	pub(crate) component_policies: Vec<ComponentPolicy>,
	pub(crate) scope_policies: ScopePolicySet,
	pub(crate) backend_registration: Box<RawValue>,
	pub(crate) pending_upgrade: Option<Box<RawValue>>,
}

impl InstallationControlState {
	pub(crate) fn new(fields: InstallationControlFields) -> Result<Self, InstallationControlError> {
		for (index, policy) in fields.component_policies.iter().enumerate() {
			if fields.component_policies[..index]
				.iter()
				.any(|previous| previous.component_identifier() == policy.component_identifier())
			{
				return Err(InstallationControlError::DuplicateComponent {
					policy_index: index,
				});
			}
		}
		Ok(Self {
			installation_id: fields.installation_id,
			generation: fields.generation,
			component_policies: fields.component_policies,
			scope_policies: fields.scope_policies,
			backend_registration: fields.backend_registration,
			pending_upgrade: fields.pending_upgrade,
		})
	}

	pub(crate) fn installation_id(&self) -> SortableId {
		self.installation_id
	}

	pub(crate) fn generation(&self) -> NonZeroU64 {
		self.generation
	}

	pub(crate) fn component_policies(&self) -> &[ComponentPolicy] {
		&self.component_policies
	}

	pub(crate) fn scope_policies(&self) -> &ScopePolicySet {
		&self.scope_policies
	}

	pub(crate) fn component(&self, identifier: &str) -> Option<&ComponentPolicy> {
		self.component_policies
			.iter()
			.find(|policy| policy.component_identifier() == identifier)
	}

	pub(crate) fn pending_upgrade(&self) -> Option<&RawValue> {
		self.pending_upgrade.as_deref()
	}

	pub(crate) fn backend_registration(&self) -> &RawValue {
		&self.backend_registration
	}

	pub(crate) fn into_fields(self) -> InstallationControlFields {
		InstallationControlFields {
			installation_id: self.installation_id,
			generation: self.generation,
			component_policies: self.component_policies,
			scope_policies: self.scope_policies,
			backend_registration: self.backend_registration,
			pending_upgrade: self.pending_upgrade,
		}
	}
}

impl fmt::Debug for InstallationControlState {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("InstallationControlState")
			.field("generation", &self.generation)
			.field("component_count", &self.component_policies.len())
			.field("scope_count", &self.scope_policies.policies().len())
			.field("has_pending_upgrade", &self.pending_upgrade.is_some())
			.finish_non_exhaustive()
	}
}

/// Strict framework bootstrap decoding retains the observed envelope version for policy binding.
/// Construction is private to the control parser; application decoders cannot produce this value.
pub(crate) struct DecodedInstallationControl {
	identity: RecordIdentity,
	representation_version: u32,
	state: InstallationControlState,
}

impl DecodedInstallationControl {
	pub(crate) fn state(&self) -> &InstallationControlState {
		&self.state
	}

	pub(crate) fn into_state(self) -> InstallationControlState {
		self.state
	}
}

/// A control value or its physical record rejected without exposing stored content.
#[derive(Debug, thiserror::Error)]
pub(crate) enum InstallationControlError {
	#[error(
		"installation control JSON is invalid: {category:?} error at line {line}, column {column}"
	)]
	Json {
		category: JsonFailureCategory,
		line: usize,
		column: usize,
	},
	#[error("installation generation must be a canonical positive decimal u64")]
	InvalidGeneration,
	#[error("installation identifier bytes are invalid")]
	InvalidInstallationId,
	#[error("component policy occurs more than once at policy index {policy_index}")]
	DuplicateComponent { policy_index: usize },
	#[error("installation component policy is invalid")]
	Policy(#[source] StateContractError),
	#[error("installation control lookup has a different record identity")]
	WrongRecordIdentity,
	#[error("installation control belongs to a different installation")]
	WrongInstallation,
	#[error("installation control records must not expire")]
	ExpiringControl,
	#[error("installation control document has a different schema")]
	WrongSchema,
	#[error("installation control representation version {version} has no reader")]
	UnsupportedReader { version: u32 },
	#[error("installation control representation version {version} has no writer")]
	UnsupportedWriter { version: u32 },
	#[error("installation control representation version {version} is not native JSON")]
	NotJsonRepresentation { version: u32 },
	#[error("installation control binding uses a different schema declaration")]
	DifferentSchemaBinding,
	#[error("installation control schema declaration is invalid")]
	InvalidSchemaDeclaration,
	#[error("installation scope policies are invalid")]
	ScopePolicies(#[from] ScopeWireError),
}

/// Strict neutral control parser bound to an exact lookup, before an installation ID is known.
/// It establishes no backend registration, installation adoption, or runtime authority.
pub(crate) struct InstallationControlReader {
	identity: RecordIdentity,
	schema: StateSchema<InstallationControlState, InstallationControlError>,
}

impl InstallationControlReader {
	pub(crate) fn new(identity: RecordIdentity) -> Self {
		Self {
			identity,
			schema: StateSchema::new(crate::records::state_schema::StateSchemaFields {
				identifier: CONTROL_SCHEMA,
				versions: vec![StateVersion {
					version: CONTROL_REPRESENTATION_VERSION,
					format: StateRepresentationFormat::Json,
					decoder: decode_control,
					encoder: Some(encode_control),
				}],
			})
			.expect("installation control declares explicit native JSON versions"),
		}
	}

	/// The caller supplies the actual lookup identity; a snapshot does not establish identity.
	/// A physically present expiring control record is rejected even after its expiry.
	pub(crate) fn decode_record(
		&self,
		actual_lookup_identity: &RecordIdentity,
		snapshot: &PointRecordRead,
	) -> Result<Option<DecodedInstallationControl>, InstallationControlError> {
		self.check_lookup(actual_lookup_identity)?;
		let Some(record) = snapshot.record() else {
			return Ok(None);
		};
		self.decode_stored(record.body(), record.metadata())
			.map(Some)
	}

	fn decode_stored(
		&self,
		encoded: &[u8],
		metadata: RecordMetadata,
	) -> Result<DecodedInstallationControl, InstallationControlError> {
		check_metadata(metadata)?;
		let (representation_version, state) =
			self.schema.decode_document(encoded).map_err(schema_error)?;
		Ok(DecodedInstallationControl {
			identity: self.identity.clone(),
			representation_version,
			state,
		})
	}

	pub(crate) fn into_component(
		self,
		installation_id: SortableId,
	) -> InstallationControlComponent {
		InstallationControlComponent {
			reader: self,
			installation_id,
		}
	}

	fn check_lookup(
		&self,
		actual_lookup_identity: &RecordIdentity,
	) -> Result<(), InstallationControlError> {
		if actual_lookup_identity != &self.identity {
			return Err(InstallationControlError::WrongRecordIdentity);
		}
		Ok(())
	}
}

/// Exact lookup, expected installation identity, and the shared strict neutral parser.
pub(crate) struct InstallationControlComponent {
	reader: InstallationControlReader,
	installation_id: SortableId,
}

impl InstallationControlComponent {
	pub(crate) fn new(identity: RecordIdentity, installation_id: SortableId) -> Self {
		Self {
			reader: InstallationControlReader::new(identity),
			installation_id,
		}
	}

	pub(crate) fn installation_id(&self) -> SortableId {
		self.installation_id
	}

	pub(crate) fn identity(&self) -> &RecordIdentity {
		&self.reader.identity
	}

	pub(crate) fn schema(
		&self,
	) -> &StateSchema<InstallationControlState, InstallationControlError> {
		&self.reader.schema
	}

	/// Fixed initial native control representation; complete administration must bind this policy.
	pub(crate) fn initial_schema_policy(&self) -> Result<SchemaPolicy, StateContractError> {
		let representation = RepresentationPolicy {
			version: CONTROL_REPRESENTATION_VERSION,
			envelope: StateStorageEnvelope::NativeJson,
		};
		SchemaPolicy::new(
			self.schema().identifier().to_owned(),
			representation,
			vec![representation],
		)
	}

	/// Parses the actual configured lookup and checks its expected installation identity.
	pub(crate) fn decode_record(
		&self,
		actual_lookup_identity: &RecordIdentity,
		snapshot: &PointRecordRead,
	) -> Result<Option<DecodedInstallationControl>, InstallationControlError> {
		let state = self
			.reader
			.decode_record(actual_lookup_identity, snapshot)?;
		if let Some(state) = &state {
			self.check_installation(state.state())?;
		}
		Ok(state)
	}

	/// The trusted bootstrap parse precedes policy discovery. No protected state may be accessed
	/// until the decoded administrative policy permits this exact observed native representation.
	pub(crate) fn require_bound_reader(
		&self,
		decoded: &DecodedInstallationControl,
		bound_schema: &BoundStateSchema<'_, InstallationControlState, InstallationControlError>,
	) -> Result<(), InstallationControlError> {
		self.check_schema_binding(bound_schema)?;
		self.reader.check_lookup(&decoded.identity)?;
		self.check_installation(decoded.state())?;
		bound_schema
			.require_reader(
				decoded.representation_version,
				StateStorageEnvelope::NativeJson,
			)
			.map_err(control_read_error)
	}

	pub(crate) fn require_bound_writer(
		&self,
		bound_schema: &BoundStateSchema<'_, InstallationControlState, InstallationControlError>,
	) -> Result<(), InstallationControlError> {
		self.check_schema_binding(bound_schema)?;
		let writer = bound_schema.policy().writer();
		if writer.envelope != StateStorageEnvelope::NativeJson {
			return Err(InstallationControlError::NotJsonRepresentation {
				version: writer.version,
			});
		}
		Ok(())
	}

	fn check_schema_binding(
		&self,
		bound_schema: &BoundStateSchema<'_, InstallationControlState, InstallationControlError>,
	) -> Result<(), InstallationControlError> {
		if !bound_schema.matches_schema_declaration(self.schema()) {
			return Err(InstallationControlError::DifferentSchemaBinding);
		}
		Ok(())
	}

	/// Encodes the selected writer without changing the supplied generation or pending request.
	/// Control uses point lookup, so its exact encoded state derives an empty index projection.
	pub(crate) fn encode_record(
		&self,
		actual_lookup_identity: &RecordIdentity,
		state: &InstallationControlState,
		bound_schema: &BoundStateSchema<'_, InstallationControlState, InstallationControlError>,
		metadata: RecordMetadata,
	) -> Result<EncodedRecord, InstallationControlError> {
		self.reader.check_lookup(actual_lookup_identity)?;
		self.check_installation(state)?;
		check_metadata(metadata)?;
		self.require_bound_writer(bound_schema)?;
		let body = bound_schema.encode(state).map_err(schema_error)?;
		EncodedRecord::derive(body, metadata, |encoded, metadata| {
			self.decode_stored(encoded, metadata)?;
			Ok(IndexFields::new())
		})
	}

	fn decode_stored(
		&self,
		encoded: &[u8],
		metadata: RecordMetadata,
	) -> Result<DecodedInstallationControl, InstallationControlError> {
		let decoded = self.reader.decode_stored(encoded, metadata)?;
		self.check_installation(decoded.state())?;
		Ok(decoded)
	}

	fn check_installation(
		&self,
		state: &InstallationControlState,
	) -> Result<(), InstallationControlError> {
		if state.installation_id != self.installation_id {
			return Err(InstallationControlError::WrongInstallation);
		}
		Ok(())
	}
}

fn control_read_error(
	error: ContractStateReadError<InstallationControlError>,
) -> InstallationControlError {
	match error {
		ContractStateReadError::Policy(error) => InstallationControlError::Policy(error),
		ContractStateReadError::Representation(error) => schema_error(error),
	}
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlWire {
	installation_id: [u8; SORTABLE_ID_LEN],
	generation: String,
	component_policies: Vec<ComponentPolicyWire<'static>>,
	scope_policies: ScopePolicySetWire<'static>,
	backend_registration: Box<RawValue>,
	#[serde(deserialize_with = "deserialize_required_option")]
	pending_upgrade: Option<PendingUpgradeWire>,
}

#[derive(Serialize)]
struct ControlWireRef<'a> {
	installation_id: &'a [u8; SORTABLE_ID_LEN],
	generation: String,
	component_policies: Vec<ComponentPolicyWire<'a>>,
	scope_policies: ScopePolicySetWire<'a>,
	backend_registration: &'a RawValue,
	pending_upgrade: Option<PendingUpgradeWireRef<'a>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingUpgradeWire {
	metadata: Box<RawValue>,
}

#[derive(Serialize)]
struct PendingUpgradeWireRef<'a> {
	metadata: &'a RawValue,
}

fn decode_control(bytes: &[u8]) -> Result<InstallationControlState, InstallationControlError> {
	let wire: ControlWire = serde_json::from_slice(bytes).map_err(json_error)?;
	parse_control(wire)
}

fn parse_control(wire: ControlWire) -> Result<InstallationControlState, InstallationControlError> {
	let generation = NonZeroU64::new(
		parse_canonical_decimal::<u64>(&wire.generation)
			.map_err(|_| InstallationControlError::InvalidGeneration)?,
	)
	.ok_or(InstallationControlError::InvalidGeneration)?;
	let component_policies = wire
		.component_policies
		.into_iter()
		.map(ComponentPolicyWire::into_policy)
		.collect::<Result<Vec<_>, _>>()
		.map_err(InstallationControlError::Policy)?;
	InstallationControlState::new(InstallationControlFields {
		installation_id: SortableId::from_bytes(&wire.installation_id)
			.map_err(|_| InstallationControlError::InvalidInstallationId)?,
		generation,
		component_policies,
		scope_policies: wire.scope_policies.into_policies()?,
		backend_registration: wire.backend_registration,
		pending_upgrade: wire.pending_upgrade.map(|pending| pending.metadata),
	})
}

fn encode_control(state: &InstallationControlState) -> Result<Vec<u8>, InstallationControlError> {
	serde_json::to_vec(&control_wire(state)).map_err(json_error)
}
fn control_wire(state: &InstallationControlState) -> ControlWireRef<'_> {
	ControlWireRef {
		installation_id: state.installation_id.as_bytes(),
		generation: state.generation.get().to_string(),
		component_policies: state
			.component_policies
			.iter()
			.map(ComponentPolicyWire::from_policy)
			.collect(),
		scope_policies: ScopePolicySetWire::from_policies(&state.scope_policies),
		backend_registration: &state.backend_registration,
		pending_upgrade: state
			.pending_upgrade
			.as_deref()
			.map(|metadata| PendingUpgradeWireRef { metadata }),
	}
}

fn check_metadata(metadata: RecordMetadata) -> Result<(), InstallationControlError> {
	if metadata.expires_at.is_some() {
		return Err(InstallationControlError::ExpiringControl);
	}
	Ok(())
}

fn json_error(error: serde_json::Error) -> InstallationControlError {
	let summary = JsonDecodeFailureSummary::from_serde_error(&error);
	InstallationControlError::Json {
		category: summary.category(),
		line: summary.line(),
		column: summary.column(),
	}
}

fn schema_error(error: StateSchemaError<InstallationControlError>) -> InstallationControlError {
	match error {
		StateSchemaError::Codec(error) => error,
		StateSchemaError::UnsupportedReadVersion { version } => {
			InstallationControlError::UnsupportedReader { version }
		}
		StateSchemaError::UnsupportedWriteVersion { version } => {
			InstallationControlError::UnsupportedWriter { version }
		}
		StateSchemaError::NotJsonRepresentation { version } => {
			InstallationControlError::NotJsonRepresentation { version }
		}
		StateSchemaError::WrongSchema { .. } => InstallationControlError::WrongSchema,
		StateSchemaError::InvalidDocument {
			category,
			line,
			column,
		} => InstallationControlError::Json {
			category: match category {
				serde_json::error::Category::Io => JsonFailureCategory::Io,
				serde_json::error::Category::Syntax => JsonFailureCategory::Syntax,
				serde_json::error::Category::Data => JsonFailureCategory::Data,
				serde_json::error::Category::Eof => JsonFailureCategory::Eof,
			},
			line,
			column,
		},
		StateSchemaError::EmptyIdentifier
		| StateSchemaError::NoRepresentations
		| StateSchemaError::VersionIsZero
		| StateSchemaError::DuplicateVersion { .. } => InstallationControlError::InvalidSchemaDeclaration,
	}
}
