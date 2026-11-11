//! Durable, parsed published-source facts independent of legacy store handles.

use super::{
	CounterKey, Deserialize, PgNativeSourceGate, PgNativeStorageBindings, PgQualifiedTableName,
	PgV10SourceLayout, PgV10SourceLocation, PgV10SourceLocationConfig, PgV10SourceRoleBinding,
	Protocol, SOURCE_BINDING_VERSION, SchemaError, Serialize, SourceRoleKind, Tx,
	V10SourceBindingError, fmt, kv,
};
use crate::db::{PgIdentifier, PgSchemaName};
use crate::fleet::RootKey;
use primitives_rs::crypto::algorithms::blake3;

const DESCRIPTOR_VERSION: u32 = 1;

/// A role has one owning association; aliases between ledger roles remain explicit.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PgV10SourceRole {
	Protocol,
	ProtocolLedger,
	RawKvLedger,
	RawKv,
	FleetLedger,
	FleetState,
	FleetClaims,
	FleetFencing,
	QueueLedger,
	QueueJobs,
	QueueDeadLetters,
	QueuePauses,
}

impl PgV10SourceRole {
	pub(super) const COUNT: usize = Self::QueuePauses as usize + 1;

	pub(super) fn name(self) -> &'static str {
		match self {
			Self::Protocol => "protocol",
			Self::ProtocolLedger => "protocol ledger",
			Self::RawKvLedger => "raw KV ledger",
			Self::RawKv => "raw KV",
			Self::FleetLedger => "Fleet ledger",
			Self::FleetState => "Fleet state",
			Self::FleetClaims => "Fleet claims",
			Self::FleetFencing => "Fleet fencing",
			Self::QueueLedger => "Queue ledger",
			Self::QueueJobs => "Queue jobs",
			Self::QueueDeadLetters => "Queue dead letters",
			Self::QueuePauses => "Queue pauses",
		}
	}

	pub(super) fn kind(self) -> SourceRoleKind {
		match self {
			Self::Protocol => SourceRoleKind::Protocol,
			Self::ProtocolLedger | Self::RawKvLedger | Self::FleetLedger | Self::QueueLedger => {
				SourceRoleKind::Ledger
			}
			_ => SourceRoleKind::State,
		}
	}
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceRoleWire {
	role: PgV10SourceRole,
	schema: String,
	table: String,
}

/// Wire DTOs enter the parsed descriptor before they can supply lookup or fingerprint facts.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PgV10SourceDescriptorWire {
	descriptor_version: u32,
	key_algorithm_version: u32,
	roles: Vec<SourceRoleWire>,
	fleet_root: String,
	requires_raw_kv_updated_at_index: bool,
	requires_fleet_state_updated_at_index: bool,
	layout: PgV10SourceLayout,
}

/// Exact immutable source mappings and validated layout captured by import preparation.
/// Tokens, holder prefixes, queue policies, credentials, and pool settings are excluded.
#[derive(Clone)]
pub(crate) struct PgV10SourceDescriptor {
	location: PgV10SourceLocation,
	layout: PgV10SourceLayout,
}

