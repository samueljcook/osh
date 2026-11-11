//! Owned fresh native installation and read-only verification of recorded native bindings.
//!
//! PostgreSQL interprets the neutral control record's opaque backend registration.
//! Recorded origin and source phase bind explicit mappings; preparation does not retire source writers.

use primitives_rs::crypto::algorithms::blake3;
use std::{
	fmt,
	num::{NonZeroU32, NonZeroU64},
	sync::Arc,
};

use primitives_rs::id::time_sortable::SortableId;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::fleet::counter::state::{CounterComponent, CounterRepresentationError};
use crate::fleet::lease::{LeaseComponent, LeaseSourceBridge};
use crate::fleet::{scalar::FleetScalarCode, scalar_codec::ScalarStateError};
use crate::installation::contract::{
	BoundStateSchema, ComponentDeclaration, StateContractError, StateStorageEnvelope,
};
use crate::installation::control_state::{
	InstallationControlComponent, InstallationControlError, InstallationControlFields,
	InstallationControlReader, InstallationControlState,
};
use crate::kv::raw::core_component::CoreRawComponent;
use crate::queue::{
	PublishedV10QueueLayout, QueueAuxiliaryComponent, QueueGroupError, QueueJobComponent,
	QueueRecoveryPolicy, QueueSourceImportDefs,
};
use crate::records::{PointRecordRead, RecordKey, RecordMetadata, RecordNamespace};
use crate::runtime::participation::state::{ParticipantSchema, ParticipationSemantics};
use crate::scope::{ScopePolicySet, selection::ScopeSelections};

use super::BootstrapError;
use super::administration::PgNativeAdministration;
use super::binding_fingerprint::frame_fingerprint_input;
use super::bootstrap_lock::acquire_transaction_lock;
use crate::db::{
	DatabaseError, SchemaError, TransactionFailure, WritePool, run_transaction_read,
	run_transaction_write,
};
use crate::installation::ProtocolError;
use crate::installation::control_storage::{ControlPinError, PgControlStorage};
use crate::installation::protocol_schema::is_protocol_table;
use crate::installation::v10::source_gate::NativeSourceGatePhase;
use crate::installation::v10::{
	PgV10SourceDescriptor, PgV10SourceLocation, PgV10SourceRole, V10SourceBindingError,
};
use crate::records::postgres::PgPointStorage;

mod app_storage;
mod backend_registration;
mod fleet_scalar;
mod initial_document_indexes;
mod initial_policy;
mod initial_scalars;
use backend_registration::BackendOrigin;
pub(crate) use backend_registration::{
	DocumentIndexLedgerSlot, PgBackendRegistration, PgBackendRegistrationBindingError,
	PgBackendRegistrationCodecError, PgBackendRegistrationSchema,
};
pub(crate) use fleet_scalar::{
	FleetScalarPlacementError, PgNativeFleetScalarStorage, PgNativeFleetScalarStorageFields,
};
use initial_document_indexes::{InitialDocumentIndexes, require_initial_index_ledger};
pub(crate) use v10::source_phase::source_data_retired_registration;
pub(crate) use v10::source_phase::source_writers_retired_registration;

/// Exact encoded and parsed next backend state; this preparation grants no mutation authority.
pub(crate) struct PreparedPgBackendRegistration {
	encoded: Box<RawValue>,
	registration: PgBackendRegistration,
}

impl PreparedPgBackendRegistration {
	pub(crate) fn into_parts(self) -> (Box<RawValue>, PgBackendRegistration) {
		(self.encoded, self.registration)
	}
}
mod queue_storage;
pub(crate) mod v10;
use app_storage::PgAppStorageBindings;
pub(crate) use queue_storage::{PgNativeQueueStorage, PgNativeQueueStorageFields};

const PHYSICAL_LAYOUT_VERSION: u32 = 1;
const FINGERPRINT_DOMAIN: &[u8] = b"paranoid.pg-native-storage-bindings";
const CREATE_FRESH_LABEL: &str = "db.native_installation.create_fresh";
const OPEN_RECORDED_LABEL: &str = "db.native_installation.open_recorded";
use crate::db::{
	DatabaseOperationKind, PgQualifiedTableName, PgSchemaName, Tx, WriteTx, owned_portable_query,
	owned_portable_query_as,
};

const INSPECT_LABEL: &str = "db.native_installation.inspect";
const CREATE_SCHEMA_LABEL: &str = "db.native_installation.create_schema";
const RELATION_EXISTS_SQL: &str = "SELECT pg_catalog.to_regclass($1) IS NOT NULL";
const SCHEMA_EXISTS_SQL: &str = "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_namespace \
	WHERE nspname OPERATOR(pg_catalog.=) $1)";

/// Creates each schema of the installation's tables that does not exist. PostgreSQL checks the
/// database's `CREATE` privilege for `CREATE SCHEMA IF NOT EXISTS` even when the schema exists,
/// so the statement runs only for an absent schema, and an installation in an existing schema
/// needs no privilege on the database. The caller holds the installation lock, so concurrent
/// creations do not race.
async fn create_absent_schemas(
	tx: &mut WriteTx<'_>,
	bindings: &PgNativeStorageBindings,
) -> Result<(), DatabaseError> {
	let schemas: std::collections::BTreeSet<&PgSchemaName> = bindings
		.unique_containers()
		.into_iter()
		.map(|storage| storage.table().schema())
		.collect();
	for schema in schemas {
		tx.record_database_operation(
			DatabaseOperationKind::FetchOne,
			CREATE_SCHEMA_LABEL,
			Some(SCHEMA_EXISTS_SQL),
		);
		let (exists,) = owned_portable_query_as::<(bool,)>(SCHEMA_EXISTS_SQL)
			.bind_validated(schema.as_str())
			.fetch_one(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(CREATE_SCHEMA_LABEL))?;
		if exists {
			continue;
		}
		let create = crate::audited_sql!("CREATE SCHEMA IF NOT EXISTS {schema}", schema = schema);
		tx.record_database_operation(
			DatabaseOperationKind::Execute,
			CREATE_SCHEMA_LABEL,
			Some(create.as_str()),
		);
		owned_portable_query(create)
			.execute(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(CREATE_SCHEMA_LABEL))?;
	}
	Ok(())
}

/// Actual storage object and namespace for one ordinary native role.
#[derive(Clone)]
pub(crate) struct PgNativeNamespaceBinding {
	storage: PgPointStorage,
	namespace: RecordNamespace,
}

impl PgNativeNamespaceBinding {
	pub(crate) fn new(storage: PgPointStorage, namespace: RecordNamespace) -> Self {
		Self { storage, namespace }
	}
	pub(crate) fn storage(&self) -> &PgPointStorage {
		&self.storage
	}
	pub(crate) fn namespace(&self) -> &RecordNamespace {
		&self.namespace
	}
}

/// Explicit unverified mappings supplied together for collision parsing.
pub(crate) struct PgNativeStorageBindingsFields {
	pub(crate) control: PgControlStorage,
	pub(crate) participants: PgNativeNamespaceBinding,
	pub(crate) counters: PgNativeNamespaceBinding,
	pub(crate) component_schemas: PgNativeNamespaceBinding,
	pub(crate) leases: PgNativeNamespaceBinding,
	pub(crate) raw_kv: PgNativeNamespaceBinding,
	pub(crate) queue: PgNativeQueueStorageFields,
	pub(crate) fleet_scalar: PgNativeFleetScalarStorageFields,
	/// The one table that holds every application scope, each in its own namespace.
	pub(crate) app: PgPointStorage,
}

