//! Every value and coverage expectation is prepared before the first value update.

use super::super::super::v10::source_point::SourceWriteTimestamps;
use super::super::super::{
	Bytea, DatabaseOperationKind, encode_index_fields, owned_portable_query_as,
};
use super::{RecordManyHeldSet, RecordManyWriteError, RecordManyWriteLayout};
use crate::db::{AuditedSql, AuditedSqlBuilder, PgQualifiedTableName};
use crate::records::{EncodedRecord, RecordMetadata};
use std::collections::BTreeSet;

const APPLY_LABEL: &str = "db.state_storage.record_many.apply";
const CLEANUP_LABEL: &str = "db.state_storage.record_many.cleanup";
pub(crate) type Row = (i64, Bytea, Bytea);
pub(super) struct Prepared {
	sql: AuditedSql,
	keys: Vec<Vec<u8>>,
	bodies: Vec<Vec<u8>>,
	indexes: Vec<String>,
	metadata: RecordMetadata,
	texts: Vec<String>,
	timestamps: Option<SourceWriteTimestamps>,
}
pub(super) fn prepare(
	held: &RecordManyHeldSet<'_, '_>,
	records: Vec<EncodedRecord>,
	timestamps: Option<SourceWriteTimestamps>,
) -> Result<Prepared, RecordManyWriteError> {
	if records.len() != held.keys.len() {
		return Err(RecordManyWriteError::Shape);
	}
	let metadata = records
		.first()
		.ok_or(RecordManyWriteError::Shape)?
		.record()
		.metadata();
	let mut bodies = Vec::with_capacity(records.len());
	let mut indexes = Vec::with_capacity(records.len());
	for (index, encoded) in records.into_iter().enumerate() {
		let record = encoded.into_record();
		if record.metadata() != metadata
			|| (matches!(held.layout, RecordManyWriteLayout::Source(_))
				&& !record.index_fields().is_empty())
		{
			return Err(held.identify(RecordManyWriteError::Shape, index));
		}
		indexes.push(encode_index_fields(record.index_fields()));
		let (body, _, _) = record.into_parts();
		bodies.push(body);
	}
	let keys = held
		.keys
		.iter()
		.map(|key| key.as_bytes().to_vec())
		.collect();
	let mut sql = AuditedSqlBuilder::new();
	let texts = match &held.layout {
		RecordManyWriteLayout::Source(storage) => {
			if timestamps.is_none() {
				return Err(RecordManyWriteError::Shape);
			}
			append_source_update(&mut sql, storage.table());
			held.source_texts
				.as_ref()
				.ok_or(RecordManyWriteError::Shape)?
				.clone()
		}
		RecordManyWriteLayout::Native(storage) => {
			append_native_update(&mut sql, storage.table());
			Vec::new()
		}
		RecordManyWriteLayout::Relocating(storage) => {
			append_native_update(&mut sql, storage.native_storage().table());
			let texts = held
				.source_texts
				.as_ref()
				.ok_or(RecordManyWriteError::Shape)?;
			let selected = held
				.source_values()
				.iter()
				.map(|index| texts[*index].clone())
				.collect::<Vec<_>>();
			if !selected.is_empty() {
				sql.push_text("; ");
				append_source_delete(
					&mut sql,
					storage.source_table(),
					1,
					SourceDeleteKeysParam::Seventh,
				);
			}
			selected
		}
	};
	Ok(Prepared {
		sql: sql.finish(),
		keys,
		bodies,
		indexes,
		metadata,
		texts,
		timestamps,
	})
}
pub(crate) fn append_native_update(sql: &mut AuditedSqlBuilder, table: &PgQualifiedTableName) {
	sql.push_text("UPDATE ONLY ");
	sql.push_argument(table);
	sql.push_text(" AS target SET body = input.body, expires_at = $4::pg_catalog.int8, updated_at = $5::pg_catalog.int8, index_fields = input.indexes::pg_catalog.jsonb FROM ROWS FROM (pg_catalog.unnest($2::pg_catalog.bytea[]),pg_catalog.unnest($3::pg_catalog.bytea[]),pg_catalog.unnest($6::pg_catalog.text[])) input(key,body,indexes) WHERE target.namespace OPERATOR(pg_catalog.=) $1::pg_catalog.bytea AND target.key OPERATOR(pg_catalog.=) input.key RETURNING 0::pg_catalog.int8,target.namespace,target.key");
}
pub(crate) fn append_source_update(sql: &mut AuditedSqlBuilder, table: &PgQualifiedTableName) {
	sql.push_text("UPDATE ONLY ");
	sql.push_argument(table);
	sql.push_text(" AS target SET value = input.body, expires_at = (pg_catalog.timestamp '1970-01-01 00:00:00' OPERATOR(pg_catalog.+) $8::pg_catalog.interval) AT TIME ZONE INTERVAL '0 seconds', updated_at = (pg_catalog.timestamp '1970-01-01 00:00:00' OPERATOR(pg_catalog.+) $9::pg_catalog.interval) AT TIME ZONE INTERVAL '0 seconds' FROM ROWS FROM (pg_catalog.unnest($7::pg_catalog.text[]),pg_catalog.unnest($3::pg_catalog.bytea[])) input(key,body) WHERE target.key OPERATOR(pg_catalog.=) input.key RETURNING 0::pg_catalog.int8,$1::pg_catalog.bytea,pg_catalog.convert_to(target.key,'UTF8')");
}
/// The text array parameter that holds the v10 keys of a v10 row deletion.
#[derive(Clone, Copy)]
pub(crate) enum SourceDeleteKeysParam {
	Third,
	Seventh,
}
pub(crate) fn append_source_delete(
	sql: &mut AuditedSqlBuilder,
	table: &PgQualifiedTableName,
	tag: i64,
	keys_param: SourceDeleteKeysParam,
) {
	sql.push_text("DELETE FROM ONLY ");
	sql.push_argument(table);
	sql.push_text(" WHERE key OPERATOR(pg_catalog.=) ANY(");
	sql.push_text(match keys_param {
		SourceDeleteKeysParam::Third => "$3",
		SourceDeleteKeysParam::Seventh => "$7",
	});
	sql.push_text("::pg_catalog.text[]) RETURNING ");
	sql.push_argument(&tag);
	sql.push_text("::pg_catalog.int8,$1::pg_catalog.bytea,pg_catalog.convert_to(key,'UTF8')");
}
pub(super) async fn apply(
	held: &mut RecordManyHeldSet<'_, '_>,
	prepared: Prepared,
) -> Result<(), RecordManyWriteError> {
	held.tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		APPLY_LABEL,
		Some(prepared.sql.as_str()),
	);
	let query = owned_portable_query_as::<Row>(prepared.sql)
		.bind_validated(held.namespace.as_bytes())
		.bind_validated(prepared.keys)
		.bind_validated(prepared.bodies)
		.bind_validated(prepared.metadata.expires_at.map(|time| time.as_i64()))
		.bind_validated(prepared.metadata.updated_at.as_i64())
		.bind_validated(prepared.indexes);
	let rows = match prepared.timestamps {
		Some(timestamps) => {
			query
				.bind_validated(prepared.texts)
				.bind_validated(timestamps.expires_at())
				.bind_validated(timestamps.updated_at())
				.fetch_all(held.tx.inner.as_mut())
				.await
		}
		None if !prepared.texts.is_empty() => {
			query
				.bind_validated(prepared.texts)
				.fetch_all(held.tx.inner.as_mut())
				.await
		}
		None => query.fetch_all(held.tx.inner.as_mut()).await,
	}
	.map_err(|error| error.with_operation_label(APPLY_LABEL))?;

	let all = (0..held.keys.len()).collect::<BTreeSet<_>>();
	let deletes = if matches!(held.layout, RecordManyWriteLayout::Relocating(_)) {
		held.source_values()
	} else {
		BTreeSet::new()
	};
	check_coverage(
		held,
		rows,
		Coverage {
			writes: &all,
			deletes: &deletes,
		},
	)
}
pub(super) async fn cleanup(
	held: &mut RecordManyHeldSet<'_, '_>,
) -> Result<(), RecordManyWriteError> {
	let inserted = held.inserted_reservations();
	if inserted.is_empty() {
		return Ok(());
	}
	let mut sql = AuditedSqlBuilder::new();
	match &held.layout {
		RecordManyWriteLayout::Source(storage) => {
			append_source_delete(&mut sql, storage.table(), 0, SourceDeleteKeysParam::Third)
		}
		_ => {
			sql.push_text("DELETE FROM ONLY ");
			sql.push_argument(
				held.layout
					.native_table()
					.ok_or(RecordManyWriteError::Shape)?,
			);
			sql.push_text(" WHERE namespace OPERATOR(pg_catalog.=) $1::pg_catalog.bytea AND key OPERATOR(pg_catalog.=) ANY($2::pg_catalog.bytea[]) AND body IS NULL RETURNING 0::pg_catalog.int8,namespace,key");
		}
	}
	let keys = inserted
		.iter()
		.map(|index| held.keys[*index].as_bytes().to_vec())
		.collect::<Vec<_>>();
	let texts = held
		.source_texts
		.as_ref()
		.map(|texts| {
			inserted
				.iter()
				.map(|index| texts[*index].clone())
				.collect::<Vec<_>>()
		})
		.unwrap_or_default();
	let sql = sql.finish();
	held.tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		CLEANUP_LABEL,
		Some(sql.as_str()),
	);
	let query = owned_portable_query_as::<Row>(sql)
		.bind_validated(held.namespace.as_bytes())
		.bind_validated(keys);
	let rows = if matches!(held.layout, RecordManyWriteLayout::Source(_)) {
		query
			.bind_validated(texts)
			.fetch_all(held.tx.inner.as_mut())
			.await
	} else {
		query.fetch_all(held.tx.inner.as_mut()).await
	}
	.map_err(|error| error.with_operation_label(CLEANUP_LABEL))?;

	check_coverage(
		held,
		rows,
		Coverage {
			writes: &inserted,
			deletes: &BTreeSet::new(),
		},
	)
}
struct Coverage<'a> {
	writes: &'a BTreeSet<usize>,
	deletes: &'a BTreeSet<usize>,
}
fn check_coverage(
	held: &RecordManyHeldSet<'_, '_>,
	rows: Vec<Row>,
	expected: Coverage<'_>,
) -> Result<(), RecordManyWriteError> {
	check_returned_coverage(
		RecordWriteCoverageFields {
			namespace: &held.namespace,
			keys: &held.keys,
			positions: &held.positions,
			writes: expected.writes,
			deletes: expected.deletes,
		},
		rows,
	)
	.map_err(|failure| match failure.index {
		Some(index) => held.identify(RecordManyWriteError::Coverage, index),
		None => RecordManyWriteError::Coverage,
	})
}
pub(crate) struct RecordWriteCoverageFields<'a> {
	pub(crate) namespace: &'a crate::records::RecordNamespace,
	pub(crate) keys: &'a [crate::records::RecordKey],
	pub(crate) positions: &'a std::collections::BTreeMap<crate::records::RecordKey, usize>,
	pub(crate) writes: &'a BTreeSet<usize>,
	pub(crate) deletes: &'a BTreeSet<usize>,
}
#[derive(Debug)]
pub(crate) struct RecordWriteCoverageFailure {
	pub(crate) key: Option<crate::records::RecordKey>,
	pub(crate) index: Option<usize>,
}
pub(crate) fn check_returned_coverage(
	fields: RecordWriteCoverageFields<'_>,
	rows: Vec<Row>,
) -> Result<(), RecordWriteCoverageFailure> {
	let mut seen_writes = BTreeSet::new();
	let mut seen_deletes = BTreeSet::new();
	for (tag, namespace, key) in rows {
		let key = crate::records::RecordKey::new(key.0);
		let index = *fields
			.positions
			.get(&key)
			.ok_or_else(|| RecordWriteCoverageFailure {
				key: Some(key.clone()),
				index: None,
			})?;
		let valid = namespace.0 == fields.namespace.as_bytes()
			&& match tag {
				0 => fields.writes.contains(&index) && seen_writes.insert(index),
				1 => fields.deletes.contains(&index) && seen_deletes.insert(index),
				_ => false,
			};
		if !valid {
			return Err(RecordWriteCoverageFailure {
				key: Some(key),
				index: Some(index),
			});
		}
	}
	if &seen_writes != fields.writes || &seen_deletes != fields.deletes {
		let index = fields
			.writes
			.difference(&seen_writes)
			.chain(fields.deletes.difference(&seen_deletes))
			.next()
			.copied();
		return Err(RecordWriteCoverageFailure {
			key: index.and_then(|i| fields.keys.get(i).cloned()),
			index,
		});
	}
	Ok(())
}