impl PgV10SourceDescriptor {
	pub(crate) fn from_location(location: PgV10SourceLocation, layout: PgV10SourceLayout) -> Self {
		Self { location, layout }
	}
	pub(crate) fn parse(wire: PgV10SourceDescriptorWire) -> Result<Self, V10SourceBindingError> {
		if wire.descriptor_version != DESCRIPTOR_VERSION {
			return Err(V10SourceBindingError::UnsupportedDescriptorVersion);
		}
		if wire.key_algorithm_version != SOURCE_BINDING_VERSION {
			return Err(V10SourceBindingError::UnsupportedKeyAlgorithmVersion);
		}
		let roles = wire
			.roles
			.into_iter()
			.map(|role| {
				let schema = PgSchemaName::from_identifier_text(&role.schema)
					.map_err(|_| V10SourceBindingError::InvalidDescriptorIdentifier)?;
				let table = PgIdentifier::new(&role.table)
					.map_err(|_| V10SourceBindingError::InvalidDescriptorIdentifier)?;
				Ok(PgV10SourceRoleBinding {
					role: role.role,
					table: PgQualifiedTableName::new(schema, table),
				})
			})
			.collect::<Result<Vec<_>, V10SourceBindingError>>()?;
		let fleet_root = RootKey::new(wire.fleet_root)
			.map_err(|_| V10SourceBindingError::InvalidDescriptorRoot)?;
		let location = PgV10SourceLocation::new(PgV10SourceLocationConfig {
			roles,
			fleet_root,
			requires_raw_kv_updated_at_index: wire.requires_raw_kv_updated_at_index,
			requires_fleet_state_updated_at_index: wire.requires_fleet_state_updated_at_index,
		})?;
		Ok(Self::from_location(location, wire.layout))
	}
	pub(crate) fn to_wire(&self) -> PgV10SourceDescriptorWire {
		PgV10SourceDescriptorWire {
			descriptor_version: DESCRIPTOR_VERSION,
			key_algorithm_version: self.location.key_algorithm_version(),
			roles: self
				.location
				.role_bindings()
				.iter()
				.map(|binding| SourceRoleWire {
					role: binding.role,
					schema: binding.table.schema().as_str().to_owned(),
					table: binding.table.table().as_str().to_owned(),
				})
				.collect(),
			fleet_root: self.fleet_root().as_str().to_owned(),
			requires_raw_kv_updated_at_index: self.requires_raw_kv_updated_at_index(),
			requires_fleet_state_updated_at_index: self.requires_fleet_state_updated_at_index(),
			layout: self.layout,
		}
	}
	pub(crate) fn location(&self) -> &PgV10SourceLocation {
		&self.location
	}
	pub(crate) fn table(&self, role: PgV10SourceRole) -> &PgQualifiedTableName {
		self.location.table(role)
	}
	pub(crate) fn fleet_root(&self) -> &RootKey {
		self.location.fleet_root()
	}
	pub(crate) fn requires_raw_kv_updated_at_index(&self) -> bool {
		self.location.requires_raw_kv_updated_at_index()
	}
	pub(crate) fn requires_fleet_state_updated_at_index(&self) -> bool {
		self.location.requires_fleet_state_updated_at_index()
	}
	pub(crate) fn layout(&self) -> PgV10SourceLayout {
		self.layout
	}
	pub(crate) fn gate(&self) -> &PgNativeSourceGate {
		self.location.gate()
	}
	pub(crate) fn protocol(&self) -> &Protocol {
		self.location.protocol()
	}
	pub(crate) fn protocol_ledger(&self) -> &PgQualifiedTableName {
		self.location.protocol_ledger()
	}
	pub(crate) fn fingerprint(&self) -> blake3::Hash {
		self.location.fingerprint()
	}
	pub(crate) fn check_native_separation(
		&self,
		native: &PgNativeStorageBindings,
	) -> Result<(), V10SourceBindingError> {
		self.location.check_native_separation(native)
	}
	pub(crate) fn matches_descriptor(&self, other: &Self) -> bool {
		self.location.matches_location(&other.location) && self.layout == other.layout
	}
	pub(crate) async fn validate_retained_protocol(
		&self,
		tx: &mut Tx<'_>,
	) -> Result<(), SchemaError> {
		self.location.validate_retained_protocol(tx).await
	}
	pub(crate) async fn validate_retained_layout(
		&self,
		tx: &mut Tx<'_>,
	) -> Result<(), SchemaError> {
		self.require_layout(self.location.read_retained_layout(tx).await?)
	}
	pub(crate) fn counter_source_key(
		&self,
		key: &CounterKey,
	) -> Result<kv::Key, V10SourceBindingError> {
		self.location.counter_source_key(key)
	}
	pub(crate) async fn validate_existing(&self, tx: &mut Tx<'_>) -> Result<(), SchemaError> {
		self.require_layout(self.location.read_validated_layout(tx).await?)
	}
	fn require_layout(&self, actual: PgV10SourceLayout) -> Result<(), SchemaError> {
		if actual != self.layout {
			return Err(SchemaError::mismatch(
				"retained source component layouts differ from durable descriptor",
			));
		}
		Ok(())
	}
}

impl fmt::Debug for PgV10SourceDescriptor {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("PgV10SourceDescriptor")
			.field("layout", &self.layout)
			.finish_non_exhaustive()
	}
}
