//! Writes prepared records under the caller's complete, canonically acquired identity set.

use super::{
	Bytea, DatabaseOperationKind, PgPointStorage, StateStorageError, WriteTx, encode_index_fields,
	owned_portable_query_as,
};
use crate::records::{EncodedRecord, RecordIdentity, RecordKey};
use std::collections::BTreeSet;

const LABEL: &str = "db.state_storage.write_record_batch";
// A conversion page can contain large values. Split transport requests without splitting
// the caller's transaction or rejecting a value that exceeds this batching target.
const TARGET_INPUT_SIZE: usize = 4 * 1024 * 1024;

#[derive(Default)]
struct PreparedBatch {
	keys: Vec<Vec<u8>>,
	bodies: Vec<Vec<u8>>,
	expirations: Vec<Option<i64>>,
	updates: Vec<i64>,
	indexes: Vec<String>,
	input_size: usize,
}

impl PgPointStorage {
	/// The caller owns acquisition, rollback, and progress. All values are prepared before any
	/// mutation, and each returned identity must occur exactly once in the requested batch.
	pub(in crate::records) async fn write_records(
		&self,
		tx: &mut WriteTx<'_>,
		records: Vec<(RecordIdentity, EncodedRecord)>,
	) -> Result<(), StateStorageError> {
		let Some((first, _)) = records.first() else {
			return Ok(());
		};
		let namespace = first.namespace().clone();
		let mut seen = BTreeSet::new();
		let mut batches = Vec::new();
		let mut batch = PreparedBatch::default();
		for (identity, encoded) in records {
			if identity.namespace() != &namespace || !seen.insert(identity.key().clone()) {
				return Err(StateStorageError::BatchWriteCoverage);
			}
			let (body, metadata, fields) = encoded.into_record().into_parts();
			let indexes = encode_index_fields(&fields);
			let size = body
				.len()
				.saturating_add(indexes.len())
				.saturating_add(identity.key().as_bytes().len());
			if !batch.keys.is_empty() && batch.input_size.saturating_add(size) > TARGET_INPUT_SIZE {
				batches.push(std::mem::take(&mut batch));
			}
			batch.keys.push(identity.key().as_bytes().to_vec());
			batch.bodies.push(body);
			batch
				.expirations
				.push(metadata.expires_at.map(|time| time.as_i64()));
			batch.updates.push(metadata.updated_at.as_i64());
			batch.indexes.push(indexes);
			batch.input_size = batch.input_size.saturating_add(size);
		}
		batches.push(batch);
		let query = crate::audited_sql!(
			r#"
UPDATE ONLY {table} AS target
SET body = input.body, expires_at = input.expiration, updated_at = input.updated,
    index_fields = input.indexes::pg_catalog.jsonb
FROM ROWS FROM (
    pg_catalog.unnest($2::pg_catalog.bytea[]), pg_catalog.unnest($3::pg_catalog.bytea[]),
    pg_catalog.unnest($4::pg_catalog.int8[]), pg_catalog.unnest($5::pg_catalog.int8[]),
    pg_catalog.unnest($6::pg_catalog.text[])
) input(key,body,expiration,updated,indexes)
WHERE target.namespace OPERATOR(pg_catalog.=) $1::pg_catalog.bytea
  AND target.key OPERATOR(pg_catalog.=) input.key
RETURNING target.namespace,target.key
"#,
			table = self.table()
		);
		for batch in batches {
			let expected = batch
				.keys
				.iter()
				.cloned()
				.map(RecordKey::new)
				.collect::<BTreeSet<_>>();
			tx.record_database_operation(
				DatabaseOperationKind::FetchAll,
				LABEL,
				Some(query.as_str()),
			);
			let rows = owned_portable_query_as::<(Bytea, Bytea)>(query.clone())
				.bind_validated(namespace.as_bytes())
				.bind_validated(batch.keys)
				.bind_validated(batch.bodies)
				.bind_validated(batch.expirations)
				.bind_validated(batch.updates)
				.bind_validated(batch.indexes)
				.fetch_all(tx.inner.as_mut())
				.await
				.map_err(|error| error.with_operation_label(LABEL))?;
			let mut returned = BTreeSet::new();
			for (actual_namespace, key) in rows {
				if actual_namespace.0.as_slice() != namespace.as_bytes()
					|| !returned.insert(RecordKey::new(key.0))
				{
					return Err(StateStorageError::BatchWriteCoverage);
				}
			}
			if returned != expected {
				return Err(StateStorageError::BatchWriteCoverage);
			}
		}
		Ok(())
	}
}
