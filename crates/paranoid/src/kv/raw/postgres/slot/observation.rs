//! Advisory metadata never grants locks; protected source absence requires actual native holding.
use super::super::delete::{acquire, records::HeaderRow};
use super::super::v10::source_point::{
	SourcePhysicalHeaderFields, append_source_snapshot_projection, decode_source_header,
};
use super::super::{
	Bytea, DatabaseOperationKind, NativePhysicalHeaderFields, decode_physical_header,
	owned_portable_query_as,
};
use super::{RawSlotStorageError, SlotOwner};
use crate::db::{AuditedSql, AuditedSqlBuilder};
use crate::records::{RecordKey, RecordMetadata, StateTimestampMicros};
use std::collections::{BTreeMap, BTreeSet};
const ADVISORY_LABEL: &str = "db.state_storage.raw_slot.advisory";
const PROBE_SOURCE_LABEL: &str = "db.state_storage.raw_slot.probe_source";
#[derive(Default)]
pub(super) struct Pair {
	pub(super) native: Option<RecordMetadata>,
	pub(super) source: Option<RecordMetadata>,
}
pub(super) struct Advisory {
	pub(super) pairs: Vec<Pair>,
	pub(super) observed_at: StateTimestampMicros,
}
pub(super) async fn observe(
	owner: &mut SlotOwner<'_, '_>,
) -> Result<Advisory, RawSlotStorageError> {
	let native = owner.native_table()?;
	let source = owner.source_table()?;
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("WITH requested AS (SELECT key,ordinality OPERATOR(pg_catalog.-) 1 AS rank FROM pg_catalog.unnest($2::pg_catalog.bytea[]) WITH ORDINALITY input(key,ordinality)) SELECT 2::pg_catalog.int8,r.key,r.rank,t.key IS NOT NULL,NULL::pg_catalog.bool,t.namespace,CASE WHEN t.key IS NOT NULL THEN t.body IS NOT NULL END,t.expires_at,t.updated_at,t.index_fields::pg_catalog.text,NULL::pg_catalog.bool,NULL::pg_catalog.bool,NULL::pg_catalog.int8 FROM requested r LEFT JOIN ONLY ");
	sql.push_argument(native);
	sql.push_text(" t ON t.namespace OPERATOR(pg_catalog.=) $1::pg_catalog.bytea AND t.key OPERATOR(pg_catalog.=) r.key ORDER BY r.rank; WITH source_raw AS (SELECT key,value,expires_at,updated_at,FALSE AS inserted_reservation FROM ONLY ");
	sql.push_argument(source);
	sql.push_text(" WHERE key OPERATOR(pg_catalog.=) ANY($3::pg_catalog.text[])");
	append_source_snapshot_projection(&mut sql);
	sql.push_text(" SELECT 3::pg_catalog.int8,pg_catalog.convert_to(r.key,'UTF8'),r.rank,h.key IS NOT NULL,NULL::pg_catalog.bool,NULL::pg_catalog.bytea,CASE WHEN h.key IS NOT NULL THEN h.value IS NOT NULL END,h.expiry_micros,h.update_micros,NULL::pg_catalog.text,h.expiry_valid,h.update_valid,NULL::pg_catalog.int8 FROM (SELECT key,ordinality OPERATOR(pg_catalog.-) 1 AS rank FROM pg_catalog.unnest($3::pg_catalog.text[]) WITH ORDINALITY input(key,ordinality)) r LEFT JOIN source_snapshot h ON h.key OPERATOR(pg_catalog.=) r.key ORDER BY r.rank");
	acquire::append_clock(&mut sql);
	let sql = sql.finish();
	owner.tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		ADVISORY_LABEL,
		Some(sql.as_str()),
	);
	let keys = owner
		.candidates
		.keys()
		.iter()
		.map(|k| k.as_bytes().to_vec())
		.collect::<Vec<_>>();
	let texts = owner.source_texts()?.to_vec();
	let rows = owned_portable_query_as::<HeaderRow>(sql)
		.bind_validated(owner.namespace.as_bytes())
		.bind_validated(keys)
		.bind_validated(texts)
		.fetch_all(owner.tx.inner.as_mut())
		.await
		.map_err(|e| e.with_operation_label(ADVISORY_LABEL))?;
	parse(owner, rows)
}
fn parse(owner: &SlotOwner<'_, '_>, rows: Vec<HeaderRow>) -> Result<Advisory, RawSlotStorageError> {
	let mut pairs = (0..owner.candidates.keys().len())
		.map(|_| Pair::default())
		.collect::<Vec<_>>();
	let mut seen = [BTreeSet::new(), BTreeSet::new()];
	let mut clock = None;
	for row in rows {
		if clock.is_some() {
			return Err(RawSlotStorageError::Coverage);
		}
		if row.0 == 4 {
			clock = Some(acquire::parse_clock(row)?);
			continue;
		}
		let (
			tag,
			key,
			rank,
			present,
			held,
			namespace,
			body,
			expiry,
			updated,
			indexes,
			expires_at_valid,
			updated_at_valid,
			time,
		) = row;
		let key = RecordKey::new(key.ok_or(RawSlotStorageError::Shape)?.0);
		let index = owner
			.candidates
			.keys()
			.binary_search(&key)
			.map_err(|_| owner.identify_error_by_key(RawSlotStorageError::Coverage, &key))?;
		let result = (|| {
			let role = match tag {
				2 => 0,
				3 => 1,
				_ => return Err(RawSlotStorageError::Coverage),
			};
			if rank != Some(i64::try_from(index).map_err(|_| RawSlotStorageError::Shape)?)
				|| held.is_some()
				|| time.is_some()
				|| !seen[role].insert(index)
			{
				return Err(RawSlotStorageError::Coverage);
			}
			let present = present.ok_or(RawSlotStorageError::Shape)?;
			if !present {
				if namespace.is_some()
					|| body.is_some()
					|| expiry.is_some()
					|| updated.is_some()
					|| indexes.is_some()
					|| expires_at_valid.is_some()
					|| updated_at_valid.is_some()
				{
					return Err(RawSlotStorageError::Shape);
				}
				return Ok(());
			}
			if role == 0 {
				if namespace.as_ref().map(|n| n.0.as_slice()) != Some(owner.namespace.as_bytes())
					|| expires_at_valid.is_some()
					|| updated_at_valid.is_some()
				{
					return Err(RawSlotStorageError::Shape);
				}
				pairs[index].native = decode_physical_header(NativePhysicalHeaderFields {
					identity: &owner.identity(&key),
					body_present: body.ok_or(RawSlotStorageError::Shape)?,
					expires_at: expiry,
					updated_at: updated,
					index_fields: &indexes.ok_or(RawSlotStorageError::Shape)?,
				})?
				.map(|h| h.metadata);
			} else {
				if namespace.is_some() || indexes.is_some() {
					return Err(RawSlotStorageError::Shape);
				}
				pairs[index].source = Some(decode_source_header(SourcePhysicalHeaderFields {
					body_present: body.ok_or(RawSlotStorageError::Shape)?,
					expires_at: expiry,
					updated_at: updated,
					expires_at_valid: expires_at_valid.ok_or(RawSlotStorageError::Shape)?,
					updated_at_valid: updated_at_valid.ok_or(RawSlotStorageError::Shape)?,
				})?);
			}
			Ok(())
		})();
		result.map_err(|e| owner.identify_error_by_key(e, &key))?;
	}
	for set in seen {
		if set.len() != pairs.len() {
			return Err(RawSlotStorageError::Coverage);
		}
	}
	for (index, pair) in pairs.iter().enumerate() {
		if pair.native.is_some() && pair.source.is_some() {
			return Err(owner.identify_error_by_key(
				RawSlotStorageError::DuplicatePhysicalValues,
				&owner.candidates.keys()[index],
			));
		}
	}
	Ok(Advisory {
		pairs,
		observed_at: clock.ok_or(RawSlotStorageError::Coverage)?,
	})
}
type ProbeRow = (Bytea, bool, Option<i64>, Option<i64>, bool, bool);
pub(super) async fn probe_source(
	owner: &mut SlotOwner<'_, '_>,
	keys: &[RecordKey],
) -> Result<BTreeMap<RecordKey, RecordMetadata>, RawSlotStorageError> {
	if keys.is_empty() {
		return Ok(BTreeMap::new());
	}
	// The private caller must hold each actual native identity before this absence probe.
	for key in keys {
		let index = *owner
			.records
			.positions
			.get(key)
			.ok_or(RawSlotStorageError::Shape)?;
		if !owner.records.candidates[index].native.is_held() {
			return Err(owner.identify_error_by_key(RawSlotStorageError::Shape, key));
		}
	}
	let texts = keys
		.iter()
		.map(|key| owner.source_text(key).map(str::to_owned))
		.collect::<Result<Vec<_>, _>>()?;
	let sql = probe_query(owner.source_table()?);
	owner.tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		PROBE_SOURCE_LABEL,
		Some(sql.as_str()),
	);
	let rows = owned_portable_query_as::<ProbeRow>(sql)
		.bind_validated(texts)
		.fetch_all(owner.tx.inner.as_mut())
		.await
		.map_err(|e| e.with_operation_label(PROBE_SOURCE_LABEL))?;
	let expected = keys.iter().collect::<BTreeSet<_>>();
	let mut output = BTreeMap::new();
	for (key, present, expiry, updated, expires_at_valid, updated_at_valid) in rows {
		let key = RecordKey::new(key.0);
		if !expected.contains(&key) || output.contains_key(&key) {
			return Err(owner.identify_error_by_key(RawSlotStorageError::Coverage, &key));
		}
		let metadata = decode_source_header(SourcePhysicalHeaderFields {
			body_present: present,
			expires_at: expiry,
			updated_at: updated,
			expires_at_valid,
			updated_at_valid,
		})
		.map_err(|e| owner.identify_error_by_key(e.into(), &key))?;
		output.insert(key, metadata);
	}
	Ok(output)
}
fn probe_query(table: &crate::db::PgQualifiedTableName) -> AuditedSql {
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("WITH source_raw AS (SELECT key,value,expires_at,updated_at,FALSE AS inserted_reservation FROM ONLY ");
	sql.push_argument(table);
	sql.push_text(" WHERE key OPERATOR(pg_catalog.=) ANY($1::pg_catalog.text[])");
	append_source_snapshot_projection(&mut sql);
	sql.push_text(" SELECT pg_catalog.convert_to(key,'UTF8'),value IS NOT NULL,expiry_micros,update_micros,expiry_valid,update_valid FROM source_snapshot ORDER BY key COLLATE pg_catalog.\"C\" USING OPERATOR(pg_catalog.<)");
	sql.finish()
}
