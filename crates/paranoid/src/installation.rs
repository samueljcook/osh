//! [`PgInstallation`] opens the installation in one PostgreSQL schema. When the schema contains no
//! installation of this version, opening creates one, or imports the published v10 installation
//! at the [`PgV10Location`] that the caller names; otherwise it checks the recorded installation
//! against this code. [`PgInstallation::inspect`] reports which state is recorded in the schema
//! without changing anything. [`PgInstallationObserver`] reads Queue and Fleet state through a
//! neutral pool without registering a participant, pinning and checking the installed contracts in
//! each read transaction.
//!
//! An import leaves the v10 installation in place. During a rolling deployment, v10 instances and
//! runtimes of this version serve from the same data: Raw values, Counters, leases, Mutexes,
//! Crons, cache invalidation, the Queue, and application schemas are shared. When no v10 instance
//! remains, [`PgRuntime::retire_v10`](crate::runtime::PgRuntime::retire_v10) fences v10 out and
//! moves its records into this version's storage.
//! [`PgRuntime::start_upgrade`](crate::runtime::PgRuntime::start_upgrade) moves the installation
//! to the formats that the newest code writes after every older deployment has stopped.
//!
//! [`ComponentSchema`] and the related items describe an application's own tables using validated
//! [`PgQualifiedTableName`] values, so component tables can reside in any PostgreSQL schema while
//! [`PgRuntime::migrate_component_schema`](crate::runtime::PgRuntime::migrate_component_schema)
//! and [`PgRuntime::validate_component_schema`](crate::runtime::PgRuntime::validate_component_schema)
//! coordinate migrations and validation against the installation's component schema ledger.

use std::{fmt, num::NonZeroU64};

use primitives_rs::id::time_sortable::SortableId;

use crate::queue::{QueueRecoveryPolicy, QueueSourceImportDefs};
use crate::{fleet::scalar::FleetScalarCode, scope::declarations::AppDeclarations};

use crate::db::{PgIdentifier, PgQualifiedTableName, PgSchemaName, Pool, WritePool};
use crate::runtime::PgNativeRuntimeInstallation;
use creation::v10::recorded_source::{
	PgImportedV10PreparationOptions, PgRecordedImportedInstallation,
};
use creation::{
	ImportedPhase, InspectedInstallation, PgNativeFreshOptions, PgNativeInstallation,
	UnexpectedV10Protocol, inspect_installation,
};
use layout::PgStorageLayout;

pub(crate) mod administration;
mod binding_fingerprint;
mod bootstrap;
mod bootstrap_lock;
pub(crate) mod component_ledger;
mod component_schema;
pub(crate) mod contract;
pub(crate) mod control_state;
pub(crate) mod control_storage;
pub(crate) mod creation;
mod error;
mod layout;
mod observer;
pub(crate) mod protocol;
pub(crate) mod protocol_schema;
pub(crate) mod schema_ledger;
mod schema_migration;
pub(crate) mod v10;
pub use crate::db::ProtocolError;
pub(crate) use bootstrap::BootstrapError;
pub use component_ledger::{PgComponentSchemaError, PgComponentSchemaErrorKind};
pub use component_schema::{
	ComponentSchema, ComponentSchemaCompatibility, ComponentSchemaError, ComponentSchemaMigration,
	ComponentSchemaMigrationResult, ComponentSchemaStatement, ComponentSchemaValidationCheck,
};
pub use creation::RecordedInstallationIdentity;
use error::InstallationOpeningFailure;
pub use error::{PgInstallationError, PgInstallationErrorKind};
pub use observer::{
	PgInstallationObserver, PgInstallationObserverFields, PgInstallationObserverOpenError,
	PgInstallationObserverOpenErrorKind,
};
pub(crate) use protocol::Protocol;
pub(crate) use schema_ledger::schema_instance_key_for_parts;
pub use schema_ledger::{
	ComponentSchemaRegistration, ComponentSchemaVersion, component_schema_instance_key_for_tables,
};
pub(crate) use schema_ledger::{
	validate_component_schema_version, validate_component_schema_version_in_current_transaction,
	validate_schema_ledger_registration_in_current_transaction,
};
pub(crate) use schema_migration::plan_component_schema_migration;
pub(crate) use schema_migration::{ComponentSchemaMigrationPlan, RecordedComponentSchemaVersion};
pub use schema_migration::{ComponentSchemaMigrationStep, ComponentSchemaMigrationVersion};
pub use v10::{PgV10Location, PgV10LocationFields};

