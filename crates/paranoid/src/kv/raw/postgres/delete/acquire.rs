//! Canonical native exclusion precedes source locking; timed callers append their real clock.
use super::super::v10::source_point::append_source_snapshot_projection;
use super::super::{DatabaseOperationKind, ReservationOwnership, owned_portable_query_as};
use super::records::{HeaderRow, PhysicalStatus, RawDeleteRecords};
use super::{
	Observation, PgRawDeleteBatch, RawDeleteLayout, RawDeleteStorageError, RecordDeletionStep,
};
use crate::db::{AuditedSql, AuditedSqlBuilder, PgQualifiedTableName};
use crate::kv::raw::model::delete::RecordDeleteContention;
use crate::records::{RecordKey, StateTimestampMicros};
use std::collections::BTreeSet;
const ACQUIRE_NATIVE_LABEL: &str = "db.state_storage.raw_delete.acquire_native";
const ACQUIRE_SOURCE_LABEL: &str = "db.state_storage.raw_delete.acquire_source";
pub(super) const NULL_HEADER_BODY_SQL: &str = "NULL::pg_catalog.bool";
pub(super) const NATIVE_HEADER_BODY_SQL: &str =
	"CASE WHEN h.key IS NOT NULL THEN h.body IS NOT NULL END";
pub(super) const SOURCE_HEADER_BODY_SQL: &str =
	"CASE WHEN h.key IS NOT NULL THEN h.value IS NOT NULL END";
