//! Complete fixed writes and cleanup plans precede the first selected-value mutation.
use super::super::delete::cmds::{self, CmdFields, PreparedCmds};
use super::super::delete::records::PhysicalStatus;
use super::super::record_many::write::apply::{self as shared, RecordWriteCoverageFields, Row};
use super::super::v10::source_point::SourceWriteTimestamps;
use super::super::{DatabaseOperationKind, encode_index_fields, owned_portable_query_as};
use super::{RawDeleteLayout, RawSlotStorageError, SlotOwner};
use crate::db::{AuditedSql, AuditedSqlBuilder};
use crate::records::{EncodedRecord, RecordMetadata};
use std::collections::BTreeSet;
const APPLY_LABEL: &str = "db.state_storage.raw_slot.apply";
pub(super) struct Prepared {
	sql: AuditedSql,
	keys: Vec<Vec<u8>>,
	bodies: Vec<Vec<u8>>,
	indexes: Vec<String>,
	metadata: RecordMetadata,
	texts: Vec<String>,
	timestamps: Option<SourceWriteTimestamps>,
	writes: BTreeSet<usize>,
	deletes: BTreeSet<usize>,
	cleanup: PreparedCmds,
}
pub(super) fn prepare_selected(
	owner: &SlotOwner<'_, '_>,
	index: usize,
	record: EncodedRecord,
) -> Result<Prepared, RawSlotStorageError> {
	let candidate = owner
		.records
		.candidates
		.get(index)
		.ok_or(RawSlotStorageError::Shape)?;
	if !(candidate.native.is_held() || candidate.source.is_held()) {
		return Err(owner.identify_error_by_key(RawSlotStorageError::Shape, &candidate.key));
	}
	let mut deletes = BTreeSet::new();
	if matches!(owner.layout, RawDeleteLayout::Relocating(_)) && candidate.source.value().is_some()
	{
		deletes.insert(index);
	}
	let writes = BTreeSet::from([index]);
	prepare_write(
		owner,
		vec![record],
		WriteSelection {
			writes,
			deletes,
			final_write: true,
		},
	)
}
pub(super) fn prepare_seeds(
	owner: &SlotOwner<'_, '_>,
	seeds: &BTreeSet<usize>,
) -> Result<Option<Prepared>, RawSlotStorageError> {
	if seeds.is_empty() {
		return Ok(None);
	}
	let intent = owner.seed.ok_or(RawSlotStorageError::Shape)?;
	let records = seeds.iter().map(|_| intent.encode()).collect::<Vec<_>>();
	// Seed application leaves every temporary reservation for the final completion plan.
	prepare_write(
		owner,
		records,
		WriteSelection {
			writes: seeds.clone(),
			deletes: BTreeSet::new(),
			final_write: false,
		},
	)
	.map(Some)
}
struct WriteSelection {
	writes: BTreeSet<usize>,
	deletes: BTreeSet<usize>,
	final_write: bool,
}
fn prepare_write(
	owner: &SlotOwner<'_, '_>,
	records: Vec<EncodedRecord>,
	selection: WriteSelection,
) -> Result<Prepared, RawSlotStorageError> {
	let WriteSelection {
		writes,
		deletes,
		final_write,
	} = selection;
	if records.len() != writes.len() || writes.is_empty() {
		return Err(RawSlotStorageError::Shape);
	}
	let metadata = records
		.first()
		.ok_or(RawSlotStorageError::Shape)?
		.record()
		.metadata();
	let mut keys = Vec::with_capacity(records.len());
	let mut bodies = Vec::with_capacity(records.len());
	let mut indexes = Vec::with_capacity(records.len());
	let mut texts = Vec::new();
	for (index, encoded) in writes.iter().zip(records) {
		let candidate = owner
			.records
			.candidates
			.get(*index)
			.ok_or(RawSlotStorageError::Shape)?;
		let record = encoded.into_record();
		if record.metadata() != metadata || !record.index_fields().is_empty() {
			return Err(owner.identify_error_by_key(RawSlotStorageError::Shape, &candidate.key));
		}
		keys.push(candidate.key.as_bytes().to_vec());
		indexes.push(encode_index_fields(record.index_fields()));
		let (body, _, _) = record.into_parts();
		bodies.push(body);
	}
	let mut sql = AuditedSqlBuilder::new();
	// Every parameter has an actual fixed type even when a placement does not use it.
	sql.push_text("WITH requested_parameters AS (SELECT $1::pg_catalog.bytea,$2::pg_catalog.bytea[],$3::pg_catalog.bytea[],$4::pg_catalog.int8,$5::pg_catalog.int8,$6::pg_catalog.text[],$7::pg_catalog.text[],$8::pg_catalog.interval,$9::pg_catalog.interval) ");
	let timestamps = if matches!(owner.layout, RawDeleteLayout::Source(_)) {
		let timestamps = SourceWriteTimestamps::parse(metadata)?;
		for index in &writes {
			texts.push(
				owner
					.source_text(&owner.records.candidates[*index].key)?
					.to_owned(),
			);
		}
		shared::append_source_update(&mut sql, owner.source_table()?);
		Some(timestamps)
	} else {
		shared::append_native_update(&mut sql, owner.native_table()?);
		if !deletes.is_empty() {
			for index in &deletes {
				texts.push(
					owner
						.source_text(&owner.records.candidates[*index].key)?
						.to_owned(),
				);
			}
			sql.push_text("; ");
			shared::append_source_delete(
				&mut sql,
				owner.source_table()?,
				1,
				shared::SourceDeleteKeysParam::Seventh,
			);
		}
		None
	};
	let cleanup = if final_write {
		owner
			.records
			.inserted_reservations()
			.difference(&writes)
			.copied()
			.collect()
	} else {
		BTreeSet::new()
	};
	let cleanup = cmds::prepare(CmdFields {
		records: &owner.records,
		layout: owner.layout,
		native: BTreeSet::new(),
		source: BTreeSet::new(),
		cleanup,
	})?;
	Ok(Prepared {
		sql: sql.finish(),
		keys,
		bodies,
		indexes,
		metadata,
		texts,
		timestamps,
		writes,
		deletes,
		cleanup,
	})
}
pub(super) async fn execute(
	owner: &mut SlotOwner<'_, '_>,
	prepared: Prepared,
) -> Result<(), RawSlotStorageError> {
	let Prepared {
		sql,
		keys,
		bodies,
		indexes,
		metadata,
		texts,
		timestamps,
		writes,
		deletes,
		cleanup,
	} = prepared;
	owner.tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		APPLY_LABEL,
		Some(sql.as_str()),
	);
	let expiry = timestamps
		.as_ref()
		.and_then(SourceWriteTimestamps::expires_at);
	let updated = timestamps.as_ref().map(SourceWriteTimestamps::updated_at);
	let rows = owned_portable_query_as::<Row>(sql)
		.bind_validated(owner.namespace.as_bytes())
		.bind_validated(keys)
		.bind_validated(bodies)
		.bind_validated(metadata.expires_at.map(|t| t.as_i64()))
		.bind_validated(metadata.updated_at.as_i64())
		.bind_validated(indexes)
		.bind_validated(texts)
		.bind_validated(expiry)
		.bind_validated(updated)
		.fetch_all(owner.tx.inner.as_mut())
		.await
		.map_err(|e| e.with_operation_label(APPLY_LABEL))?;
	let all_keys = owner
		.records
		.candidates
		.iter()
		.map(|c| c.key.clone())
		.collect::<Vec<_>>();
	shared::check_returned_coverage(
		RecordWriteCoverageFields {
			namespace: owner.namespace,
			keys: &all_keys,
			positions: &owner.records.positions,
			writes: &writes,
			deletes: &deletes,
		},
		rows,
	)
	.map_err(|e| match e.key {
		Some(key) => owner.identify_error_by_key(RawSlotStorageError::Coverage, &key),
		None => RawSlotStorageError::Coverage,
	})?;
	cmds::apply(owner.tx, &owner.records, cleanup).await?;
	for index in writes {
		let candidate = &mut owner.records.candidates[index];
		let physical = if matches!(owner.layout, RawDeleteLayout::Source(_)) {
			&mut candidate.source
		} else {
			&mut candidate.native
		};
		*physical = PhysicalStatus::Value(metadata);
	}
	for index in deletes {
		owner.records.candidates[index].source = PhysicalStatus::Deleted;
	}
	Ok(())
}
pub(super) async fn keep(owner: &mut SlotOwner<'_, '_>) -> Result<(), RawSlotStorageError> {
	let cleanup = owner.records.inserted_reservations();
	let prepared = cmds::prepare(CmdFields {
		records: &owner.records,
		layout: owner.layout,
		native: BTreeSet::new(),
		source: BTreeSet::new(),
		cleanup,
	})?;
	cmds::apply(owner.tx, &owner.records, prepared).await?;
	Ok(())
}
