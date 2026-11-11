//! Direct published v10 KV point storage during source-writer coexistence.
//!
//! Callers must obtain source-aware admission in source-gate then native-control order.
//! Physical validation supplies no admission or retirement proof. Raw source values remain
//! bytes; this adapter does not relocate records or select a JSON decoder.

use crate::records::RecordHeaderObservation;
use std::num::NonZeroU32;

use super::super::{
	AuditedSql, BUILTIN_CHECK_EXPRESSIONS_SQL, Bytea, DatabaseError, DatabaseOperationKind,
	PG_COLUMN_METADATA_SQL, PgPointStorage, PgQualifiedTableName, PointGuardSnapshot,
	ReservationOwnership, SchemaError, StateMutationCompletion, StateStorageError, TABLE_SHAPE_SQL,
	Tx, WriteTx, builtin_btree_index_conditions, normalize_builtin_check_expression,
	owned_portable_query, owned_portable_query_as, owned_portable_query_scalar,
};
use crate::db::{AuditedSqlBuilder, TransactionFailure};
use crate::records::v10_key::Key;
use crate::records::{
	EncodedMetadataUpdate, EncodedRecord, IndexFields, PointRecordLock, PointRecordRead,
	RecordIdentity, RecordMetadata, SourcePointRecord, StateTimestampMicros, StoredRecord,
};

const VALIDATE_LABEL: &str = "db.state_storage.source_point.validate";
const READ_LABEL: &str = "db.state_storage.source_point.read";
const LOCK_LABEL: &str = "db.state_storage.source_point.lock";
const WRITE_LABEL: &str = "db.state_storage.source_point.write";
const UPDATE_METADATA_LABEL: &str = "db.state_storage.source_point.update_metadata";
const DELETE_LABEL: &str = "db.state_storage.source_point.delete";

const SOURCE_IDENTITY_SQL: &str = concat!(
	r#"
SELECT EXISTS (
    SELECT 1 FROM pg_catalog.pg_index idx
    JOIN pg_catalog.pg_class index_class
      ON index_class.oid OPERATOR(pg_catalog.=) idx.indexrelid
    JOIN pg_catalog.pg_attribute key_attr
      ON key_attr.attrelid OPERATOR(pg_catalog.=) idx.indrelid
     AND key_attr.attname OPERATOR(pg_catalog.=) 'key' AND NOT key_attr.attisdropped
    JOIN pg_catalog.pg_opclass key_ops
      ON key_ops.oid OPERATOR(pg_catalog.=) idx.indclass[0]
    WHERE idx.indrelid OPERATOR(pg_catalog.=) pg_catalog.to_regclass($1)
      AND idx.indisunique AND idx.indimmediate
      AND idx.indnkeyatts OPERATOR(pg_catalog.=) 1
      AND idx.indpred IS NULL AND idx.indexprs IS NULL
      AND idx.indkey[0] OPERATOR(pg_catalog.=) key_attr.attnum
      AND idx.indoption[0] OPERATOR(pg_catalog.=) 0
      AND idx.indcollation[0] OPERATOR(pg_catalog.=) key_attr.attcollation
      AND key_attr.attcollation OPERATOR(pg_catalog.=) ANY (
          ARRAY['pg_catalog."C"'::pg_catalog.regcollation,
                'pg_catalog."POSIX"'::pg_catalog.regcollation]::pg_catalog.oid[])
      AND key_ops.opcnamespace OPERATOR(pg_catalog.=) 'pg_catalog'::pg_catalog.regnamespace
      AND key_ops.opcname OPERATOR(pg_catalog.=) 'text_ops'
      AND key_ops.opcmethod OPERATOR(pg_catalog.=) 403
"#,
	builtin_btree_index_conditions!(),
	")"
);

#[derive(Clone, Copy, Debug)]
pub(crate) enum SourceTimestampField {
	ExpiresAt,
	UpdatedAt,
}

pub(in crate::records::postgres) type SourcePhysicalRow =
	(Option<Bytea>, Option<i64>, Option<i64>, bool, bool);

/// Identity-free physical parsing shared by direct source and relocation operations.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SourcePhysicalError {
	#[error("source timestamp {field:?} does not fit the native finite microsecond domain")]
	TimestampUnrepresentable { field: SourceTimestampField },
	#[error("source point physical value is invalid: {reason}")]
	Invalid { reason: &'static str },
}

pub(crate) struct SourcePhysicalRecord {
	body: Vec<u8>,
	metadata: RecordMetadata,
}

impl SourcePhysicalRecord {
	pub(in crate::records::postgres) fn into_source(self) -> SourcePointRecord {
		SourcePointRecord::new(self.body, self.metadata)
	}
	pub(crate) fn into_point(self) -> StoredRecord {
		StoredRecord::from_storage(self.body, self.metadata, IndexFields::new())
	}
}

/// Actual source body presence and timestamp projections, without opaque payload transfer.
pub(crate) struct SourcePhysicalHeaderFields {
	pub(crate) body_present: bool,
	pub(crate) expires_at: Option<i64>,
	pub(crate) updated_at: Option<i64>,
	pub(crate) expires_at_valid: bool,
	pub(crate) updated_at_valid: bool,
}

pub(crate) fn decode_source_header(
	fields: SourcePhysicalHeaderFields,
) -> Result<RecordMetadata, SourcePhysicalError> {
	if !fields.expires_at_valid {
		return Err(SourcePhysicalError::TimestampUnrepresentable {
			field: SourceTimestampField::ExpiresAt,
		});
	}
	if !fields.updated_at_valid {
		return Err(SourcePhysicalError::TimestampUnrepresentable {
			field: SourceTimestampField::UpdatedAt,
		});
	}
	if !fields.body_present {
		return Err(SourcePhysicalError::Invalid {
			reason: "value is null",
		});
	}
	let updated_at = fields.updated_at.ok_or(SourcePhysicalError::Invalid {
		reason: "modification timestamp is null",
	})?;
	Ok(RecordMetadata {
		expires_at: fields.expires_at.map(StateTimestampMicros::new),
		updated_at: StateTimestampMicros::new(updated_at),
	})
}

pub(crate) fn decode_source_record(
	row: SourcePhysicalRow,
) -> Result<SourcePhysicalRecord, SourcePhysicalError> {
	let (body, expires_at, updated_at, expires_at_valid, updated_at_valid) = row;
	let metadata = decode_source_header(SourcePhysicalHeaderFields {
		body_present: body.is_some(),
		expires_at,
		updated_at,
		expires_at_valid,
		updated_at_valid,
	})?;
	Ok(SourcePhysicalRecord {
		body: body
			.expect("validated source header requires a present body")
			.0,
		metadata,
	})
}

/// Pins a present source heap before checking its physical behavior. The relation lock lasts
/// through transaction completion; this check supplies no compatibility or retirement proof.
pub(crate) async fn validate_source_heap(
	source_table: &PgQualifiedTableName,
	tx: &mut Tx<'_>,
) -> Result<(), SchemaError> {
	let mut completion = tx.track_state_mutation();
	let source_name = source_table.quoted().to_string();
	let mut builder = AuditedSqlBuilder::new();
	builder.push_text("LOCK TABLE ONLY ");
	builder.push_argument(source_table);
	builder.push_text(" IN ACCESS SHARE MODE; ");
	builder.push_text(TABLE_SHAPE_SQL);
	let statement = builder.finish();
	tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		VALIDATE_LABEL,
		Some(statement.as_str()),
	);
	let shape =
		owned_portable_query_as::<(String, String, bool, bool, bool, bool, bool)>(statement)
			.bind_validated(source_name.as_str())
			.fetch_all(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(VALIDATE_LABEL))?;
	if !matches!(shape.as_slice(), [(kind, persistence, false, false, false, false, false)] if kind == "r" && persistence == "p")
	{
		return Err(SchemaError::mismatch(
			"source relation must be a permanent heap without row policies, inheritance, rules, enabled triggers, or foreign-key associations",
		));
	}
	completion.complete();
	Ok(())
}

