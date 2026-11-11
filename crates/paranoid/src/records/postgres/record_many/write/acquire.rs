//! Ordered reservation/conflict locks precede a fresh held-row command and one final clock.

use super::super::super::ReservationOwnership;
use super::super::super::v10::source_point::{
	SourcePhysicalHeaderFields, append_source_snapshot_projection, decode_source_header,
};
use super::super::super::{
	Bytea, DatabaseOperationKind, NativePhysicalHeaderFields, decode_physical_header,
	owned_portable_query_as,
};
use super::{HeldManyRow, RecordManyHeldSet, RecordManyWriteError, RecordManyWriteLayout};
use crate::db::{AuditedSql, AuditedSqlBuilder, PgQualifiedTableName};
use crate::records::{RecordIdentity, RecordKey, StateTimestampMicros};
use std::collections::BTreeSet;

const ACQUIRE_NATIVE_LABEL: &str = "db.state_storage.record_many.acquire_native";
const ACQUIRE_SOURCE_LABEL: &str = "db.state_storage.record_many.acquire_source";
// tag, key, namespace, body length, expiry, update, indexes, expiry-valid, update-valid, clock
// Every SQL command in one request emits this same typed shape.
type Row = (
	i64,
	Option<Bytea>,
	Option<Bytea>,
	Option<i64>,
	Option<i64>,
	Option<i64>,
	Option<String>,
	Option<bool>,
	Option<bool>,
	Option<i64>,
);

