//! Fixed published source seed and candidate selection SQL share their actual predicate owner.
use crate::db::{AuditedSqlBuilder, PgQualifiedTableName};
use crate::kv::raw::v10::catalog::KvCatalog;
pub(crate) fn append_source_slot_seed(sql: &mut AuditedSqlBuilder, table: &PgQualifiedTableName) {
	sql.push_text("INSERT INTO ");
	sql.push_argument(table);
	sql.push_text(" (key, value, expires_at, updated_at) SELECT key, ''::pg_catalog.bytea, pg_catalog.statement_timestamp() OPERATOR(pg_catalog.-) INTERVAL '1 second', pg_catalog.statement_timestamp() FROM pg_catalog.UNNEST($1::pg_catalog.text[]) AS input(key) ORDER BY input.key COLLATE pg_catalog.\"C\" ON CONFLICT (key) DO NOTHING");
}
/// Selects and locks the first expired candidate of `$1` in bytewise key order and skips the
/// candidates that another transaction has locked.
pub(crate) fn append_source_slot_candidate(
	sql: &mut AuditedSqlBuilder,
	table: &PgQualifiedTableName,
) {
	sql.push_text("SELECT key,value,expires_at,updated_at FROM ONLY ");
	sql.push_argument(table);
	sql.push_text(" WHERE key OPERATOR(pg_catalog.=) ANY($1::pg_catalog.text[]) AND ");
	KvCatalog::append_expired_filter(sql);
	sql.push_text(
		" ORDER BY key COLLATE pg_catalog.\"C\" USING OPERATOR(pg_catalog.<) LIMIT 1 FOR UPDATE SKIP LOCKED",
	);
}
