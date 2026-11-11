//! Genuine neutral values preserve their initial statement time and exact physical history.
use super::super::delete::records::PhysicalStatus;
use super::super::delete::{acquire, records::HeaderRow};
use super::super::v10::source_point::{
	SourcePhysicalHeaderFields, append_source_snapshot_projection, decode_source_header,
};
use super::super::{
	DatabaseOperationKind, NativePhysicalHeaderFields, decode_physical_header,
	owned_portable_query_as,
};
use super::{RawSlotStorageError, SlotOwner};
use crate::db::{AuditedSql, AuditedSqlBuilder};
use crate::kv::raw::model::slot::{
	RawSlotSeedIntent, RawSlotSeedOverflowError, prepare_raw_slot_seed,
};
use crate::records::RecordKey;
use std::collections::BTreeSet;
const SEED_NATIVE_LABEL: &str = "db.state_storage.raw_slot.seed_native";
const OBSERVE_SEED_SUBSET_LABEL: &str = "db.state_storage.raw_slot.observe_seed_subset";
const SEED_SOURCE_LABEL: &str = "db.state_storage.raw_slot.seed_source";
type SourceSeedRow = (
	super::super::Bytea,
	bool,
	bool,
	Option<i64>,
	Option<i64>,
	bool,
	bool,
);
pub(super) async fn source(owner: &mut SlotOwner<'_, '_>) -> Result<(), RawSlotStorageError> {
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("WITH source_raw AS (");
	crate::kv::raw::v10::append_source_slot_seed(&mut sql, owner.source_table()?);
	sql.push_text(" RETURNING key,value,expires_at,updated_at,FALSE AS inserted_reservation");
	append_source_snapshot_projection(&mut sql);
	sql.push_text(" SELECT pg_catalog.convert_to(key,'UTF8'),value IS NOT NULL,COALESCE(pg_catalog.octet_length(value) OPERATOR(pg_catalog.=) 0,FALSE),expiry_micros,update_micros,expiry_valid,update_valid FROM source_snapshot ORDER BY key COLLATE pg_catalog.\"C\" USING OPERATOR(pg_catalog.<)");
	let sql = sql.finish();
	owner.tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		SEED_SOURCE_LABEL,
		Some(sql.as_str()),
	);
	let texts = owner.source_texts()?.to_vec();
	let rows = owned_portable_query_as::<SourceSeedRow>(sql)
		.bind_validated(texts)
		.fetch_all(owner.tx.inner.as_mut())
		.await
		.map_err(|e| e.with_operation_label(SEED_SOURCE_LABEL))?;
	for (
		key,
		body_present,
		body_empty,
		expires_at,
		updated_at,
		expires_at_valid,
		updated_at_valid,
	) in rows
	{
		let key = RecordKey::new(key.0);
		if owner.candidates.keys().binary_search(&key).is_err() || !owner.seeded.insert(key.clone())
		{
			return Err(owner.identify_error_by_key(RawSlotStorageError::Coverage, &key));
		}
		let parsed = (|| {
			let metadata = decode_source_header(SourcePhysicalHeaderFields {
				body_present,
				expires_at,
				updated_at,
				expires_at_valid,
				updated_at_valid,
			})?;
			let intent = prepare_raw_slot_seed(metadata.updated_at)?;
			if !body_empty
				|| metadata != intent.metadata()
				|| owner.seed.is_some_and(|seed| seed.metadata() != metadata)
			{
				return Err(RawSlotStorageError::Coverage);
			}
			Ok((metadata, intent))
		})()
		.map_err(|error| owner.identify_error_by_key(error, &key))?;
		owner.seed = Some(parsed.1);
		owner.records.push(key)?;
		let record = owner
			.records
			.candidates
			.last_mut()
			.ok_or(RawSlotStorageError::Shape)?;
		record.source = PhysicalStatus::Value(parsed.0);
	}
	Ok(())
}
pub(super) async fn native(owner: &mut SlotOwner<'_, '_>) -> Result<(), RawSlotStorageError> {
	let sql = native_query(owner.native_table()?);
	owner.tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		SEED_NATIVE_LABEL,
		Some(sql.as_str()),
	);
	let keys = owner
		.candidates
		.keys()
		.iter()
		.map(|k| k.as_bytes().to_vec())
		.collect::<Vec<_>>();
	let rows = owned_portable_query_as::<HeaderRow>(sql)
		.bind_validated(owner.namespace.as_bytes())
		.bind_validated(keys)
		.fetch_all(owner.tx.inner.as_mut())
		.await
		.map_err(|e| e.with_operation_label(SEED_NATIVE_LABEL))?;
	let mut subset = BTreeSet::new();
	let mut seeded = BTreeSet::new();
	let mut headers = Vec::new();
	let mut clock = None;
	let mut counts = [None, None];
	for row in rows {
		match row.0 {
			0 => {
				let key =
					RecordKey::new(row.1.as_ref().ok_or(RawSlotStorageError::Shape)?.0.clone());
				if owner.candidates.keys().binary_search(&key).is_err()
					|| !seeded.insert(key.clone())
				{
					return Err(owner.identify_error_by_key(RawSlotStorageError::Coverage, &key));
				}
				headers.push((key, row));
			}
			1 => {
				let mut row = row;
				let key = RecordKey::new(row.1.take().ok_or(RawSlotStorageError::Shape)?.0);
				if owner.candidates.keys().binary_search(&key).is_err()
					|| row.2.is_some()
					|| !super::super::delete::records::is_empty_frame(&row)
					|| !subset.insert(key.clone())
				{
					return Err(owner.identify_error_by_key(RawSlotStorageError::Coverage, &key));
				}
			}
			2 => {
				let key =
					RecordKey::new(row.1.as_ref().ok_or(RawSlotStorageError::Shape)?.0.clone());
				if owner.candidates.keys().binary_search(&key).is_err()
					|| row.2.is_some()
					|| row.3 != Some(true)
					|| row.4.is_some()
					|| row.5.as_ref().map(|n| n.0.as_slice()) != Some(owner.namespace.as_bytes())
					|| row.6 != Some(false)
					|| row.10.is_some()
					|| row.11.is_some()
					|| row.12.is_some()
				{
					return Err(owner.identify_error_by_key(RawSlotStorageError::Coverage, &key));
				}
				let identity = owner.identity(&key);
				let parsed = decode_physical_header(NativePhysicalHeaderFields {
					identity: &identity,
					body_present: false,
					expires_at: row.7,
					updated_at: row.8,
					index_fields: row.9.as_deref().ok_or(RawSlotStorageError::Shape)?,
				});
				return Err(owner.identify_error_by_key(
					match parsed {
						Err(e) => e.into(),
						Ok(_) => RawSlotStorageError::Coverage,
					},
					&key,
				));
			}
			5 | 6 => {
				let mut row = row;
				let slot = if row.0 == 5 { 0 } else { 1 };
				let count = usize::try_from(row.2.take().ok_or(RawSlotStorageError::Shape)?)
					.map_err(|_| RawSlotStorageError::Coverage)?;
				if row.1.is_some()
					|| !super::super::delete::records::is_empty_frame(&row)
					|| counts[slot].replace(count).is_some()
				{
					return Err(RawSlotStorageError::Coverage);
				}
			}
			4 => {
				if clock.is_some() {
					return Err(RawSlotStorageError::Coverage);
				}
				if row.12.is_none() {
					if row.1.is_some()
						|| row.2.is_some() || !super::super::delete::records::is_empty_frame(&row)
					{
						return Err(RawSlotStorageError::Coverage);
					}
					return Err(RawSlotSeedOverflowError.into());
				}
				clock = Some(acquire::parse_clock(row)?);
			}
			_ => return Err(RawSlotStorageError::Coverage),
		}
	}
	if !seeded.is_subset(&subset) || counts != [Some(subset.len()), Some(seeded.len())] {
		return Err(RawSlotStorageError::Coverage);
	}
	let intent = prepare_raw_slot_seed(clock.ok_or(RawSlotStorageError::Coverage)?)?;
	for (key, row) in headers {
		if row.2.is_some()
			|| row.3 != Some(true)
			|| row.4 != Some(true)
			|| row.5.as_ref().map(|n| n.0.as_slice()) != Some(owner.namespace.as_bytes())
			|| row.10.is_some()
			|| row.11.is_some()
			|| row.12.is_some()
		{
			return Err(owner.identify_error_by_key(RawSlotStorageError::Coverage, &key));
		}
		let identity = owner.identity(&key);
		let header = decode_physical_header(NativePhysicalHeaderFields {
			identity: &identity,
			body_present: row.6.ok_or(RawSlotStorageError::Shape)?,
			expires_at: row.7,
			updated_at: row.8,
			index_fields: row.9.as_deref().ok_or(RawSlotStorageError::Shape)?,
		})
		.map_err(|e| owner.identify_error_by_key(e.into(), &key))?
		.ok_or_else(|| owner.identify_error_by_key(RawSlotStorageError::Coverage, &key))?;
		if header.metadata != intent.metadata() || !header.index_fields.is_empty() {
			return Err(owner.identify_error_by_key(RawSlotStorageError::Coverage, &key));
		}
	}
	owner.seed = Some(intent);
	owner.seeded = seeded.clone();
	if subset.is_empty() {
		return Ok(());
	}
	observe_subset(
		owner,
		SeedSubset {
			keys: subset,
			written: seeded,
		},
		intent,
	)
	.await
}
fn native_query(table: &crate::db::PgQualifiedTableName) -> AuditedSql {
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("WITH requested AS MATERIALIZED (SELECT key FROM pg_catalog.unnest($2::pg_catalog.bytea[]) input(key) ORDER BY key USING OPERATOR(pg_catalog.<)), prior AS MATERIALIZED (SELECT r.key,t.key IS NOT NULL AS present,CASE WHEN t.key IS NOT NULL THEN t.body IS NOT NULL END AS body_present,t.expires_at,t.updated_at,t.index_fields FROM requested r LEFT JOIN ONLY ");
	sql.push_argument(table);
	sql.push_text(" t ON t.namespace OPERATOR(pg_catalog.=) $1::pg_catalog.bytea AND t.key OPERATOR(pg_catalog.=) r.key), invalid_null AS MATERIALIZED (SELECT * FROM prior WHERE present AND NOT body_present AND (expires_at IS NOT NULL OR updated_at IS NOT NULL OR index_fields OPERATOR(pg_catalog.<>) '{}'::pg_catalog.jsonb)), seed_keys AS MATERIALIZED (SELECT key FROM prior WHERE NOT present OR (NOT body_present AND expires_at IS NULL AND updated_at IS NULL AND index_fields OPERATOR(pg_catalog.=) '{}'::pg_catalog.jsonb) ORDER BY key USING OPERATOR(pg_catalog.<)), seed_clock AS MATERIALIZED (SELECT pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.statement_timestamp()) OPERATOR(pg_catalog.*) 1000000) AS micros), safe_clock AS MATERIALIZED (SELECT CASE WHEN micros OPERATOR(pg_catalog.>=) '-9223372036853775808'::pg_catalog.numeric AND micros OPERATOR(pg_catalog.<=) '9223372036854775807'::pg_catalog.numeric THEN micros::pg_catalog.int8 END AS seed_at FROM seed_clock), seeded AS (INSERT INTO ");
	sql.push_argument(table);
	sql.push_text(" AS target (namespace,key,body,expires_at,updated_at,index_fields) SELECT $1::pg_catalog.bytea,s.key,''::pg_catalog.bytea,c.seed_at OPERATOR(pg_catalog.-) 1000000::pg_catalog.int8,c.seed_at,'{}'::pg_catalog.jsonb FROM seed_keys s CROSS JOIN safe_clock c WHERE c.seed_at IS NOT NULL AND NOT EXISTS (SELECT 1 FROM invalid_null) ORDER BY s.key USING OPERATOR(pg_catalog.<) ON CONFLICT (namespace,key) DO UPDATE SET body=EXCLUDED.body,expires_at=EXCLUDED.expires_at,updated_at=EXCLUDED.updated_at,index_fields=EXCLUDED.index_fields WHERE target.body IS NULL AND target.expires_at IS NULL AND target.updated_at IS NULL AND target.index_fields OPERATOR(pg_catalog.=) '{}'::pg_catalog.jsonb RETURNING target.namespace,target.key,target.body IS NOT NULL AS body_present,target.expires_at,target.updated_at,target.index_fields) SELECT 0::pg_catalog.int8,key,NULL::pg_catalog.int8,TRUE,TRUE,namespace,body_present,expires_at,updated_at,index_fields::pg_catalog.text,NULL::pg_catalog.bool,NULL::pg_catalog.bool,NULL::pg_catalog.int8 FROM seeded UNION ALL SELECT 1::pg_catalog.int8,key,NULL::pg_catalog.int8,NULL::pg_catalog.bool,NULL::pg_catalog.bool,NULL::pg_catalog.bytea,NULL::pg_catalog.bool,NULL::pg_catalog.int8,NULL::pg_catalog.int8,NULL::pg_catalog.text,NULL::pg_catalog.bool,NULL::pg_catalog.bool,NULL::pg_catalog.int8 FROM seed_keys UNION ALL SELECT 2::pg_catalog.int8,key,NULL::pg_catalog.int8,TRUE,NULL::pg_catalog.bool,$1::pg_catalog.bytea,body_present,expires_at,updated_at,index_fields::pg_catalog.text,NULL::pg_catalog.bool,NULL::pg_catalog.bool,NULL::pg_catalog.int8 FROM invalid_null UNION ALL SELECT 4::pg_catalog.int8,NULL::pg_catalog.bytea,NULL::pg_catalog.int8,NULL::pg_catalog.bool,NULL::pg_catalog.bool,NULL::pg_catalog.bytea,NULL::pg_catalog.bool,NULL::pg_catalog.int8,NULL::pg_catalog.int8,NULL::pg_catalog.text,NULL::pg_catalog.bool,NULL::pg_catalog.bool,seed_at FROM safe_clock UNION ALL SELECT 5::pg_catalog.int8,NULL::pg_catalog.bytea,pg_catalog.count(*)::pg_catalog.int8,NULL::pg_catalog.bool,NULL::pg_catalog.bool,NULL::pg_catalog.bytea,NULL::pg_catalog.bool,NULL::pg_catalog.int8,NULL::pg_catalog.int8,NULL::pg_catalog.text,NULL::pg_catalog.bool,NULL::pg_catalog.bool,NULL::pg_catalog.int8 FROM seed_keys UNION ALL SELECT 6::pg_catalog.int8,NULL::pg_catalog.bytea,pg_catalog.count(*)::pg_catalog.int8,NULL::pg_catalog.bool,NULL::pg_catalog.bool,NULL::pg_catalog.bytea,NULL::pg_catalog.bool,NULL::pg_catalog.int8,NULL::pg_catalog.int8,NULL::pg_catalog.text,NULL::pg_catalog.bool,NULL::pg_catalog.bool,NULL::pg_catalog.int8 FROM seeded");
	sql.finish()
}
struct SeedSubset {
	keys: BTreeSet<RecordKey>,
	written: BTreeSet<RecordKey>,
}
async fn observe_subset(
	owner: &mut SlotOwner<'_, '_>,
	subset: SeedSubset,
	intent: RawSlotSeedIntent,
) -> Result<(), RawSlotStorageError> {
	let SeedSubset {
		keys: subset,
		written: seeded,
	} = subset;
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("SELECT 2::pg_catalog.int8,key,NULL::pg_catalog.int8,TRUE,TRUE,namespace,body IS NOT NULL,expires_at,updated_at,index_fields::pg_catalog.text,NULL::pg_catalog.bool,NULL::pg_catalog.bool,NULL::pg_catalog.int8 FROM ONLY ");
	sql.push_argument(owner.native_table()?);
	sql.push_text(" WHERE namespace OPERATOR(pg_catalog.=) $1::pg_catalog.bytea AND key OPERATOR(pg_catalog.=) ANY($2::pg_catalog.bytea[]) ORDER BY key USING OPERATOR(pg_catalog.<)");
	let sql = sql.finish();
	owner.tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		OBSERVE_SEED_SUBSET_LABEL,
		Some(sql.as_str()),
	);
	let keys = subset
		.iter()
		.map(|k| k.as_bytes().to_vec())
		.collect::<Vec<_>>();
	let rows = owned_portable_query_as::<HeaderRow>(sql)
		.bind_validated(owner.namespace.as_bytes())
		.bind_validated(keys)
		.fetch_all(owner.tx.inner.as_mut())
		.await
		.map_err(|e| e.with_operation_label(OBSERVE_SEED_SUBSET_LABEL))?;
	let mut seen = BTreeSet::new();
	for row in rows {
		let key = RecordKey::new(row.1.as_ref().ok_or(RawSlotStorageError::Shape)?.0.clone());
		if !subset.contains(&key)
			|| !seen.insert(key.clone())
			|| row.0 != 2
			|| row.2.is_some()
			|| row.3 != Some(true)
			|| row.4 != Some(true)
			|| row.5.as_ref().map(|n| n.0.as_slice()) != Some(owner.namespace.as_bytes())
			|| row.10.is_some()
			|| row.11.is_some()
			|| row.12.is_some()
		{
			return Err(owner.identify_error_by_key(RawSlotStorageError::Coverage, &key));
		}
		let identity = owner.identity(&key);
		let header = decode_physical_header(NativePhysicalHeaderFields {
			identity: &identity,
			body_present: row.6.ok_or(RawSlotStorageError::Shape)?,
			expires_at: row.7,
			updated_at: row.8,
			index_fields: row.9.as_deref().ok_or(RawSlotStorageError::Shape)?,
		})
		.map_err(|e| owner.identify_error_by_key(e.into(), &key))?
		.ok_or_else(|| owner.identify_error_by_key(RawSlotStorageError::Coverage, &key))?;
		if seeded.contains(&key)
			&& (header.metadata != intent.metadata() || !header.index_fields.is_empty())
		{
			return Err(owner.identify_error_by_key(RawSlotStorageError::Coverage, &key));
		}
		owner.records.push(key)?;
		let record = owner
			.records
			.candidates
			.last_mut()
			.ok_or(RawSlotStorageError::Shape)?;
		record.native = PhysicalStatus::Value(header.metadata);
	}
	if seen != subset {
		return Err(RawSlotStorageError::Coverage);
	}
	Ok(())
}
