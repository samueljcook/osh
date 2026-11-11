//! Advisory candidate selection and fixed single-location locking share actual physical projections.
use super::super::page::{RawReadLayout, append_logical_projection, text_bound};
use super::super::v10::source_point::append_source_snapshot_projection;
use super::super::{DatabaseOperationKind, owned_portable_query_as};
use super::error::RawDeleteRecordErrorFields;
use super::records::HeaderRow;
use super::{
	Observation, PgRawDeleteBatch, RawDeleteLayout, RawDeleteStorageError, RecordDeletionStep,
};
use crate::db::{AuditedSql, AuditedSqlBuilder, PgQualifiedTableName};
use crate::kv::{
	Key,
	raw::model::delete::{RecordDeleteContention, RecordDeleteKind},
};
use crate::records::{RecordIdentity, RecordKey, StateTimestampMicros};

const DISCOVER_LABEL: &str = "db.state_storage.raw_delete.discover";
const BOUNDS_SQL: &str = " WHERE ($2::pg_catalog.bytea IS NULL OR key_bytes OPERATOR(pg_catalog.>=) $2::pg_catalog.bytea) AND ($3::pg_catalog.bytea IS NULL OR key_bytes OPERATOR(pg_catalog.<) $3::pg_catalog.bytea)";
const WINDOW_BOUND_SQL: &str =
	" AND ($5::pg_catalog.bytea IS NULL OR key_bytes OPERATOR(pg_catalog.>) $5::pg_catalog.bytea)";
// The byte bounds remain authoritative. The text forms of `$5`, `$2`, and `$3` (or `$2` and `$3`
// on an unpaged selection) also permit a seek on the source key index; a bound that is not valid
// UTF-8 is NULL and keeps only its byte comparison.
const SOURCE_SELECTION_TEXT_BOUNDS_SQL: &str = " AND ($4::pg_catalog.text IS NULL OR key OPERATOR(pg_catalog.~>=~) $4::pg_catalog.text) AND ($5::pg_catalog.text IS NULL OR key OPERATOR(pg_catalog.~<~) $5::pg_catalog.text)";
const SOURCE_TEXT_BOUNDS_SQL: &str = " AND ($6::pg_catalog.text IS NULL OR key OPERATOR(pg_catalog.~>~) $6::pg_catalog.text) AND ($7::pg_catalog.text IS NULL OR key OPERATOR(pg_catalog.~>=~) $7::pg_catalog.text) AND ($8::pg_catalog.text IS NULL OR key OPERATOR(pg_catalog.~<~) $8::pg_catalog.text)";

/// The key bounds that a statement applies after its namespace parameter `$1`.
#[derive(Clone, Copy)]
pub(super) enum KeyBounds {
	/// The selection bounds `$2` and `$3`, and, on a source table, the text forms `$4` and `$5` of
	/// `$2` and `$3`.
	Selection,
	/// The selection bounds, the exclusive lower key bound `$5`, and, on a source table, the
	/// text forms `$6`, `$7`, and `$8` of `$5`, `$2`, and `$3`.
	SelectionAfter,
}

/// Discovers one timed batch of candidates from the beginning of the selection.
pub(super) async fn discover_timed<Clock>(
	batch: &mut PgRawDeleteBatch<'_, '_, Clock>,
) -> Result<StateTimestampMicros, RawDeleteStorageError> {
	discover(batch, Observation::Timed, None)
		.await?
		.ok_or(RawDeleteStorageError::Coverage)
}

/// Discovers one window of a selection deletion. Every candidate key is greater than `after`.
pub(super) async fn discover_window<Clock>(
	batch: &mut PgRawDeleteBatch<'_, '_, Clock>,
	after: Option<&RecordKey>,
) -> Result<(), RawDeleteStorageError> {
	discover(batch, Observation::UntimedAllValued, after).await?;
	Ok(())
}

