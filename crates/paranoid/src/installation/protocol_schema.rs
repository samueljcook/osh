use super::protocol::{ProtocolDef, ProtocolUpgradePolicy};
use super::{
	ComponentSchemaVersion, Protocol, ProtocolError, schema_instance_key_for_parts,
	validate_component_schema_version_in_current_transaction,
};
use crate::db::{
	DatabaseError, DatabaseOperationKind, PgQualifiedTableName, SchemaError, Tx,
	owned_portable_query_as, owned_portable_query_scalar,
};

/// The protocol relation's own physical identity in the schema ledger.
const PROTOCOL_SCHEMA_COMPONENT: &str = "paranoid_protocol";
const PROTOCOL_SCHEMA_VERSION: i32 = 1;
const PROTOCOL_SCHEMA_FINGERPRINT: &str = "paranoid.protocol-table.v1";

fn protocol_schema_version(instance_key: &str) -> ComponentSchemaVersion<'_> {
	ComponentSchemaVersion {
		component: PROTOCOL_SCHEMA_COMPONENT,
		instance_key,
		version: PROTOCOL_SCHEMA_VERSION,
		fingerprint: PROTOCOL_SCHEMA_FINGERPRINT,
	}
}

fn protocol_schema_instance_key(protocol: &Protocol) -> String {
	schema_instance_key_for_parts([("protocol_table", protocol.table_name())])
}

/// The caller holds the exclusive protocol relation lock until completion.
async fn record_protocol_def(
	tx: &mut Tx<'_>,
	protocol: &Protocol,
	def: &ProtocolDef,
	label: &'static str,
) -> Result<(), SchemaError> {
	let statement = format!(
		"INSERT INTO {} (singleton, epoch, fingerprint) VALUES (1, $1, $2) \
         ON CONFLICT (singleton) DO UPDATE SET epoch = EXCLUDED.epoch, fingerprint = EXCLUDED.fingerprint \
         RETURNING singleton, epoch, fingerprint",
		protocol.table_name().quoted()
	);
	tx.record_database_operation(DatabaseOperationKind::FetchAll, label, Some(&statement));
	let rows =
		owned_portable_query_as::<(i32, i32, String)>(sqlx::AssertSqlSafe(statement.as_str()))
			.bind_validated(def.epoch)
			.bind_validated(def.fingerprint.as_ref())
			.fetch_all(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(label))?;
	if rows.as_slice() != [(1, def.epoch, def.fingerprint.clone().into_owned())] {
		return Err(ProtocolError::State {
			reason: "protocol write did not return the selected epoch and fingerprint",
		}
		.into());
	}
	Ok(())
}

/// Checks existing protocol metadata and its physical identity without changing either.
pub(crate) async fn validate_prepared_protocol(
	tx: &mut Tx<'_>,
	protocol: &Protocol,
	ledger: &PgQualifiedTableName,
) -> Result<(), SchemaError> {
	validate_protocol_catalog(tx, protocol).await?;
	let instance_key = protocol_schema_instance_key(protocol);
	validate_component_schema_version_in_current_transaction(
		tx,
		ledger,
		protocol_schema_version(&instance_key),
	)
	.await
}

/// The caller holds the exclusive protocol lock and has validated the prepared state.
pub(crate) async fn record_activation(
	tx: &mut Tx<'_>,
	protocol: &Protocol,
	policy: &ProtocolUpgradePolicy,
) -> Result<(), SchemaError> {
	record_protocol_def(tx, protocol, policy.target(), "db.protocol.activate").await
}

async fn validate_protocol_catalog(
	tx: &mut Tx<'_>,
	protocol: &Protocol,
) -> Result<(), SchemaError> {
	match protocol_catalog_mismatch(tx, protocol.table_name()).await? {
		None => Ok(()),
		Some(reason) => Err(ProtocolError::State { reason }.into()),
	}
}

