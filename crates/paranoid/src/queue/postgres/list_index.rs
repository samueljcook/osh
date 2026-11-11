//! A task and key index bounds administrative pages without depending on execution status.

use crate::db::{AuditedSql, AuditedSqlBuilder, PgIdentifier, PgQualifiedTableName};

pub(in crate::queue) fn index_name() -> PgIdentifier {
	PgIdentifier::new("paranoid_queue_task_key")
		.expect("the Queue task listing index name is valid")
}

pub(crate) fn create_index_statement(
	table: &PgQualifiedTableName,
	allow_existing: bool,
) -> AuditedSql {
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("CREATE INDEX ");
	if allow_existing {
		sql.push_text("IF NOT EXISTS ");
	}
	sql.push_argument(&index_name());
	sql.push_text(" ON ONLY ");
	sql.push_argument(table);
	sql.push_text(
		r#" USING btree (namespace,
    ((index_fields OPERATOR(pg_catalog.->) '7461736b5f6e616d65'::pg_catalog.text
        OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text) COLLATE pg_catalog."C"), key)
WHERE body IS NOT NULL
  AND (index_fields OPERATOR(pg_catalog.->) '7461736b5f6e616d65'::pg_catalog.text
      OPERATOR(pg_catalog.->>) 'value'::pg_catalog.text) IS NOT NULL"#,
	);
	sql.finish()
}

pub(crate) fn index_validation_check(table: &PgQualifiedTableName) -> AuditedSql {
	let index = PgQualifiedTableName::new(table.schema().clone(), index_name())
		.quoted()
		.to_string();
	let table = table.quoted().to_string();
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
          AND i.indcollation[0] OPERATOR(pg_catalog.=) 0::pg_catalog.oid
          AND i.indcollation[1] OPERATOR(pg_catalog.=) 'pg_catalog."C"'::pg_catalog.regcollation::pg_catalog.oid
          AND i.indcollation[2] OPERATOR(pg_catalog.=) 0::pg_catalog.oid
          AND pg_catalog.regexp_replace(pg_catalog.pg_get_expr(i.indpred, i.indrelid, false), '[()]', '', 'g')
              OPERATOR(pg_catalog.=) 'body IS NOT NULL AND index_fields -> ''7461736b5f6e616d65''::text ->> ''value''::text IS NOT NULL'
          AND pg_catalog.pg_get_indexdef(i.indexrelid, 1, false) OPERATOR(pg_catalog.=) 'namespace'
          AND pg_catalog.pg_get_indexdef(i.indexrelid, 3, false) OPERATOR(pg_catalog.=) 'key'
          AND pg_catalog.regexp_replace(pg_catalog.pg_get_indexdef(i.indexrelid, 2, false), '[()]', '', 'g')
              OPERATOR(pg_catalog.=) 'index_fields -> ''7461736b5f6e616d65''::text ->> ''value''::text'
          AND NOT EXISTS (
              SELECT 1 FROM pg_catalog.unnest(i.indclass::pg_catalog.oid[]) WITH ORDINALITY AS classes(oid, position)
              JOIN pg_catalog.pg_opclass op ON op.oid OPERATOR(pg_catalog.=) classes.oid
              WHERE op.opcnamespace OPERATOR(pg_catalog.<>) 'pg_catalog'::pg_catalog.regnamespace
                 OR op.opcname OPERATOR(pg_catalog.<>) CASE WHEN classes.position OPERATOR(pg_catalog.=) 2 THEN 'text_ops' ELSE 'bytea_ops' END
          )
    )"#,
		index_name = index.as_bytes(),
		table_name = table.as_bytes()
	)
}