/// Caller retains relation authority and its IO tracker through this shared physical check.
pub(in crate::records::postgres) async fn validate_source_existing(
	source_table: &PgQualifiedTableName,
	tx: &mut Tx<'_>,
) -> Result<(), SchemaError> {
	let source_name = source_table.quoted().to_string();
	validate_source_heap(source_table, tx).await?;
	tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		VALIDATE_LABEL,
		Some(PG_COLUMN_METADATA_SQL),
	);
	let columns = owned_portable_query_as::<(String, String, bool, Option<String>, bool)>(
		PG_COLUMN_METADATA_SQL,
	)
	.bind_validated(source_name.as_str())
	.fetch_all(tx.inner.as_mut())
	.await
	.map_err(|error| error.with_operation_label(VALIDATE_LABEL))?;
	let required = [
		("key", "text", true),
		("value", "bytea", true),
		("expires_at", "timestamp with time zone", false),
		("updated_at", "timestamp with time zone", true),
	];
	if columns.len() != required.len()
		|| required.iter().any(|(name, data_type, not_null)| {
			!columns.iter().any(|actual| {
				actual.0 == *name
					&& actual.1 == *data_type
					&& actual.2 == *not_null
					&& actual.4 && if *name == "key" {
					matches!(actual.3.as_deref(), Some("C" | "POSIX"))
				} else {
					actual.3.is_none()
				}
			})
		}) {
		return Err(SchemaError::mismatch(
			"legacy point source columns differ from published v10 writable shape",
		));
	}
	tx.record_database_operation(
		DatabaseOperationKind::FetchOne,
		VALIDATE_LABEL,
		Some(SOURCE_IDENTITY_SQL),
	);
	let identity = owned_portable_query_scalar::<bool>(SOURCE_IDENTITY_SQL)
		.bind_validated(source_name.as_str())
		.fetch_one(tx.inner.as_mut())
		.await
		.map_err(|error| error.with_operation_label(VALIDATE_LABEL))?;
	if !identity {
		return Err(SchemaError::mismatch(
			"legacy point source requires an immediate built-in text btree key arbiter with C or POSIX collation",
		));
	}
	tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		VALIDATE_LABEL,
		Some(BUILTIN_CHECK_EXPRESSIONS_SQL),
	);
	let checks = owned_portable_query_scalar::<String>(BUILTIN_CHECK_EXPRESSIONS_SQL)
		.bind_validated(source_name.as_str())
		.fetch_all(tx.inner.as_mut())
		.await
		.map_err(|error| error.with_operation_label(VALIDATE_LABEL))?;
	if !checks.iter().any(|check| {
		normalize_builtin_check_expression(check)
			== "(octet_length(key)>0)AND(octet_length(key)<=2048)"
	}) {
		return Err(SchemaError::mismatch(
			"legacy point source requires the published 1..=2048 byte key bound",
		));
	}
	Ok(())
}