/// Parsed immutable mappings. Parsing itself verifies no database or installed record.
#[derive(Clone)]
pub(crate) struct PgNativeStorageBindings {
	control: PgControlStorage,
	participants: PgNativeNamespaceBinding,
	counters: PgNativeNamespaceBinding,
	component_schemas: PgNativeNamespaceBinding,
	leases: PgNativeNamespaceBinding,
	raw_kv: PgNativeNamespaceBinding,
	queue: PgNativeQueueStorage,
	fleet_scalar: PgNativeFleetScalarStorage,
	app_storage: PgAppStorageBindings,
	backend_schema: Arc<PgBackendRegistrationSchema>,
}

struct BindingRole<'a> {
	name: &'static str,
	storage: &'a PgPointStorage,
	namespace: &'a RecordNamespace,
	control_key: Option<&'a RecordKey>,
}

impl PgNativeStorageBindings {
	pub(crate) fn new(
		fields: PgNativeStorageBindingsFields,
	) -> Result<Self, NativeInstallationError> {
		let bindings = Self {
			control: fields.control,
			participants: fields.participants,
			counters: fields.counters,
			component_schemas: fields.component_schemas,
			leases: fields.leases,
			raw_kv: fields.raw_kv,
			queue: PgNativeQueueStorage::new(fields.queue)?,
			fleet_scalar: PgNativeFleetScalarStorage::new(fields.fleet_scalar)?,
			app_storage: PgAppStorageBindings::new(fields.app),
			backend_schema: Arc::new(PgBackendRegistrationSchema::new()),
		};
		let roles = bindings.roles();
		for (index, role) in roles.iter().enumerate().skip(1) {
			if bindings.control.shares_container(role.storage) {
				return Err(NativeInstallationError::SharedControlContainer);
			}
			for prior in &roles[1..index] {
				if role.storage.shares_container(prior.storage) && role.namespace == prior.namespace
				{
					return Err(NativeInstallationError::OverlappingNamespaces {
						left: prior.name,
						right: role.name,
					});
				}
			}
		}
		if bindings
			.control
			.shares_container(bindings.app_storage.storage())
		{
			return Err(NativeInstallationError::SharedControlContainer);
		}
		Ok(bindings)
	}

	pub(crate) fn backend_schema(&self) -> &PgBackendRegistrationSchema {
		&self.backend_schema
	}

	pub(crate) fn control(&self) -> &PgControlStorage {
		&self.control
	}
	pub(crate) fn participants(&self) -> &PgNativeNamespaceBinding {
		&self.participants
	}
	pub(crate) fn counters(&self) -> &PgNativeNamespaceBinding {
		&self.counters
	}
	pub(crate) fn component_schemas(&self) -> &PgNativeNamespaceBinding {
		&self.component_schemas
	}
	pub(crate) fn raw_kv(&self) -> &PgNativeNamespaceBinding {
		&self.raw_kv
	}
	pub(crate) fn leases(&self) -> &PgNativeNamespaceBinding {
		&self.leases
	}
	pub(crate) fn queue(&self) -> &PgNativeQueueStorage {
		&self.queue
	}
	pub(crate) fn fleet_scalar(&self) -> &PgNativeFleetScalarStorage {
		&self.fleet_scalar
	}

	/// Recorded inactive entries preserve physical metadata without authorizing a role.
	/// An active maintained identity must also match its complete current logical shape.
	fn check_document_index_associations(
		&self,
		registration: &PgBackendRegistration,
		policies: &ScopePolicySet,
	) -> Result<(), NativeInstallationError> {
		let containers = self.unique_containers();
		let roles = self.roles();
		for (entry_index, entry) in registration.document_indexes().iter().enumerate() {
			let storage = self.app_storage(entry.scope())?;
			if storage.table() != entry.table() {
				return Err(NativeInstallationError::DocumentIndexContainer { entry_index });
			}
			if roles.iter().any(|role| {
				storage.shares_container(role.storage)
					&& entry.scope().namespace() == role.namespace
			}) {
				return Err(NativeInstallationError::DocumentIndexReservedScope { entry_index });
			}
			if containers.iter().any(|container| {
				container.table().schema() == entry.table().schema()
					&& container.table().table() == entry.index_name()
			}) {
				return Err(NativeInstallationError::DocumentIndexContainerName { entry_index });
			}
			let Ok(policy) = policies.lookup(entry.scope()) else {
				continue;
			};
			let crate::scope::ScopeOwner::Document(document) = policy.owner() else {
				continue;
			};
			let Some(maintained) = document
				.maintenance_indexes()
				.find(|index| index.identity() == entry.index().identity())
			else {
				continue;
			};
			if document.schema().identifier() != entry.schema_identifier() {
				return Err(NativeInstallationError::DocumentIndexOwner { entry_index });
			}
			if !maintained.matches_descriptor(entry.index()) {
				return Err(NativeInstallationError::DocumentIndexShape { entry_index });
			}
		}
		Ok(())
	}