/// The published v10 installation that a database can contain. Opening imports the v10
/// installation when the database contains one and no installation of this Paranoid version has
/// imported it. The payload of every v10 Queue job stays the JSON that v10 stored.
///
/// Opening imports a v10 installation, and opens an installation that still reads v10 data,
/// only in a database with the encoding `UTF8` or `SQL_ASCII` because in those encodings
/// PostgreSQL orders the v10 tables' text keys by their stored bytes and accepts all UTF-8
/// client parameters. In a `SQL_ASCII` database, PostgreSQL stores the bytes that each client
/// sends and `pg_catalog.convert_to(..., 'UTF8')` copies those raw bytes without UTF-8
/// validation, while Rust `str::from_utf8` validates decoded keys; a v10 key with bytes that
/// are not valid UTF-8 causes key decoding to fail with an error. In a database with another
/// encoding, opening fails with [`PgInstallationErrorKind::UnsupportedDatabaseEncoding`], so a
/// v10 installation in such a database cannot be imported.
#[derive(Clone, Debug)]
pub struct PgV10Fields {
	/// Where the v10 tables are.
	pub location: PgV10Location,
}

/// An opened installation: opening created it, imported a v10 installation into it, or
/// checked the recorded installation against this code.
///
/// `PgRuntime::start` takes the installation. Opening does not register a participant. When
/// creation or import fails, opening checks the database again and reopens the installation if
/// the failed attempt or another deployment recorded one; otherwise it returns the error.
pub struct PgInstallation {
	pub(crate) installation: PgNativeRuntimeInstallation,
	pub(crate) app_declarations: AppDeclarations,
}

impl PgInstallation {
	/// The installation's ID, which the installation records when opening creates it.
	pub fn installation_id(&self) -> SortableId {
		match &self.installation {
			PgNativeRuntimeInstallation::Fresh(installation) => installation.installation_id(),
			PgNativeRuntimeInstallation::Imported(installation) => installation.installation_id(),
		}
	}
}

/// What a database contains in one PostgreSQL schema and at an optional v10 location.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PgInstallationState {
	/// Neither an installation of this Paranoid version nor a published v10 installation at the
	/// v10 location exists. Opening still rejects creation when the schema itself contains a v10
	/// `protocol` table and the v10 location does not name the schema.
	Absent,
	/// The schema contains no installation of this version, and a v10 protocol table exists at the
	/// location. Opening attempts to import it, which fails if another installation has
	/// already imported it.
	PublishedV10,
	/// An installation that this Paranoid version created without a v10 import exists.
	Fresh,
	/// An installation that imported a published v10 installation exists.
	ImportedFromV10 {
		/// How far the import has progressed.
		phase: PgV10ImportPhase,
	},
}

/// How far an installation has progressed in retiring the v10 installation that it imported.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PgV10ImportPhase {
	/// v10 deployments can still write; both share the data.
	Coexisting,
	/// v10 deployments can no longer write; v10 data remains to be moved.
	WritersRetired,
	/// Every v10 record was moved; the v10 tables hold no data.
	///
	/// Every opening of the installation still reads the v10 `protocol` and `schema_ledger`
	/// tables, and the component schema ledger reads a component's v10 row until a native
	/// migration writes the component's native entry. The application must keep both tables:
	/// when either is dropped, opening fails.
	DataRetired,
}

/// The initial choices that apply when opening creates an installation or imports v10.
pub struct PgInitialInstallationOptions {
	/// The installation's Queue recovery settings.
	///
	/// When opening imports v10, v10 deployments and this version's workers share v10's Queue
	/// tables until v10 retires, and each reclaims the other's running jobs by its own stale
	/// threshold. Give the stale threshold the value that the v10 deployments use for
	/// reclaiming, and keep every worker's heartbeat interval, in both versions, below it;
	/// otherwise one version reclaims jobs that the other is still running.
	pub queue_recovery: QueueRecoveryPolicy,
}

/// Everything that opening needs on every start of every deployment.
pub struct PgOpenInstallationFields<'a> {
	/// The pool for installation administration.
	pub control_pool: &'a WritePool,
	/// The PostgreSQL schema that holds the installation. When opening creates an
	/// installation or imports v10 and the schema does not exist, opening creates the schema,
	/// which needs the `CREATE` privilege on the database; an existing schema needs no
	/// privilege on the database. Paranoid chooses every table, namespace, and index inside
	/// the schema.
	pub schema: PgSchemaName,
	/// Where a published v10 installation can be. Opening imports the v10 installation at this
	/// location when no installation of this Paranoid version holds the schema. An installation
	/// that imported v10 records its v10 location, and opening it accepts `None` or the
	/// recorded location; another location is an error. When the schema itself holds a
	/// published v10 installation, opening creates or imports an installation only when this
	/// location names the schema. Opening recognizes that installation by a `protocol` table
	/// with the columns, primary key, and checks of a v10 protocol table, so an application's
	/// own relation of that name in the schema does not count.
	pub v10: Option<PgV10Fields>,
	/// The app's scopes and this deployment's roles in them. When opening creates an
	/// installation or imports v10, it makes each scope's newest version that the code writes
	/// as JSON the writer version, maintains every declared index, and makes every index that
	/// the declarations query ready for queries; the runtime that starts from this
	/// installation uses these declarations. Opening an existing installation adds no scope and
	/// makes no index ready. A runtime that starts from it adds each declared scope with a JSON
	/// writer that the installation does not have, empty, unless an upgrade is pending (see
	/// [`PgRuntime::start`](crate::runtime::PgRuntime::start)), and
	/// [`PgRuntime::start_upgrade`](crate::runtime::PgRuntime::start_upgrade) makes a queried index
	/// ready.
	pub app_declarations: AppDeclarations,
	/// The choices that apply when opening creates an installation or imports v10.
	pub initial: PgInitialInstallationOptions,
}

