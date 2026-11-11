//! One request relation preserves every caller marker, including physical absence.

use super::super::page::{
	PhysicalRecordObservation, RawPhysicalRecordFields, RawReadLayout, append_logical_projection,
	decode_physical_observation,
};
use super::super::v10::source_point::append_source_snapshot_projection;
use super::super::{AuditedSql, Bytea, DatabaseOperationKind, Tx, owned_portable_query_as};
use super::{
	PhysicalManyReadFields, RawManyReadFields, RawManyStorageError, RawPhysicalManyObservation,
};
use crate::db::{AuditedSqlBuilder, PgQualifiedTableName};
use crate::kv::raw::model::many::RawManyReadResult;
use crate::records::{RecordIdentity, RecordKey, StateTimestampMicros};

const READ_RAW_MANY_LABEL: &str = "db.state_storage.read_raw_many";
type ReadRow = (
	i64,
	Bytea,
	Option<Bytea>,
	Option<Bytea>,
	Option<i64>,
	Option<i64>,
	Option<String>,
	bool,
	Option<Bytea>,
	Option<i64>,
	Option<i64>,
	Option<bool>,
	Option<bool>,
	i64,
);

pub(super) async fn read(
	tx: &mut Tx<'_>,
	fields: RawManyReadFields<'_>,
	layout: RawReadLayout<'_>,
) -> Result<RawManyReadResult, RawManyStorageError> {
	let physical_fields = PhysicalManyReadFields {
		namespace: fields.namespace,
		keys: fields.request.keys(),
		source_texts: fields.source_texts,
	};
	require_fields(&physical_fields, &layout)?;
	let mut completion = tx.track_state_mutation();
	let observation = fetch_observation(tx, physical_fields, layout).await?;
	let records = observation
		.records
		.into_iter()
		.map(PhysicalRecordObservation::into_raw)
		.collect();
	let outcome = fields.request.finish(records, observation.observed_at);
	completion.complete();
	Ok(outcome)
}
fn require_fields(
	fields: &PhysicalManyReadFields<'_>,
	layout: &RawReadLayout<'_>,
) -> Result<(), RawManyStorageError> {
	if fields.keys.is_empty() {
		return Err(RawManyStorageError::EmptyRequest);
	}
	if layout.is_source_domain() != fields.source_texts.is_some() {
		return Err(RawManyStorageError::SourceBindingMismatch);
	}
	Ok(())
}
async fn fetch_observation(
	tx: &mut Tx<'_>,
	fields: PhysicalManyReadFields<'_>,
	layout: RawReadLayout<'_>,
) -> Result<RawPhysicalManyObservation, RawManyStorageError> {
	let keys = fields
		.keys
		.iter()
		.map(|key| key.as_bytes().to_vec())
		.collect::<Vec<_>>();
	let sql = build_query(&layout);
	tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		READ_RAW_MANY_LABEL,
		Some(sql.as_str()),
	);
	let query = owned_portable_query_as::<ReadRow>(sql)
		.bind_validated(fields.namespace.as_bytes())
		.bind_validated(keys);
	let rows = match fields.source_texts {
		Some(texts) => {
			query
				.bind_validated(texts)
				.fetch_all(tx.inner.as_mut())
				.await
		}
		None => query.fetch_all(tx.inner.as_mut()).await,
	}
	.map_err(|error| error.with_operation_label(READ_RAW_MANY_LABEL))?;
	parse_rows(fields, rows)
}

fn append_native(sql: &mut AuditedSqlBuilder, table: &PgQualifiedTableName) {
	sql.push_text("native_selected_rows AS (SELECT namespace, key AS key_bytes, body, expires_at, updated_at, index_fields FROM ONLY ");
	sql.push_argument(table);
	sql.push_text(" WHERE namespace OPERATOR(pg_catalog.=) $1::pg_catalog.bytea AND key OPERATOR(pg_catalog.=) ANY($2::pg_catalog.bytea[]) AND body IS NOT NULL)");
}
fn append_source(sql: &mut AuditedSqlBuilder, table: &PgQualifiedTableName) {
	sql.push_text("source_raw AS (SELECT key, value, expires_at, updated_at, FALSE AS inserted_reservation FROM ONLY ");
	sql.push_argument(table);
	sql.push_text(" WHERE key OPERATOR(pg_catalog.=) ANY($3::pg_catalog.text[])");
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
), requested AS (
    SELECT input.key_bytes, input.ordinality::pg_catalog.int8 AS marker
    FROM pg_catalog.unnest($2::pg_catalog.bytea[]) WITH ORDINALITY input(key_bytes, ordinality)
), observation AS MATERIALIZED (
    SELECT pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.clock_timestamp())
        OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8 AS observed_at,
        $1::pg_catalog.bytea AS requested_namespace
)
SELECT requested.marker, requested.key_bytes, record.native_namespace,
    record.native_body, record.native_expiry, record.native_update, record.native_indexes,
    COALESCE(record.source_present, FALSE), record.source_body, record.source_expiry,
    record.source_update, record.expiry_valid, record.update_valid, observation.observed_at
FROM requested CROSS JOIN observation
LEFT JOIN logical_records record ON requested.key_bytes OPERATOR(pg_catalog.=) record.key_bytes
ORDER BY requested.marker USING OPERATOR(pg_catalog.<)
"#,
	);
	sql.finish()
}

fn parse_rows(
	fields: PhysicalManyReadFields<'_>,
	rows: Vec<ReadRow>,
) -> Result<RawPhysicalManyObservation, RawManyStorageError> {
	let count = fields.keys.len();
	if rows.len() != count {
		return Err(RawManyStorageError::RequestCoverage);
	}
	let observed_at =
		StateTimestampMicros::new(rows.first().ok_or(RawManyStorageError::RequestCoverage)?.13);
	let mut records: Vec<Option<PhysicalRecordObservation>> = (0..count).map(|_| None).collect();
	for (
		marker,
		key,
		native_namespace,
		native_body,
		native_expires_at,
		native_updated_at,
		native_indexes,
		source_present,
		source_body,
		source_expires_at,
		source_updated_at,
		expires_at_valid,
		updated_at_valid,
		clock,
	) in rows
	{
		let key = RecordKey::new(key.0);
		let identity = RecordIdentity::new(fields.namespace.clone(), key.clone());
		let input_index = usize::try_from(marker)
			.ok()
			.and_then(|marker| marker.checked_sub(1))
			.filter(|index| *index < count);
		let value = (|| {
			let input_index = input_index.ok_or(RawManyStorageError::RequestCoverage)?;
			if fields.keys[input_index] != key || records[input_index].is_some() {
				return Err(RawManyStorageError::RequestCoverage);
			}
			if clock != observed_at.as_i64() {
				return Err(RawManyStorageError::InconsistentClock);
			}
			decode_physical_observation(
				&identity,
				RawPhysicalRecordFields {
					native_namespace,
					native_body,
					native_expires_at,
					native_updated_at,
					native_indexes,
					source_present,
					source_body,
					source_expires_at,
					source_updated_at,
					expires_at_valid,
					updated_at_valid,
				},
			)
			.map_err(RawManyStorageError::from)
		})()
		.map_err(|error| error.with_identity(identity, input_index))?;
		records[input_index.ok_or(RawManyStorageError::RequestCoverage)?] = Some(value);
	}
	let records = records
		.into_iter()
		.map(|record| record.ok_or(RawManyStorageError::RequestCoverage))
		.collect::<Result<Vec<_>, _>>()?;
	Ok(RawPhysicalManyObservation {
		records,
		observed_at,
	})
}