	// Collision checks, container enumeration, and fingerprints use this actual mapping view.
	fn roles(&self) -> Vec<BindingRole<'_>> {
		let mut roles = self.non_fleet_roles();
		roles.extend(self.fleet_scalar().roles());
		roles
	}

	fn non_fleet_roles(&self) -> Vec<BindingRole<'_>> {
		let mut roles = vec![
			BindingRole {
				name: "control",
				storage: self.control.point_storage(),
				namespace: self.control.identity().namespace(),
				control_key: Some(self.control.identity().key()),
			},
			BindingRole {
				name: "participation",
				storage: self.participants.storage(),
				namespace: self.participants.namespace(),
				control_key: None,
			},
			BindingRole {
				name: "Counter",
				storage: self.counters.storage(),
				namespace: self.counters.namespace(),
				control_key: None,
			},
			BindingRole {
				name: "component schema ledger",
				storage: self.component_schemas.storage(),
				namespace: self.component_schemas.namespace(),
				control_key: None,
			},
			BindingRole {
				name: "leases",
				storage: self.leases.storage(),
				namespace: self.leases.namespace(),
				control_key: None,
			},
			BindingRole {
				name: "raw KV",
				storage: self.raw_kv.storage(),
				namespace: self.raw_kv.namespace(),
				control_key: None,
			},
		];
		roles.extend(self.queue().roles());
		roles
	}

	pub(crate) fn raw_record_containers(&self) -> Vec<&PgPointStorage> {
		let core = self.raw_kv.storage();
		let app = self.app_storage.storage();
		if core.shares_container(app) {
			vec![core]
		} else {
			vec![core, app]
		}
	}

	pub(crate) fn unique_containers(&self) -> Vec<&PgPointStorage> {
		let mut containers = Vec::new();
		for role in self.roles() {
			if !containers
				.iter()
				.any(|prior: &&PgPointStorage| role.storage.shares_container(prior))
			{
				containers.push(role.storage);
			}
		}
		let application = self.app_storage.storage();
		if !containers
			.iter()
			.any(|prior| application.shares_container(prior))
		{
			containers.push(application);
		}
		containers
	}

	fn fingerprint(&self, origin: NativeMappingOrigin) -> blake3::Hash {
		let mut hasher = blake3::Hasher::new();
		frame_fingerprint_input(&mut hasher, FINGERPRINT_DOMAIN);
		frame_fingerprint_input(&mut hasher, &PHYSICAL_LAYOUT_VERSION.to_be_bytes());
		frame_fingerprint_input(&mut hasher, origin.fingerprint_tag());
		for role in self.non_fleet_roles() {
			frame_fingerprint_input(&mut hasher, role.name.as_bytes());
			frame_fingerprint_input(
				&mut hasher,
				role.storage.table().schema().as_str().as_bytes(),
			);
			frame_fingerprint_input(
				&mut hasher,
				role.storage.table().table().as_str().as_bytes(),
			);
			frame_fingerprint_input(&mut hasher, role.namespace.as_bytes());
			match role.control_key {
				Some(key) => {
					frame_fingerprint_input(&mut hasher, &[1]);
					frame_fingerprint_input(&mut hasher, key.as_bytes());
				}
				None => frame_fingerprint_input(&mut hasher, &[0]),
			}
		}
		self.app_storage.frame_fingerprint(&mut hasher);
		self.queue().frame_index_fingerprint(&mut hasher);
		self.fleet_scalar().frame_fingerprint(&mut hasher);
		hasher.finalize()
	}

	fn require_fleet_scalar_registration(
		&self,
		registration: &PgBackendRegistration,
	) -> Result<(), NativeInstallationError> {
		if registration.fleet_scalar.as_ref() != Some(self.fleet_scalar().placement()) {
			return Err(NativeInstallationError::FleetScalarPlacementMismatch);
		}
		Ok(())
	}

	/// The initial backend registration with ledger entries for the physical index of each
	/// query-ready index in `policies`.
	fn registration(
		&self,
		origin: BackendOrigin,
		backend: &BoundStateSchema<'_, PgBackendRegistration, PgBackendRegistrationCodecError>,
		policies: &ScopePolicySet,
		indexes: &InitialDocumentIndexes<'_>,
	) -> Result<PreparedPgBackendRegistration, NativeInstallationError> {
		let mut registration = PgBackendRegistration::initial(
			self.fingerprint(origin.mapping_origin()),
			origin,
			Some(self.fleet_scalar().placement().clone()),
		);
		for descriptor in indexes.descriptors() {
			registration = registration.with_document_index(descriptor.clone())?;
		}
		self.check_document_index_associations(&registration, policies)?;
		require_initial_index_ledger(policies, &registration)?;
		let encoded = self
			.backend_schema
			.encode_bound_document(&registration, backend)?;
		Ok(PreparedPgBackendRegistration {
			encoded,
			registration,
		})
	}
}

impl fmt::Debug for PgNativeStorageBindings {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("PgNativeStorageBindings")
			.field("unique_container_count", &self.unique_containers().len())
			.finish_non_exhaustive()
	}
}

/// Code-bound application choices, explicit Queue settings, generation and conflict budget.
pub(crate) struct PgNativeFreshOptions<'code> {
	/// The document code, which builds the physical index of each query-ready index.
	pub(crate) documents: &'code crate::kv::document::schema::managed::ManagedDocumentRegistry,
	pub(crate) fleet_scalar_code: &'code FleetScalarCode,
	pub(crate) generation: NonZeroU64,
	pub(crate) scope_selections: ScopeSelections,
	pub(crate) queue_recovery: &'code QueueRecoveryPolicy,
	pub(crate) conflict_attempts: NonZeroU32,
	/// The v10 protocol tables that must not exist when creation holds the installation lock.
	pub(crate) unexpected_v10_protocols: &'code [UnexpectedV10Protocol<'code>],
}

/// A v10 protocol table that must not exist when creation or import holds the installation
/// lock. A v10 bootstrap creates its tables in one transaction that holds the same lock, so a
/// check under the lock sees every v10 installation that a bootstrap committed after opening
/// inspected the database.
#[derive(Clone, Copy)]
pub(crate) enum UnexpectedV10Protocol<'a> {
	/// The protocol table of a v10 installation in the installation's own schema, which the
	/// opening's v10 location does not name. The schema can hold an application's own relation
	/// of the same name, so only a relation that the catalog shows to be a v10 protocol table
	/// counts.
	InSchemaUnnamed(&'a PgQualifiedTableName),
	/// The protocol table at the opening's v10 location, which inspection found absent before
	/// opening chose to create an installation. Every relation of this name counts, as it does
	/// for inspection, which treats it as the v10 installation that opening imports.
	AtLocation(&'a PgQualifiedTableName),
}

impl UnexpectedV10Protocol<'_> {
	pub(crate) async fn require_absent(
		self,
		tx: &mut Tx<'_>,
	) -> Result<(), NativeInstallationError> {
		match self {
			Self::InSchemaUnnamed(table) => {
				if is_protocol_table(tx, table).await? {
					return Err(NativeInstallationError::V10InSchemaNotNamed);
				}
			}
			Self::AtLocation(table) => {
				if relation_exists(tx, table).await? {
					return Err(NativeInstallationError::V10AppearedAtLocation);
				}
			}
		}
		Ok(())
	}
}

/// Explicit recorded identity discovery or enforcement; neither mode mints an identity.
pub enum RecordedInstallationIdentity {
	/// Reads the existing recorded identity without creating a replacement.
	DiscoverRecorded,
	/// Rejects recorded state with an installation identity different from this value.
	Require(SortableId),
}

/// Verified NativeFresh bindings, constructible only after owned create/open completion.
pub(crate) struct PgNativeInstallation {
	bindings: PgNativeStorageBindings,
	control_component: InstallationControlComponent,
	installation_id: SortableId,
}