/// What [`PgInstallation::inspect`] reads.
pub struct PgInspectInstallationFields<'a> {
	/// The pool for the read.
	pub pool: &'a Pool,
	/// The PostgreSQL schema that holds the installation.
	pub schema: PgSchemaName,
	/// Where a published v10 installation can be, as
	/// [`PgOpenInstallationFields::v10`] names it.
	pub v10_location: Option<PgV10Location>,
}

impl PgInstallation {
	/// Reports what the database holds in the schema and at the v10 location, without changing
	/// anything. It fails when the schema holds an installation that did not import v10 and
	/// the v10 location holds a published v10 installation, and when the schema holds an
	/// installation that imported v10 from another location.
	pub async fn inspect(
		fields: PgInspectInstallationFields<'_>,
	) -> Result<PgInstallationState, PgInstallationError> {
		Self::inspect_schema(fields.pool, &fields.schema, fields.v10_location.as_ref()).await
	}

	async fn inspect_schema(
		pool: &Pool,
		schema: &PgSchemaName,
		v10_location: Option<&PgV10Location>,
	) -> Result<PgInstallationState, PgInstallationError> {
		let bindings = PgStorageLayout::new(schema).into_bindings();
		let state = inspect_installation(
			pool,
			&bindings,
			v10_location.map(PgV10Location::source_location),
		)
		.await?;
		Ok(match state {
			InspectedInstallation::Absent => PgInstallationState::Absent,
			InspectedInstallation::PublishedV10 => PgInstallationState::PublishedV10,
			InspectedInstallation::Fresh => PgInstallationState::Fresh,
			InspectedInstallation::ImportedFromV10(phase) => PgInstallationState::ImportedFromV10 {
				phase: match phase {
					ImportedPhase::Coexisting => PgV10ImportPhase::Coexisting,
					ImportedPhase::SourceWritersRetired => PgV10ImportPhase::WritersRetired,
					ImportedPhase::SourceDataRetired => PgV10ImportPhase::DataRetired,
				},
			},
		})
	}

	/// Opens the installation that the database holds: creates a fresh one when none exists,
	/// imports a published v10 installation that no installation of this Paranoid version has
	/// imported, or reopens a recorded one. When another deployment creates or imports the
	/// installation at the same time, opening reopens the installation that the other
	/// deployment recorded.
	pub async fn open(fields: PgOpenInstallationFields<'_>) -> Result<Self, PgInstallationError> {
		let PgOpenInstallationFields {
			control_pool,
			schema,
			v10,
			app_declarations,
			initial,
		} = fields;
		let installation =
			Self::open_installation(control_pool, &schema, v10, &app_declarations, initial).await?;
		Ok(Self {
			installation,
			app_declarations,
		})
	}

