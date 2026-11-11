//! Read-only validation of the physical Queue versions admitted by published v10.

use serde::{Deserialize, Serialize};

use crate::db::{PgQualifiedTableName, SchemaError, Tx};
use crate::installation::schema_ledger::fetch_component_registration_in_current_transaction;
use crate::installation::{
	ComponentSchemaRegistration, ComponentSchemaVersion, schema_instance_key_for_parts,
};
use crate::queue::{QUEUE_SCHEMA_SOURCE_EPOCH_TARGET, QUEUE_SCHEMA_V1_TARGET};

/// Owning Queue associations used for ledger lookup without constructing a Store.
#[derive(Clone, Copy)]
pub(crate) struct QueueSourceSchemaTables<'a> {
	pub(crate) ledger: &'a PgQualifiedTableName,
	pub(crate) jobs: &'a PgQualifiedTableName,
	pub(crate) dead_letters: &'a PgQualifiedTableName,
	pub(crate) pauses: &'a PgQualifiedTableName,
}

impl<'a> QueueSourceSchemaTables<'a> {
	pub(super) fn table(self, table: super::schema_model::QueueTable) -> &'a PgQualifiedTableName {
		match table {
			super::schema_model::QueueTable::Jobs => self.jobs,
			super::schema_model::QueueTable::DeadLetter => self.dead_letters,
			super::schema_model::QueueTable::Pause => self.pauses,
		}
	}
}

impl super::schema_model::QueueSchemaTableBinding for QueueSourceSchemaTables<'_> {
	fn schema_tables(&self) -> QueueSourceSchemaTables<'_> {
		*self
	}
}

/// Validates actual source data catalogs after the caller checks retained registrations.
pub(crate) async fn validate_published_v10_physical_in_current_transaction(
	tx: &mut Tx<'_>,
	tables: QueueSourceSchemaTables<'_>,
	layout: PublishedV10QueueLayout,
) -> Result<(), SchemaError> {
	super::schema_validation::validate_published_v10_schema_in_current_transaction(
		tx, &tables, layout,
	)
	.await
}

pub(super) fn queue_source_schema_instance_key(tables: QueueSourceSchemaTables<'_>) -> String {
	schema_instance_key_for_parts([
		("jobs_table", tables.jobs),
		("dead_letter_table", tables.dead_letters),
		("pause_table", tables.pauses),
	])
}

/// Exact source ledger identities distinguish an absent token column from prepared token storage.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PublishedV10QueueLayout {
	PublishedV1,
	PreparedV2,
}

/// Checks exact recorded layout without accessing deleted source data tables.
pub(crate) async fn read_published_v10_layout_in_current_transaction(
	tx: &mut Tx<'_>,
	tables: QueueSourceSchemaTables<'_>,
) -> Result<PublishedV10QueueLayout, SchemaError> {
	let instance_key = queue_source_schema_instance_key(tables);
	let recorded = fetch_component_registration_in_current_transaction(
		tx,
		tables.ledger,
		ComponentSchemaVersion {
			component: crate::queue::QUEUE_SCHEMA_COMPONENT,
			instance_key: &instance_key,
			version: QUEUE_SCHEMA_V1_TARGET.version(),
			fingerprint: QUEUE_SCHEMA_V1_TARGET.fingerprint(),
		},
	)
	.await?;
	let layout = match recorded {
		ComponentSchemaRegistration::Recorded {
			version: 1,
			fingerprint,
		} if fingerprint == QUEUE_SCHEMA_V1_TARGET.fingerprint() => PublishedV10QueueLayout::PublishedV1,
		ComponentSchemaRegistration::Recorded {
			version: 2,
			fingerprint,
		} if fingerprint == QUEUE_SCHEMA_SOURCE_EPOCH_TARGET.fingerprint() => {
			PublishedV10QueueLayout::PreparedV2
		}
		_ => {
			return Err(SchemaError::mismatch(
				"published-v10 Queue source registration is absent or unsupported",
			));
		}
	};
	Ok(layout)
}