impl PgNativeInstallation {
	/// Creates every absent schema, every selected container, and the physical index of each
	/// query-ready index, and commits the registration. A failed or uncertain commit returns
	/// no verified installation or reusable generated ID.
	pub(crate) async fn create_fresh(
		control_pool: &WritePool,
		bindings: PgNativeStorageBindings,
		options: PgNativeFreshOptions<'_>,
	) -> Result<Self, NativeInstallationError> {
		let scope_policies = options
			.scope_selections
			.into_initial_policies()
			.map_err(initial_policy::InitialPolicyError::from)?;
		bindings.check_scope_policies(&scope_policies)?;
		let initial_indexes =
			InitialDocumentIndexes::new(&scope_policies, options.documents, &bindings)?;
		let installation_id = SortableId::generate().map_err(NativeInstallationError::Random)?;
		let component =
			InstallationControlComponent::new(bindings.control.identity().clone(), installation_id);
		let participant = ParticipantSchema::new();
		let declaration = PgNativeAdministration::declaration_from_schemas(
			&participant,
			&component,
			bindings.backend_schema(),
		);
		let component_policies =
			initial_policy::component_policies(initial_policy::InitialComponentPolicyFields {
				participant: &participant,
				control: &component,
				backend: bindings.backend_schema(),
				scalar_code: options.fleet_scalar_code,
				queue_recovery: options.queue_recovery,
				origin: NativeMappingOrigin::NativeFresh,
			})?;
		let (backend_registration, parsed_backend) = {
			let policy = component_policies
				.iter()
				.find(|policy| policy.component_identifier() == declaration.identifier())
				.ok_or(NativeInstallationError::AdministrativePolicyAbsent)?;
			let binding = declaration.bind(policy)?;
			let backend = binding.bind_schema(bindings.backend_schema().schema())?;
			bindings
				.registration(
					BackendOrigin::NativeFresh,
					&backend,
					&scope_policies,
					&initial_indexes,
				)?
				.into_parts()
		};
		let state = InstallationControlState::new(InstallationControlFields {
			installation_id,
			generation: options.generation,
			component_policies,
			scope_policies,
			backend_registration,
			pending_upgrade: None,
		})?;

		check_initial_policies(
			&state,
			&parsed_backend,
			&declaration,
			InitialInstallationMode::NativeFresh,
			bindings.queue(),
			options.fleet_scalar_code,
		)?;
		let binding = declaration.bind(
			state
				.component(declaration.identifier())
				.expect("initial required administrative policy was checked"),
		)?;
		let participant_schema = binding.bind_schema(participant.schema())?;
		if participant_schema.policy().writer().envelope != StateStorageEnvelope::NativeJson {
			return Err(NativeInstallationError::NativeAdminWriterRequired);
		}
		let backend_schema = binding.bind_schema(bindings.backend_schema().schema())?;
		bindings
			.backend_schema()
			.require_bound_writer(&backend_schema)?;
		let control_schema = binding.bind_schema(component.schema())?;
		component.require_bound_writer(&control_schema)?;
		run_transaction_write(control_pool, CREATE_FRESH_LABEL, async |tx| {
			acquire_transaction_lock(tx).await?;
			create_absent_schemas(tx, &bindings).await?;
			for unexpected in options.unexpected_v10_protocols {
				unexpected.require_absent(tx).await?;
			}
			for storage in bindings.unique_containers() {
				storage.create_fresh(tx).await?;
			}
			let queue = bindings.queue();
			queue
				.storage()
				.create_queue_selection_indexes(tx, queue.selection_indexes())
				.await?;
			for storage in bindings.raw_record_containers() {
				storage.create_record_expiry_index(tx).await?;
			}
			initial_indexes.create(tx).await?;
			let lock = bindings
				.control
				.lock_exclusive(tx, options.conflict_attempts)
				.await?;
			if lock.snapshot().record().is_some() {
				return Err(NativeInstallationError::ControlAlreadyPresent);
			}
			let record = component.encode_record(
				bindings.control.identity(),
				&state,
				&control_schema,
				RecordMetadata {
					expires_at: None,
					updated_at: lock.snapshot().observed_at(),
				},
			)?;
			lock.write(record).await?;
			Ok::<_, NativeInstallationError>(())
		})
		.await?;
		Ok(Self {
			bindings,
			control_component: component,
			installation_id,
		})
	}

	/// Reads only committed recorded state through an owned read-only pool transaction.
	/// Missing containers, metadata, or control are rejected without creation or reservations.
	pub(crate) async fn open_recorded(
		control_pool: &crate::db::Pool,
		bindings: PgNativeStorageBindings,
		identity: RecordedInstallationIdentity,
	) -> Result<Self, NativeInstallationError> {
		let participant = ParticipantSchema::new();
		let reader = InstallationControlReader::new(bindings.control.identity().clone());
		let (installation_id, control_component) =
			run_transaction_read(control_pool, OPEN_RECORDED_LABEL, async |tx| {
				// Pin before catalog validation so coordinated physical changes cannot split this observation.
				let snapshot = bindings.control.read_pinned_shared(tx).await?;
				let (state, control_component) =
					decode_recorded_control(reader, &bindings, &snapshot, identity, &participant)?;
				let declaration = PgNativeAdministration::declaration_from_schemas(
					&participant,
					&control_component,
					bindings.backend_schema(),
				);
				let bound = declaration.bind(
					state
						.component(declaration.identifier())
						.ok_or(NativeInstallationError::AdministrativePolicyAbsent)?,
				)?;
				let backend = bound.bind_schema(bindings.backend_schema().schema())?;
				verify_fresh_registration(&state, &bindings, &backend)?;
				for storage in bindings.unique_containers() {
					storage.validate_existing(tx).await?;
				}
				let queue = bindings.queue();
				queue
					.storage()
					.validate_queue_selection_indexes(tx, queue.selection_indexes())
					.await?;
				Ok::<_, NativeInstallationError>((state.installation_id(), control_component))
			})
			.await?;
		Ok(Self {
			control_component,
			bindings,
			installation_id,
		})
	}

	pub(crate) fn installation_id(&self) -> SortableId {
		self.installation_id
	}

	/// Consumes verified ownership; no fieldwise constructor of a verified wrapper is exposed.
	pub(crate) fn into_parts(self) -> (PgNativeStorageBindings, InstallationControlComponent) {
		(self.bindings, self.control_component)
	}
}

impl fmt::Debug for PgNativeInstallation {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("PgNativeInstallation")
			.field("bindings", &self.bindings)
			.finish_non_exhaustive()
	}
}

/// Persisted source lifecycle. Writer retirement preserves source-aware routing until cleanup.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ImportedPhase {
	Coexisting,
	SourceWritersRetired,
	SourceDataRetired,
}

#[derive(Clone, Copy)]
enum NativeMappingOrigin {
	NativeFresh,
	ImportedV10,
}

impl NativeMappingOrigin {
	fn fingerprint_tag(self) -> &'static [u8] {
		match self {
			Self::NativeFresh => b"native_fresh",
			Self::ImportedV10 => b"imported_v10",
		}
	}
}

/// What a database holds for one configured native layout and optional v10 source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InspectedInstallation {
	Absent,
	PublishedV10,
	Fresh,
	ImportedFromV10(ImportedPhase),
}

/// Reads, without changing anything, whether the layout's control record exists and how it was
/// installed, or else whether the v10 source's protocol table exists. When the caller supplies
/// `v10_location`, an installation that imported v10 from another location is an error.
pub(crate) async fn inspect_installation(
	pool: &crate::db::Pool,
	bindings: &PgNativeStorageBindings,
	v10_location: Option<&PgV10SourceLocation>,
) -> Result<InspectedInstallation, NativeInstallationError> {
	let v10_protocol_table = v10_location.map(|location| location.table(PgV10SourceRole::Protocol));
	let participant = ParticipantSchema::new();
	run_transaction_read(pool, INSPECT_LABEL, async |tx| {
		if relation_exists(tx, bindings.control.point_storage().table()).await? {
			let snapshot = bindings.control.read_pinned_shared(tx).await?;
			if snapshot.record().is_some() {
				let reader = InstallationControlReader::new(bindings.control.identity().clone());
				let (state, control_component) = decode_recorded_control(
					reader,
					bindings,
					&snapshot,
					RecordedInstallationIdentity::DiscoverRecorded,
					&participant,
				)?;
				let declaration = PgNativeAdministration::declaration_from_schemas(
					&participant,
					&control_component,
					bindings.backend_schema(),
				);
				let bound = declaration.bind(
					state
						.component(declaration.identifier())
						.ok_or(NativeInstallationError::AdministrativePolicyAbsent)?,
				)?;
				let backend = bound.bind_schema(bindings.backend_schema().schema())?;
				let registration = bindings
					.backend_schema
					.decode_bound_document(state.backend_registration(), &backend)?;
				return Ok(match registration.origin {
					// A v10 installation that the recorded installation never imported would
					// keep its data and writers apart from this installation.
					BackendOrigin::NativeFresh => {
						if let Some(protocol) = v10_protocol_table
							&& relation_exists(tx, protocol).await?
						{
							return Err(NativeInstallationError::V10BesideNativeInstallation);
						}
						InspectedInstallation::Fresh
					}
					BackendOrigin::ImportedV10 { descriptor, phase } => {
						if let Some(location) = v10_location
							&& !descriptor.location().matches_location(location)
						{
							return Err(NativeInstallationError::V10LocationMismatch);
						}
						InspectedInstallation::ImportedFromV10(phase)
					}
				});
			}
		}
		if let Some(protocol) = v10_protocol_table
			&& relation_exists(tx, protocol).await?
		{
			return Ok(InspectedInstallation::PublishedV10);
		}
		Ok::<_, NativeInstallationError>(InspectedInstallation::Absent)
	})
	.await
}