pub(super) async fn acquire(
	held: &mut RecordManyHeldSet<'_, '_>,
) -> Result<StateTimestampMicros, RecordManyWriteError> {
	if let Some(table) = held.layout.native_table() {
		let final_clock = matches!(held.layout, RecordManyWriteLayout::Native(_));
		let sql = native_query(table, final_clock);
		let rows = fetch(held, sql, Kind::Native).await?;
		let parsed = parse(held, rows, Kind::Native, final_clock)?;
		// Native acquisition returns every key: a value, a found reservation, or an inserted
		// reservation.
		held.rows = (0..held.keys.len())
			.map(|index| {
				if parsed.inserted.contains(&index) {
					HeldManyRow::Reservation(ReservationOwnership::Inserted)
				} else if parsed.values.contains(&index) {
					HeldManyRow::Value
				} else {
					HeldManyRow::Reservation(ReservationOwnership::Preexisting)
				}
			})
			.collect();
		if let Some(clock) = parsed.clock {
			return Ok(clock);
		}
	}
	let table = held
		.layout
		.source_table()
		.ok_or(RecordManyWriteError::Shape)?;
	let reserve = matches!(held.layout, RecordManyWriteLayout::Source(_));
	let sql = source_query(table, reserve);
	let rows = fetch(held, sql, Kind::Source { reserve }).await?;
	let parsed = parse(held, rows, Kind::Source { reserve }, true)?;
	if reserve {
		// v10 acquisition returns every key: a value, or a reservation that it inserted.
		held.rows = (0..held.keys.len())
			.map(|index| {
				if parsed.inserted.contains(&index) {
					HeldManyRow::Reservation(ReservationOwnership::Inserted)
				} else {
					HeldManyRow::Value
				}
			})
			.collect();
	} else {
		// During relocation, a v10 value must have a native reservation row. A native value
		// beside it is a duplicate value.
		for index in &parsed.values {
			match held.rows[*index] {
				HeldManyRow::Reservation(ownership) => {
					held.rows[*index] = HeldManyRow::SourceValue(ownership);
				}
				HeldManyRow::Value | HeldManyRow::SourceValue(_) => {
					return Err(
						held.identify(RecordManyWriteError::DuplicatePhysicalValues, *index)
					);
				}
			}
		}
	}
	parsed.clock.ok_or(RecordManyWriteError::Coverage)
}
async fn fetch(
	held: &mut RecordManyHeldSet<'_, '_>,
	sql: AuditedSql,
	kind: Kind,
) -> Result<Vec<Row>, RecordManyWriteError> {
	let operation = match kind {
		Kind::Native => ACQUIRE_NATIVE_LABEL,
		Kind::Source { .. } => ACQUIRE_SOURCE_LABEL,
	};
	held.tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		operation,
		Some(sql.as_str()),
	);
	let query = owned_portable_query_as::<Row>(sql);
	let rows = match kind {
		Kind::Native => {
			let keys = held
				.keys
				.iter()
				.map(|key| key.as_bytes().to_vec())
				.collect::<Vec<_>>();
			query
				.bind_validated(held.namespace.as_bytes())
				.bind_validated(keys)
				.fetch_all(held.tx.inner.as_mut())
				.await
		}
		Kind::Source { .. } => {
			query
				.bind_validated(
					held.source_texts
						.as_ref()
						.ok_or(RecordManyWriteError::Shape)?
						.as_slice(),
				)
				.fetch_all(held.tx.inner.as_mut())
				.await
		}
	}
	.map_err(|error| error.with_operation_label(operation))?;

	Ok(rows)
}
fn append_clock(sql: &mut AuditedSqlBuilder) {
	sql.push_text("; SELECT 3::pg_catalog.int8, NULL::pg_catalog.bytea, NULL::pg_catalog.bytea, NULL::pg_catalog.int8, NULL::pg_catalog.int8, NULL::pg_catalog.int8, NULL::pg_catalog.text, NULL::pg_catalog.bool, NULL::pg_catalog.bool, pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.clock_timestamp()) OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8");
}
fn native_query(table: &PgQualifiedTableName, final_clock: bool) -> AuditedSql {
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("WITH ordered_keys AS MATERIALIZED (SELECT key FROM pg_catalog.unnest($2::pg_catalog.bytea[]) input(key) ORDER BY key USING OPERATOR(pg_catalog.<)) INSERT INTO ");
	sql.push_argument(table);
	sql.push_text(" AS target (namespace,key,body,expires_at,updated_at,index_fields) SELECT $1::pg_catalog.bytea,key,NULL,NULL,NULL,'{}'::pg_catalog.jsonb FROM ordered_keys ORDER BY key USING OPERATOR(pg_catalog.<) ON CONFLICT (namespace,key) DO UPDATE SET key = target.key WHERE FALSE RETURNING 0::pg_catalog.int8,key,namespace,pg_catalog.octet_length(body)::pg_catalog.int8,expires_at,updated_at,index_fields::pg_catalog.text,NULL::pg_catalog.bool,NULL::pg_catalog.bool,NULL::pg_catalog.int8; SELECT 1::pg_catalog.int8,key,namespace,pg_catalog.octet_length(body)::pg_catalog.int8,expires_at,updated_at,index_fields::pg_catalog.text,NULL::pg_catalog.bool,NULL::pg_catalog.bool,NULL::pg_catalog.int8 FROM ONLY ");
	sql.push_argument(table);
	sql.push_text(" WHERE namespace OPERATOR(pg_catalog.=) $1::pg_catalog.bytea AND key OPERATOR(pg_catalog.=) ANY($2::pg_catalog.bytea[]) ORDER BY key USING OPERATOR(pg_catalog.<)");
	if final_clock {
		append_clock(&mut sql);
	}
	sql.finish()
}
fn source_query(table: &PgQualifiedTableName, reserve: bool) -> AuditedSql {
	let mut sql = AuditedSqlBuilder::new();
	if reserve {
		sql.push_text("WITH ordered_keys AS MATERIALIZED (SELECT key FROM pg_catalog.unnest($1::pg_catalog.text[]) input(key) ORDER BY key COLLATE pg_catalog.\"C\" USING OPERATOR(pg_catalog.<)) INSERT INTO ");
		sql.push_argument(table);
		sql.push_text(" AS target (key,value,expires_at,updated_at) SELECT key,''::pg_catalog.bytea,pg_catalog.statement_timestamp(),pg_catalog.statement_timestamp() FROM ordered_keys ORDER BY key COLLATE pg_catalog.\"C\" USING OPERATOR(pg_catalog.<) ON CONFLICT (key) DO UPDATE SET key = target.key WHERE FALSE RETURNING 0::pg_catalog.int8,pg_catalog.convert_to(key,'UTF8'),NULL::pg_catalog.bytea,NULL::pg_catalog.int8,NULL::pg_catalog.int8,NULL::pg_catalog.int8,NULL::pg_catalog.text,NULL::pg_catalog.bool,NULL::pg_catalog.bool,NULL::pg_catalog.int8; ");
	}
	sql.push_text("WITH source_raw AS (SELECT key,value,expires_at,updated_at,FALSE AS inserted_reservation FROM ONLY ");
	sql.push_argument(table);
	sql.push_text(" WHERE key OPERATOR(pg_catalog.=) ANY($1::pg_catalog.text[]) ORDER BY key COLLATE pg_catalog.\"C\" USING OPERATOR(pg_catalog.<)");
	if !reserve {
		sql.push_text(" FOR UPDATE");
	}
	append_source_snapshot_projection(&mut sql);
	sql.push_text("SELECT 2::pg_catalog.int8,pg_catalog.convert_to(key,'UTF8'),NULL::pg_catalog.bytea,pg_catalog.octet_length(value)::pg_catalog.int8,expiry_micros,update_micros,NULL::pg_catalog.text,expiry_valid,update_valid,NULL::pg_catalog.int8 FROM source_snapshot ORDER BY key COLLATE pg_catalog.\"C\" USING OPERATOR(pg_catalog.<)");
	append_clock(&mut sql);
	sql.finish()
}
enum Kind {
	Native,
	Source { reserve: bool },
}
struct Parsed {
	inserted: BTreeSet<usize>,
	values: BTreeSet<usize>,
	clock: Option<StateTimestampMicros>,
}
struct ParsedRecords {
	inserted: BTreeSet<usize>,
	seen: BTreeSet<usize>,
	values: BTreeSet<usize>,
}
fn parse(
	held: &RecordManyHeldSet<'_, '_>,
	rows: Vec<Row>,
	kind: Kind,
	final_clock: bool,
) -> Result<Parsed, RecordManyWriteError> {
	let mut records = ParsedRecords {
		inserted: BTreeSet::new(),
		seen: BTreeSet::new(),
		values: BTreeSet::new(),
	};
	let mut rows = rows.into_iter().peekable();
	while let Some(row) = rows.next_if(|row| row.0 == 0) {
		parse_record(held, row, &kind, &mut records)?;
	}
	let mut clock = None;
	for row in rows {
		if clock.is_some() || row.0 == 0 {
			return Err(RecordManyWriteError::Coverage);
		}
		if row.0 == 3 {
			let (
				_,
				key,
				namespace,
				body_length,
				expiry,
				updated,
				indexes,
				expires_at_valid,
				updated_at_valid,
				time,
			) = row;
			if !final_clock
				|| key.is_some()
				|| namespace.is_some()
				|| body_length.is_some()
				|| expiry.is_some()
				|| updated.is_some()
				|| indexes.is_some()
				|| expires_at_valid.is_some()
				|| updated_at_valid.is_some()
			{
				return Err(RecordManyWriteError::Coverage);
			}
			clock = Some(StateTimestampMicros::new(
				time.ok_or(RecordManyWriteError::Shape)?,
			));
		} else {
			parse_record(held, row, &kind, &mut records)?;
		}
	}
	let ParsedRecords {
		inserted,
		seen,
		mut values,
	} = records;
	let complete = matches!(kind, Kind::Native | Kind::Source { reserve: true });
	if complete && let Some(index) = (0..held.keys.len()).find(|index| !seen.contains(index)) {
		return Err(held.identify(RecordManyWriteError::Coverage, index));
	}
	if !inserted.is_subset(&seen) || clock.is_some() != final_clock {
		return Err(RecordManyWriteError::Coverage);
	}
	for index in &inserted {
		values.remove(index);
	}
	Ok(Parsed {
		inserted,
		values,
		clock,
	})
}
fn parse_record(
	held: &RecordManyHeldSet<'_, '_>,
	row: Row,
	kind: &Kind,
	records: &mut ParsedRecords,
) -> Result<(), RecordManyWriteError> {
	let ParsedRecords {
		inserted,
		seen,
		values,
	} = records;
	let (
		tag,
		key,
		namespace,
		body_length,
		expiry,
		updated,
		indexes,
		expires_at_valid,
		updated_at_valid,
		time,
	) = row;
	let key = RecordKey::new(key.ok_or(RecordManyWriteError::Shape)?.0);
	let index = *held
		.positions
		.get(&key)
		.ok_or(RecordManyWriteError::Coverage)?;
	let result = (|| {
		if time.is_some() {
			return Err(RecordManyWriteError::Shape);
		}
		let identity = RecordIdentity::new(held.namespace.clone(), key);
		match (kind, tag) {
			(Kind::Native, 0 | 1) => {
				if namespace.as_ref().map(|v| v.0.as_slice()) != Some(held.namespace.as_bytes())
					|| expires_at_valid.is_some()
					|| updated_at_valid.is_some()
				{
					return Err(RecordManyWriteError::Shape);
				}
				let record = decode_physical_header(NativePhysicalHeaderFields {
					identity: &identity,
					body_present: body_length.is_some(),
					expires_at: expiry,
					updated_at: updated,
					index_fields: &indexes.ok_or(RecordManyWriteError::Shape)?,
				})?;
				if tag == 0 {
					if record.is_some() || !inserted.insert(index) {
						return Err(RecordManyWriteError::Coverage);
					}
				} else {
					if !seen.insert(index) {
						return Err(RecordManyWriteError::Coverage);
					}
					if record.is_some() {
						if inserted.contains(&index) {
							return Err(RecordManyWriteError::Shape);
						}
						values.insert(index);
					}
				}
			}
			(Kind::Source { reserve: true }, 0) => {
				if namespace.is_some()
					|| body_length.is_some()
					|| expiry.is_some()
					|| updated.is_some()
					|| indexes.is_some()
					|| expires_at_valid.is_some()
					|| updated_at_valid.is_some()
					|| !inserted.insert(index)
				{
					return Err(RecordManyWriteError::Shape);
				}
			}
			(Kind::Source { .. }, 2) => {
				if namespace.is_some() || indexes.is_some() || !seen.insert(index) {
					return Err(RecordManyWriteError::Shape);
				}
				if inserted.contains(&index)
					&& (body_length != Some(0) || expiry.is_none() || expiry != updated)
				{
					return Err(RecordManyWriteError::Shape);
				}
				decode_source_header(SourcePhysicalHeaderFields {
					body_present: body_length.is_some(),
					expires_at: expiry,
					updated_at: updated,
					expires_at_valid: expires_at_valid.ok_or(RecordManyWriteError::Shape)?,
					updated_at_valid: updated_at_valid.ok_or(RecordManyWriteError::Shape)?,
				})?;
				values.insert(index);
			}
			_ => return Err(RecordManyWriteError::Coverage),
		}
		Ok(())
	})();
	result.map_err(|error| held.identify(error, index))?;
	Ok(())
}
