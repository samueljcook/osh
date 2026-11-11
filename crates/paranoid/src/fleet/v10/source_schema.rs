//! Read-only validation of the physical Fleet versions admitted by published v10.

use serde::{Deserialize, Serialize};

use crate::db::{PgQualifiedTableName, SchemaError, Tx};
use crate::fleet::FLEET_SCHEMA_V1_TARGET;
use crate::installation::schema_ledger::fetch_component_registration_in_current_transaction;
use crate::installation::{
	ComponentSchemaRegistration, ComponentSchemaVersion, schema_instance_key_for_parts,
};

/// Exact owning associations used for recorded Fleet layout lookup, without data handles.
#[derive(Clone, Copy)]
pub(crate) struct FleetSourceSchemaTables<'a> {
	pub(crate) ledger: &'a PgQualifiedTableName,
	pub(crate) root: &'a crate::fleet::RootKey,
	pub(crate) state: &'a PgQualifiedTableName,
	pub(crate) claim_table: &'a PgQualifiedTableName,
	pub(crate) fencing_counter_table: &'a PgQualifiedTableName,
}

pub(super) fn fleet_source_schema_instance_key(tables: FleetSourceSchemaTables<'_>) -> String {
	format!(
		"root={};{}",
		tables.root.as_str(),
		schema_instance_key_for_parts([
			("state_table", tables.state),
			("coordination_table", tables.claim_table),
			("fencing_counter_table", tables.fencing_counter_table),
		])
	)
}

/// Derives the actual published Counter key without constructing a source Store.
pub(crate) fn source_counter_key(
	root: &crate::fleet::RootKey,
	key: &crate::fleet::CounterKey,
) -> Result<crate::kv::Key, crate::kv::KeyError> {
	let prefix = crate::fleet::namespace_keys::build_counter_prefix(root, key)?;
	crate::kv::Key::from_prefix_and_parts(&prefix, [crate::fleet::FLEET_COUNTER_VALUE_KEY_PART])
}

/// Exact recorded source ledger identities; neither variant selects a payload decoder.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PublishedV10FleetLayout {
	PublishedV1,
	PreparedV8,
}

/// Checks retained registration evidence without accessing source state tables.
pub(crate) async fn read_published_v10_layout_in_current_transaction(
	tx: &mut Tx<'_>,
	tables: FleetSourceSchemaTables<'_>,
) -> Result<PublishedV10FleetLayout, SchemaError> {
	let instance_key = fleet_source_schema_instance_key(tables);
	let recorded = fetch_component_registration_in_current_transaction(
		tx,
		tables.ledger,
		ComponentSchemaVersion {
			component: crate::fleet::FLEET_SCHEMA_COMPONENT,
			instance_key: &instance_key,
			version: FLEET_SCHEMA_V1_TARGET.version(),
			fingerprint: FLEET_SCHEMA_V1_TARGET.fingerprint(),
		},
	)
	.await?;
	let layout = match recorded {
		ComponentSchemaRegistration::Recorded {
			version: 1,
			fingerprint,
		} if fingerprint == FLEET_SCHEMA_V1_TARGET.fingerprint() => PublishedV10FleetLayout::PublishedV1,
		ComponentSchemaRegistration::Recorded {
			version: 8,
			fingerprint,
		} if fingerprint == "paranoid.fleet.v8" => PublishedV10FleetLayout::PreparedV8,
		_ => {
			return Err(SchemaError::mismatch(
				"published-v10 Fleet source registration is absent or unsupported",
			));
		}
	};
	Ok(layout)
}