async fn relation_exists(
	tx: &mut Tx<'_>,
	table: &PgQualifiedTableName,
) -> Result<bool, NativeInstallationError> {
	tx.record_database_operation(
		DatabaseOperationKind::FetchOne,
		INSPECT_LABEL,
		Some(RELATION_EXISTS_SQL),
	);
	let (exists,) = owned_portable_query_as::<(bool,)>(RELATION_EXISTS_SQL)
		.bind_validated(table.quoted().to_string())
		.fetch_one(tx.inner.as_mut())
		.await
		.map_err(|error| SchemaError::from(error.with_operation_label(INSPECT_LABEL)))?;
	Ok(exists)
}

/// Rechecks the actual immutable native mapping on each fresh admission observation.
pub(crate) fn verify_fresh_registration(
	control: &InstallationControlState,
	bindings: &PgNativeStorageBindings,
	backend: &BoundStateSchema<'_, PgBackendRegistration, PgBackendRegistrationCodecError>,
) -> Result<PgBackendRegistration, NativeInstallationError> {
	let registration = bindings
		.backend_schema
		.decode_bound_document(control.backend_registration(), backend)?;
	if !matches!(&registration.origin, BackendOrigin::NativeFresh) {
		return Err(NativeInstallationError::WrongOrigin);
	}
	if registration.fingerprint != bindings.fingerprint(NativeMappingOrigin::NativeFresh) {
		return Err(NativeInstallationError::BindingFingerprintMismatch);
	}
	bindings.require_fleet_scalar_registration(&registration)?;
	bindings.check_scope_policies(control.scope_policies())?;
	bindings.check_document_index_associations(&registration, control.scope_policies())?;
	Ok(registration)
}

/// Descriptor-only admission still rechecks exact durable facts and observed source phase.
/// A descriptor supplies lookup facts; only installer opening validates retained physical proof.
pub(crate) fn inspect_descriptor_imported_registration(
	control: &InstallationControlState,
	bindings: &PgNativeStorageBindings,
	source: &PgV10SourceDescriptor,
	observed_gate: NativeSourceGatePhase,
	backend: &BoundStateSchema<'_, PgBackendRegistration, PgBackendRegistrationCodecError>,
) -> Result<(PgBackendRegistration, ImportedPhase), NativeInstallationError> {
	let registration = bindings
		.backend_schema
		.decode_bound_document(control.backend_registration(), backend)?;
	let (observed, phase) = require_imported_registration(control, bindings, &registration)?;
	require_observed_source_phase(phase, observed_gate)?;
	if !source.matches_descriptor(observed) {
		return Err(NativeInstallationError::SourceBindingFingerprintMismatch);
	}
	Ok((registration, phase))
}

fn recorded_imported_descriptor(
	control: &InstallationControlState,
	bindings: &PgNativeStorageBindings,
	backend: &BoundStateSchema<'_, PgBackendRegistration, PgBackendRegistrationCodecError>,
) -> Result<(PgV10SourceDescriptor, ImportedPhase), NativeInstallationError> {
	let registration = bindings
		.backend_schema
		.decode_bound_document(control.backend_registration(), backend)?;
	imported_descriptor_from_registration(control, bindings, registration)
}

fn recorded_imported_descriptor_format_only(
	control: &InstallationControlState,
	bindings: &PgNativeStorageBindings,
) -> Result<(PgV10SourceDescriptor, ImportedPhase), NativeInstallationError> {
	let registration = bindings
		.backend_schema()
		.decode_document(control.backend_registration())?;
	imported_descriptor_from_registration(control, bindings, registration)
}

fn imported_descriptor_from_registration(
	control: &InstallationControlState,
	bindings: &PgNativeStorageBindings,
	registration: PgBackendRegistration,
) -> Result<(PgV10SourceDescriptor, ImportedPhase), NativeInstallationError> {
	let (_, phase) = require_imported_registration(control, bindings, &registration)?;
	let BackendOrigin::ImportedV10 { descriptor, .. } = registration.origin else {
		unreachable!("validated imported registration contains its descriptor")
	};
	Ok((*descriptor, phase))
}

fn require_imported_registration<'registration>(
	control: &InstallationControlState,
	bindings: &PgNativeStorageBindings,
	registration: &'registration PgBackendRegistration,
) -> Result<(&'registration PgV10SourceDescriptor, ImportedPhase), NativeInstallationError> {
	bindings.require_fleet_scalar_registration(registration)?;
	bindings.check_document_index_associations(registration, control.scope_policies())?;
	let BackendOrigin::ImportedV10 { descriptor, phase } = &registration.origin else {
		return Err(NativeInstallationError::WrongOrigin);
	};
	descriptor.check_native_separation(bindings)?;
	if registration.fingerprint != bindings.fingerprint(NativeMappingOrigin::ImportedV10) {
		return Err(NativeInstallationError::BindingFingerprintMismatch);
	}
	bindings.check_scope_policies(control.scope_policies())?;
	Ok((descriptor, *phase))
}

impl PgNativeStorageBindings {
	/// Rechecks a proposed scope overlay against this already verified actual backend value.
	/// This pure association check supplies no namespace absence, catalog or mutation authority.
	pub(crate) fn require_scope_policy_associations(
		&self,
		registration: &PgBackendRegistration,
		policies: &ScopePolicySet,
	) -> Result<(), NativeInstallationError> {
		self.check_scope_policies(policies)?;
		self.check_document_index_associations(registration, policies)
	}

	fn require_document_index_registration(
		&self,
		current: &InstallationControlState,
		registration: &PgBackendRegistration,
	) -> Result<(), NativeInstallationError> {
		if registration.fingerprint != self.fingerprint(registration.origin.mapping_origin()) {
			return Err(NativeInstallationError::BindingFingerprintMismatch);
		}
		if let BackendOrigin::ImportedV10 { descriptor, .. } = &registration.origin {
			descriptor.check_native_separation(self)?;
		}
		self.require_fleet_scalar_registration(registration)?;
		self.check_scope_policies(current.scope_policies())?;
		self.check_document_index_associations(registration, current.scope_policies())?;
		Ok(())
	}

