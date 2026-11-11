//! Index definitions for selecting expired Raw records within a namespace.

use super::{DatabaseOperationKind, PgPointStorage, SchemaError, WriteTx, owned_portable_query};
use crate::db::{AuditedSql, AuditedSqlBuilder, PgIdentifier, PgQualifiedTableName};
use primitives_rs::crypto::algorithms::blake3;

pub(crate) fn record_expiry_index_name(table: &PgQualifiedTableName) -> PgIdentifier {
	let digest = blake3::hash(table.quoted().to_string().as_bytes()).to_hex();
	PgIdentifier::new(format!("paranoid_record_expiry_{}", &digest[..24]))
		.expect("the generated expiry index name is a valid identifier")
}

pub(crate) fn record_expiry_index_statement(
	table: &PgQualifiedTableName,
	index: &PgIdentifier,
	allow_existing: bool,
) -> AuditedSql {
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("CREATE INDEX ");
	if allow_existing {
		sql.push_text("IF NOT EXISTS ");
	}
	sql.push_argument(index);
	sql.push_text(" ON ONLY ");
	sql.push_argument(table);
	sql.push_text(
		" USING btree (namespace, expires_at, key) WHERE body IS NOT NULL AND expires_at IS NOT NULL",
	);
	sql.finish()
}

pub(crate) fn record_expiry_index_check(
	table: &PgQualifiedTableName,
	index: &PgIdentifier,
) -> AuditedSql {
	let qualified_index = PgQualifiedTableName::new(table.schema().clone(), index.clone());
	let index_name = qualified_index.quoted().to_string();
	let table_name = table.quoted().to_string();
	crate::audited_sql!(
		r#"EXISTS (
            SELECT 1 FROM pg_catalog.pg_index i
            JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) i.indexrelid
            JOIN pg_catalog.pg_am am ON am.oid OPERATOR(pg_catalog.=) c.relam
            WHERE i.indexrelid OPERATOR(pg_catalog.=) pg_catalog.to_regclass(pg_catalog.convert_from({index_name}, 'UTF8'))
              AND i.indrelid OPERATOR(pg_catalog.=) pg_catalog.to_regclass(pg_catalog.convert_from({table_name}, 'UTF8'))
              AND am.amname OPERATOR(pg_catalog.=) 'btree'
              AND i.indisvalid AND i.indisready AND NOT i.indisunique
              AND i.indnatts OPERATOR(pg_catalog.=) 3 AND i.indnkeyatts OPERATOR(pg_catalog.=) 3
              AND i.indoption OPERATOR(pg_catalog.=) '0 0 0'::pg_catalog.int2vector
              AND i.indcollation OPERATOR(pg_catalog.=) '0 0 0'::pg_catalog.oidvector
              AND pg_catalog.regexp_replace(pg_catalog.pg_get_expr(i.indpred, i.indrelid, false), '[()]', '', 'g')
                  OPERATOR(pg_catalog.=) 'body IS NOT NULL AND expires_at IS NOT NULL'
              AND pg_catalog.pg_get_indexdef(i.indexrelid, 1, false) OPERATOR(pg_catalog.=) 'namespace'
              AND pg_catalog.pg_get_indexdef(i.indexrelid, 2, false) OPERATOR(pg_catalog.=) 'expires_at'
              AND pg_catalog.pg_get_indexdef(i.indexrelid, 3, false) OPERATOR(pg_catalog.=) 'key'
              AND NOT EXISTS (
                  SELECT 1 FROM pg_catalog.unnest(i.indclass::pg_catalog.oid[]) WITH ORDINALITY AS classes(oid, position)
                  JOIN pg_catalog.pg_opclass op ON op.oid OPERATOR(pg_catalog.=) classes.oid
                  WHERE op.opcnamespace OPERATOR(pg_catalog.<>) 'pg_catalog'::pg_catalog.regnamespace
                     OR op.opcname OPERATOR(pg_catalog.<>) CASE WHEN classes.position OPERATOR(pg_catalog.=) 2 THEN 'int8_ops' ELSE 'bytea_ops' END
              )
        )"#,
		index_name = index_name.as_bytes(),
		table_name = table_name.as_bytes(),
	)
}

impl PgPointStorage {
	pub(crate) async fn create_record_expiry_index(
		&self,
		tx: &mut WriteTx<'_>,
	) -> Result<(), SchemaError> {
		let sql = record_expiry_index_statement(
			self.table(),
			&record_expiry_index_name(self.table()),
			false,
		);
		const LABEL: &str = "db.state_storage.create_record_expiry_index";
		tx.record_database_operation(DatabaseOperationKind::Execute, LABEL, Some(sql.as_str()));
		owned_portable_query(sql)
			.execute(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(LABEL))?;
		Ok(())
	}
}
