//! One fixed physical delete/cleanup plan and exact returned-identity coverage.
use super::super::{Bytea, DatabaseOperationKind, WriteTx, owned_portable_query_as};
use super::records::RawDeleteRecords;
use super::{RawDeleteLayout, RawDeleteStorageError, RecordDeletionStep};
use crate::db::{AuditedSql, AuditedSqlBuilder};
use crate::records::RecordKey;
use std::collections::BTreeSet;
const APPLY_LABEL: &str = "db.state_storage.raw_delete.apply";
type Row = (i64, Bytea, Bytea);
pub(in crate::kv) struct CmdFields<'a> {
	pub(in crate::kv) records: &'a RawDeleteRecords,
	pub(in crate::kv) layout: RawDeleteLayout<'a>,
	pub(in crate::kv) native: BTreeSet<usize>,
	pub(in crate::kv) source: BTreeSet<usize>,
	pub(in crate::kv) cleanup: BTreeSet<usize>,
}
pub(in crate::kv) struct PreparedCmds {
	sql: Option<AuditedSql>,
	native_keys: Vec<Vec<u8>>,
	source_texts: Vec<String>,
	cleanup_keys: Vec<Vec<u8>>,
	native: BTreeSet<usize>,
	source: BTreeSet<usize>,
	cleanup: BTreeSet<usize>,
}
pub(in crate::kv) fn prepare(fields: CmdFields<'_>) -> Result<PreparedCmds, RawDeleteStorageError> {
	let records = fields.records;
	let layout = fields.layout;
	let native = fields.native;
	let source = fields.source;
	let native_keys = native
		.iter()
		.map(|i| records.candidates[*i].key.as_bytes().to_vec())
		.collect::<Vec<_>>();
	let source_texts = source
		.iter()
		.map(|i| {
			std::str::from_utf8(records.candidates[*i].key.as_bytes())
				.map(str::to_owned)
				.map_err(|e| records.identify(e.into(), *i, RecordDeletionStep::Source))
		})
		.collect::<Result<Vec<_>, _>>()?;
	let cleanup = fields.cleanup;
	if !cleanup.is_disjoint(&native) {
		return Err(RawDeleteStorageError::Shape);
	}
	let cleanup_keys = cleanup
		.iter()
		.map(|i| records.candidates[*i].key.as_bytes().to_vec())
		.collect::<Vec<_>>();
	let sql = if native.is_empty() && source.is_empty() && cleanup.is_empty() {
		None
	} else {
		let mut sql = AuditedSqlBuilder::new();
		// Every binding has an explicit type even when one physical role has no command.
		sql.push_text("WITH requested_parameters AS (SELECT $1::pg_catalog.bytea AS namespace,$2::pg_catalog.bytea[] AS native_keys,$3::pg_catalog.text[] AS source_keys,$4::pg_catalog.bytea[] AS cleanup_keys) ");
		if !native.is_empty() {
			sql.push_text("DELETE FROM ONLY ");
			sql.push_argument(layout.native_table().ok_or(RawDeleteStorageError::Shape)?);
			sql.push_text(" WHERE namespace OPERATOR(pg_catalog.=) $1::pg_catalog.bytea AND key OPERATOR(pg_catalog.=) ANY($2::pg_catalog.bytea[]) AND body IS NOT NULL RETURNING 0::pg_catalog.int8,namespace,key");
		}
		if !source.is_empty() {
			if !native.is_empty() {
				sql.push_text("; ");
			}
			sql.push_text("DELETE FROM ONLY ");
			sql.push_argument(layout.source_table().ok_or(RawDeleteStorageError::Shape)?);
			sql.push_text(" WHERE key OPERATOR(pg_catalog.=) ANY($3::pg_catalog.text[]) RETURNING 1::pg_catalog.int8,$1::pg_catalog.bytea,pg_catalog.convert_to(key,'UTF8')");
		}
		if !cleanup.is_empty() {
			if !native.is_empty() || !source.is_empty() {
				sql.push_text("; ");
			}
			if !matches!(layout, RawDeleteLayout::Relocating(_)) {
				return Err(RawDeleteStorageError::Shape);
			}
			super::super::reservation_cleanup::append_native_reservation_cleanup(
				&mut sql,
				super::super::reservation_cleanup::NativeReservationCleanupSqlFields {
					table: layout.native_table().ok_or(RawDeleteStorageError::Shape)?,
					keys_param:
						super::super::reservation_cleanup::NativeReservationCleanupKeysParam::Fourth,
				},
			);
		}
		Some(sql.finish())
	};
	Ok(PreparedCmds {
		sql,
		native_keys,
		source_texts,
		cleanup_keys,
		native,
		source,
		cleanup,
	})
}
pub(in crate::kv) async fn apply(
	tx: &mut WriteTx<'_>,
	records: &RawDeleteRecords,
	prepared: PreparedCmds,
) -> Result<(), RawDeleteStorageError> {
	let PreparedCmds {
		sql,
		native_keys,
		source_texts,
		cleanup_keys,
		native,
		source,
		cleanup,
	} = prepared;
	let Some(sql) = sql else {
		return Ok(());
	};
	tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		APPLY_LABEL,
		Some(sql.as_str()),
	);
	let rows = owned_portable_query_as::<Row>(sql)
		.bind_validated(records.namespace.as_bytes())
		.bind_validated(native_keys)
		.bind_validated(source_texts)
		.bind_validated(cleanup_keys)
		.fetch_all(tx.inner.as_mut())
		.await
		.map_err(|e| e.with_operation_label(APPLY_LABEL))?;
	let expected = [native, source, cleanup];
	let mut seen = [BTreeSet::new(), BTreeSet::new(), BTreeSet::new()];
	for (tag, namespace, key) in rows {
		let key = RecordKey::new(key.0);
		let known_role = match tag {
			0 => RecordDeletionStep::Native,
			1 => RecordDeletionStep::Source,
			2 => RecordDeletionStep::ReservationCleanup,
			_ => RecordDeletionStep::Discovery,
		};
		let index = *records.positions.get(&key).ok_or_else(|| {
			records.identify_unexpected(RawDeleteStorageError::Coverage, key.clone(), known_role)
		})?;
		let step = match tag {
			0 => RecordDeletionStep::Native,
			1 => RecordDeletionStep::Source,
			2 => RecordDeletionStep::ReservationCleanup,
			_ => {
				return Err(records.identify(
					RawDeleteStorageError::Coverage,
					index,
					RecordDeletionStep::Discovery,
				));
			}
		};
		let tag = tag as usize;
		if namespace.0 != records.namespace.as_bytes()
			|| !expected[tag].contains(&index)
			|| !seen[tag].insert(index)
		{
			return Err(records.identify(RawDeleteStorageError::Coverage, index, step));
		}
	}
	for (tag, (expected, seen)) in expected.iter().zip(&seen).enumerate() {
		if let Some(index) = expected.difference(seen).next() {
			let step = match tag {
				0 => RecordDeletionStep::Native,
				1 => RecordDeletionStep::Source,
				_ => RecordDeletionStep::ReservationCleanup,
			};
			return Err(records.identify(RawDeleteStorageError::Coverage, *index, step));
		}
	}
	Ok(())
}