	/// Preserves full recorded origin, phase, placement and ledger while appending one entry.
	/// Actual control authority, code association and catalog correctness belong to the executor.
	pub(crate) fn encode_document_index_registration(
		&self,
		current: &InstallationControlState,
		backend: &BoundStateSchema<'_, PgBackendRegistration, PgBackendRegistrationCodecError>,
		registration: PgBackendRegistration,
		descriptor: &crate::kv::document::postgres::index::PgDocumentIndexDescriptor,
	) -> Result<PreparedPgBackendRegistration, NativeInstallationError> {
		self.require_document_index_registration(current, &registration)?;
		let registration = registration.with_document_index(descriptor.clone())?;
		self.check_document_index_associations(&registration, current.scope_policies())?;
		let encoded = self
			.backend_schema
			.encode_bound_document(&registration, backend)?;
		Ok(PreparedPgBackendRegistration {
			encoded,
			registration,
		})
	}
}

fn descriptor_registration(
	control: &InstallationControlState,
	bindings: &PgNativeStorageBindings,
	observed_gate: NativeSourceGatePhase,
	backend: &BoundStateSchema<'_, PgBackendRegistration, PgBackendRegistrationCodecError>,
) -> Result<(PgV10SourceDescriptor, ImportedPhase), NativeInstallationError> {
	let (descriptor, phase) = recorded_imported_descriptor(control, bindings, backend)?;
	require_observed_source_phase(phase, observed_gate)?;
	Ok((descriptor, phase))
}

fn require_observed_source_phase(
	phase: ImportedPhase,
	observed_gate: NativeSourceGatePhase,
) -> Result<(), NativeInstallationError> {
	let required_gate = match phase {
		ImportedPhase::Coexisting => NativeSourceGatePhase::PublishedV10,
		ImportedPhase::SourceWritersRetired | ImportedPhase::SourceDataRetired => {
			NativeSourceGatePhase::Retired
		}
	};
	if observed_gate != required_gate {
		return Err(NativeInstallationError::SourcePhaseMismatch {
			recorded: phase,
			observed: observed_gate,
		});
	}
	Ok(())
}

fn decode_recorded_control(
	reader: InstallationControlReader,
	bindings: &PgNativeStorageBindings,
	snapshot: &PointRecordRead,
	identity: RecordedInstallationIdentity,
	participant: &ParticipantSchema,
) -> Result<(InstallationControlState, InstallationControlComponent), NativeInstallationError> {
	let decoded_control = reader
		.decode_record(bindings.control.identity(), snapshot)?
		.ok_or(NativeInstallationError::ControlAbsent)?;
	let state = decoded_control.state();
	if let RecordedInstallationIdentity::Require(expected) = identity
		&& expected != state.installation_id()
	{
		return Err(InstallationControlError::WrongInstallation.into());
	}
	let control_component = reader.into_component(state.installation_id());
	let declaration = PgNativeAdministration::declaration_from_schemas(
		participant,
		&control_component,
		bindings.backend_schema(),
	);
	let binding = declaration.bind(
		state
			.component(declaration.identifier())
			.ok_or(NativeInstallationError::AdministrativePolicyAbsent)?,
	)?;
	let control_schema = binding.bind_schema(control_component.schema())?;
	control_component.require_bound_reader(&decoded_control, &control_schema)?;
	Ok((decoded_control.into_state(), control_component))
}

#[derive(Clone, Copy)]
enum InitialInstallationMode<'a> {
	NativeFresh,
	ImportedCoexisting {
		queue_layout: PublishedV10QueueLayout,
		queue_imports: &'a QueueSourceImportDefs,
	},
}

fn check_initial_policies(
	state: &InstallationControlState,
	backend: &PgBackendRegistration,
	admin_declaration: &ComponentDeclaration<'_, ParticipationSemantics>,
	mode: InitialInstallationMode<'_>,
	_queue_storage: &PgNativeQueueStorage,
	scalar_code: &FleetScalarCode,
) -> Result<(), NativeInstallationError> {
	// A query-ready index needs the physical index that the creation transaction creates and
	// the ledger entry that records it.
	require_initial_index_ledger(state.scope_policies(), backend)?;
	let counter = CounterComponent::new();
	let lease = LeaseComponent::new();
	let raw_kv = CoreRawComponent::new();
	let ledger = crate::installation::component_ledger::ComponentLedgerComponent::new();
	let counter_declaration = counter.declaration();
	let ledger_declaration = ledger.declaration();
	let lease_declaration = lease.declaration();
	let raw_declaration = raw_kv.declaration();
	let source_bridge = match mode {
		InitialInstallationMode::NativeFresh => None,
		InitialInstallationMode::ImportedCoexisting { .. } => Some(LeaseSourceBridge::new()),
	};
	let source_declaration = source_bridge.as_ref().map(LeaseSourceBridge::declaration);
	let jobs = QueueJobComponent::new();
	let auxiliary = QueueAuxiliaryComponent::new();
	let queue_job_declaration = jobs.declaration();
	let queue_auxiliary_declaration = auxiliary.declaration();
	let queue_sources = match mode {
		InitialInstallationMode::ImportedCoexisting { queue_imports, .. } => Some(queue_imports),
		InitialInstallationMode::NativeFresh => None,
	};
	let queue_source_job_declaration = queue_sources.map(|imports| imports.job_declaration());
	let queue_source_auxiliary_declaration =
		queue_sources.map(|imports| imports.auxiliary_declaration());
	let scalars = initial_scalars::InitialScalarDeclarations::new(
		initial_scalars::InitialScalarDeclarationFields {
			code: scalar_code,
			mode,
		},
	);
	let mut required = vec![
		counter_declaration.identifier(),
		ledger_declaration.identifier(),
		lease_declaration.identifier(),
		raw_declaration.identifier(),
		admin_declaration.identifier(),
		queue_job_declaration.identifier(),
		queue_auxiliary_declaration.identifier(),
	];
	if let Some(source_declaration) = &source_declaration {
		required.push(source_declaration.identifier());
	}
	if let Some(declaration) = &queue_source_job_declaration {
		required.push(declaration.identifier());
	}
	if let Some(declaration) = &queue_source_auxiliary_declaration {
		required.push(declaration.identifier());
	}
	scalars.extend_required(&mut required);
	for (index, identifier) in required.iter().enumerate() {
		if state.component(identifier).is_none() {
			return Err(NativeInstallationError::MissingInitialComponent { index });
		}
	}
	for (index, policy) in state.component_policies().iter().enumerate() {
		if !required.contains(&policy.component_identifier()) {
			return Err(NativeInstallationError::UnknownInitialComponent { index });
		}
	}
	scalars.bind(state)?;
	let counter_binding = counter_declaration.bind(
		state
			.component(counter_declaration.identifier())
			.expect("required component was checked"),
	)?;
	let counter_schema = counter.bind_schema(&counter_binding)?;
	match mode {
		InitialInstallationMode::NativeFresh => counter_schema.require_native_writer()?,
		InitialInstallationMode::ImportedCoexisting { .. } => {
			counter_schema.require_source_writer()?
		}
	}
	let ledger_binding = ledger_declaration.bind(
		state
			.component(ledger_declaration.identifier())
			.expect("required component was checked"),
	)?;
	ledger.require_bound_policy(&ledger_binding)?;
	let lease_binding = lease_declaration.bind(
		state
			.component(lease_declaration.identifier())
			.expect("required component was checked"),
	)?;
	let lease_schema = lease_binding.bind_schema(lease.schema())?;
	if lease_schema.policy().writer().envelope != StateStorageEnvelope::NativeJson {
		return Err(NativeInstallationError::NativeLeaseWriterRequired);
	}
	raw_declaration.bind(
		state
			.component(raw_declaration.identifier())
			.expect("required component was checked"),
	)?;
	if let Some(source_declaration) = &source_declaration {
		let source_binding = source_declaration.bind(
			state
				.component(source_declaration.identifier())
				.expect("required source bridge was checked"),
		)?;
		source_binding
			.transitions()
			.require_sql_execution(&source_binding)
			.map_err(NativeInstallationError::SourceLeaseExecutionDef)?;
	}
	let job_binding = queue_job_declaration.bind(
		state
			.component(queue_job_declaration.identifier())
			.expect("initial required Queue job policy was checked"),
	)?;
	jobs.require_bound_policy(&job_binding).map_err(|error| {
		NativeInstallationError::QueuePolicy(QueueGroupError::JobBinding(error))
	})?;
	let auxiliary_binding = queue_auxiliary_declaration.bind(
		state
			.component(queue_auxiliary_declaration.identifier())
			.expect("initial required Queue auxiliary policy was checked"),
	)?;
	auxiliary
		.require_bound_policy(&auxiliary_binding)
		.map_err(NativeInstallationError::QueuePolicy)?;
	if let Some(imports) = queue_sources {
		let InitialInstallationMode::ImportedCoexisting { queue_layout, .. } = mode else {
			unreachable!("source Queue declarations require the actual imported layout");
		};
		let job_declaration = queue_source_job_declaration
			.as_ref()
			.expect("source Queue declarations derive from the supplied actual definitions");
		let jobs = job_declaration.bind(
			state
				.component(job_declaration.identifier())
				.expect("required source Queue job policy was checked"),
		)?;
		let auxiliary_declaration = queue_source_auxiliary_declaration
			.as_ref()
			.expect("source Queue declarations derive from the supplied actual definitions");
		let auxiliary = auxiliary_declaration.bind(
			state
				.component(auxiliary_declaration.identifier())
				.expect("required source Queue auxiliary policy was checked"),
		)?;
		imports
			.require_bound_pair(&jobs, &auxiliary, queue_layout)
			.map_err(NativeInstallationError::QueueSourceImportPolicy)?;
	}
	Ok(())
}