pub(in crate::kv) struct QueryFields<'a> {
	pub(in crate::kv) table: &'a PgQualifiedTableName,
	pub(in crate::kv) contention: RecordDeleteContention,
}
pub(in crate::kv) fn receive_native(
	records: &mut RawDeleteRecords,
	layout: RawDeleteLayout<'_>,
	rows: Vec<HeaderRow>,
) -> Result<(), RawDeleteStorageError> {
	let mut statuses = BTreeSet::new();
	let mut inserted = BTreeSet::new();
	let mut rows = rows.into_iter().peekable();
	while let Some(mut row) = rows.next_if(|row| row.0 == 1) {
		if row.5.as_ref().map(|n| n.0.as_slice()) != Some(records.namespace.as_bytes())
			|| row.2.is_some()
		{
			return Err(RawDeleteStorageError::Coverage);
		}
		row.5 = None;
		if !super::records::is_empty_frame(&row) {
			return Err(RawDeleteStorageError::Coverage);
		}
		let key = RecordKey::new(row.1.ok_or(RawDeleteStorageError::Shape)?.0);
		let index = *records.positions.get(&key).ok_or_else(|| {
			records.identify_unexpected(
				RawDeleteStorageError::Coverage,
				key.clone(),
				RecordDeletionStep::Native,
			)
		})?;
		if !inserted.insert(index) {
			return Err(records.identify(
				RawDeleteStorageError::Coverage,
				index,
				RecordDeletionStep::Native,
			));
		}
	}
	for row in rows {
		if row.0 != 2 {
			return Err(RawDeleteStorageError::Coverage);
		}
		let index = records.parse_status(layout, row)?;
		if !statuses.insert(index) {
			return Err(records.identify(
				RawDeleteStorageError::Coverage,
				index,
				RecordDeletionStep::Native,
			));
		}
	}
	if let Some(index) = (0..records.candidates.len()).find(|i| !statuses.contains(i)) {
		return Err(records.identify(
			RawDeleteStorageError::Coverage,
			index,
			RecordDeletionStep::Native,
		));
	}
	for index in inserted {
		if !matches!(
			records.candidates[index].native,
			PhysicalStatus::Reservation(_)
		) {
			return Err(records.identify(
				RawDeleteStorageError::Coverage,
				index,
				RecordDeletionStep::Native,
			));
		}
		records.candidates[index].native =
			PhysicalStatus::Reservation(ReservationOwnership::Inserted);
	}
	Ok(())
}
pub(super) fn receive_source(
	records: &mut RawDeleteRecords,
	layout: RawDeleteLayout<'_>,
	expected: &BTreeSet<usize>,
	rows: Vec<HeaderRow>,
	observation: Observation,
) -> Result<Option<StateTimestampMicros>, RawDeleteStorageError> {
	let mut seen = BTreeSet::new();
	let mut clock = None;
	for row in rows {
		if clock.is_some() {
			return Err(RawDeleteStorageError::Coverage);
		}
		if row.0 == 4 {
			if !observation.is_timed() {
				return Err(RawDeleteStorageError::Coverage);
			}
			clock = Some(parse_clock(row)?);
			continue;
		}
		if row.0 != 3 {
			return Err(RawDeleteStorageError::Coverage);
		}
		let index = records.parse_status(layout, row)?;
		if !expected.contains(&index) || !seen.insert(index) {
			return Err(records.identify(
				RawDeleteStorageError::Coverage,
				index,
				RecordDeletionStep::Source,
			));
		}
	}
	if let Some(index) = expected.difference(&seen).next() {
		return Err(records.identify(
			RawDeleteStorageError::Coverage,
			*index,
			RecordDeletionStep::Source,
		));
	}
	if observation.is_timed() && clock.is_none() {
		return Err(RawDeleteStorageError::Coverage);
	}
	Ok(clock)
}
pub(super) async fn relocation(
	batch: &mut PgRawDeleteBatch<'_, '_, ()>,
) -> Result<StateTimestampMicros, RawDeleteStorageError> {
	let sql = native_query(QueryFields {
		table: batch
			.layout
			.native_table()
			.ok_or(RawDeleteStorageError::Shape)?,
		contention: batch.request.contention(),
	});
	batch.tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		ACQUIRE_NATIVE_LABEL,
		Some(sql.as_str()),
	);
	let keys = batch
		.records
		.candidates
		.iter()
		.map(|c| c.key.as_bytes().to_vec())
		.collect::<Vec<_>>();
	let rows = owned_portable_query_as::<HeaderRow>(sql)
		.bind_validated(batch.records.namespace.as_bytes())
		.bind_validated(keys)
		.fetch_all(batch.tx.inner.as_mut())
		.await
		.map_err(|e| e.with_operation_label(ACQUIRE_NATIVE_LABEL))?;
	receive_native(&mut batch.records, batch.layout, rows)?;
	let mut texts = Vec::new();
	let mut ranks = Vec::new();
	let mut expected = BTreeSet::new();
	for (index, candidate) in batch.records.candidates.iter().enumerate() {
		if candidate.native.is_held() {
			texts.push(
				std::str::from_utf8(candidate.key.as_bytes())
					.map_err(|e| {
						batch
							.records
							.identify(e.into(), index, RecordDeletionStep::Source)
					})?
					.to_owned(),
			);
			ranks.push(i64::try_from(index).map_err(|_| {
				batch.records.identify(
					RawDeleteStorageError::Shape,
					index,
					RecordDeletionStep::Source,
				)
			})?);
			expected.insert(index);
		}
	}
	if expected.is_empty() {
		return super::super::PgPointStorage::observe_time(batch.tx)
			.await
			.map_err(RawDeleteStorageError::from);
	}
	let mut builder = source_query_builder(QueryFields {
		table: batch
			.layout
			.source_table()
			.ok_or(RawDeleteStorageError::Shape)?,
		contention: batch.request.contention(),
	});
	append_clock(&mut builder);
	let sql = builder.finish();
	batch.tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		ACQUIRE_SOURCE_LABEL,
		Some(sql.as_str()),
	);
	let rows = owned_portable_query_as::<HeaderRow>(sql)
		.bind_validated(texts)
		.bind_validated(ranks)
		.fetch_all(batch.tx.inner.as_mut())
		.await
		.map_err(|e| e.with_operation_label(ACQUIRE_SOURCE_LABEL))?;
	receive_source(
		&mut batch.records,
		batch.layout,
		&expected,
		rows,
		Observation::Timed,
	)?
	.ok_or(RawDeleteStorageError::Coverage)
}
pub(in crate::kv) fn parse_clock(
	row: HeaderRow,
) -> Result<StateTimestampMicros, RawDeleteStorageError> {
	if row.0 != 4
		|| row.1.is_some()
		|| row.2.is_some()
		|| row.3.is_some()
		|| row.4.is_some()
		|| row.5.is_some()
		|| row.6.is_some()
		|| row.7.is_some()
		|| row.8.is_some()
		|| row.9.is_some()
		|| row.10.is_some()
		|| row.11.is_some()
	{
		return Err(RawDeleteStorageError::Shape);
	}
	Ok(StateTimestampMicros::new(
		row.12.ok_or(RawDeleteStorageError::Shape)?,
	))
}
pub(in crate::kv) fn append_clock(sql: &mut AuditedSqlBuilder) {
	sql.push_text("; SELECT 4::pg_catalog.int8,NULL::pg_catalog.bytea,NULL::pg_catalog.int8,NULL::pg_catalog.bool,NULL::pg_catalog.bool,NULL::pg_catalog.bytea,");
	sql.push_text(NULL_HEADER_BODY_SQL);
	sql.push_text(",NULL::pg_catalog.int8,NULL::pg_catalog.int8,NULL::pg_catalog.text,NULL::pg_catalog.bool,NULL::pg_catalog.bool,pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.clock_timestamp()) OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8");
}
pub(in crate::kv) fn receive_source_untimed(
	records: &mut RawDeleteRecords,
	layout: RawDeleteLayout<'_>,
	expected: &BTreeSet<usize>,
	rows: Vec<HeaderRow>,
) -> Result<(), RawDeleteStorageError> {
	receive_source(
		records,
		layout,
		expected,
		rows,
		Observation::UntimedAllValued,
	)
	.map(|_| ())
}
pub(in crate::kv) fn native_query(fields: QueryFields<'_>) -> AuditedSql {
	let QueryFields { table, contention } = fields;
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("WITH ordered_keys AS MATERIALIZED (SELECT key FROM pg_catalog.unnest($2::pg_catalog.bytea[]) input(key) ORDER BY key USING OPERATOR(pg_catalog.<)) INSERT INTO ");
	sql.push_argument(table);
	sql.push_text(" (namespace,key,body,expires_at,updated_at,index_fields) SELECT $1::pg_catalog.bytea,key,NULL,NULL,NULL,'{}'::pg_catalog.jsonb FROM ordered_keys ORDER BY key USING OPERATOR(pg_catalog.<) ON CONFLICT (namespace,key) DO NOTHING RETURNING 1::pg_catalog.int8,key,NULL::pg_catalog.int8,NULL::pg_catalog.bool,NULL::pg_catalog.bool,namespace,");
	sql.push_text(NULL_HEADER_BODY_SQL);
	sql.push_text(",NULL::pg_catalog.int8,NULL::pg_catalog.int8,NULL::pg_catalog.text,NULL::pg_catalog.bool,NULL::pg_catalog.bool,NULL::pg_catalog.int8; WITH requested AS MATERIALIZED (SELECT key,ordinality OPERATOR(pg_catalog.-) 1 AS rank FROM pg_catalog.unnest($2::pg_catalog.bytea[]) WITH ORDINALITY input(key,ordinality)), present_native AS MATERIALIZED (SELECT key FROM ONLY ");
	sql.push_argument(table);
	sql.push_text(" WHERE namespace OPERATOR(pg_catalog.=) $1::pg_catalog.bytea AND key OPERATOR(pg_catalog.=) ANY($2::pg_catalog.bytea[])), locked_native AS MATERIALIZED (SELECT target.namespace,target.key,target.body,target.expires_at,target.updated_at,target.index_fields FROM ONLY ");
	sql.push_argument(table);
	sql.push_text(" target JOIN requested r ON target.key OPERATOR(pg_catalog.=) r.key WHERE target.namespace OPERATOR(pg_catalog.=) $1::pg_catalog.bytea ORDER BY target.key USING OPERATOR(pg_catalog.<)");
	super::discovery::append_lock_clause(&mut sql, contention);
	sql.push_text(
		") SELECT 2::pg_catalog.int8,r.key,r.rank,p.key IS NOT NULL,h.key IS NOT NULL,h.namespace,",
	);
	sql.push_text(NATIVE_HEADER_BODY_SQL);
	sql.push_text(",h.expires_at,h.updated_at,h.index_fields::pg_catalog.text,NULL::pg_catalog.bool,NULL::pg_catalog.bool,NULL::pg_catalog.int8 FROM requested r LEFT JOIN present_native p ON p.key OPERATOR(pg_catalog.=) r.key LEFT JOIN locked_native h ON h.key OPERATOR(pg_catalog.=) r.key ORDER BY r.rank");
	sql.finish()
}
pub(in crate::kv) fn source_query_builder(fields: QueryFields<'_>) -> AuditedSqlBuilder {
	let QueryFields { table, contention } = fields;
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("WITH requested AS MATERIALIZED (SELECT key,rank FROM ROWS FROM (pg_catalog.unnest($1::pg_catalog.text[]),pg_catalog.unnest($2::pg_catalog.int8[])) input(key,rank)), present_source AS MATERIALIZED (SELECT key FROM ONLY ");
	sql.push_argument(table);
	sql.push_text(" WHERE key OPERATOR(pg_catalog.=) ANY($1::pg_catalog.text[])), locked_source AS MATERIALIZED (SELECT target.key,target.value,target.expires_at,target.updated_at FROM ONLY ");
	sql.push_argument(table);
	sql.push_text(" target JOIN requested r ON target.key OPERATOR(pg_catalog.=) r.key ORDER BY target.key COLLATE pg_catalog.\"C\" USING OPERATOR(pg_catalog.<)");
	super::discovery::append_lock_clause(&mut sql, contention);
	sql.push_text("), source_raw AS (SELECT key,value,expires_at,updated_at,FALSE AS inserted_reservation FROM locked_source");
	append_source_snapshot_projection(&mut sql);
	sql.push_text(" SELECT 3::pg_catalog.int8,r.key_bytes,r.rank,p.key IS NOT NULL,h.key IS NOT NULL,NULL::pg_catalog.bytea,");
	sql.push_text(SOURCE_HEADER_BODY_SQL);
	sql.push_text(",h.expiry_micros,h.update_micros,NULL::pg_catalog.text,h.expiry_valid,h.update_valid,NULL::pg_catalog.int8 FROM (SELECT key,rank,pg_catalog.convert_to(key,'UTF8') AS key_bytes FROM requested) r LEFT JOIN present_source p ON p.key OPERATOR(pg_catalog.=) r.key LEFT JOIN source_snapshot h ON h.key OPERATOR(pg_catalog.=) r.key ORDER BY r.rank");
	sql
}
