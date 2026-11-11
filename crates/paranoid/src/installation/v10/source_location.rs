//! The location of a published v10 installation's tables, derived from its configuration.
//!
//! A v10 `BootstrapConfig` created its tables under the fixed names below in one schema. An
//! application never names a Paranoid table, so these names are not public.

use std::fmt;

use crate::fleet::RootKey;

use super::{
	PgV10SourceLocation, PgV10SourceLocationConfig, PgV10SourceRole, PgV10SourceRoleBinding,
};
use crate::db::{PgIdentifier, PgQualifiedTableName, PgSchemaName};

/// The schema-local name of the protocol table that a v10 `BootstrapConfig` creates.
pub(crate) const BOOTSTRAP_PROTOCOL_TABLE_NAME: &str = "protocol";

/// The schema-local name of the schema ledger table that a v10 `BootstrapConfig` creates.
const BOOTSTRAP_SCHEMA_LEDGER_TABLE_NAME: &str = "schema_ledger";

/// The schema-local name of the KV table that a v10 `BootstrapConfig` creates.
const BOOTSTRAP_KV_TABLE_NAME: &str = "kv_store";

/// The schema-local name of the Fleet state table that a v10 `BootstrapConfig` creates.
const BOOTSTRAP_FLEET_STATE_TABLE_NAME: &str = "fleet_state";

/// The schema-local name of the Fleet coordination table that a v10 `BootstrapConfig` creates.
const BOOTSTRAP_FLEET_COORDINATION_TABLE_NAME: &str = "fleet_coordination";

/// The schema-local name of the Fleet fencing counter table that a v10 `BootstrapConfig`
/// creates.
const BOOTSTRAP_FLEET_FENCING_COUNTER_TABLE_NAME: &str = "fleet_fencing_counters";

/// The schema-local name of the Queue jobs table that a v10 `BootstrapConfig` creates.
const BOOTSTRAP_QUEUE_JOBS_TABLE_NAME: &str = "queue_jobs";

/// The schema-local name of the Queue dead-letter table that a v10 `BootstrapConfig` creates.
const BOOTSTRAP_QUEUE_DEAD_LETTER_TABLE_NAME: &str = "queue_dead_letters";

/// The schema-local name of the Queue pause table that a v10 `BootstrapConfig` creates.
const BOOTSTRAP_QUEUE_PAUSE_TABLE_NAME: &str = "queue_pauses";

/// The configuration of a published v10 installation, as its `BootstrapConfig` stated it. A
/// v10 `BootstrapConfig` always used the Fleet root `__paranoid_fleet`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PgV10LocationFields {
	/// The schema that holds every table of the v10 installation.
	pub schema_name: PgSchemaName,
	/// The root key that namespaces the v10 installation's Fleet records.
	pub fleet_root_key: RootKey,
}

/// The tables of a published v10 installation.
///
/// Building a location does no database work. Opening the installation checks that the
/// tables exist and match a v10 release.
#[derive(Clone)]
pub struct PgV10Location {
	location: PgV10SourceLocation,
}

impl PgV10Location {
	/// Locates the tables that a published v10 release created for this configuration. This
	/// performs no database work; opening the installation checks the tables.
	pub fn new(fields: PgV10LocationFields) -> Self {
		use PgV10SourceRole as Role;
		let table = |name: &str| {
			PgQualifiedTableName::new(
				fields.schema_name.clone(),
				PgIdentifier::new(name).expect("a v10 table name is a valid identifier"),
			)
		};
		let bindings = [
			(Role::Protocol, BOOTSTRAP_PROTOCOL_TABLE_NAME),
			(Role::ProtocolLedger, BOOTSTRAP_SCHEMA_LEDGER_TABLE_NAME),
			(Role::RawKvLedger, BOOTSTRAP_SCHEMA_LEDGER_TABLE_NAME),
			(Role::RawKv, BOOTSTRAP_KV_TABLE_NAME),
			(Role::FleetLedger, BOOTSTRAP_SCHEMA_LEDGER_TABLE_NAME),
			(Role::FleetState, BOOTSTRAP_FLEET_STATE_TABLE_NAME),
			(Role::FleetClaims, BOOTSTRAP_FLEET_COORDINATION_TABLE_NAME),
			(
				Role::FleetFencing,
				BOOTSTRAP_FLEET_FENCING_COUNTER_TABLE_NAME,
			),
			(Role::QueueLedger, BOOTSTRAP_SCHEMA_LEDGER_TABLE_NAME),
			(Role::QueueJobs, BOOTSTRAP_QUEUE_JOBS_TABLE_NAME),
			(
				Role::QueueDeadLetters,
				BOOTSTRAP_QUEUE_DEAD_LETTER_TABLE_NAME,
			),
			(Role::QueuePauses, BOOTSTRAP_QUEUE_PAUSE_TABLE_NAME),
		];
		let location = PgV10SourceLocation::new(PgV10SourceLocationConfig {
			roles: bindings
				.into_iter()
				.map(|(role, name)| PgV10SourceRoleBinding {
					role,
					table: table(name),
				})
				.collect(),
			fleet_root: fields.fleet_root_key,
			// A published v10 bootstrap creates both updated-at indexes.
			requires_raw_kv_updated_at_index: true,
			requires_fleet_state_updated_at_index: true,
		})
		.expect("the published v10 tables have distinct source roles");
		Self { location }
	}

	pub(crate) fn source_location(&self) -> &PgV10SourceLocation {
		&self.location
	}

	pub(crate) fn into_location(self) -> PgV10SourceLocation {
		self.location
	}

	pub(crate) fn protocol_table(&self) -> &PgQualifiedTableName {
		self.location.table(PgV10SourceRole::Protocol)
	}
}

impl fmt::Debug for PgV10Location {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("PgV10Location { .. }")
	}
}