/// Typed operation/control/rollback causes remain intact; backend content is never formatted.
#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeInstallationError {
	#[error("the Queue's due-job and running-job indexes must have different names")]
	QueueIndexNamesEqual,
	#[error("native installation cannot derive initial policies from its code and choices")]
	InitialPolicy(#[source] Box<initial_policy::InitialPolicyError>),
	#[error("initial native Fleet scalar policy cannot bind its code")]
	FleetScalarPolicy(#[from] ScalarStateError),
	#[error("native Fleet scalar placement is invalid")]
	FleetScalarPlacement(#[from] FleetScalarPlacementError),
	#[error("recorded Fleet scalar placement differs from its configured placement")]
	FleetScalarPlacementMismatch,
	#[error("source writer retirement requires recorded coexistence, observed {recorded:?}")]
	SourceWriterRetirementPhase { recorded: ImportedPhase },
	#[error("source data retirement requires recorded writer retirement, observed {recorded:?}")]
	SourceDataRetirementPhase { recorded: ImportedPhase },
	#[error("a query-ready initial document index has no document code")]
	InitialDocumentIndexCode {
		scope: crate::scope::ScopeIdentity,
		#[source]
		source: Box<crate::kv::document::schema::managed::ManagedDocumentRegistryError>,
	},
	#[error("a query-ready initial document index cannot bind its code and placement")]
	InitialDocumentIndexBinding {
		scope: crate::scope::ScopeIdentity,
		#[source]
		source: Box<crate::kv::document::postgres::index::managed::PgManagedDocumentIndexError>,
	},
	#[error("the physical index of a query-ready initial document index failed")]
	InitialDocumentIndexPhysical {
		scope: crate::scope::ScopeIdentity,
		#[source]
		source: Box<crate::kv::document::postgres::index::physical::PgDocumentIndexPhysicalError>,
	},
	#[error("another relation has the name of a query-ready initial document index")]
	InitialDocumentIndexNameOccupied { scope: crate::scope::ScopeIdentity },
	#[error("the created physical index of a query-ready initial document index is absent")]
	InitialDocumentIndexAbsent { scope: crate::scope::ScopeIdentity },
	#[error("a query-ready initial document index has no ledger entry")]
	InitialDocumentIndexUnrecorded { scope: crate::scope::ScopeIdentity },
	#[error("the initial index ledger records an index that is not query-ready")]
	InitialDocumentIndexLedgerExtra,
	#[error("application scope {index} repeats the namespace of an earlier scope")]
	ScopeNamespaceRepeated { index: usize },
	#[error("application scope {index} overlaps reserved core namespace {role}")]
	ScopeOverlapsCoreNamespace { index: usize, role: &'static str },
	#[error("native installation control container is also used for ordinary state")]
	SharedControlContainer,
	#[error("native installation namespaces overlap between {left} and {right}")]
	OverlappingNamespaces {
		left: &'static str,
		right: &'static str,
	},
	#[error("native installation identifier generation failed")]
	Random(#[source] primitives_rs::random::secure_bytes::RandomError),
	#[error("native installation bootstrap serialization failed")]
	Bootstrap(#[from] BootstrapError),
	#[error("native installation physical storage validation or creation failed")]
	Schema(#[from] SchemaError),
	#[error("native installation control storage failed")]
	ControlPin(#[from] ControlPinError),
	#[error("native installation control state failed")]
	Control(#[from] InstallationControlError),
	#[error("native installation initial policy binding failed")]
	Policy(#[from] StateContractError),
	#[error("native installation counter representation is unsupported")]
	Counter(#[from] CounterRepresentationError),
	#[error("native installation Queue policy cannot bind its implementation")]
	QueuePolicy(#[source] QueueGroupError),
	#[error("imported Queue source policies do not select one layout-compatible definition pair")]
	QueueSourceImportPolicy(#[source] crate::queue::QueueSourceImportBindingError),
	#[error("native installation source lease policy does not select SQL execution")]
	SourceLeaseExecutionDef(#[source] crate::fleet::lease::LeaseSourceBindingError),
	#[error("native installation lease writer requires native JSON")]
	NativeLeaseWriterRequired,
	#[error("native installation participation writer requires native JSON")]
	NativeAdminWriterRequired,
	#[error("native installation initial required component {index} is missing")]
	MissingInitialComponent { index: usize },
	#[error("native installation initial component {index} is unsupported")]
	UnknownInitialComponent { index: usize },
	#[error("native installation has no mandatory administrative policy")]
	AdministrativePolicyAbsent,
	#[error("published-v10 source protocol already has an imported destination association")]
	SourceAlreadyAssociated,
	#[error(
		"the schema holds an installation that did not import v10, and the v10 location holds a published v10 installation"
	)]
	V10BesideNativeInstallation,
	#[error(
		"the schema holds a published v10 installation, and opening does not name the schema as its v10 location"
	)]
	V10InSchemaNotNamed,
	#[error(
		"a relation named protocol appeared at the v10 location while opening created an installation"
	)]
	V10AppearedAtLocation,
	#[error(
		"the v10 location differs from the v10 location that the installation recorded when it imported v10"
	)]
	V10LocationMismatch,
	#[error(
		"the database encoding is {encoding}, and Paranoid imports and reads v10 tables only in a database with the encoding UTF8 or SQL_ASCII"
	)]
	UnsupportedDatabaseEncoding { encoding: String },
	#[error("published-v10 source protocol has no recorded imported destination association")]
	SourceAssociationAbsent,
	#[error(
		"published-v10 source protocol association differs from the recorded native destination"
	)]
	SourceAssociationMismatch,
	#[error("native installation has a different recorded origin")]
	WrongOrigin,
	#[error("native installation published-v10 source mappings do not match recorded evidence")]
	SourceBindingFingerprintMismatch,
	#[error(
		"native installation source phase {recorded:?} disagrees with source gate {observed:?}"
	)]
	SourcePhaseMismatch {
		recorded: ImportedPhase,
		observed: NativeSourceGatePhase,
	},
	#[error("native installation source mapping is invalid")]
	SourceBinding(#[from] V10SourceBindingError),
	#[error("native installation control is absent")]
	ControlAbsent,
	#[error("fresh native installation unexpectedly contains control state")]
	ControlAlreadyPresent,
	#[error("native installation backend registration policy failed")]
	BackendBinding(#[from] PgBackendRegistrationBindingError),
	#[error("backend document index ledger conflicts with the requested association")]
	DocumentIndexLedger(#[from] backend_registration::DocumentIndexCodecError),
	#[error("native installation backend registration format is invalid")]
	BackendRegistration(
		#[from] crate::records::state_schema::StateSchemaError<PgBackendRegistrationCodecError>,
	),
	#[error("recorded document index {entry_index} has another configured container")]
	DocumentIndexContainer { entry_index: usize },
	#[error("recorded document index {entry_index} uses a reserved core namespace")]
	DocumentIndexReservedScope { entry_index: usize },
	#[error("recorded document index {entry_index} name aliases a configured container")]
	DocumentIndexContainerName { entry_index: usize },
	#[error("active recorded document index {entry_index} has another owner or schema")]
	DocumentIndexOwner { entry_index: usize },
	#[error("active recorded document index {entry_index} has another ordered field shape")]
	DocumentIndexShape { entry_index: usize },
	#[error("native installation recorded mappings differ from the supplied mappings")]
	BindingFingerprintMismatch,
	#[error("native installation transaction control failed")]
	Database(#[from] DatabaseError),
	#[error("native installation protocol rejected transaction work")]
	Protocol(#[from] ProtocolError),
	#[error("native installation failed, then transaction rollback failed")]
	RollbackFailed {
		#[source]
		operation_error: Box<Self>,
		rollback_error: DatabaseError,
	},
}

impl From<initial_policy::InitialPolicyError> for NativeInstallationError {
	fn from(error: initial_policy::InitialPolicyError) -> Self {
		Self::InitialPolicy(Box::new(error))
	}
}

impl NativeInstallationError {
	/// The class that `PgInstallationError::kind` reports for this failure.
	pub(crate) fn opening_kind(&self) -> crate::installation::PgInstallationErrorKind {
		use crate::installation::PgInstallationErrorKind as Kind;
		use crate::kv::document::postgres::index::physical::PgDocumentIndexPhysicalError;
		if self.database_error().is_some() {
			return Kind::Database;
		}
		match self {
			Self::ScopeNamespaceRepeated { .. } | Self::ScopeOverlapsCoreNamespace { .. } => {
				Kind::ScopeSelections
			}
			Self::InitialDocumentIndexPhysical { source, .. }
				if matches!(
					source.as_ref(),
					PgDocumentIndexPhysicalError::KeyCapacity { .. }
						| PgDocumentIndexPhysicalError::KeyCountOverflow
				) =>
			{
				Kind::ScopeSelections
			}
			Self::UnsupportedDatabaseEncoding { .. } => Kind::UnsupportedDatabaseEncoding,
			Self::SourceAlreadyAssociated
			| Self::V10BesideNativeInstallation
			| Self::V10InSchemaNotNamed
			| Self::V10AppearedAtLocation
			| Self::V10LocationMismatch
			| Self::SourceAssociationAbsent
			| Self::SourceAssociationMismatch
			| Self::WrongOrigin
			| Self::SourceBindingFingerprintMismatch
			| Self::SourcePhaseMismatch { .. }
			| Self::BindingFingerprintMismatch
			| Self::ControlAbsent
			| Self::ControlAlreadyPresent
			| Self::FleetScalarPlacementMismatch
			| Self::MissingInitialComponent { .. }
			| Self::UnknownInitialComponent { .. }
			| Self::AdministrativePolicyAbsent
			| Self::DocumentIndexContainer { .. }
			| Self::DocumentIndexReservedScope { .. }
			| Self::DocumentIndexContainerName { .. }
			| Self::DocumentIndexOwner { .. }
			| Self::DocumentIndexShape { .. }
			| Self::InitialDocumentIndexNameOccupied { .. }
			| Self::Schema(SchemaError::SchemaMismatch { .. }) => Kind::RecordedInstallationMismatch,
			// pre.10's protocol state does not admit this release, unless this release's own
			// protocol configuration is wrong.
			Self::Protocol(error) | Self::Schema(SchemaError::Protocol(error)) => match error {
				ProtocolError::Config { .. } => Kind::Internal,
				_ => Kind::RecordedInstallationMismatch,
			},
			_ => Kind::Internal,
		}
	}

	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Database(error) => Some(error),
			Self::Bootstrap(BootstrapError::Database(error)) => Some(error),
			Self::Schema(error) => schema_database_error(error),
			Self::ControlPin(error) => error.database_error(),
			Self::InitialDocumentIndexPhysical { source, .. } => source.database_error(),
			Self::RollbackFailed {
				operation_error,
				rollback_error,
			} => operation_error.database_error().or(Some(rollback_error)),
			_ => None,
		}
	}
}

fn schema_database_error(error: &SchemaError) -> Option<&DatabaseError> {
	match error {
		SchemaError::Database(error) => Some(error),
		SchemaError::RollbackFailed {
			operation_error,
			rollback_error,
		} => schema_database_error(operation_error).or(Some(rollback_error)),
		_ => None,
	}
}

impl TransactionFailure for NativeInstallationError {
	fn from_protocol_error(error: ProtocolError) -> Self {
		Self::Protocol(error)
	}
	fn from_database_error(error: DatabaseError) -> Self {
		Self::Database(error)
	}
	fn from_rollback_failure(operation_error: Self, rollback_error: DatabaseError) -> Self {
		Self::RollbackFailed {
			operation_error: Box::new(operation_error),
			rollback_error,
		}
	}
	fn involves_database_failure(&self) -> bool {
		match self {
			Self::Database(_) | Self::RollbackFailed { .. } | Self::Bootstrap(_) => true,
			Self::Schema(error) => error.involves_database_failure(),
			Self::ControlPin(error) => error.database_error().is_some(),
			Self::InitialDocumentIndexPhysical { source, .. } => source.database_error().is_some(),
			_ => false,
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Database(error) => error.commit_outcome_is_unknown(),
			Self::Bootstrap(BootstrapError::Database(error)) => error.commit_outcome_is_unknown(),
			Self::Schema(error) => error.commit_outcome_is_unknown(),
			Self::RollbackFailed {
				operation_error, ..
			} => operation_error.commit_outcome_is_unknown(),
			_ => false,
		}
	}
}
