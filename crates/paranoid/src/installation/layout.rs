//! The physical placement of one PostgreSQL installation, derived from its schema name.
//!
//! The application names only the PostgreSQL schema. Paranoid chooses every table, record
//! namespace, and index inside it:
//!
//! - `paranoid_control`: the installation's administrative record, alone in its table;
//! - `paranoid_core`: participants, counters, the component schema ledger, leases, and raw
//!   bytes, each in its own namespace;
//! - `paranoid_queue`: Queue jobs, active deduplication, dead letters, dead-letter originals,
//!   and pauses, with the indexes `paranoid_queue_due`, `paranoid_queue_due_task`, `paranoid_queue_running`,
//!   `paranoid_queue_finished`, and `paranoid_queue_dead_letters`;
//! - `paranoid_fleet`: Once completions, cache epochs, Topic sequences, and subscription
//!   cursors;
//! - `paranoid_app`: every application scope, each in its own namespace.
//!
//! Fresh installations create every index. Existing installations obtain the additive
//! `paranoid_queue_due_task` index when `PgRuntime::start_upgrade` or
//! `PgRuntime::retire_v10` runs; opening and starting an existing installation do not create it.

use std::fmt;

use crate::records::{RecordIdentity, RecordKey, RecordNamespace};

use super::creation::{
	PgNativeFleetScalarStorageFields, PgNativeNamespaceBinding, PgNativeQueueStorageFields,
	PgNativeStorageBindings, PgNativeStorageBindingsFields,
};
use crate::db::{PgIdentifier, PgQualifiedTableName, PgSchemaName};
use crate::installation::control_storage::PgControlStorage;
use crate::records::postgres::PgPointStorage;

/// The derived placement of one installation.
#[derive(Clone)]
pub(crate) struct PgStorageLayout {
	bindings: PgNativeStorageBindings,
}

impl PgStorageLayout {
	/// Derives every table, namespace, and index in `schema`. This performs no database work.
	pub(crate) fn new(schema: &PgSchemaName) -> Self {
		let table = |name: &str| {
			PgPointStorage::new(PgQualifiedTableName::new(schema.clone(), identifier(name)))
		};
		let core = table("paranoid_core");
		let in_core = |name: &[u8]| PgNativeNamespaceBinding::new(core.clone(), namespace(name));
		let bindings = PgNativeStorageBindings::new(PgNativeStorageBindingsFields {
			control: PgControlStorage::new(
				table("paranoid_control"),
				RecordIdentity::new(
					namespace(b"installation"),
					RecordKey::new(b"control".to_vec()),
				),
			),
			participants: in_core(b"participants"),
			counters: in_core(b"counters"),
			component_schemas: in_core(b"component_schemas"),
			leases: in_core(b"leases"),
			raw_kv: in_core(b"raw"),
			queue: PgNativeQueueStorageFields {
				storage: table("paranoid_queue"),
				jobs: namespace(b"jobs"),
				active_dedupe: namespace(b"active_dedupe"),
				dead_letters: namespace(b"dead_letters"),
				dead_letter_originals: namespace(b"dead_letter_originals"),
				pauses: namespace(b"pauses"),
				due_jobs_index: identifier("paranoid_queue_due"),
				running_jobs_index: identifier("paranoid_queue_running"),
				finished_jobs_index: identifier("paranoid_queue_finished"),
				dead_letters_index: identifier("paranoid_queue_dead_letters"),
			},
			fleet_scalar: PgNativeFleetScalarStorageFields {
				storage: table("paranoid_fleet"),
				once_completion: namespace(b"once_completion"),
				cache_epoch: namespace(b"cache_epoch"),
				topic_sequence: namespace(b"topic_sequence"),
				subscription_cursor: namespace(b"subscription_cursor"),
			},
			app: table("paranoid_app"),
		})
		.expect("the derived layout has distinct tables, namespaces, and index names");
		Self { bindings }
	}

	pub(crate) fn into_bindings(self) -> PgNativeStorageBindings {
		self.bindings
	}
}

fn identifier(name: &str) -> PgIdentifier {
	PgIdentifier::new(name).expect("a derived layout name is a valid identifier")
}

fn namespace(name: &[u8]) -> RecordNamespace {
	RecordNamespace::new(name.to_vec()).expect("a derived layout namespace is valid")
}

impl fmt::Debug for PgStorageLayout {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("PgStorageLayout { .. }")
	}
}
