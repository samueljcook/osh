//! Immutable inverse association from the actual source protocol to one native destination.
//!
//! One additive row uses the existing protocol schema ledger. Component versions and source
//! data are preserved. The source protocol relation identity determines the row key, so a
//! changed Fleet root or source role mapping cannot claim a second row in the bound ledger.

use super::super::{
	NativeInstallationError, NativeMappingOrigin, PgNativeStorageBindings, PgV10SourceDescriptor,
	frame_fingerprint_input,
};
use crate::db::{PgQualifiedTableName, Tx, WriteTx};
use crate::installation::schema_ledger::{
	claim_component_registration_in_existing_ledger,
	fetch_component_registration_in_current_transaction,
};
use crate::installation::{
	ComponentSchemaRegistration, ComponentSchemaVersion, schema_instance_key_for_parts,
};
use primitives_rs::crypto::algorithms::blake3;
use primitives_rs::id::time_sortable::SortableId;

const IMPORT_BINDING_COMPONENT: &str = "paranoid.native_v10_import_binding";
const IMPORT_BINDING_VERSION: i32 = 1;
const IMPORT_BINDING_DOMAIN: &[u8] = b"paranoid.pg-v10-import-association";

struct ImportAssociation {
	instance_key: String,
	fingerprint: String,
}

impl ImportAssociation {
	fn new(
		protocol: &PgQualifiedTableName,
		source_fingerprint: blake3::Hash,
		native: &PgNativeStorageBindings,
		installation_id: SortableId,
	) -> Self {
		let mut hasher = blake3::Hasher::new();
		frame_fingerprint_input(&mut hasher, IMPORT_BINDING_DOMAIN);
		frame_fingerprint_input(&mut hasher, &IMPORT_BINDING_VERSION.to_be_bytes());
		frame_fingerprint_input(&mut hasher, source_fingerprint.as_bytes());
		frame_fingerprint_input(
			&mut hasher,
			native
				.fingerprint(NativeMappingOrigin::ImportedV10)
				.as_bytes(),
		);
		frame_fingerprint_input(&mut hasher, installation_id.as_bytes());
		Self {
			instance_key: schema_instance_key_for_parts([("protocol_table", protocol)]),
			fingerprint: hasher.finalize().to_hex().to_string(),
		}
	}
	fn version(&self) -> ComponentSchemaVersion<'_> {
		ComponentSchemaVersion {
			component: IMPORT_BINDING_COMPONENT,
			instance_key: &self.instance_key,
			version: IMPORT_BINDING_VERSION,
			fingerprint: &self.fingerprint,
		}
	}
}

/// The caller owns bootstrap serialization and source shared authority through the commit.
/// Any occupied association is rejected; uncertain creation recovers through recorded opening.
pub(super) async fn claim_descriptor_import_association(
	tx: &mut WriteTx<'_>,
	source: &PgV10SourceDescriptor,
	native: &PgNativeStorageBindings,
	installation_id: SortableId,
) -> Result<(), NativeInstallationError> {
	let mut completion = tx.track_state_mutation();
	let association = ImportAssociation::new(
		source.protocol().table_name(),
		source.fingerprint(),
		native,
		installation_id,
	);
	let ledger = source.protocol_ledger();
	if claim_component_registration_in_existing_ledger(tx, ledger, association.version())
		.await?
		.is_some()
	{
		return Err(NativeInstallationError::SourceAlreadyAssociated);
	}
	completion.complete();
	Ok(())
}

/// Uses the same marker inputs from parsed durable facts, without source data handles.
pub(crate) async fn verify_descriptor_import_association(
	tx: &mut Tx<'_>,
	source: &PgV10SourceDescriptor,
	native: &PgNativeStorageBindings,
	installation_id: SortableId,
) -> Result<(), NativeInstallationError> {
	let association = ImportAssociation::new(
		source.protocol().table_name(),
		source.fingerprint(),
		native,
		installation_id,
	);
	verify_in_ledger(tx, source.protocol_ledger(), association).await
}

async fn verify_in_ledger(
	tx: &mut Tx<'_>,
	ledger: &PgQualifiedTableName,
	association: ImportAssociation,
) -> Result<(), NativeInstallationError> {
	let mut completion = tx.track_state_mutation();
	let recorded =
		fetch_component_registration_in_current_transaction(tx, ledger, association.version())
			.await?;
	match recorded {
		ComponentSchemaRegistration::Recorded {
			version,
			fingerprint,
		} if version == IMPORT_BINDING_VERSION && fingerprint == association.fingerprint => {}
		ComponentSchemaRegistration::Absent => {
			return Err(NativeInstallationError::SourceAssociationAbsent);
		}
		_ => return Err(NativeInstallationError::SourceAssociationMismatch),
	}
	completion.complete();
	Ok(())
}
