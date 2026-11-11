//! One actual candidate lock precedes its final clock; held intended seeds need no priority reread.
use super::super::delete::records::PhysicalStatus;
use super::super::delete::{acquire, records::HeaderRow};
use super::super::v10::source_point::{
	SourcePhysicalHeaderFields, append_source_snapshot_projection, decode_source_header,
};
use super::super::{
	DatabaseOperationKind, NativePhysicalHeaderFields, PgPointStorage, decode_physical_header,
	owned_portable_query_as,
};
use super::{RawSlotStorageError, SlotOwner};
use crate::db::{AuditedSql, AuditedSqlBuilder};
use crate::records::{RecordKey, RecordMetadata, StateTimestampMicros};
use std::collections::BTreeSet;
const LOCK_NATIVE_CANDIDATE_LABEL: &str = "db.state_storage.raw_slot.lock_native_candidate";
const LOCK_SOURCE_CANDIDATE_LABEL: &str = "db.state_storage.raw_slot.lock_source_candidate";
pub(super) struct Selected {
	pub(super) index: usize,
	pub(super) observed_at: StateTimestampMicros,
}
pub(super) enum Opportunity {
	Selected(Selected),
	Unavailable(StateTimestampMicros),
}
pub(super) async fn select(
	owner: &mut SlotOwner<'_, '_>,
) -> Result<Opportunity, RawSlotStorageError> {
	let mut remaining = owner.candidates.keys().to_vec();
	loop {
		let (current, observed_at) = lock_one(owner, &remaining).await?;
		let found = current.is_some();
		if let Some((key, metadata)) = current {
			let index = install(owner, key.clone(), metadata)?;
			if !metadata.is_live_at(observed_at) {
				return Ok(Opportunity::Selected(Selected { index, observed_at }));
			}
			remaining.retain(|k| k != &key);
		}
		if let Some(index) = eligible_held_seed(owner, observed_at) {
			return Ok(Opportunity::Selected(Selected { index, observed_at }));
		}
		if !found || remaining.is_empty() {
			return Ok(Opportunity::Unavailable(observed_at));
		}
	}
}
pub(super) fn eligible_held_seed(
	owner: &SlotOwner<'_, '_>,
	time: StateTimestampMicros,
) -> Option<usize> {
	owner
		.records
		.candidates
		.iter()
		.enumerate()
		.filter(|(_, c)| owner.seeded.contains(&c.key))
		.find_map(|(i, c)| {
			let metadata = if c.native.is_held() {
				c.native.value()
			} else if c.source.is_held() {
				c.source.value()
			} else {
				None
			};
			metadata.filter(|m| !m.is_live_at(time)).map(|_| i)
		})
}
fn install(
	owner: &mut SlotOwner<'_, '_>,
	key: RecordKey,
	metadata: RecordMetadata,
) -> Result<usize, RawSlotStorageError> {
	let index = match owner.records.positions.get(&key) {
		Some(index) => *index,
		None => {
			owner.records.push(key)?;
			owner.records.candidates.len() - 1
		}
	};
	let source = matches!(owner.layout, super::RawDeleteLayout::Source(_));
	let candidate = &mut owner.records.candidates[index];
	let physical = if source {
		&mut candidate.source
	} else {
		&mut candidate.native
	};
	*physical = PhysicalStatus::Value(metadata);
	Ok(index)
}
pub(super) async fn observe_time(
	owner: &mut SlotOwner<'_, '_>,
) -> Result<StateTimestampMicros, RawSlotStorageError> {
	PgPointStorage::observe_time(owner.tx)
		.await
		.map_err(RawSlotStorageError::from)
}
async fn lock_one(
	owner: &mut SlotOwner<'_, '_>,
	remaining: &[RecordKey],
) -> Result<(Option<(RecordKey, RecordMetadata)>, StateTimestampMicros), RawSlotStorageError> {
	let source = matches!(owner.layout, super::RawDeleteLayout::Source(_));
	let operation = if source {
		LOCK_SOURCE_CANDIDATE_LABEL
	} else {
		LOCK_NATIVE_CANDIDATE_LABEL
	};
	let sql = if source {
		source_query(owner.source_table()?)
	} else {
		native_query(owner.native_table()?)
	};
	owner.tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		operation,
		Some(sql.as_str()),
	);
	let rows = if source {
		let texts = remaining
			.iter()
			.map(|k| owner.source_text(k).map(str::to_owned))
			.collect::<Result<Vec<_>, _>>()?;
		owned_portable_query_as::<HeaderRow>(sql)
			.bind_validated(texts)
			.fetch_all(owner.tx.inner.as_mut())
			.await
	} else {
		let keys = remaining
			.iter()
			.map(|k| k.as_bytes().to_vec())
			.collect::<Vec<_>>();
		owned_portable_query_as::<HeaderRow>(sql)
			.bind_validated(owner.namespace.as_bytes())
			.bind_validated(keys)
			.fetch_all(owner.tx.inner.as_mut())
			.await
	}
	.map_err(|e| e.with_operation_label(operation))?;
	let expected = remaining.iter().collect::<BTreeSet<_>>();
	let mut current = None;
	let mut clock = None;
	for row in rows {
		if clock.is_some() {
			return Err(RawSlotStorageError::Coverage);
		}
		if row.0 == 4 {
			clock = Some(acquire::parse_clock(row)?);
			continue;
		}
		let key = RecordKey::new(row.1.as_ref().ok_or(RawSlotStorageError::Shape)?.0.clone());
		if !expected.contains(&key)
			|| current.is_some()
			|| row.0 != if source { 3 } else { 2 }
			|| row.2.is_some()
			|| row.3 != Some(true)
			|| row.4 != Some(true)
			|| row.12.is_some()
		{
			return Err(owner.identify_error_by_key(RawSlotStorageError::Coverage, &key));
		}
		let parsed = (|| {
			if source {
				if row.5.is_some() || row.9.is_some() {
					return Err(RawSlotStorageError::Shape);
				}
				Ok(decode_source_header(SourcePhysicalHeaderFields {
					body_present: row.6.ok_or(RawSlotStorageError::Shape)?,
					expires_at: row.7,
					updated_at: row.8,
					expires_at_valid: row.10.ok_or(RawSlotStorageError::Shape)?,
					updated_at_valid: row.11.ok_or(RawSlotStorageError::Shape)?,
				})?)
			} else {
				if row.5.as_ref().map(|n| n.0.as_slice()) != Some(owner.namespace.as_bytes())
					|| row.10.is_some()
					|| row.11.is_some()
				{
					return Err(RawSlotStorageError::Shape);
				}
				let identity = owner.identity(&key);
				let h = decode_physical_header(NativePhysicalHeaderFields {
					identity: &identity,
					body_present: row.6.ok_or(RawSlotStorageError::Shape)?,
					expires_at: row.7,
					updated_at: row.8,
					index_fields: row.9.as_deref().ok_or(RawSlotStorageError::Shape)?,
				})?
				.ok_or(RawSlotStorageError::Coverage)?;
				Ok(h.metadata)
			}
		})()
		.map_err(|e| owner.identify_error_by_key(e, &key))?;
		current = Some((key, parsed));
	}
	Ok((current, clock.ok_or(RawSlotStorageError::Coverage)?))
}
fn native_query(table: &crate::db::PgQualifiedTableName) -> AuditedSql {
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("WITH locked_native AS MATERIALIZED (SELECT namespace,key,body,expires_at,updated_at,index_fields FROM ONLY ");
	sql.push_argument(table);
	sql.push_text(" WHERE namespace OPERATOR(pg_catalog.=) $1::pg_catalog.bytea AND key OPERATOR(pg_catalog.=) ANY($2::pg_catalog.bytea[]) AND body IS NOT NULL AND expires_at OPERATOR(pg_catalog.<=) (EXTRACT(EPOCH FROM pg_catalog.statement_timestamp()) OPERATOR(pg_catalog.*) 1000000) ORDER BY key USING OPERATOR(pg_catalog.<) LIMIT 1 FOR UPDATE SKIP LOCKED) SELECT 2::pg_catalog.int8,key,NULL::pg_catalog.int8,TRUE,TRUE,namespace,body IS NOT NULL,expires_at,updated_at,index_fields::pg_catalog.text,NULL::pg_catalog.bool,NULL::pg_catalog.bool,NULL::pg_catalog.int8 FROM locked_native");
	acquire::append_clock(&mut sql);
	sql.finish()
}
fn source_query(table: &crate::db::PgQualifiedTableName) -> AuditedSql {
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("WITH source_raw AS (SELECT candidate.key,candidate.value,candidate.expires_at,candidate.updated_at,FALSE AS inserted_reservation FROM (");
	crate::kv::raw::v10::append_source_slot_candidate(&mut sql, table);
	sql.push_text(") candidate");
	append_source_snapshot_projection(&mut sql);
	sql.push_text(" SELECT 3::pg_catalog.int8,pg_catalog.convert_to(key,'UTF8'),NULL::pg_catalog.int8,TRUE,TRUE,NULL::pg_catalog.bytea,value IS NOT NULL,expiry_micros,update_micros,NULL::pg_catalog.text,expiry_valid,update_valid,NULL::pg_catalog.int8 FROM source_snapshot");
	acquire::append_clock(&mut sql);
	sql.finish()
}
