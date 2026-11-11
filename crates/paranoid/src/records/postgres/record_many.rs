//! One native requested-key relation retains actual stored projections and a shared clock.

use crate::records::{
	PointRecordRead, RecordIdentity, RecordKey, RecordNamespace, StateTimestampMicros,
};

use super::{
	Bytea, DatabaseError, DatabaseOperationKind, PgPointStorage, StateStorageError, Tx,
	decode_physical_record, owned_portable_query_as,
};

const READ_RECORD_MANY_LABEL: &str = "db.state_storage.read_record_many";
type ReadRow = (
	i64,
	Bytea,
	Option<Bytea>,
	Option<Bytea>,
	Option<Bytea>,
	Option<i64>,
	Option<i64>,
	Option<String>,
	i64,
);
pub(crate) struct RecordManyReadFields<'a> {
	pub(crate) namespace: &'a RecordNamespace,
	pub(crate) keys: &'a [RecordKey],
}
pub(crate) struct RecordManyRead {
	pub(crate) records: Vec<PointRecordRead>,
	pub(crate) observed_at: StateTimestampMicros,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RecordManyStorageError {
	#[error("native record batch database work failed")]
	Database(#[from] DatabaseError),
	#[error("native record batch physical record is invalid")]
	Physical(#[from] StateStorageError),
	#[error("native record batch returned an unknown, repeated, missing or mismatched marker")]
	Coverage,
	#[error("native record batch does not share one authoritative clock")]
	InconsistentClock,
	#[error("native record batch returned an invalid physical observation")]
	Shape,
	#[error("native record batch member observation failed")]
	Record {
		identity: RecordIdentity,
		input_index: Option<usize>,
		#[source]
		source: Box<Self>,
	},
}
impl RecordManyStorageError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Database(source) => Some(source),
			Self::Physical(source) => source.database_error(),
			Self::Record { source, .. } => source.database_error(),
			_ => None,
		}
	}
	pub(crate) fn identity(&self) -> Option<&RecordIdentity> {
		match self {
			Self::Record { identity, .. } => Some(identity),
			_ => None,
		}
	}
	pub(crate) fn input_index(&self) -> Option<usize> {
		match self {
			Self::Record { input_index, .. } => *input_index,
			_ => None,
		}
	}
}
impl PgPointStorage {
	pub(crate) async fn read_many(
		&self,
		tx: &mut Tx<'_>,
		fields: RecordManyReadFields<'_>,
	) -> Result<RecordManyRead, RecordManyStorageError> {
		if fields.keys.is_empty() {
			return Err(RecordManyStorageError::Coverage);
		}
		let mut completion = tx.track_state_mutation();
		let sql = crate::audited_sql!(
			r#"
WITH requested AS (
    SELECT input.key_bytes, input.ordinality::pg_catalog.int8 AS marker
    FROM pg_catalog.unnest($2::pg_catalog.bytea[]) WITH ORDINALITY input(key_bytes, ordinality)
), observation AS MATERIALIZED (
    SELECT pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.clock_timestamp())
        OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8 AS observed_at
)
SELECT requested.marker, requested.key_bytes, record.namespace, record.key, record.body,
    record.expires_at, record.updated_at, record.index_fields::pg_catalog.text,
    observation.observed_at
FROM requested CROSS JOIN observation
LEFT JOIN ONLY {table} record
    ON record.namespace OPERATOR(pg_catalog.=) $1::pg_catalog.bytea
   AND record.key OPERATOR(pg_catalog.=) requested.key_bytes
ORDER BY requested.marker USING OPERATOR(pg_catalog.<)
"#,
			table = self.table()
		);
		tx.record_database_operation(
			DatabaseOperationKind::FetchAll,
			READ_RECORD_MANY_LABEL,
			Some(sql.as_str()),
		);
		let rows = owned_portable_query_as::<ReadRow>(sql)
			.bind_validated(fields.namespace.as_bytes())
			.bind_validated(
				fields
					.keys
					.iter()
					.map(|key| key.as_bytes().to_vec())
					.collect::<Vec<_>>(),
			)
			.fetch_all(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(READ_RECORD_MANY_LABEL))?;
		let observation = parse_rows(fields, rows)?;
		completion.complete();
		Ok(observation)
	}
}
fn parse_rows(
	fields: RecordManyReadFields<'_>,
	rows: Vec<ReadRow>,
) -> Result<RecordManyRead, RecordManyStorageError> {
	if rows.len() != fields.keys.len() {
		return Err(RecordManyStorageError::Coverage);
	}
	let observed_at =
		StateTimestampMicros::new(rows.first().ok_or(RecordManyStorageError::Coverage)?.8);
	let mut records: Vec<Option<PointRecordRead>> = (0..fields.keys.len()).map(|_| None).collect();
	for (marker, key, namespace, actual_key, body, expiry, update, indexes, clock) in rows {
		let identity = RecordIdentity::new(fields.namespace.clone(), RecordKey::new(key.0));
		let input_index = usize::try_from(marker)
			.ok()
			.and_then(|i| i.checked_sub(1))
			.filter(|i| *i < fields.keys.len());
		let snapshot = (|| {
			let input_index = input_index.ok_or(RecordManyStorageError::Coverage)?;
			if &fields.keys[input_index] != identity.key() || records[input_index].is_some() {
				return Err(RecordManyStorageError::Coverage);
			}
			if clock != observed_at.as_i64() {
				return Err(RecordManyStorageError::InconsistentClock);
			}
			let record = match namespace {
				Some(namespace) => {
					if namespace.0 != fields.namespace.as_bytes()
						|| actual_key.as_ref().map(|key| key.0.as_slice())
							!= Some(identity.key().as_bytes())
					{
						return Err(RecordManyStorageError::Shape);
					}
					decode_physical_record(
						&identity,
						(
							body,
							expiry,
							update,
							indexes.ok_or(RecordManyStorageError::Shape)?,
						),
					)?
				}
				None => {
					if actual_key.is_some()
						|| body.is_some() || expiry.is_some()
						|| update.is_some()
						|| indexes.is_some()
					{
						return Err(RecordManyStorageError::Shape);
					}
					None
				}
			};
			Ok(PointRecordRead::new(record, observed_at))
		})()
		.map_err(|error| RecordManyStorageError::Record {
			identity,
			input_index,
			source: Box::new(error),
		})?;
		records[input_index.ok_or(RecordManyStorageError::Coverage)?] = Some(snapshot);
	}
	Ok(RecordManyRead {
		records: records
			.into_iter()
			.map(|record| record.ok_or(RecordManyStorageError::Coverage))
			.collect::<Result<_, _>>()?,
		observed_at,
	})
}

pub(crate) mod write;
