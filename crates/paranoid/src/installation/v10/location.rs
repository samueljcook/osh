//! Parsed published-source locations with no operational legacy Store settings.

use super::{
	CounterKey, PgNativeSourceGate, PgNativeStorageBindings, PgQualifiedTableName,
	PgV10SourceLayout, PgV10SourceRole, Protocol, RootKey, SOURCE_BINDING_VERSION, SchemaError,
	SourceRole, SourceRoleKind, Tx, V10SourceBindingError, check_native_role_separation,
	check_role_separation, fmt, kv, source_fingerprint, validate_retained_source_metadata,
	validate_source_heap,
};
use primitives_rs::crypto::algorithms::blake3;

/// One explicit app-selected source role and its parsed qualified relation.
#[derive(Clone)]
pub(crate) struct PgV10SourceRoleBinding {
	pub(crate) role: PgV10SourceRole,
	pub(crate) table: PgQualifiedTableName,
}

/// Source layout is discovered from recorded database evidence, never configured here.
pub(crate) struct PgV10SourceLocationConfig {
	pub(crate) roles: Vec<PgV10SourceRoleBinding>,
	pub(crate) fleet_root: RootKey,
	pub(crate) requires_raw_kv_updated_at_index: bool,
	pub(crate) requires_fleet_state_updated_at_index: bool,
}

/// Actual mapping facts shared by preparation, durable descriptors, and source routes.
#[derive(Clone)]
pub(crate) struct PgV10SourceLocation {
	roles: Vec<PgV10SourceRoleBinding>,
	fleet_root: RootKey,
	requires_raw_kv_updated_at_index: bool,
	requires_fleet_state_updated_at_index: bool,
	gate: PgNativeSourceGate,
}

impl PgV10SourceLocation {
	pub(crate) fn new(
		mut config: PgV10SourceLocationConfig,
	) -> Result<Self, V10SourceBindingError> {
		if config.roles.len() != PgV10SourceRole::COUNT {
			return Err(V10SourceBindingError::InvalidSourceRoles);
		}
		for (index, role) in config.roles.iter().enumerate() {
			if config.roles[..index]
				.iter()
				.any(|prior| prior.role == role.role)
			{
				return Err(V10SourceBindingError::InvalidSourceRoles);
			}
		}
		config.roles.sort_by_key(|binding| binding.role);
		let protocol = Protocol::new(
			config.roles[PgV10SourceRole::Protocol as usize]
				.table
				.clone(),
		);
		let location = Self {
			roles: config.roles,
			fleet_root: config.fleet_root,
			requires_raw_kv_updated_at_index: config.requires_raw_kv_updated_at_index,
			requires_fleet_state_updated_at_index: config.requires_fleet_state_updated_at_index,
			gate: PgNativeSourceGate::new(protocol),
		};
		check_role_separation(&location.roles())?;
		Ok(location)
	}

	pub(crate) fn table(&self, role: PgV10SourceRole) -> &PgQualifiedTableName {
		&self.roles[role as usize].table
	}
	pub(crate) fn fleet_root(&self) -> &RootKey {
		&self.fleet_root
	}
	pub(crate) fn requires_raw_kv_updated_at_index(&self) -> bool {
		self.requires_raw_kv_updated_at_index
	}
	pub(crate) fn requires_fleet_state_updated_at_index(&self) -> bool {
		self.requires_fleet_state_updated_at_index
	}
	pub(crate) fn gate(&self) -> &PgNativeSourceGate {
		&self.gate
	}
	pub(crate) fn protocol(&self) -> &Protocol {
		self.gate.protocol()
	}
	pub(crate) fn protocol_ledger(&self) -> &PgQualifiedTableName {
		self.table(PgV10SourceRole::ProtocolLedger)
	}
	pub(crate) fn key_algorithm_version(&self) -> u32 {
		SOURCE_BINDING_VERSION
	}
	pub(crate) fn fingerprint(&self) -> blake3::Hash {
		source_fingerprint(
			&self.roles(),
			&self.fleet_root,
			self.requires_raw_kv_updated_at_index,
			self.requires_fleet_state_updated_at_index,
		)
	}
	pub(crate) fn check_native_separation(
		&self,
		native: &PgNativeStorageBindings,
	) -> Result<(), V10SourceBindingError> {
		check_native_role_separation(&self.roles(), native)
	}
	pub(crate) fn matches_location(&self, other: &Self) -> bool {
		self.roles
			.iter()
			.zip(&other.roles)
			.all(|(left, right)| left.role == right.role && left.table == right.table)
			&& self.fleet_root == other.fleet_root
			&& self.requires_raw_kv_updated_at_index == other.requires_raw_kv_updated_at_index
			&& self.requires_fleet_state_updated_at_index
				== other.requires_fleet_state_updated_at_index
	}
	pub(crate) fn counter_source_key(
		&self,
		key: &CounterKey,
	) -> Result<kv::Key, V10SourceBindingError> {
		Ok(crate::fleet::v10::source_counter_key(
			self.fleet_root(),
			key,
		)?)
	}

