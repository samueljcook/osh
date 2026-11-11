//! One merged aggregate reports its own clock and the first deterministic error identity.

use super::super::page::{RawReadLayout, append_logical_projection, text_bound};
use super::super::v10::source_point::append_source_snapshot_projection;
use super::super::{Bytea, DatabaseOperationKind, Tx, owned_portable_query_as};
use super::{RawAggregateStorageError, RawCountSelection};
use crate::db::{AuditedSql, AuditedSqlBuilder, PgQualifiedTableName};
use crate::kv::raw::model::aggregate::RecordLiveCount;
use crate::records::{RecordIdentity, RecordKey, StateTimestampMicros};

const COUNT_RAW_LIVE_LABEL: &str = "db.state_storage.count_raw_live";
type Row = (i64, i64, Option<Bytea>, Option<i64>);
pub(super) async fn count(
	tx: &mut Tx<'_>,
	fields: RawCountSelection<'_>,
	layout: RawReadLayout<'_>,
) -> Result<RecordLiveCount, RawAggregateStorageError> {
	let mut completion = tx.track_state_mutation();
	let sql = build_query(&layout);
	let prefix = fields.selection.prefix();
	let upper = prefix.and_then(|prefix| prefix.exclusive_upper_bound());
	tx.record_database_operation(
		DatabaseOperationKind::FetchOne,
		COUNT_RAW_LIVE_LABEL,
		Some(sql.as_str()),
	);
	let query = owned_portable_query_as::<Row>(sql)
		.bind_validated(fields.namespace.as_bytes())
		.bind_validated(prefix.map(|prefix| prefix.as_bytes()))
		.bind_validated(upper.as_deref());
	let query = if layout.is_source_domain() {
		query
			.bind_validated(text_bound(prefix.map(|prefix| prefix.as_bytes())))
			.bind_validated(text_bound(upper.as_deref()))
	} else {
		query
	};
	let (count, clock, error_key, error_kind) = query
		.fetch_one(tx.inner.as_mut())
		.await
		.map_err(|error| error.with_operation_label(COUNT_RAW_LIVE_LABEL))?;
	let count = u64::try_from(count).map_err(|_| RawAggregateStorageError::InvalidObservation)?;
	match (error_key, error_kind) {
		(None, None) => {}
		(Some(key), Some(kind @ (1 | 2))) => {
			let key = RecordKey::new(key.0);
			if !fields.selection.contains(&key)
				|| !layout.is_source_domain()
				|| (kind == 1 && !matches!(layout, RawReadLayout::Relocating(_)))
			{
				return Err(RawAggregateStorageError::InvalidObservation);
			}
			let identity = RecordIdentity::new(fields.namespace.clone(), key);
			return Err(if kind == 1 {
				RawAggregateStorageError::DuplicatePhysicalValues { identity }
			} else {
				RawAggregateStorageError::SourceExpiry { identity }
			});
		}
		_ => return Err(RawAggregateStorageError::InvalidObservation),
	}
	let result = RecordLiveCount::new(count, StateTimestampMicros::new(clock));
	completion.complete();
	Ok(result)
}
const BOUNDS_SQL: &str = " WHERE ($2::pg_catalog.bytea IS NULL OR key_bytes OPERATOR(pg_catalog.>=) $2::pg_catalog.bytea) AND ($3::pg_catalog.bytea IS NULL OR key_bytes OPERATOR(pg_catalog.<) $3::pg_catalog.bytea)";
// The byte bounds remain authoritative. The text forms of `$2` and `$3` also permit a seek on the
// source key index; a bound that is not valid UTF-8 is NULL and keeps only its byte comparison.
const SOURCE_TEXT_BOUNDS_SQL: &str = " AND ($4::pg_catalog.text IS NULL OR key OPERATOR(pg_catalog.~>=~) $4::pg_catalog.text) AND ($5::pg_catalog.text IS NULL OR key OPERATOR(pg_catalog.~<~) $5::pg_catalog.text)";
fn append_native(sql: &mut AuditedSqlBuilder, table: &PgQualifiedTableName) {
	sql.push_text("native_rows AS (SELECT namespace,key AS key_bytes,body,expires_at,updated_at,index_fields FROM ONLY ");
	sql.push_argument(table);
	sql.push_text(" WHERE namespace OPERATOR(pg_catalog.=) $1::pg_catalog.bytea AND body IS NOT NULL), native_selected_rows AS (SELECT * FROM native_rows");
	sql.push_text(BOUNDS_SQL);
	sql.push_text(")");
}
fn append_source(sql: &mut AuditedSqlBuilder, table: &PgQualifiedTableName) {
	sql.push_text("source_raw AS (SELECT key,value,expires_at,updated_at,FALSE AS inserted_reservation FROM (SELECT key,value,expires_at,updated_at,pg_catalog.convert_to(key,'UTF8') AS key_bytes FROM ONLY ");
	sql.push_argument(table);
	sql.push_text(") source_rows");
	sql.push_text(BOUNDS_SQL);
	sql.push_text(SOURCE_TEXT_BOUNDS_SQL);
	append_source_snapshot_projection(sql);
}
fn build_query(layout: &RawReadLayout<'_>) -> AuditedSql {
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("WITH ");
	match layout {
		RawReadLayout::Native(storage) => append_native(&mut sql, storage.table()),
		RawReadLayout::Source(storage) => append_source(&mut sql, storage.table()),
		RawReadLayout::Relocating(storage) => {
			append_source(&mut sql, storage.source_table());
			sql.push_text(", ");
			append_native(&mut sql, storage.native_storage().table());
		}
	}
	sql.push_text(", logical_records AS (");
	append_logical_projection(&mut sql, layout);
	sql.push_text(
		r#"
), observation AS MATERIALIZED (
    SELECT pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.clock_timestamp())
        OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8 AS observed_at,
        $1::pg_catalog.bytea AS requested_namespace
), classified AS (
    SELECT key_bytes,
        CASE WHEN native_namespace IS NOT NULL AND source_present THEN 1::pg_catalog.int8
             WHEN source_present AND NOT expiry_valid THEN 2::pg_catalog.int8 END AS error_kind,
        CASE WHEN native_namespace IS NOT NULL THEN native_expiry ELSE source_expiry END AS expiry,
        observation.observed_at
    FROM logical_records CROSS JOIN observation
), first_error AS (
    SELECT key_bytes,error_kind FROM classified WHERE error_kind IS NOT NULL
    ORDER BY key_bytes USING OPERATOR(pg_catalog.<) LIMIT 1
), total AS (
    SELECT pg_catalog.count(*) FILTER (WHERE error_kind IS NULL
        AND (expiry IS NULL OR expiry OPERATOR(pg_catalog.>) observed_at)) AS live_count
    FROM classified
)
SELECT total.live_count,observation.observed_at,first_error.key_bytes,first_error.error_kind
FROM total CROSS JOIN observation LEFT JOIN first_error ON TRUE
"#,
	);
	sql.finish()
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::db::{PgIdentifier, PgSchemaName};
	use crate::records::postgres::PgPointStorage;
	use crate::records::postgres::v10::{
		relocating_point::PgRelocatingPointStorage, source_point::PgSourcePointStorage,
	};

	fn table(name: &str) -> PgQualifiedTableName {
		PgQualifiedTableName::new(
			PgSchemaName::new(PgIdentifier::new("public").expect("valid schema")),
			PgIdentifier::new(name).expect("valid table"),
		)
	}

	#[test]
	fn source_domain_count_live_queries_push_down_text_bounds() {
		let source = PgSourcePointStorage::from_validated_table(table("kv_v10"));
		let source_sql = build_query(&RawReadLayout::Source(&source));
		assert!(
			source_sql
				.as_str()
				.contains("key OPERATOR(pg_catalog.~>=~) $4::pg_catalog.text")
		);
		assert!(
			source_sql
				.as_str()
				.contains("key OPERATOR(pg_catalog.~<~) $5::pg_catalog.text")
		);

		let relocating = PgRelocatingPointStorage::for_tests(
			PgPointStorage::new(table("paranoid_records")),
			table("kv_v10"),
		);
		let relocating_sql = build_query(&RawReadLayout::Relocating(&relocating));
		assert!(
			relocating_sql
				.as_str()
				.contains("key OPERATOR(pg_catalog.~>=~) $4::pg_catalog.text")
		);
		assert!(
			relocating_sql
				.as_str()
				.contains("key OPERATOR(pg_catalog.~<~) $5::pg_catalog.text")
		);

		let native = PgPointStorage::new(table("paranoid_records"));
		let native_sql = build_query(&RawReadLayout::Native(&native));
		assert!(!native_sql.as_str().contains("OPERATOR(pg_catalog.~>=~)"));
	}
}