async fn discover<Clock>(
	batch: &mut PgRawDeleteBatch<'_, '_, Clock>,
	observation: Observation,
	after: Option<&RecordKey>,
) -> Result<Option<StateTimestampMicros>, RawDeleteStorageError> {
	let sql = build_query(batch, observation);
	let prefix = batch.request.selection().prefix();
	let upper = prefix.and_then(|p| p.exclusive_upper_bound());
	batch.tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		DISCOVER_LABEL,
		Some(sql.as_str()),
	);
	let after_bytes = after.map(RecordKey::as_bytes);
	let query = owned_portable_query_as::<HeaderRow>(sql)
		.bind_validated(batch.records.namespace.as_bytes())
		.bind_validated(prefix.map(|p| p.as_bytes()))
		.bind_validated(upper.as_deref())
		.bind_validated(i64::from(batch.request.limits().max_candidates().get()) + 1)
		.bind_validated(after_bytes);
	let query = if batch.layout.source_table().is_some() {
		query
			.bind_validated(text_bound(after_bytes))
			.bind_validated(text_bound(prefix.map(|p| p.as_bytes())))
			.bind_validated(text_bound(upper.as_deref()))
	} else {
		query
	};
	let rows = query
		.fetch_all(batch.tx.inner.as_mut())
		.await
		.map_err(|e| e.with_operation_label(DISCOVER_LABEL))?;
	let cap = batch.request.limits().max_candidates().get() as usize;
	let mut count = 0usize;
	let mut statuses = std::collections::BTreeSet::new();
	let mut clock = None;
	let mut rows = rows.into_iter().peekable();
	while let Some(row) = rows.next_if(|row| row.0 == 0) {
		if row.2 != Some(i64::try_from(count).map_err(|_| RawDeleteStorageError::Shape)?)
			|| count > cap
			|| !super::records::is_empty_frame(&row)
		{
			return Err(RawDeleteStorageError::Coverage);
		}
		let key = RecordKey::new(row.1.ok_or(RawDeleteStorageError::Shape)?.0);
		let identity = RecordIdentity::new(batch.records.namespace.clone(), key.clone());
		let result = (|| {
			if !batch.physical_selection().contains(&key)
				|| after.is_some_and(|after| &key <= after)
			{
				return Err(RawDeleteStorageError::Coverage);
			}
			if batch.layout.source_table().is_some() {
				Key::from_persisted_text(std::str::from_utf8(key.as_bytes())?.to_owned())?;
			}
			if batch.records.positions.contains_key(&key) {
				return Err(RawDeleteStorageError::Coverage);
			}
			Ok(())
		})();
		result.map_err(|e: RawDeleteStorageError| {
			e.with_identity(RawDeleteRecordErrorFields {
				identity,
				candidate_rank: Some(count),
				step: RecordDeletionStep::Discovery,
			})
		})?;
		if count == cap {
			batch.more = true;
		} else {
			batch.records.push(key)?;
		}
		count += 1;
	}
	for row in rows {
		if clock.is_some() {
			return Err(RawDeleteStorageError::Coverage);
		}
		if row.0 == 4 {
			if !observation.is_timed() {
				return Err(RawDeleteStorageError::Coverage);
			}
			clock = Some(super::acquire::parse_clock(row)?);
			continue;
		}
		let expected_tag = match batch.layout {
			RawDeleteLayout::Native(_) => 2,
			RawDeleteLayout::Source(_) => 3,
			RawDeleteLayout::Relocating(_) => return Err(RawDeleteStorageError::Coverage),
		};
		if row.0 != expected_tag {
			return Err(RawDeleteStorageError::Coverage);
		}
		let index = batch.records.parse_status(batch.layout, row)?;
		if !statuses.insert(index) {
			return Err(batch.records.identify(
				RawDeleteStorageError::Coverage,
				index,
				RecordDeletionStep::Discovery,
			));
		}
	}
	if !matches!(batch.layout, RawDeleteLayout::Relocating(_))
		&& let Some(index) = (0..batch.records.candidates.len()).find(|i| !statuses.contains(i))
	{
		let step = if matches!(batch.layout, RawDeleteLayout::Native(_)) {
			RecordDeletionStep::Native
		} else {
			RecordDeletionStep::Source
		};
		return Err(batch
			.records
			.identify(RawDeleteStorageError::Coverage, index, step));
	}
	if matches!(batch.layout, RawDeleteLayout::Relocating(_)) && !statuses.is_empty() {
		return Err(RawDeleteStorageError::Coverage);
	}
	Ok(clock)
}
pub(super) fn append_native(
	sql: &mut AuditedSqlBuilder,
	table: &PgQualifiedTableName,
	bounds: KeyBounds,
) {
	sql.push_text("native_rows AS (SELECT namespace,key AS key_bytes,body,expires_at,updated_at,index_fields FROM ONLY ");
	sql.push_argument(table);
	sql.push_text(" WHERE namespace OPERATOR(pg_catalog.=) $1::pg_catalog.bytea AND body IS NOT NULL), native_selected_rows AS (SELECT * FROM native_rows");
	sql.push_text(BOUNDS_SQL);
	if let KeyBounds::SelectionAfter = bounds {
		sql.push_text(WINDOW_BOUND_SQL);
	}
	sql.push_text(")");
}
pub(super) fn append_source(
	sql: &mut AuditedSqlBuilder,
	table: &PgQualifiedTableName,
	bounds: KeyBounds,
) {
	sql.push_text("source_raw AS (SELECT key,value,expires_at,updated_at,FALSE AS inserted_reservation FROM (SELECT key,value,expires_at,updated_at,pg_catalog.convert_to(key,'UTF8') AS key_bytes FROM ONLY ");
	sql.push_argument(table);
	sql.push_text(") source_rows");
	sql.push_text(BOUNDS_SQL);
	match bounds {
		KeyBounds::Selection => {
			sql.push_text(SOURCE_SELECTION_TEXT_BOUNDS_SQL);
		}
		KeyBounds::SelectionAfter => {
			sql.push_text(WINDOW_BOUND_SQL);
			sql.push_text(SOURCE_TEXT_BOUNDS_SQL);
		}
	}
	append_source_snapshot_projection(sql);
}
fn append_candidates<Clock>(
	sql: &mut AuditedSqlBuilder,
	batch: &PgRawDeleteBatch<'_, '_, Clock>,
	observation: Observation,
) {
	if matches!(batch.layout, RawDeleteLayout::Native(_)) {
		let expired = batch.request.kind() == RecordDeleteKind::Expired;
		sql.push_text(", candidate_window AS MATERIALIZED (SELECT key_bytes,expires_at AS expiry FROM native_selected_rows");
		if expired {
			sql.push_text(" WHERE expires_at OPERATOR(pg_catalog.<=) pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.statement_timestamp()) OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8");
		}
		let order = if expired {
			"expiry USING OPERATOR(pg_catalog.<), key_bytes USING OPERATOR(pg_catalog.<)"
		} else {
			"key_bytes USING OPERATOR(pg_catalog.<)"
		};
		sql.push_text(" ORDER BY ");
		sql.push_text(order);
		sql.push_text(" LIMIT $4), candidates AS MATERIALIZED (SELECT key_bytes,(pg_catalog.row_number() OVER (ORDER BY ");
		sql.push_text(order);
		sql.push_text(")) OPERATOR(pg_catalog.-) 1 AS rank FROM candidate_window)");
		return;
	}
	sql.push_text(", logical_records AS (");
	match &batch.layout {
		RawDeleteLayout::Native(s) => append_logical_projection(sql, &RawReadLayout::Native(s)),
		RawDeleteLayout::Source(s) => append_logical_projection(sql, &RawReadLayout::Source(s)),
		RawDeleteLayout::Relocating(s) => {
			append_logical_projection(sql, &RawReadLayout::Relocating(s))
		}
	}
	sql.push_text("), classified AS (SELECT key_bytes,CASE WHEN native_namespace IS NOT NULL AND source_present THEN 1 WHEN source_present AND NOT expiry_valid THEN 2 END AS error_kind,CASE WHEN native_namespace IS NOT NULL THEN native_expiry ELSE source_expiry END AS expiry FROM logical_records), ");
	if batch.request.kind() == RecordDeleteKind::Expired {
		sql.push_text("discovery_clock AS MATERIALIZED (SELECT pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.clock_timestamp()) OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8 AS observed_at), eligible AS (SELECT classified.* FROM classified CROSS JOIN discovery_clock WHERE error_kind IS NOT NULL OR expiry OPERATOR(pg_catalog.<=) observed_at), ");
	} else {
		sql.push_text("eligible AS (SELECT * FROM classified), ");
	}
	// A timed batch orders invalid records (a v10 record with an unrepresentable expiry, or a key
	// with both a v10 and a native value) first. A selection deletion window orders every record
	// by key, so the greatest candidate key of one window bounds the next window. An invalid
	// record still fails the deletion when a window holds it.
	let order = match (batch.request.kind(), observation) {
		(RecordDeleteKind::Expired, _) => {
			"(error_kind IS NULL), CASE WHEN error_kind IS NOT NULL THEN key_bytes END USING OPERATOR(pg_catalog.<), expiry USING OPERATOR(pg_catalog.<), key_bytes USING OPERATOR(pg_catalog.<)"
		}
		(RecordDeleteKind::AllValued { .. }, Observation::Timed) => {
			"(error_kind IS NULL),key_bytes USING OPERATOR(pg_catalog.<)"
		}
		(RecordDeleteKind::AllValued { .. }, Observation::UntimedAllValued) => {
			"key_bytes USING OPERATOR(pg_catalog.<)"
		}
	};
	sql.push_text(
		"candidate_window AS MATERIALIZED (SELECT key_bytes,error_kind,expiry FROM eligible ORDER BY ",
	);
	sql.push_text(order);
	sql.push_text(" LIMIT $4), candidates AS MATERIALIZED (SELECT key_bytes,(pg_catalog.row_number() OVER (ORDER BY ");
	sql.push_text(order);
	sql.push_text(")) OPERATOR(pg_catalog.-) 1 AS rank FROM candidate_window)");
}
pub(super) fn append_lock_clause(sql: &mut AuditedSqlBuilder, contention: RecordDeleteContention) {
	sql.push_text(" FOR UPDATE OF target");
	if contention == RecordDeleteContention::SkipLocked {
		sql.push_text(" SKIP LOCKED");
	}
}
fn build_query<Clock>(
	batch: &PgRawDeleteBatch<'_, '_, Clock>,
	observation: Observation,
) -> AuditedSql {
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("WITH request_context AS (SELECT $1::pg_catalog.bytea AS namespace), ");
	match &batch.layout {
		RawDeleteLayout::Native(s) => append_native(&mut sql, s.table(), KeyBounds::SelectionAfter),
		RawDeleteLayout::Source(s) => append_source(&mut sql, s.table(), KeyBounds::SelectionAfter),
		RawDeleteLayout::Relocating(s) => {
			append_source(&mut sql, s.source_table(), KeyBounds::SelectionAfter);
			sql.push_text(", ");
			append_native(
				&mut sql,
				s.native_storage().table(),
				KeyBounds::SelectionAfter,
			);
		}
	}
	append_candidates(&mut sql, batch, observation);
	match &batch.layout {
		RawDeleteLayout::Native(storage) => {
			sql.push_text(", locked_native AS MATERIALIZED (SELECT target.namespace,target.key,target.body,target.expires_at,target.updated_at,target.index_fields FROM ONLY ");
			sql.push_argument(storage.table());
			sql.push_text(" target JOIN candidates c ON target.key OPERATOR(pg_catalog.=) c.key_bytes WHERE target.namespace OPERATOR(pg_catalog.=) $1::pg_catalog.bytea AND c.rank OPERATOR(pg_catalog.<) ($4::pg_catalog.int8 OPERATOR(pg_catalog.-) 1) ORDER BY target.key USING OPERATOR(pg_catalog.<)");
			append_lock_clause(&mut sql, batch.request.contention());
			sql.push_text(")");
		}
		RawDeleteLayout::Source(storage) => {
			sql.push_text(", locked_source AS MATERIALIZED (SELECT target.key,target.value,target.expires_at,target.updated_at FROM ONLY ");
			sql.push_argument(storage.table());
			sql.push_text(" target JOIN candidates c ON target.key OPERATOR(pg_catalog.=) pg_catalog.convert_from(c.key_bytes,'UTF8') WHERE c.rank OPERATOR(pg_catalog.<) ($4::pg_catalog.int8 OPERATOR(pg_catalog.-) 1) ORDER BY target.key COLLATE pg_catalog.\"C\" USING OPERATOR(pg_catalog.<)");
			append_lock_clause(&mut sql, batch.request.contention());
			sql.push_text("), held_source AS (WITH source_raw AS (SELECT key,value,expires_at,updated_at,FALSE AS inserted_reservation FROM locked_source");
			append_source_snapshot_projection(&mut sql);
			sql.push_text("SELECT * FROM source_snapshot)");
		}
		RawDeleteLayout::Relocating(_) => {}
	}
	sql.push_text(" SELECT 0::pg_catalog.int8 AS tag,key_bytes AS key,rank,NULL::pg_catalog.bool AS present,NULL::pg_catalog.bool AS held,NULL::pg_catalog.bytea AS namespace,");
	sql.push_text(super::acquire::NULL_HEADER_BODY_SQL);
	sql.push_text(" AS body,NULL::pg_catalog.int8 AS expiry,NULL::pg_catalog.int8 AS updated,NULL::pg_catalog.text AS indexes,NULL::pg_catalog.bool AS expiry_valid,NULL::pg_catalog.bool AS update_valid,NULL::pg_catalog.int8 AS clock FROM candidates");
	match &batch.layout {
		RawDeleteLayout::Native(_) => {
			sql.push_text(
				" UNION ALL SELECT 2::pg_catalog.int8,c.key_bytes,c.rank,TRUE,h.key IS NOT NULL,h.namespace,",
			);
			sql.push_text(super::acquire::NATIVE_HEADER_BODY_SQL);
			sql.push_text(",h.expires_at,h.updated_at,h.index_fields::pg_catalog.text,NULL::pg_catalog.bool,NULL::pg_catalog.bool,NULL::pg_catalog.int8 FROM candidates c LEFT JOIN locked_native h ON h.key OPERATOR(pg_catalog.=) c.key_bytes WHERE c.rank OPERATOR(pg_catalog.<) ($4::pg_catalog.int8 OPERATOR(pg_catalog.-) 1)");
		}
		RawDeleteLayout::Source(_) => {
			sql.push_text(" UNION ALL SELECT 3::pg_catalog.int8,c.key_bytes,c.rank,TRUE,h.key IS NOT NULL,NULL::pg_catalog.bytea,");
			sql.push_text(super::acquire::SOURCE_HEADER_BODY_SQL);
			sql.push_text(",h.expiry_micros,h.update_micros,NULL::pg_catalog.text,h.expiry_valid,h.update_valid,NULL::pg_catalog.int8 FROM candidates c LEFT JOIN held_source h ON pg_catalog.convert_to(h.key,'UTF8') OPERATOR(pg_catalog.=) c.key_bytes WHERE c.rank OPERATOR(pg_catalog.<) ($4::pg_catalog.int8 OPERATOR(pg_catalog.-) 1)");
		}
		RawDeleteLayout::Relocating(_) => {}
	}
	sql.push_text(" ORDER BY tag,rank");
	if observation.is_timed() {
		super::acquire::append_clock(&mut sql);
	}
	sql.finish()
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::db::{PgIdentifier, PgSchemaName};

	#[test]
	fn unpaged_source_selection_includes_text_index_bounds() {
		let table = PgQualifiedTableName::new(
			PgSchemaName::new(PgIdentifier::new("public").expect("valid schema")),
			PgIdentifier::new("kv").expect("valid table"),
		);
		let mut sql = AuditedSqlBuilder::new();
		append_source(&mut sql, &table, KeyBounds::Selection);
		let rendered = sql.finish();
		let text = rendered.as_str();
		assert!(text.contains("key OPERATOR(pg_catalog.~>=~) $4::pg_catalog.text"));
		assert!(text.contains("key OPERATOR(pg_catalog.~<~) $5::pg_catalog.text"));
		assert!(!text.contains("OPERATOR(pg_catalog.~>~)"));
	}
}
