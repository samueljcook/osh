//! Actual published-v10 bootstrap mappings for native import preparation and routing.
//!
//! Construction parses configured associations without inspecting or adopting the database.
//! Source catalogs, recorded component versions, native preparation, and durable source phase
//! must be verified by the imported installer before these mappings can admit operations.

use primitives_rs::crypto::algorithms::blake3;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::fleet::{CounterKey, RootKey};
use crate::kv;

use crate::db::{PgQualifiedTableName, SchemaError, Tx};
use crate::installation::Protocol;
use crate::installation::binding_fingerprint::frame_fingerprint_input;
use crate::installation::creation::PgNativeStorageBindings;
use crate::installation::protocol_schema::validate_prepared_protocol;
use crate::installation::validate_schema_ledger_registration_in_current_transaction;
use crate::records::postgres::v10::source_point::validate_source_heap;
use source_gate::PgNativeSourceGate;

pub(crate) mod candidate_page;
mod data_emptiness;
pub(crate) use data_emptiness::{PgV10SourceDataEmptinessStorage, SourceDataEmptinessError};
mod descriptor;
mod location;
pub(crate) use descriptor::{PgV10SourceDescriptor, PgV10SourceDescriptorWire, PgV10SourceRole};
pub(crate) use location::{PgV10SourceLocation, PgV10SourceLocationConfig, PgV10SourceRoleBinding};
pub(crate) mod source_gate;
mod source_location;
pub(crate) use source_location::BOOTSTRAP_PROTOCOL_TABLE_NAME;
pub use source_location::{PgV10Location, PgV10LocationFields};

const SOURCE_BINDING_VERSION: u32 = 1;
const SOURCE_BINDING_DOMAIN: &[u8] = b"paranoid.pg-v10-bootstrap-binding";

#[derive(Clone, Copy, Eq, PartialEq)]
enum SourceRoleKind {
	Protocol,
	Ledger,
	State,
}

struct SourceRole<'a> {
	role: PgV10SourceRole,
	table: &'a PgQualifiedTableName,
}

/// Exact source component layout evidence from successful read-only validation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PgV10SourceLayout {
	fleet: crate::fleet::v10::PublishedV10FleetLayout,
	queue: crate::queue::PublishedV10QueueLayout,
}

impl PgV10SourceLayout {
	pub(crate) fn queue_layout(self) -> crate::queue::PublishedV10QueueLayout {
		self.queue
	}
}

fn check_role_separation(roles: &[SourceRole<'_>]) -> Result<(), V10SourceBindingError> {
	for (index, role) in roles.iter().enumerate() {
		for prior in &roles[..index] {
			if role.table == prior.table
				&& !(role.role.kind() == SourceRoleKind::Ledger
					&& prior.role.kind() == SourceRoleKind::Ledger)
			{
				return Err(V10SourceBindingError::OverlappingSourceRoles {
					left: prior.role.name(),
					right: role.role.name(),
				});
			}
		}
	}
	Ok(())
}

fn check_native_role_separation(
	roles: &[SourceRole<'_>],
	native: &PgNativeStorageBindings,
) -> Result<(), V10SourceBindingError> {
	let containers = native.unique_containers();
	for role in roles {
		if containers
			.iter()
			.any(|container| container.table() == role.table)
		{
			return Err(V10SourceBindingError::NativeSourceContainerOverlap {
				source_role: role.role.name(),
			});
		}
	}
	Ok(())
}

fn source_fingerprint(
	roles: &[SourceRole<'_>],
	root: &RootKey,
	requires_raw_kv_updated_at_index: bool,
	requires_fleet_state_updated_at_index: bool,
) -> blake3::Hash {
	let mut hasher = blake3::Hasher::new();
	frame_fingerprint_input(&mut hasher, SOURCE_BINDING_DOMAIN);
	frame_fingerprint_input(&mut hasher, &SOURCE_BINDING_VERSION.to_be_bytes());
	for role in roles {
		frame_fingerprint_input(&mut hasher, role.role.name().as_bytes());
		frame_fingerprint_input(&mut hasher, role.table.schema().as_str().as_bytes());
		frame_fingerprint_input(&mut hasher, role.table.table().as_str().as_bytes());
	}
	frame_fingerprint_input(&mut hasher, root.as_str().as_bytes());
	frame_fingerprint_input(&mut hasher, &[u8::from(requires_raw_kv_updated_at_index)]);
	frame_fingerprint_input(
		&mut hasher,
		&[u8::from(requires_fleet_state_updated_at_index)],
	);
	hasher.finalize()
}

async fn validate_retained_source_metadata(
	tx: &mut Tx<'_>,
	protocol: &Protocol,
	protocol_ledger: &PgQualifiedTableName,
	roles: &[SourceRole<'_>],
) -> Result<(), SchemaError> {
	validate_source_heap(protocol.table_name(), tx).await?;
	let mut checked = Vec::new();
	for role in roles
		.iter()
		.filter(|role| role.role.kind() == SourceRoleKind::Ledger)
	{
		if !checked.contains(&role.table) {
			validate_source_heap(role.table, tx).await?;
			validate_schema_ledger_registration_in_current_transaction(tx, role.table).await?;
			checked.push(role.table);
		}
	}
	validate_prepared_protocol(tx, protocol, protocol_ledger).await
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum V10SourceBindingError {
	#[error("published-v10 source roles overlap between {left} and {right}")]
	OverlappingSourceRoles {
		left: &'static str,
		right: &'static str,
	},
	#[error("native container overlaps published-v10 source role {source_role}")]
	NativeSourceContainerOverlap { source_role: &'static str },
	#[error("published-v10 source descriptor version is unsupported")]
	UnsupportedDescriptorVersion,
	#[error("published-v10 source key algorithm version is unsupported")]
	UnsupportedKeyAlgorithmVersion,
	#[error("published-v10 source roles are missing or repeated")]
	InvalidSourceRoles,
	#[error("published-v10 source descriptor has an invalid qualified identifier")]
	InvalidDescriptorIdentifier,
	#[error("published-v10 source descriptor has an invalid Fleet root")]
	InvalidDescriptorRoot,

	#[error("published-v10 logical key mapping is invalid")]
	Key(#[from] kv::KeyError),
}