/// Fixed backend operation shapes; none accepts consumer SQL or unparsed identifiers.
pub(in crate::records::postgres) enum SourceSnapshotQueryKind<'a> {
	Read,
	ReadHeader,
	LockOrReserve,
	LockOrReserveHeader,
	RelocationLock,
	RelocationHeaderLock,
	RelocationHeaderRead { native: &'a PgQualifiedTableName },
	RelocationRead { native: &'a PgQualifiedTableName },
}

/// Builds one shared source physical projection using fixed text and quoted table identities.
pub(in crate::records::postgres) fn source_snapshot_query(
	source: &PgQualifiedTableName,
	kind: SourceSnapshotQueryKind<'_>,
) -> AuditedSql {
	let mut sql = AuditedSqlBuilder::new();
	let header = matches!(
		kind,
		SourceSnapshotQueryKind::ReadHeader | SourceSnapshotQueryKind::RelocationHeaderRead { .. }
	);
	match &kind {
		SourceSnapshotQueryKind::LockOrReserve | SourceSnapshotQueryKind::LockOrReserveHeader => {
			sql.push_text(
				"WITH locked_existing AS (SELECT key, value, expires_at, updated_at FROM ONLY ",
			);
			sql.push_argument(source);
			sql.push_text(
				" WHERE key OPERATOR(pg_catalog.=) $1 FOR UPDATE), inserted_reservation AS (INSERT INTO ",
			);
			sql.push_argument(source);
			sql.push_text(
				r#" (key, value, expires_at, updated_at)
SELECT $1, ''::pg_catalog.bytea, pg_catalog.statement_timestamp(), pg_catalog.statement_timestamp()
WHERE NOT EXISTS (SELECT 1 FROM locked_existing)
ON CONFLICT (key) DO NOTHING RETURNING key, value, expires_at, updated_at),
source_raw AS (
SELECT key, value, expires_at, updated_at, FALSE AS inserted_reservation FROM locked_existing
UNION ALL
SELECT key, value, expires_at, updated_at, TRUE AS inserted_reservation FROM inserted_reservation
"#,
			);
		}
		_ => {
			sql.push_text("WITH source_raw AS (SELECT key, value, expires_at, updated_at, FALSE AS inserted_reservation FROM ONLY ");
			sql.push_argument(source);
			sql.push_text(" WHERE key OPERATOR(pg_catalog.=) ");
			sql.push_text(match &kind {
				SourceSnapshotQueryKind::RelocationRead { .. }
				| SourceSnapshotQueryKind::RelocationHeaderRead { .. } => "$3",
				_ => "$1",
			});
			if matches!(
				kind,
				SourceSnapshotQueryKind::RelocationLock
					| SourceSnapshotQueryKind::RelocationHeaderLock
			) {
				sql.push_text(" FOR UPDATE");
			}
		}
	}
	append_source_snapshot_projection(&mut sql);
	match kind {
		SourceSnapshotQueryKind::RelocationLock => sql.push_text(
			"SELECT value, expiry_micros, update_micros, expiry_valid, update_valid FROM source_snapshot",
		),
		SourceSnapshotQueryKind::LockOrReserve => sql.push_text(
			"SELECT inserted_reservation, value, expiry_micros, update_micros, expiry_valid, update_valid FROM source_snapshot",
		),
        SourceSnapshotQueryKind::RelocationHeaderLock => sql.push_text(
            "SELECT value IS NOT NULL, expiry_micros, update_micros, expiry_valid, update_valid FROM source_snapshot",
        ),
        SourceSnapshotQueryKind::LockOrReserveHeader => sql.push_text(
            "SELECT inserted_reservation, value IS NOT NULL, expiry_micros, update_micros, expiry_valid, update_valid FROM source_snapshot",
        ),
		SourceSnapshotQueryKind::Read | SourceSnapshotQueryKind::ReadHeader => { sql.push_text(r#"
, observation AS MATERIALIZED (
    SELECT pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.clock_timestamp())
        OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8 AS observed_at
)
SELECT source_snapshot.key IS NOT NULL, "#);
            sql.push_text(if header { "value IS NOT NULL" } else { "value" });
            sql.push_text(r#", expiry_micros, update_micros,
    COALESCE(expiry_valid, FALSE), COALESCE(update_valid, FALSE), observation.observed_at
FROM observation LEFT JOIN source_snapshot ON TRUE
"#); },
		SourceSnapshotQueryKind::RelocationRead { native } | SourceSnapshotQueryKind::RelocationHeaderRead { native } => {
			sql.push_text(", native_snapshot AS (SELECT namespace, body, expires_at, updated_at, index_fields FROM ONLY ");
			sql.push_argument(native);
			sql.push_text(r#"
WHERE namespace OPERATOR(pg_catalog.=) $1 AND key OPERATOR(pg_catalog.=) $2)
SELECT native_snapshot.namespace IS NOT NULL, "#);
            sql.push_text(if header { "native_snapshot.body IS NOT NULL" } else { "native_snapshot.body" });
            sql.push_text(r#",
    native_snapshot.expires_at, native_snapshot.updated_at, native_snapshot.index_fields::pg_catalog.text,
    source_snapshot.key IS NOT NULL, "#);
            sql.push_text(if header { "source_snapshot.value IS NOT NULL" } else { "source_snapshot.value" });
            sql.push_text(r#", source_snapshot.expiry_micros,
    source_snapshot.update_micros, COALESCE(source_snapshot.expiry_valid, FALSE),
    COALESCE(source_snapshot.update_valid, FALSE),
    pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.clock_timestamp())
        OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8
FROM (SELECT 1) observation
LEFT JOIN native_snapshot ON TRUE
LEFT JOIN source_snapshot ON TRUE
"#);
		}
	}
	sql.finish()
}

/// Appends the exact shared timestamp projection after a source_raw CTE body.
pub(crate) fn append_source_snapshot_projection(sql: &mut AuditedSqlBuilder) {
	sql.push_text(
		r#"
), source_epoch AS (
    SELECT key, value, expires_at, inserted_reservation,
        CASE WHEN pg_catalog.isfinite(expires_at)
            THEN EXTRACT(EPOCH FROM expires_at) OPERATOR(pg_catalog.*) 1000000 END AS expiry_epoch,
        CASE WHEN pg_catalog.isfinite(updated_at)
            THEN EXTRACT(EPOCH FROM updated_at) OPERATOR(pg_catalog.*) 1000000 END AS update_epoch
    FROM source_raw
), source_snapshot AS (
    SELECT key, value, inserted_reservation,
        CASE WHEN expiry_epoch OPERATOR(pg_catalog.>=) '-9223372036854775808'::pg_catalog.numeric
               AND expiry_epoch OPERATOR(pg_catalog.<=) '9223372036854775807'::pg_catalog.numeric
            THEN expiry_epoch::pg_catalog.int8 END AS expiry_micros,
        CASE WHEN update_epoch OPERATOR(pg_catalog.>=) '-9223372036854775808'::pg_catalog.numeric
               AND update_epoch OPERATOR(pg_catalog.<=) '9223372036854775807'::pg_catalog.numeric
            THEN update_epoch::pg_catalog.int8 END AS update_micros,
        (expires_at IS NULL OR COALESCE(
            expiry_epoch OPERATOR(pg_catalog.>=) '-9223372036854775808'::pg_catalog.numeric
            AND expiry_epoch OPERATOR(pg_catalog.<=) '9223372036854775807'::pg_catalog.numeric,
            FALSE)) AS expiry_valid,
        COALESCE(
            update_epoch OPERATOR(pg_catalog.>=) '-9223372036854775808'::pg_catalog.numeric
            AND update_epoch OPERATOR(pg_catalog.<=) '9223372036854775807'::pg_catalog.numeric,
            FALSE) AS update_valid
    FROM source_epoch
)
"#,
	);
}

pub(in crate::records::postgres) fn source_delete_query(
	source: &PgQualifiedTableName,
) -> AuditedSql {
	crate::audited_sql!(
		"DELETE FROM ONLY {source} WHERE key OPERATOR(pg_catalog.=) $1",
		source = source
	)
}

/// Default formatting contains no key or value bytes and retains exact cleanup failures.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SourcePointError {
	#[error("source point database work failed")]
	Database(#[from] DatabaseError),
	#[error("source point physical layout is incompatible")]
	Schema(#[from] SchemaError),
	#[error("source point physical value could not be read")]
	Physical(#[source] SourcePhysicalError),
	#[error("source point clock observation failed")]
	Clock(#[from] StateStorageError),
	#[error("source point mutation exhausted its conflict-attempt budget")]
	ConflictBudgetExhausted,
	#[error("source point layout cannot store native projections")]
	ProjectionUnsupported,
	#[error("native timestamp {field:?} does not fit finite PostgreSQL source storage")]
	NativeTimestampUnrepresentable { field: SourceTimestampField },
	#[error("source point lost its acquired mutation row")]
	MutationRowMissing,
	#[error("source metadata update requires an existing physical value")]
	MetadataValueAbsent,
	#[error("source point operation failed and reservation cleanup also failed")]
	CleanupFailed {
		#[source]
		operation_error: Box<Self>,
		cleanup_error: Box<Self>,
	},
}

impl From<SourcePhysicalError> for SourcePointError {
	fn from(error: SourcePhysicalError) -> Self {
		Self::Physical(error)
	}
}

impl SourcePointError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Database(error) => Some(error),
			Self::Schema(error) => error.database_error(),
			Self::Clock(error) => error.database_error(),
			Self::CleanupFailed {
				operation_error,
				cleanup_error,
			} => operation_error
				.database_error()
				.or_else(|| cleanup_error.database_error()),
			_ => None,
		}
	}
	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::Database(_) => true,
			Self::Schema(error) => error.involves_database_failure(),
			Self::Clock(error) => error.database_error().is_some(),
			Self::CleanupFailed {
				operation_error,
				cleanup_error,
			} => {
				operation_error.involves_database_failure()
					|| cleanup_error.involves_database_failure()
			}
			Self::Physical(_)
			| Self::ConflictBudgetExhausted
			| Self::ProjectionUnsupported
			| Self::NativeTimestampUnrepresentable { .. }
			| Self::MutationRowMissing
			| Self::MetadataValueAbsent => false,
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Database(error) => error.commit_outcome_is_unknown(),
			Self::Schema(error) => error.commit_outcome_is_unknown(),
			Self::Clock(error) => error
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
			Self::CleanupFailed {
				operation_error,
				cleanup_error,
			} => {
				operation_error.commit_outcome_is_unknown()
					|| cleanup_error.commit_outcome_is_unknown()
			}
			Self::Physical(_)
			| Self::ConflictBudgetExhausted
			| Self::ProjectionUnsupported
			| Self::NativeTimestampUnrepresentable { .. }
			| Self::MutationRowMissing
			| Self::MetadataValueAbsent => false,
		}
	}
}

/// A catalog-validated source-only relation. It owns no native storage or admission proof.
#[derive(Clone, Debug)]
pub(crate) struct PgSourcePointStorage {
	table: PgQualifiedTableName,
	read: AuditedSql,
	lock: AuditedSql,
	lock_header: AuditedSql,
	write: AuditedSql,
	update_metadata: AuditedSql,
	delete: AuditedSql,
}

type ReadRow = (
	bool,
	Option<Bytea>,
	Option<i64>,
	Option<i64>,
	bool,
	bool,
	i64,
);
type LockRow = (bool, Option<Bytea>, Option<i64>, Option<i64>, bool, bool);

/// Canonical source key lock or owned new expired-row reservation in the exact write Tx.
pub(crate) struct PgSourcePointRecordLock<'a, 'tx, Observation = PointRecordRead> {
	storage: &'a PgSourcePointStorage,
	tx: &'a mut WriteTx<'tx>,
	key: Key,
	snapshot: Observation,
	reservation: ReservationOwnership,
	completion: StateMutationCompletion,
}

impl PgSourcePointStorage {
	/// Internal construction from an existing adapter's exact catalog-validated source relation.
	pub(crate) fn from_validated_table(table: PgQualifiedTableName) -> Self {
		let read = source_snapshot_query(&table, SourceSnapshotQueryKind::Read);
		let lock = source_snapshot_query(&table, SourceSnapshotQueryKind::LockOrReserve);
		let write = crate::audited_sql!(
			r#"
UPDATE ONLY {table} SET value = $2,
    expires_at = (pg_catalog.timestamp '1970-01-01 00:00:00' OPERATOR(pg_catalog.+) $3::pg_catalog.interval) AT TIME ZONE INTERVAL '0 seconds',
    updated_at = (pg_catalog.timestamp '1970-01-01 00:00:00' OPERATOR(pg_catalog.+) $4::pg_catalog.interval) AT TIME ZONE INTERVAL '0 seconds'
WHERE key OPERATOR(pg_catalog.=) $1
"#,
			table = &table
		);
		let update_metadata = crate::audited_sql!(
			r#"
UPDATE ONLY {table} SET
    expires_at = (pg_catalog.timestamp '1970-01-01 00:00:00' OPERATOR(pg_catalog.+) $2::pg_catalog.interval) AT TIME ZONE INTERVAL '0 seconds',
    updated_at = (pg_catalog.timestamp '1970-01-01 00:00:00' OPERATOR(pg_catalog.+) $3::pg_catalog.interval) AT TIME ZONE INTERVAL '0 seconds'
WHERE key OPERATOR(pg_catalog.=) $1
"#,
			table = &table
		);
		let delete = source_delete_query(&table);
		Self {
			table: table.clone(),
			read,
			lock,
			lock_header: source_snapshot_query(
				&table,
				SourceSnapshotQueryKind::LockOrReserveHeader,
			),
			write,
			update_metadata,
			delete,
		}
	}

	pub(crate) fn table(&self) -> &PgQualifiedTableName {
		&self.table
	}

	/// Includes expired physical values and samples one execution-time clock, including absence.
	pub(crate) async fn read(
		&self,
		tx: &mut Tx<'_>,
		key: &Key,
	) -> Result<PointRecordRead, SourcePointError> {
		let mut completion = tx.track_state_mutation();
		tx.record_database_operation(
			DatabaseOperationKind::FetchOne,
			READ_LABEL,
			Some(self.read.as_str()),
		);
		let (exists, body, expiry, updated, expires_at_valid, updated_at_valid, observed_at) =
			owned_portable_query_as::<ReadRow>(self.read.clone())
				.bind_validated(key.as_str())
				.fetch_one(tx.inner.as_mut())
				.await
				.map_err(|error| error.with_operation_label(READ_LABEL))?;
		let record = exists
			.then(|| {
				decode_source_record((body, expiry, updated, expires_at_valid, updated_at_valid))
			})
			.transpose()?
			.map(SourcePhysicalRecord::into_point);
		let snapshot = PointRecordRead::new(record, StateTimestampMicros::new(observed_at));
		completion.complete();
		Ok(snapshot)
	}

	/// Uses the published expired empty-byte placeholder convention and exact INSERT ownership.
	pub(crate) async fn lock<'a, 'tx>(
		&'a self,
		tx: &'a mut WriteTx<'tx>,
		key: &Key,
		conflict_attempts: NonZeroU32,
	) -> Result<PgSourcePointRecordLock<'a, 'tx>, SourcePointError> {
		let completion = tx.track_state_mutation();
		for _ in 0..conflict_attempts.get() {
			tx.record_database_operation(
				DatabaseOperationKind::FetchOptional,
				LOCK_LABEL,
				Some(self.lock.as_str()),
			);
			let row = owned_portable_query_as::<LockRow>(self.lock.clone())
				.bind_validated(key.as_str())
				.fetch_optional(tx.inner.as_mut())
				.await
				.map_err(|error| error.with_operation_label(LOCK_LABEL))?;
			let Some((
				has_inserted_reservation,
				body,
				expiry,
				updated,
				expires_at_valid,
				updated_at_valid,
			)) = row
			else {
				continue;
			};
			let physical =
				decode_source_record((body, expiry, updated, expires_at_valid, updated_at_valid))?;
			let reservation = ReservationOwnership::from_inserted(has_inserted_reservation);
			let record = match reservation {
				ReservationOwnership::Preexisting => Some(physical.into_point()),
				ReservationOwnership::Inserted => None,
			};
			let observed_at = PgPointStorage::observe_time(tx).await?;
			return Ok(PgSourcePointRecordLock {
				storage: self,
				tx,
				key: key.clone(),
				snapshot: PointRecordRead::new(record, observed_at),
				reservation,
				completion,
			});
		}
		Err(SourcePointError::ConflictBudgetExhausted)
	}
}

pub(crate) type PgSourcePointRecordHeaderLock<'a, 'tx> =
	PgSourcePointRecordLock<'a, 'tx, RecordHeaderObservation>;

impl PgSourcePointStorage {
	pub(crate) async fn lock_header<'a, 'tx>(
		&'a self,
		tx: &'a mut WriteTx<'tx>,
		key: &Key,
		conflict_attempts: NonZeroU32,
	) -> Result<PgSourcePointRecordHeaderLock<'a, 'tx>, SourcePointError> {
		let completion = tx.track_state_mutation();
		for _ in 0..conflict_attempts.get() {
			tx.record_database_operation(
				DatabaseOperationKind::FetchOptional,
				LOCK_LABEL,
				Some(self.lock_header.as_str()),
			);
			let row =
				owned_portable_query_as::<(bool, bool, Option<i64>, Option<i64>, bool, bool)>(
					self.lock_header.clone(),
				)
				.bind_validated(key.as_str())
				.fetch_optional(tx.inner.as_mut())
				.await
				.map_err(|error| error.with_operation_label(LOCK_LABEL))?;
			let Some((
				has_inserted_reservation,
				body_present,
				expires_at,
				updated_at,
				expires_at_valid,
				updated_at_valid,
			)) = row
			else {
				continue;
			};
			let metadata = decode_source_header(SourcePhysicalHeaderFields {
				body_present,
				expires_at,
				updated_at,
				expires_at_valid,
				updated_at_valid,
			})?;
			let reservation = ReservationOwnership::from_inserted(has_inserted_reservation);
			let metadata = match reservation {
				ReservationOwnership::Preexisting => Some(metadata),
				ReservationOwnership::Inserted => None,
			};
			let observed_at = PgPointStorage::observe_time(tx).await?;
			return Ok(PgSourcePointRecordLock {
				storage: self,
				tx,
				key: key.clone(),
				snapshot: RecordHeaderObservation::new(metadata, observed_at),
				reservation,
				completion,
			});
		}
		Err(SourcePointError::ConflictBudgetExhausted)
	}
}
impl PgSourcePointRecordHeaderLock<'_, '_> {
	pub(crate) fn header(&self) -> RecordHeaderObservation {
		self.snapshot
	}
	pub(crate) async fn delete(self) -> Result<bool, SourcePointError> {
		let existed = self.snapshot.metadata().is_some();
		self.delete_value().await?;
		Ok(existed)
	}
}

impl<Observation> PgSourcePointRecordLock<'_, '_, Observation> {
	pub(in crate::records) async fn update_value_metadata(
		mut self,
		update: EncodedMetadataUpdate,
	) -> Result<(), SourcePointError>
	where
		Observation: PointGuardSnapshot,
	{
		if !update.index_fields().is_empty() {
			return Err(self.reject(SourcePointError::ProjectionUnsupported).await);
		}
		if !self.snapshot.holds_value() {
			return Err(self.reject(SourcePointError::MetadataValueAbsent).await);
		}
		let timestamps = match SourceWriteTimestamps::parse(update.metadata()) {
			Ok(timestamps) => timestamps,
			Err(error) => return Err(self.reject(error).await),
		};
		self.tx.record_database_operation(
			DatabaseOperationKind::Execute,
			UPDATE_METADATA_LABEL,
			Some(self.storage.update_metadata.as_str()),
		);
		let affected = owned_portable_query(self.storage.update_metadata.clone())
			.bind_validated(self.key.as_str())
			.bind_validated(timestamps.expires_at.as_deref())
			.bind_validated(timestamps.updated_at.as_str())
			.execute(self.tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(UPDATE_METADATA_LABEL))?
			.rows_affected();
		if affected != 1 {
			return Err(SourcePointError::MutationRowMissing);
		}
		self.completion.complete();
		Ok(())
	}

	pub(in crate::records) async fn write_value(
		mut self,
		record: EncodedRecord,
	) -> Result<(), SourcePointError> {
		if !record.record().index_fields().is_empty() {
			return Err(self.reject(SourcePointError::ProjectionUnsupported).await);
		}
		let timestamps = match SourceWriteTimestamps::parse(record.record().metadata()) {
			Ok(timestamps) => timestamps,
			Err(error) => return Err(self.reject(error).await),
		};
		self.tx.record_database_operation(
			DatabaseOperationKind::Execute,
			WRITE_LABEL,
			Some(self.storage.write.as_str()),
		);
		let affected = owned_portable_query(self.storage.write.clone())
			.bind_validated(self.key.as_str())
			.bind_validated(record.record().body())
			.bind_validated(timestamps.expires_at.as_deref())
			.bind_validated(timestamps.updated_at.as_str())
			.execute(self.tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(WRITE_LABEL))?
			.rows_affected();
		if affected != 1 {
			return Err(SourcePointError::MutationRowMissing);
		}
		self.completion.complete();
		Ok(())
	}
	pub(in crate::records) async fn keep_value(mut self) -> Result<(), SourcePointError> {
		if matches!(self.reservation, ReservationOwnership::Inserted) {
			self.delete_physical().await?;
		}
		self.completion.complete();
		Ok(())
	}
	pub(in crate::records) async fn delete_value(mut self) -> Result<(), SourcePointError> {
		self.delete_physical().await?;
		self.completion.complete();
		Ok(())
	}

	async fn delete_physical(&mut self) -> Result<(), SourcePointError> {
		self.tx.record_database_operation(
			DatabaseOperationKind::Execute,
			DELETE_LABEL,
			Some(self.storage.delete.as_str()),
		);
		let affected = owned_portable_query(self.storage.delete.clone())
			.bind_validated(self.key.as_str())
			.execute(self.tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(DELETE_LABEL))?
			.rows_affected();
		if affected != 1 {
			return Err(SourcePointError::MutationRowMissing);
		}
		Ok(())
	}
	async fn reject(self, operation_error: SourcePointError) -> SourcePointError {
		match self.keep_value().await {
			Ok(()) => operation_error,
			Err(cleanup_error) => SourcePointError::CleanupFailed {
				operation_error: Box::new(operation_error),
				cleanup_error: Box::new(cleanup_error),
			},
		}
	}
}

impl PointRecordLock for PgSourcePointRecordLock<'_, '_> {
	type Error = SourcePointError;
	fn snapshot(&self) -> &PointRecordRead {
		&self.snapshot
	}
	async fn observe_time(&mut self) -> Result<StateTimestampMicros, Self::Error> {
		let mut completion = self.tx.track_state_mutation();
		let observed_at = PgPointStorage::observe_time(self.tx).await?;
		completion.complete();
		Ok(observed_at)
	}
	async fn write(self, record: EncodedRecord) -> Result<(), Self::Error> {
		self.write_value(record).await
	}
	async fn update_metadata(self, update: EncodedMetadataUpdate) -> Result<(), Self::Error> {
		self.update_value_metadata(update).await
	}

	async fn delete(self) -> Result<bool, Self::Error> {
		let existed = self.snapshot.record().is_some();
		self.delete_value().await?;
		Ok(existed)
	}
	async fn keep(self) -> Result<(), Self::Error> {
		self.keep_value().await
	}
}

pub(crate) struct SourceWriteTimestamps {
	expires_at: Option<String>,
	updated_at: String,
}

impl SourceWriteTimestamps {
	pub(crate) fn expires_at(&self) -> Option<&str> {
		self.expires_at.as_deref()
	}
	pub(crate) fn updated_at(&self) -> &str {
		&self.updated_at
	}

	pub(crate) fn parse(metadata: RecordMetadata) -> Result<Self, SourcePointError> {
		Ok(Self {
			expires_at: metadata
				.expires_at
				.map(|timestamp| source_interval(timestamp, SourceTimestampField::ExpiresAt))
				.transpose()?,
			updated_at: source_interval(metadata.updated_at, SourceTimestampField::UpdatedAt)?,
		})
	}
}

/// Integral chunks fit the source interval parser and explicitly sign every component.
/// Timestamp-without-zone addition followed by UTC avoids session timezone/daylight rules.
fn source_interval(
	timestamp: StateTimestampMicros,
	field: SourceTimestampField,
) -> Result<String, SourcePointError> {
	const PG_EPOCH_UNIX_MICROS: i128 = 946_684_800_000_000;
	const PG_MIN_TIMESTAMP: i128 = -211_813_488_000_000_000;
	const PG_END_TIMESTAMP: i128 = 9_223_371_331_200_000_000;
	const MICROS_PER_DAY: i64 = 86_400_000_000;
	let micros = timestamp.as_i64();
	let pg_micros = i128::from(micros) - PG_EPOCH_UNIX_MICROS;
	if !(PG_MIN_TIMESTAMP..PG_END_TIMESTAMP).contains(&pg_micros) {
		return Err(SourcePointError::NativeTimestampUnrepresentable { field });
	}
	let days = micros / MICROS_PER_DAY;
	let remainder = micros % MICROS_PER_DAY;
	let seconds = remainder / 1_000_000;
	let subsecond_micros = remainder % 1_000_000;
	Ok(format!(
		"{days:+} days {seconds:+} seconds {subsecond_micros:+} microseconds"
	))
}

/// Canonical native point authority paired with an external v10 source import on one transaction.
///
/// The guard tracks completion across native acquisition, companion v10 source row locking, and
/// final native write/metadata/keep plus v10 source row retirement. Dropping, cancelling, or
/// failing before a terminal method completes leaves the transaction's mutation tracker unresolved.
pub(crate) struct PgExternalSourcePointLock<'a, 'tx> {
	storage: &'a PgPointStorage,
	tx: &'a mut WriteTx<'tx>,
	identity: RecordIdentity,
	reservation: ReservationOwnership,
	completion: StateMutationCompletion,
}

impl PgPointStorage {
	/// Locks or reserves `identity` before locking external v10 source rows on the same transaction.
	pub(crate) async fn lock_for_external_source_import<'a, 'tx>(
		&'a self,
		tx: &'a mut WriteTx<'tx>,
		identity: RecordIdentity,
		conflict_attempts: NonZeroU32,
	) -> Result<PgExternalSourcePointLock<'a, 'tx>, StateStorageError> {
		let completion = tx.track_state_mutation();
		let (_, reservation) = self
			.acquire_identity(tx, &identity, conflict_attempts)
			.await?
			.into_point();
		Ok(PgExternalSourcePointLock {
			storage: self,
			tx,
			identity,
			reservation,
			completion,
		})
	}
}

pub(crate) struct PgWrittenExternalSourcePoint<'a, 'tx> {
	tx: &'a mut WriteTx<'tx>,
	completion: StateMutationCompletion,
}

impl<'tx> PgWrittenExternalSourcePoint<'_, 'tx> {
	pub(crate) fn source_tx(&mut self) -> &mut WriteTx<'tx> {
		self.tx
	}

	pub(crate) fn finish(mut self) {
		self.completion.complete();
	}
}