	async fn open_installation(
		control_pool: &WritePool,
		schema: &PgSchemaName,
		v10: Option<PgV10Fields>,
		app_declarations: &AppDeclarations,
		initial: PgInitialInstallationOptions,
	) -> Result<PgNativeRuntimeInstallation, PgInstallationError> {
		let initial_selections = || {
			app_declarations.initial_selections().map_err(|error| {
				PgInstallationError::new(InstallationOpeningFailure::ScopeSelections(error))
			})
		};
		let location = v10.map(|fields| fields.location);
		let layout = PgStorageLayout::new(schema);
		let fleet_scalar_code = FleetScalarCode::new();
		// A published v10 installation in `schema` itself has this protocol table.
		let schema_v10_protocol = PgQualifiedTableName::new(
			schema.clone(),
			PgIdentifier::new(v10::BOOTSTRAP_PROTOCOL_TABLE_NAME)
				.expect("the v10 protocol table name is a valid identifier"),
		);
		let import_published_v10 = |source: PgV10Location, scope_selections| async {
			let unexpected_v10_protocols = if source.protocol_table() == &schema_v10_protocol {
				Vec::new()
			} else {
				vec![UnexpectedV10Protocol::InSchemaUnnamed(&schema_v10_protocol)]
			};
			let queue_imports = QueueSourceImportDefs::new();
			PgRecordedImportedInstallation::prepare_imported(
				control_pool,
				source.into_location(),
				layout.clone().into_bindings(),
				PgImportedV10PreparationOptions {
					documents: app_declarations.document_registry(),
					fleet_scalar_code: &fleet_scalar_code,
					generation: NonZeroU64::MIN,
					scope_selections,
					queue_recovery: &initial.queue_recovery,
					conflict_attempts: crate::runtime::parameters::CONFLICT_ATTEMPTS,
					queue_source: queue_imports.initial_source_selection(),
					unexpected_v10_protocols: &unexpected_v10_protocols,
				},
			)
			.await
			.map(|installation| PgNativeRuntimeInstallation::Imported(Box::new(installation)))
		};
		let state = Self::inspect_schema(control_pool, schema, location.as_ref()).await?;
		let installed = match state {
			PgInstallationState::Fresh => {
				return Self::reopen_fresh(control_pool, &layout).await;
			}
			PgInstallationState::ImportedFromV10 { .. } => {
				return Self::reopen_imported(control_pool, &layout).await;
			}
			PgInstallationState::Absent => {
				let unexpected_v10_protocols = match location.as_ref() {
					Some(location) if location.protocol_table() == &schema_v10_protocol => {
						vec![UnexpectedV10Protocol::AtLocation(&schema_v10_protocol)]
					}
					Some(location) => vec![
						UnexpectedV10Protocol::InSchemaUnnamed(&schema_v10_protocol),
						UnexpectedV10Protocol::AtLocation(location.protocol_table()),
					],
					None => vec![UnexpectedV10Protocol::InSchemaUnnamed(&schema_v10_protocol)],
				};
				PgNativeInstallation::create_fresh(
					control_pool,
					layout.clone().into_bindings(),
					PgNativeFreshOptions {
						documents: app_declarations.document_registry(),
						fleet_scalar_code: &fleet_scalar_code,
						generation: NonZeroU64::MIN,
						scope_selections: initial_selections()?,
						queue_recovery: &initial.queue_recovery,
						conflict_attempts: crate::runtime::parameters::CONFLICT_ATTEMPTS,
						unexpected_v10_protocols: &unexpected_v10_protocols,
					},
				)
				.await
				.map(|installation| PgNativeRuntimeInstallation::Fresh(Box::new(installation)))
			}
			PgInstallationState::PublishedV10 => {
				let source = location
					.clone()
					.expect("inspection reports a published v10 installation only at a location");
				import_published_v10(source, initial_selections()?).await
			}
		};
		let error = match installed {
			Ok(installation) => return Ok(installation),
			Err(error) => PgInstallationError::from(error),
		};
		// Another deployment may have recorded an installation first.
		match Self::inspect_schema(control_pool, schema, location.as_ref()).await {
			Ok(PgInstallationState::Fresh) => Self::reopen_fresh(control_pool, &layout).await,
			Ok(PgInstallationState::ImportedFromV10 { .. }) => {
				Self::reopen_imported(control_pool, &layout).await
			}
			_ => Err(error),
		}
	}

	/// Opens recorded native-origin state using its stored policies and existing containers.
	/// This creates or repairs no table and selects no replacement initial format.
	async fn reopen_fresh(
		control_pool: &WritePool,
		layout: &PgStorageLayout,
	) -> Result<PgNativeRuntimeInstallation, PgInstallationError> {
		let installation = PgNativeInstallation::open_recorded(
			control_pool,
			layout.clone().into_bindings(),
			RecordedInstallationIdentity::DiscoverRecorded,
		)
		.await?;
		Ok(PgNativeRuntimeInstallation::Fresh(Box::new(installation)))
	}

	/// Opens an imported installation by its recorded v10 location and import phase. A first
	/// transaction reads the recorded v10 location and ends before a second transaction locks
	/// the v10 installation, reads the installation's control record, and checks the v10
	/// tables and the installation's tables. After v10 retirement, the opened installation
	/// does not use the v10 data.
	async fn reopen_imported(
		control_pool: &WritePool,
		layout: &PgStorageLayout,
	) -> Result<PgNativeRuntimeInstallation, PgInstallationError> {
		let installation = PgRecordedImportedInstallation::open_recorded(
			control_pool,
			layout.clone().into_bindings(),
			RecordedInstallationIdentity::DiscoverRecorded,
		)
		.await?;
		Ok(PgNativeRuntimeInstallation::Imported(Box::new(
			installation,
		)))
	}
}

impl fmt::Debug for PgInstallation {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("PgInstallation { .. }")
	}
}