/// Whether `table` is a protocol table with the columns, primary key, and checks that a v10
/// bootstrap creates. The check reads only the catalog, so a relation of another kind or shape
/// with the same name yields `false`, and so does an absent relation.
pub(crate) async fn is_protocol_table(
	tx: &mut Tx<'_>,
	table: &PgQualifiedTableName,
) -> Result<bool, DatabaseError> {
	Ok(protocol_catalog_mismatch(tx, table).await?.is_none())
}

/// Why the catalog does not describe `table` as a protocol table, or `None` when it does.
async fn protocol_catalog_mismatch(
	tx: &mut Tx<'_>,
	table: &PgQualifiedTableName,
) -> Result<Option<&'static str>, DatabaseError> {
	let statement = r#"
SELECT (
    SELECT pg_catalog.count(*) OPERATOR(pg_catalog.=) 3 FROM pg_catalog.pg_attribute a
    LEFT JOIN pg_catalog.pg_collation c ON c.oid OPERATOR(pg_catalog.=) a.attcollation
    WHERE a.attrelid OPERATOR(pg_catalog.=) pg_catalog.to_regclass($1) AND a.attnum OPERATOR(pg_catalog.>) 0 AND NOT a.attisdropped AND a.attnotnull
      AND ((a.attname OPERATOR(pg_catalog.=) ANY (ARRAY['singleton', 'epoch']::pg_catalog.text[]) AND a.atttypid OPERATOR(pg_catalog.=) 'pg_catalog.int4'::pg_catalog.regtype)
        OR (a.attname OPERATOR(pg_catalog.=) 'fingerprint' AND a.atttypid OPERATOR(pg_catalog.=) 'pg_catalog.text'::pg_catalog.regtype AND c.collname OPERATOR(pg_catalog.=) 'C'))
) AND EXISTS (
    SELECT 1 FROM pg_catalog.pg_index i
    JOIN pg_catalog.pg_attribute a ON a.attrelid OPERATOR(pg_catalog.=) i.indrelid AND a.attname OPERATOR(pg_catalog.=) 'singleton'
    JOIN pg_catalog.pg_opclass o ON o.oid OPERATOR(pg_catalog.=) i.indclass[0]
    WHERE i.indrelid OPERATOR(pg_catalog.=) pg_catalog.to_regclass($1) AND i.indisprimary AND i.indisunique
      AND i.indisvalid AND i.indisready AND i.indimmediate AND i.indnkeyatts OPERATOR(pg_catalog.=) 1
      AND i.indkey[0] OPERATOR(pg_catalog.=) a.attnum AND i.indexprs IS NULL AND i.indpred IS NULL
      AND o.opcname OPERATOR(pg_catalog.=) 'int4_ops'
)"#;
	tx.record_database_operation(
		DatabaseOperationKind::FetchOne,
		"db.protocol.validate_columns_and_primary_key",
		Some(statement),
	);
	let valid = owned_portable_query_scalar::<bool>(statement)
		.bind_validated(table.quoted().to_string())
		.fetch_one(tx.inner.as_mut())
		.await
		.map_err(|error| {
			error.with_operation_label("db.protocol.validate_columns_and_primary_key")
		})?;
	if !valid {
		return Ok(Some("protocol columns or primary key are incompatible"));
	}
	let statement = crate::db::catalog::BUILTIN_CHECK_EXPRESSIONS_SQL;
	tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		"db.protocol.validate_checks",
		Some(statement),
	);
	let checks = owned_portable_query_scalar::<String>(statement)
		.bind_validated(table.quoted().to_string())
		.fetch_all(tx.inner.as_mut())
		.await
		.map_err(|error| error.with_operation_label("db.protocol.validate_checks"))?;
	let normalized = checks
		.iter()
		.map(|value| crate::db::catalog::normalize_builtin_check_expression(value))
		.collect::<Vec<_>>();
	for required in [
		"singleton=1",
		"epoch>0",
		"(octet_length(fingerprint)>0)AND(octet_length(fingerprint)<=256)",
	] {
		if !normalized.iter().any(|value| value == required) {
			return Ok(Some("protocol domain constraints are incompatible"));
		}
	}
	Ok(None)
}