impl<'a, 'tx> PgExternalSourcePointLock<'a, 'tx> {
	pub(crate) fn source_tx(&mut self) -> &mut WriteTx<'tx> {
		self.tx
	}

	pub(crate) async fn observe_time(&mut self) -> Result<StateTimestampMicros, StateStorageError> {
		let mut completion = self.tx.track_state_mutation();
		let observed_at = PgPointStorage::observe_time(self.tx).await?;
		completion.complete();
		Ok(observed_at)
	}

	pub(crate) async fn keep(mut self) -> Result<(), StateStorageError> {
		if matches!(self.reservation, ReservationOwnership::Inserted) {
			self.storage
				.delete_record(self.tx, &self.identity, true)
				.await?;
		}
		self.completion.complete();
		Ok(())
	}

	pub(crate) async fn write_native_only(
		mut self,
		encoded: EncodedRecord,
	) -> Result<(), StateStorageError> {
		let indexes = super::super::encode_index_fields(encoded.record().index_fields());
		self.storage
			.write_record(self.tx, &self.identity, &encoded, &indexes)
			.await?;
		self.completion.complete();
		Ok(())
	}

	pub(crate) async fn write_native_before_source_retirement(
		self,
		encoded: EncodedRecord,
	) -> Result<PgWrittenExternalSourcePoint<'a, 'tx>, StateStorageError> {
		let indexes = super::super::encode_index_fields(encoded.record().index_fields());
		self.storage
			.write_record(self.tx, &self.identity, &encoded, &indexes)
			.await?;
		Ok(PgWrittenExternalSourcePoint {
			tx: self.tx,
			completion: self.completion,
		})
	}

	pub(crate) async fn update_metadata(
		mut self,
		update: EncodedMetadataUpdate,
	) -> Result<(), StateStorageError> {
		self.storage
			.update_record_metadata(self.tx, &self.identity, &update)
			.await?;
		self.completion.complete();
		Ok(())
	}
}