	pub(crate) async fn validate_retained_protocol(
		&self,
		tx: &mut Tx<'_>,
	) -> Result<(), SchemaError> {
		validate_retained_source_metadata(
			tx,
			self.protocol(),
			self.protocol_ledger(),
			&self.roles(),
		)
		.await
	}

	/// Reads owning component registrations using source names only as instance-key facts.
	pub(super) async fn read_retained_layout(
		&self,
		tx: &mut Tx<'_>,
	) -> Result<PgV10SourceLayout, SchemaError> {
		use PgV10SourceRole as Role;
		for binding in [
			crate::kv::raw::v10::RecordedKvSchemaBinding {
				table: self.table(Role::RawKv),
				ledger: self.table(Role::RawKvLedger),
			},
			crate::kv::raw::v10::RecordedKvSchemaBinding {
				table: self.table(Role::FleetState),
				ledger: self.table(Role::FleetLedger),
			},
		] {
			crate::kv::raw::v10::validate_recorded_kv_schema_in_current_transaction(tx, binding)
				.await?;
		}
		let fleet = crate::fleet::v10::read_published_v10_layout_in_current_transaction(
			tx,
			crate::fleet::v10::FleetSourceSchemaTables {
				ledger: self.table(Role::FleetLedger),
				root: self.fleet_root(),
				state: self.table(Role::FleetState),
				claim_table: self.table(Role::FleetClaims),
				fencing_counter_table: self.table(Role::FleetFencing),
			},
		)
		.await?;
		let queue = crate::queue::read_published_v10_layout_in_current_transaction(
			tx,
			crate::queue::QueueSourceSchemaTables {
				ledger: self.table(Role::QueueLedger),
				jobs: self.table(Role::QueueJobs),
				dead_letters: self.table(Role::QueueDeadLetters),
				pauses: self.table(Role::QueuePauses),
			},
		)
		.await?;
		Ok(PgV10SourceLayout { fleet, queue })
	}

	/// The caller retains the source gate. All evidence is read-only and no table is repaired.
	pub(crate) async fn read_validated_layout(
		&self,
		tx: &mut Tx<'_>,
	) -> Result<PgV10SourceLayout, SchemaError> {
		self.validate_retained_protocol(tx).await?;
		for role in self
			.roles()
			.iter()
			.filter(|entry| entry.role.kind() == SourceRoleKind::State)
		{
			validate_source_heap(role.table, tx).await?;
		}
		let layout = self.read_retained_layout(tx).await?;
		self.validate_physical_source(tx, layout).await?;
		Ok(layout)
	}

	async fn validate_physical_source(
		&self,
		tx: &mut Tx<'_>,
		layout: PgV10SourceLayout,
	) -> Result<(), SchemaError> {
		use PgV10SourceRole as Role;
		for binding in [
			crate::kv::raw::v10::KvPhysicalSchemaBinding {
				table: self.table(Role::RawKv),
				requires_updated_at_index: self.requires_raw_kv_updated_at_index,
			},
			crate::kv::raw::v10::KvPhysicalSchemaBinding {
				table: self.table(Role::FleetState),
				requires_updated_at_index: self.requires_fleet_state_updated_at_index,
			},
		] {
			crate::kv::raw::v10::validate_kv_physical_schema_in_current_transaction(tx, binding)
				.await?;
		}
		let lease = crate::fleet::lease::StoreConfig {
			protocol: self.protocol().clone(),
			claim_table: self.table(Role::FleetClaims).clone(),
			fencing_counter_table: self.table(Role::FleetFencing).clone(),
		};
		crate::fleet::lease::validate_schema_in_current_transaction(tx, &lease).await?;
		crate::queue::validate_published_v10_physical_in_current_transaction(
			tx,
			crate::queue::QueueSourceSchemaTables {
				ledger: self.table(Role::QueueLedger),
				jobs: self.table(Role::QueueJobs),
				dead_letters: self.table(Role::QueueDeadLetters),
				pauses: self.table(Role::QueuePauses),
			},
			layout.queue_layout(),
		)
		.await
	}

	pub(super) fn role_bindings(&self) -> &[PgV10SourceRoleBinding] {
		&self.roles
	}
	pub(super) fn roles(&self) -> [SourceRole<'_>; PgV10SourceRole::COUNT] {
		std::array::from_fn(|index| SourceRole {
			role: self.roles[index].role,
			table: &self.roles[index].table,
		})
	}
}

impl fmt::Debug for PgV10SourceLocation {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("PgV10Location")
			.finish_non_exhaustive()
	}
}
