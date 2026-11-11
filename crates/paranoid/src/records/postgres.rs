//! PostgreSQL record storage for the native state foundation.
//!
//! Callers own installation admission and the decision to create a fresh container or
//! validate an existing one. This adapter supplies no runtime or legacy protocol admission.

mod acquisition;
pub(crate) use acquisition::{HeldNativeRow, ReservationOwnership};
pub(crate) mod attempt_boundary;
pub(crate) mod group;
pub(crate) mod group_attempt;
mod point_header;
pub(crate) mod projection;
mod record_batch_write;
pub(crate) mod record_many;
pub(crate) mod record_set;
pub(crate) mod reservation_cleanup;
pub(crate) mod scan;
pub(crate) mod v10;

use std::collections::BTreeSet;
use std::num::NonZeroU32;

use crate::records::{
	EncodedMetadataUpdate, EncodedRecord, GroupMutationError, IndexFields, PointRecordLock,
	PointRecordRead, RecordGroupRead, RecordHeaderObservation, RecordIdentity, RecordMetadata,
	RecordMutation, RecordMutationGroup, StateTimestampMicros, StoredRecord,
};

pub(crate) use crate::db::StateMutationCompletion;
use crate::db::bytea::Bytea;
use crate::db::catalog::{BUILTIN_CHECK_EXPRESSIONS_SQL, normalize_builtin_check_expression};
use crate::db::{
	AuditedSql, DatabaseError, DatabaseOperationKind, PG_COLUMN_METADATA_SQL, PgQualifiedTableName,
	SchemaError, Tx, WriteTx, builtin_btree_index_conditions, owned_portable_query,
	owned_portable_query_as, owned_portable_query_scalar,
};
use group::PreparedPgRecordGroupMutation;
use projection::decode_index_fields;
pub(crate) use projection::encode_index_fields;

const CREATE_FRESH_LABEL: &str = "db.state_storage.create_fresh";
const VALIDATE_TABLE_LABEL: &str = "db.state_storage.validate_table";
const VALIDATE_COLUMNS_LABEL: &str = "db.state_storage.validate_columns";
const VALIDATE_IDENTITY_LABEL: &str = "db.state_storage.validate_identity";
const VALIDATE_CHECKS_LABEL: &str = "db.state_storage.validate_checks";
const READ_POINT_LABEL: &str = "db.state_storage.read_point";
const READ_LOCKED_POINT_LABEL: &str = "db.state_storage.read_locked_point";
const LOCK_POINT_LABEL: &str = "db.state_storage.lock_point";
const RESERVE_POINT_LABEL: &str = "db.state_storage.reserve_point";
const OBSERVE_TIME_LABEL: &str = "db.state_storage.observe_time";
const WRITE_POINT_LABEL: &str = "db.state_storage.write_point";
const UPDATE_METADATA_LABEL: &str = "db.state_storage.update_metadata";
const DELETE_POINT_LABEL: &str = "db.state_storage.delete_point";
const CLEANUP_RESERVATION_LABEL: &str = "db.state_storage.cleanup_reservation";

const OBSERVE_TIME_SQL: &str = r#"
SELECT pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.clock_timestamp())
    OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8
"#;

const TABLE_SHAPE_SQL: &str = r#"
SELECT relation.relkind::pg_catalog.text, relation.relpersistence::pg_catalog.text,
    relation.relrowsecurity, relation.relforcerowsecurity, relation.relhasrules,
    EXISTS (
        SELECT 1 FROM pg_catalog.pg_inherits inheritance
        WHERE inheritance.inhrelid OPERATOR(pg_catalog.=) relation.oid
           OR inheritance.inhparent OPERATOR(pg_catalog.=) relation.oid
    ),
    EXISTS (
        SELECT 1 FROM pg_catalog.pg_trigger trigger
        WHERE trigger.tgrelid OPERATOR(pg_catalog.=) relation.oid
          AND trigger.tgenabled OPERATOR(pg_catalog.<>) 'D'
    ) OR EXISTS (
        SELECT 1 FROM pg_catalog.pg_constraint constraint_record
        WHERE constraint_record.contype OPERATOR(pg_catalog.=) 'f'
          AND (constraint_record.conrelid OPERATOR(pg_catalog.=) relation.oid
            OR constraint_record.confrelid OPERATOR(pg_catalog.=) relation.oid)
    )
FROM pg_catalog.pg_class relation
WHERE relation.oid OPERATOR(pg_catalog.=) pg_catalog.to_regclass($1)
"#;

const IDENTITY_INDEX_SQL: &str = concat!(
	r#"
SELECT EXISTS (
    SELECT 1 FROM pg_catalog.pg_index idx
    JOIN pg_catalog.pg_class index_class
      ON index_class.oid OPERATOR(pg_catalog.=) idx.indexrelid
    JOIN pg_catalog.pg_attribute namespace_attr
      ON namespace_attr.attrelid OPERATOR(pg_catalog.=) idx.indrelid
     AND namespace_attr.attname OPERATOR(pg_catalog.=) 'namespace'
     AND NOT namespace_attr.attisdropped
    JOIN pg_catalog.pg_attribute key_attr
      ON key_attr.attrelid OPERATOR(pg_catalog.=) idx.indrelid
     AND key_attr.attname OPERATOR(pg_catalog.=) 'key'
     AND NOT key_attr.attisdropped
    JOIN pg_catalog.pg_opclass namespace_ops
      ON namespace_ops.oid OPERATOR(pg_catalog.=) idx.indclass[0]
    JOIN pg_catalog.pg_opclass key_ops
      ON key_ops.oid OPERATOR(pg_catalog.=) idx.indclass[1]
    WHERE idx.indrelid OPERATOR(pg_catalog.=) pg_catalog.to_regclass($1)
      AND idx.indisunique AND idx.indimmediate
      AND idx.indnkeyatts OPERATOR(pg_catalog.=) 2
      AND idx.indpred IS NULL AND idx.indexprs IS NULL
      AND idx.indkey[0] OPERATOR(pg_catalog.=) namespace_attr.attnum
      AND idx.indkey[1] OPERATOR(pg_catalog.=) key_attr.attnum
      AND idx.indoption[0] OPERATOR(pg_catalog.=) 0
      AND idx.indoption[1] OPERATOR(pg_catalog.=) 0
      AND idx.indcollation[0] OPERATOR(pg_catalog.=) 0
      AND idx.indcollation[1] OPERATOR(pg_catalog.=) 0
      AND namespace_ops.opcnamespace OPERATOR(pg_catalog.=) 'pg_catalog'::pg_catalog.regnamespace
      AND key_ops.opcnamespace OPERATOR(pg_catalog.=) 'pg_catalog'::pg_catalog.regnamespace
      AND namespace_ops.opcname OPERATOR(pg_catalog.=) 'bytea_ops'
      AND key_ops.opcname OPERATOR(pg_catalog.=) 'bytea_ops'
      AND namespace_ops.opcmethod OPERATOR(pg_catalog.=) 403
      AND key_ops.opcmethod OPERATOR(pg_catalog.=) 403
"#,
	builtin_btree_index_conditions!(),
	")"
);

/// A point-storage failure. Default formatting exposes no body, projection, or key bytes.
/// Typed identities retain explicit accessors for deliberate inspection and recovery.
#[derive(Debug, thiserror::Error)]
pub(crate) enum StateStorageError {
	#[error("native record database work failed")]
	Database(#[from] DatabaseError),
	#[error("native record mutation exhausted its conflict-attempt budget")]
	ConflictBudgetExhausted,
	#[error("native record {identity:?} is invalid: {reason}")]
	InvalidRecord {
		identity: RecordIdentity,
		reason: &'static str,
	},
	#[error("native record {identity:?} lost its acquired mutation row")]
	MutationRowMissing { identity: RecordIdentity },
	#[error("native record {identity:?} has no value for a metadata update")]
	MetadataValueAbsent { identity: RecordIdentity },
	#[error("native batch write returned missing, repeated, or unexpected identities")]
	BatchWriteCoverage,
}

impl StateStorageError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Database(error) => Some(error),
			Self::ConflictBudgetExhausted
			| Self::InvalidRecord { .. }
			| Self::MutationRowMissing { .. }
			| Self::BatchWriteCoverage
			| Self::MetadataValueAbsent { .. } => None,
		}
	}
}

/// A physical container binding. Admission and physical readiness belong to its caller.
#[derive(Clone, Debug)]
pub(crate) struct PgPointStorage {
	table: PgQualifiedTableName,
	queries: PointQueries,
}

#[derive(Clone, Debug)]
struct PointQueries {
	read: AuditedSql,
	read_locked: AuditedSql,
	read_header: AuditedSql,
	lock: AuditedSql,
	lock_header: AuditedSql,
	reserve: AuditedSql,
	write: AuditedSql,
	update_metadata: AuditedSql,
	delete: AuditedSql,
	cleanup: AuditedSql,
}

/// A row lock and optional reservation owned by one borrowed write transaction.
///
/// Successful `write`, `delete`, or `keep` completes the tracked mutation. Primitive
/// rejection paths use `keep` to resolve their reservation without requiring rollback.
/// Dropping this guard or abandoning acquisition/completion requires transaction rollback;
/// the transaction must not send a commit. No destructor executes asynchronous SQL.
/// Existing migration reservations are preserved by `keep`.
pub(crate) struct PgPointRecordLock<'a, 'tx, Observation = PointRecordRead> {
	storage: &'a PgPointStorage,
	tx: &'a mut WriteTx<'tx>,
	identity: RecordIdentity,
	snapshot: Observation,
	reservation: ReservationOwnership,
	completion: StateMutationCompletion,
}

pub(crate) type PgPointRecordHeaderLock<'a, 'tx> =
	PgPointRecordLock<'a, 'tx, RecordHeaderObservation>;

/// All canonical row locks retained by one borrowed write transaction and one tracker.
/// The tracker remains unfinished until every action and reservation cleanup succeeds.
/// The snapshot records which reservation rows the acquisition inserted.
pub(crate) struct PgRecordMutationGroup<'a, 'tx> {
	storage: &'a PgPointStorage,
	tx: &'a mut WriteTx<'tx>,
	snapshot: RecordGroupRead,
	completion: StateMutationCompletion,
}

type PhysicalPointRow = (Option<Bytea>, Option<i64>, Option<i64>, String);

impl PgPointStorage {
	pub(crate) fn table(&self) -> &PgQualifiedTableName {
		&self.table
	}

	pub(crate) fn shares_container(&self, other: &Self) -> bool {
		self.table == other.table
	}

	pub(crate) fn new(table: PgQualifiedTableName) -> Self {
		let queries = PointQueries::new(&table);
		Self { table, queries }
	}

	/// Creates a new container. An existing relation is an error.
	pub(crate) async fn create_fresh(&self, tx: &mut WriteTx<'_>) -> Result<(), SchemaError> {
		let statement = crate::audited_sql!(
			r#"
CREATE TABLE {table} (
    namespace pg_catalog.bytea NOT NULL,
    key pg_catalog.bytea NOT NULL,
    body pg_catalog.bytea,
    expires_at pg_catalog.int8,
    updated_at pg_catalog.int8,
    index_fields pg_catalog.jsonb NOT NULL,
    PRIMARY KEY (namespace, key),
    CHECK (pg_catalog.octet_length(namespace) OPERATOR(pg_catalog.>) 0),
    CHECK (pg_catalog.jsonb_typeof(index_fields) OPERATOR(pg_catalog.=) 'object'::pg_catalog.text),
    CHECK ((body IS NULL) OPERATOR(pg_catalog.=) (updated_at IS NULL)),
    CHECK ((body IS NOT NULL) OR ((expires_at IS NULL)
        AND (index_fields OPERATOR(pg_catalog.=) '{{}}'::pg_catalog.jsonb)))
)
"#,
			table = &self.table,
		);
		tx.record_database_operation(
			DatabaseOperationKind::Execute,
			CREATE_FRESH_LABEL,
			Some(statement.as_str()),
		);
		owned_portable_query(statement)
			.execute(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(CREATE_FRESH_LABEL))?;
		self.validate_existing(tx).await
	}

	/// Checks the actual container without creating or repairing a recorded relation.
	pub(crate) async fn validate_existing(&self, tx: &mut Tx<'_>) -> Result<(), SchemaError> {
		let table_name = self.table.quoted().to_string();
		tx.record_database_operation(
			DatabaseOperationKind::FetchOptional,
			VALIDATE_TABLE_LABEL,
			Some(TABLE_SHAPE_SQL),
		);
		let shape = owned_portable_query_as::<(String, String, bool, bool, bool, bool, bool)>(
			TABLE_SHAPE_SQL,
		)
		.bind_validated(table_name.as_str())
		.fetch_optional(tx.inner.as_mut())
		.await
		.map_err(|error| error.with_operation_label(VALIDATE_TABLE_LABEL))?;
		if !matches!(shape, Some((ref kind, ref persistence, false, false, false, false, false)) if kind == "r" && persistence == "p")
		{
			return Err(SchemaError::mismatch(
				"native record container must be an existing permanent heap without row policies, inheritance, rules, enabled triggers, or foreign-key associations",
			));
		}
		tx.record_database_operation(
			DatabaseOperationKind::FetchAll,
			VALIDATE_COLUMNS_LABEL,
			Some(PG_COLUMN_METADATA_SQL),
		);
		let columns = owned_portable_query_as::<(String, String, bool, Option<String>, bool)>(
			PG_COLUMN_METADATA_SQL,
		)
		.bind_validated(table_name.as_str())
		.fetch_all(tx.inner.as_mut())
		.await
		.map_err(|error| error.with_operation_label(VALIDATE_COLUMNS_LABEL))?;
		let expected = [
			("namespace", "bytea", true),
			("key", "bytea", true),
			("body", "bytea", false),
			("expires_at", "bigint", false),
			("updated_at", "bigint", false),
			("index_fields", "jsonb", true),
		];
		if columns.len() != expected.len()
			|| expected.iter().any(|(name, data_type, not_null)| {
				!columns.iter().any(|actual| {
					actual.0 == *name
						&& actual.1 == *data_type
						&& actual.2 == *not_null
						&& actual.3.is_none()
						&& actual.4
				})
			}) {
			return Err(SchemaError::mismatch(
				"native record container columns differ from its fixed writable shape",
			));
		}
		tx.record_database_operation(
			DatabaseOperationKind::FetchOne,
			VALIDATE_IDENTITY_LABEL,
			Some(IDENTITY_INDEX_SQL),
		);
		let identity_index = owned_portable_query_scalar::<bool>(IDENTITY_INDEX_SQL)
			.bind_validated(table_name.as_str())
			.fetch_one(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(VALIDATE_IDENTITY_LABEL))?;
		if !identity_index {
			return Err(SchemaError::mismatch(
				"native record container requires an immediate built-in bytea btree identity arbiter over namespace and key",
			));
		}
		tx.record_database_operation(
			DatabaseOperationKind::FetchAll,
			VALIDATE_CHECKS_LABEL,
			Some(BUILTIN_CHECK_EXPRESSIONS_SQL),
		);
		let checks = owned_portable_query_scalar::<String>(BUILTIN_CHECK_EXPRESSIONS_SQL)
			.bind_validated(table_name.as_str())
			.fetch_all(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(VALIDATE_CHECKS_LABEL))?;
		let checks = checks
			.iter()
			.map(|expression| {
				normalize_builtin_check_expression(expression)
					.replace("pg_catalog.jsonb_typeof(", "jsonb_typeof(")
					.replace("::pg_catalog.jsonb", "::jsonb")
			})
			.collect::<Vec<_>>();
		for required in [
			"octet_length(namespace)>0",
			"jsonb_typeof(index_fields)='object'::text",
			"(bodyISNULL)=(updated_atISNULL)",
			"(bodyISNOTNULL)OR((expires_atISNULL)AND(index_fields='{}'::jsonb))",
		] {
			if !checks.iter().any(|check| check == required) {
				return Err(SchemaError::mismatch(format!(
					"native record container lacks required built-in check {required}"
				)));
			}
		}
		Ok(())
	}

	/// Reads one committed snapshot without reserving or converting its identity.
	pub(crate) async fn read(
		&self,
		tx: &mut Tx<'_>,
		identity: &RecordIdentity,
	) -> Result<PointRecordRead, StateStorageError> {
		self.read_with_query(tx, identity, &self.queries.read, READ_POINT_LABEL)
			.await
	}

	/// Retains a shared lock on an existing row and samples time after lock acquisition.
	/// Absence is an observation, not exclusion of a later insert.
	pub(crate) async fn read_locked(
		&self,
		tx: &mut WriteTx<'_>,
		identity: &RecordIdentity,
	) -> Result<PointRecordRead, StateStorageError> {
		self.read_with_query(
			tx,
			identity,
			&self.queries.read_locked,
			READ_LOCKED_POINT_LABEL,
		)
		.await
	}

	async fn read_with_query(
		&self,
		tx: &mut Tx<'_>,
		identity: &RecordIdentity,
		query: &AuditedSql,
		label: &'static str,
	) -> Result<PointRecordRead, StateStorageError> {
		let mut completion = tx.track_state_mutation();
		tx.record_database_operation(DatabaseOperationKind::FetchOne, label, Some(query.as_str()));
		let (body, expires_at, updated_at, index_fields, observed_at, exists) =
			owned_portable_query_as::<(
				Option<Bytea>,
				Option<i64>,
				Option<i64>,
				Option<String>,
				i64,
				bool,
			)>(query.clone())
			.bind_validated(identity.namespace().as_bytes())
			.bind_validated(identity.key().as_bytes())
			.fetch_one(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(label))?;
		let record = if exists {
			let index_fields =
				index_fields.ok_or_else(|| invalid_record(identity, "index map is null"))?;
			decode_physical_record(identity, (body, expires_at, updated_at, index_fields))?
		} else {
			None
		};
		let snapshot = PointRecordRead::new(record, StateTimestampMicros::new(observed_at));
		completion.complete();
		Ok(snapshot)
	}

	/// Locks an existing identity or inserts an absent reservation, then observes time.
	pub(crate) async fn lock<'a, 'tx>(
		&'a self,
		tx: &'a mut WriteTx<'tx>,
		identity: &RecordIdentity,
		conflict_attempts: NonZeroU32,
	) -> Result<PgPointRecordLock<'a, 'tx>, StateStorageError> {
		let completion = tx.track_state_mutation();
		let (record, reservation) = self
			.acquire_identity(tx, identity, conflict_attempts)
			.await?
			.into_point();
		let observed_at = Self::observe_time(tx).await?;
		Ok(PgPointRecordLock {
			storage: self,
			tx,
			identity: identity.clone(),
			snapshot: PointRecordRead::new(record, observed_at),
			reservation,
			completion,
		})
	}

	/// Acquires and validates a physical header without transferring the stored body.
	pub(crate) async fn lock_header<'a, 'tx>(
		&'a self,
		tx: &'a mut WriteTx<'tx>,
		identity: &RecordIdentity,
		conflict_attempts: NonZeroU32,
	) -> Result<PgPointRecordHeaderLock<'a, 'tx>, StateStorageError> {
		let completion = tx.track_state_mutation();
		let acquired = self
			.acquire_projected_identity(acquisition::NativeRecordAcquisitionFields {
				tx,
				identity,
				conflict_attempts,
				projection: acquisition::NativeRecordProjection::Header,
			})
			.await?;
		let acquisition::AcquiredNativeRecord::Header(row) = acquired else {
			unreachable!("header acquisition returns a physical header")
		};
		let (header, reservation) = row.into_point();
		let observed_at = Self::observe_time(tx).await?;
		Ok(PgPointRecordLock {
			storage: self,
			tx,
			identity: identity.clone(),
			snapshot: RecordHeaderObservation::new(header.map(|h| h.metadata), observed_at),
			reservation,
			completion,
		})
	}

	/// Acquires the complete identity set in bytewise namespace/key order.
	/// Every member shares one time observed after all locks and reservation waits.
	/// The conflict budget bounds retries per identity; it does not bound lock wait time.
	pub(crate) async fn lock_group<'a, 'tx>(
		&'a self,
		tx: &'a mut WriteTx<'tx>,
		identities: Vec<RecordIdentity>,
		conflict_attempts: NonZeroU32,
	) -> Result<PgRecordMutationGroup<'a, 'tx>, StateStorageError> {
		let identities: BTreeSet<_> = identities.into_iter().collect();
		let completion = tx.track_state_mutation();
		let acquired = self
			.acquire_group_identities(tx, identities, conflict_attempts)
			.await?;
		let observed_at = Self::observe_time(tx).await?;
		Ok(PgRecordMutationGroup {
			storage: self,
			tx,
			snapshot: RecordGroupRead::new(acquired.records, observed_at),
			completion,
		})
	}

	pub(crate) async fn observe_time(
		tx: &mut WriteTx<'_>,
	) -> Result<StateTimestampMicros, StateStorageError> {
		tx.record_database_operation(
			DatabaseOperationKind::FetchOne,
			OBSERVE_TIME_LABEL,
			Some(OBSERVE_TIME_SQL),
		);
		let observed_at = owned_portable_query_scalar::<i64>(OBSERVE_TIME_SQL)
			.fetch_one(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(OBSERVE_TIME_LABEL))?;
		Ok(StateTimestampMicros::new(observed_at))
	}

	/// Internal aggregate callers retain canonical authority and completion tracking.
	pub(in crate::records) async fn write_record(
		&self,
		tx: &mut WriteTx<'_>,
		identity: &RecordIdentity,
		encoded: &EncodedRecord,
		index_fields: &str,
	) -> Result<(), StateStorageError> {
		let record = encoded.record();
		let metadata = record.metadata();
		tx.record_database_operation(
			DatabaseOperationKind::Execute,
			WRITE_POINT_LABEL,
			Some(self.queries.write.as_str()),
		);
		let affected = owned_portable_query(self.queries.write.clone())
			.bind_validated(identity.namespace().as_bytes())
			.bind_validated(identity.key().as_bytes())
			.bind_validated(record.body())
			.bind_validated(metadata.expires_at.map(StateTimestampMicros::as_i64))
			.bind_validated(metadata.updated_at.as_i64())
			.bind_validated(index_fields)
			.execute(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(WRITE_POINT_LABEL))?
			.rows_affected();
		Self::require_mutation_row(identity, affected)
	}

	/// Both point and aggregate callers retain canonical authority and completion tracking.
	pub(in crate::records) async fn update_record_metadata(
		&self,
		tx: &mut WriteTx<'_>,
		identity: &RecordIdentity,
		update: &EncodedMetadataUpdate,
	) -> Result<(), StateStorageError> {
		let metadata = update.metadata();
		let index_fields = encode_index_fields(update.index_fields());
		tx.record_database_operation(
			DatabaseOperationKind::Execute,
			UPDATE_METADATA_LABEL,
			Some(self.queries.update_metadata.as_str()),
		);
		let affected = owned_portable_query(self.queries.update_metadata.clone())
			.bind_validated(identity.namespace().as_bytes())
			.bind_validated(identity.key().as_bytes())
			.bind_validated(metadata.expires_at.map(StateTimestampMicros::as_i64))
			.bind_validated(metadata.updated_at.as_i64())
			.bind_validated(index_fields.as_str())
			.execute(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(UPDATE_METADATA_LABEL))?
			.rows_affected();
		Self::require_mutation_row(identity, affected)?;
		Ok(())
	}

	/// Internal aggregate callers retain canonical authority and completion tracking.
	pub(in crate::records) async fn delete_record(
		&self,
		tx: &mut WriteTx<'_>,
		identity: &RecordIdentity,
		reservation_only: bool,
	) -> Result<(), StateStorageError> {
		let (operation, query) = if reservation_only {
			(CLEANUP_RESERVATION_LABEL, &self.queries.cleanup)
		} else {
			(DELETE_POINT_LABEL, &self.queries.delete)
		};
		tx.record_database_operation(
			DatabaseOperationKind::Execute,
			operation,
			Some(query.as_str()),
		);
		let affected = owned_portable_query(query.clone())
			.bind_validated(identity.namespace().as_bytes())
			.bind_validated(identity.key().as_bytes())
			.execute(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(operation))?
			.rows_affected();
		Self::require_mutation_row(identity, affected)
	}

	fn require_mutation_row(
		identity: &RecordIdentity,
		affected: u64,
	) -> Result<(), StateStorageError> {
		if affected != 1 {
			return Err(StateStorageError::MutationRowMissing {
				identity: identity.clone(),
			});
		}
		Ok(())
	}

	/// Locks the single control record inside a dedicated control relation, permitting companion
	/// container work on the same transaction before the control record is persisted or kept.
	pub(crate) async fn lock_pinned_control_row<'a, 'tx>(
		&'a self,
		tx: &'a mut WriteTx<'tx>,
		identity: &RecordIdentity,
		conflict_attempts: NonZeroU32,
	) -> Result<PgPinnedControlRowLock<'a, 'tx>, StateStorageError> {
		let point = self.lock(tx, identity, conflict_attempts).await?;
		Ok(PgPinnedControlRowLock { point })
	}
}

/// Canonical point authority on a dedicated control relation, allowing companion-container work
/// on the same transaction before the control record is persisted or kept.
pub(crate) struct PgPinnedControlRowLock<'a, 'tx> {
	point: PgPointRecordLock<'a, 'tx>,
}

impl<'a, 'tx> PgPinnedControlRowLock<'a, 'tx> {
	pub(crate) fn snapshot(&self) -> &PointRecordRead {
		self.point.snapshot()
	}

	pub(crate) fn shares_container(&self, storage: &PgPointStorage) -> bool {
		self.point.storage.shares_container(storage)
	}

	pub(crate) fn identity(&self) -> &RecordIdentity {
		&self.point.identity
	}

	pub(crate) fn companion_tx(&mut self) -> &mut WriteTx<'tx> {
		self.point.tx
	}

	pub(crate) async fn observe_time(&mut self) -> Result<StateTimestampMicros, StateStorageError> {
		self.point.observe_time().await
	}

	pub(crate) async fn write(self, encoded: EncodedRecord) -> Result<(), StateStorageError> {
		self.point.write(encoded).await
	}

	pub(crate) async fn keep(self) -> Result<(), StateStorageError> {
		self.point.keep().await
	}
}

impl<'tx> PgPointRecordLock<'_, 'tx> {
	/// Borrows the locked record's transaction as read-only for companion reads while the lock is held.
	pub(crate) fn as_read_tx(&mut self) -> &mut Tx<'tx> {
		self.tx
	}
}

/// A point guard's snapshot, which tells whether the guard holds a value.
pub(in crate::records) trait PointGuardSnapshot {
	fn holds_value(&self) -> bool;
}
impl PointGuardSnapshot for PointRecordRead {
	fn holds_value(&self) -> bool {
		self.record().is_some()
	}
}
impl PointGuardSnapshot for RecordHeaderObservation {
	fn holds_value(&self) -> bool {
		self.metadata().is_some()
	}
}

impl<Observation> PgPointRecordLock<'_, '_, Observation> {
	pub(in crate::records) async fn update_value_metadata(
		mut self,
		update: EncodedMetadataUpdate,
	) -> Result<(), StateStorageError>
	where
		Observation: PointGuardSnapshot,
	{
		if !self.snapshot.holds_value() {
			return Err(StateStorageError::MetadataValueAbsent {
				identity: self.identity,
			});
		}
		self.storage
			.update_record_metadata(self.tx, &self.identity, &update)
			.await?;
		self.completion.complete();
		Ok(())
	}

	pub(in crate::records) async fn write_value(
		mut self,
		encoded: EncodedRecord,
	) -> Result<(), StateStorageError> {
		let index_fields = encode_index_fields(encoded.record().index_fields());
		self.storage
			.write_record(self.tx, &self.identity, &encoded, &index_fields)
			.await?;
		self.completion.complete();
		Ok(())
	}

	pub(in crate::records) async fn delete_value(mut self) -> Result<(), StateStorageError> {
		self.storage
			.delete_record(self.tx, &self.identity, false)
			.await?;
		self.completion.complete();
		Ok(())
	}

	pub(in crate::records) async fn keep_value(mut self) -> Result<(), StateStorageError> {
		if matches!(self.reservation, ReservationOwnership::Inserted) {
			self.storage
				.delete_record(self.tx, &self.identity, true)
				.await?;
		}
		self.completion.complete();
		Ok(())
	}
}

impl PgPointRecordHeaderLock<'_, '_> {
	pub(crate) fn header(&self) -> RecordHeaderObservation {
		self.snapshot
	}

	pub(crate) async fn delete(self) -> Result<bool, StateStorageError> {
		let present = self.snapshot.metadata().is_some();
		self.delete_value().await?;
		Ok(present)
	}
}

impl PointRecordLock for PgPointRecordLock<'_, '_> {
	type Error = StateStorageError;

	fn snapshot(&self) -> &PointRecordRead {
		&self.snapshot
	}

	async fn observe_time(&mut self) -> Result<StateTimestampMicros, Self::Error> {
		let mut observation_completion = self.tx.track_state_mutation();
		let observed_at = PgPointStorage::observe_time(self.tx).await?;
		observation_completion.complete();
		Ok(observed_at)
	}

	async fn update_metadata(self, update: EncodedMetadataUpdate) -> Result<(), Self::Error> {
		self.update_value_metadata(update).await
	}

	async fn write(self, encoded: EncodedRecord) -> Result<(), Self::Error> {
		self.write_value(encoded).await
	}

	async fn delete(self) -> Result<bool, Self::Error> {
		let present = self.snapshot.record().is_some();
		self.delete_value().await?;
		Ok(present)
	}

	async fn keep(self) -> Result<(), Self::Error> {
		self.keep_value().await
	}
}

impl PgRecordMutationGroup<'_, '_> {
	async fn cleanup_new_reservations(&mut self) -> Result<(), StateStorageError> {
		self.storage
			.cleanup_group_reservations(self.tx, &self.snapshot.inserted_reservations())
			.await
	}
}

impl RecordMutationGroup for PgRecordMutationGroup<'_, '_> {
	type Error = StateStorageError;

	fn snapshot(&self) -> &RecordGroupRead {
		&self.snapshot
	}

	async fn apply(
		mut self,
		actions: Vec<(RecordIdentity, RecordMutation)>,
	) -> Result<(), GroupMutationError<Self::Error>> {
		let prepared = match PreparedPgRecordGroupMutation::prepare(&self.snapshot, actions) {
			Ok(plan) => plan,
			Err(rejection) => {
				if let Err(cleanup) = self.cleanup_new_reservations().await {
					return Err(GroupMutationError::RejectionCleanup {
						rejection_error: rejection,
						cleanup_error: cleanup,
					});
				}
				self.completion.complete();
				return Err(GroupMutationError::Rejected(rejection));
			}
		};
		self.storage
			.apply_group_mutation(self.tx, prepared, &self.snapshot.inserted_reservations())
			.await
			.map_err(GroupMutationError::Backend)?;
		self.completion.complete();
		Ok(())
	}

	async fn keep(mut self) -> Result<(), Self::Error> {
		self.cleanup_new_reservations().await?;
		self.completion.complete();
		Ok(())
	}
}

impl PointQueries {
	fn new(table: &PgQualifiedTableName) -> Self {
		Self {
			read_header: point_header::read_query(table),
			read: crate::audited_sql!(
				r#"
SELECT record.body, record.expires_at, record.updated_at,
    record.index_fields::pg_catalog.text,
    pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.clock_timestamp())
        OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8,
    record.namespace IS NOT NULL
FROM (SELECT 1) observation
LEFT JOIN ONLY {table} record
    ON record.namespace OPERATOR(pg_catalog.=) $1
   AND record.key OPERATOR(pg_catalog.=) $2
"#,
				table = table
			),
			read_locked: crate::audited_sql!(
				r#"
WITH locked AS MATERIALIZED (
    SELECT namespace, body, expires_at, updated_at, index_fields
    FROM ONLY {table}
    WHERE namespace OPERATOR(pg_catalog.=) $1 AND key OPERATOR(pg_catalog.=) $2
    FOR SHARE
)
SELECT record.body, record.expires_at, record.updated_at,
    record.index_fields::pg_catalog.text,
    pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.clock_timestamp())
        OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8,
    record.namespace IS NOT NULL
FROM (SELECT 1) observation
LEFT JOIN locked record ON TRUE
"#,
				table = table
			),
			lock: crate::audited_sql!(
				r#"
SELECT body, expires_at, updated_at, index_fields::pg_catalog.text
FROM ONLY {table}
WHERE namespace OPERATOR(pg_catalog.=) $1 AND key OPERATOR(pg_catalog.=) $2
FOR UPDATE
"#,
				table = table
			),
			lock_header: crate::audited_sql!(
				r#"
SELECT body IS NOT NULL, expires_at, updated_at, index_fields::pg_catalog.text
FROM ONLY {table}
WHERE namespace OPERATOR(pg_catalog.=) $1 AND key OPERATOR(pg_catalog.=) $2
FOR UPDATE
"#,
				table = table
			),
			reserve: crate::audited_sql!(
				r#"
INSERT INTO {table} (namespace, key, body, expires_at, updated_at, index_fields)
VALUES ($1, $2, NULL, NULL, NULL, '{{}}'::pg_catalog.jsonb)
ON CONFLICT (namespace, key) DO NOTHING
RETURNING body, expires_at, updated_at, index_fields::pg_catalog.text
"#,
				table = table
			),
			write: crate::audited_sql!(
				r#"
UPDATE ONLY {table}
SET body = $3, expires_at = $4, updated_at = $5, index_fields = $6::pg_catalog.jsonb
WHERE namespace OPERATOR(pg_catalog.=) $1 AND key OPERATOR(pg_catalog.=) $2
"#,
				table = table
			),
			update_metadata: crate::audited_sql!(
				r#"
UPDATE ONLY {table}
SET expires_at = $3, updated_at = $4, index_fields = $5::pg_catalog.jsonb
WHERE namespace OPERATOR(pg_catalog.=) $1 AND key OPERATOR(pg_catalog.=) $2
  AND body IS NOT NULL
"#,
				table = table
			),
			delete: crate::audited_sql!(
				r#"
DELETE FROM ONLY {table}
WHERE namespace OPERATOR(pg_catalog.=) $1 AND key OPERATOR(pg_catalog.=) $2
"#,
				table = table
			),
			cleanup: crate::audited_sql!(
				r#"
DELETE FROM ONLY {table}
WHERE namespace OPERATOR(pg_catalog.=) $1 AND key OPERATOR(pg_catalog.=) $2
  AND body IS NULL
"#,
				table = table
			),
		}
	}
}

pub(crate) fn invalid_record(identity: &RecordIdentity, reason: &'static str) -> StateStorageError {
	StateStorageError::InvalidRecord {
		identity: identity.clone(),
		reason,
	}
}

/// Actual native envelope and projection fields, without transferring its opaque body.
pub(crate) struct NativePhysicalHeaderFields<'a> {
	pub(crate) identity: &'a RecordIdentity,
	pub(crate) body_present: bool,
	pub(crate) expires_at: Option<i64>,
	pub(crate) updated_at: Option<i64>,
	pub(crate) index_fields: &'a str,
}

/// Parsed value metadata and projections. None denotes only the exact valid reservation shape.
pub(crate) struct NativePhysicalHeader {
	pub(crate) metadata: RecordMetadata,
	pub(crate) index_fields: IndexFields,
}

pub(crate) fn decode_physical_header(
	fields: NativePhysicalHeaderFields<'_>,
) -> Result<Option<NativePhysicalHeader>, StateStorageError> {
	let index_fields = decode_index_fields(fields.index_fields)
		.map_err(|reason| invalid_record(fields.identity, reason))?;
	match (fields.body_present, fields.updated_at) {
		(true, Some(updated_at)) => Ok(Some(NativePhysicalHeader {
			metadata: RecordMetadata {
				expires_at: fields.expires_at.map(StateTimestampMicros::new),
				updated_at: StateTimestampMicros::new(updated_at),
			},
			index_fields,
		})),
		(false, None) if fields.expires_at.is_none() && index_fields.is_empty() => Ok(None),
		_ => Err(invalid_record(
			fields.identity,
			"body and envelope do not form a value or an absent reservation",
		)),
	}
}

pub(crate) fn decode_physical_record(
	identity: &RecordIdentity,
	(body, expires_at, updated_at, index_fields): PhysicalPointRow,
) -> Result<Option<StoredRecord>, StateStorageError> {
	let header = decode_physical_header(NativePhysicalHeaderFields {
		identity,
		body_present: body.is_some(),
		expires_at,
		updated_at,
		index_fields: &index_fields,
	})?;
	Ok(body.zip(header).map(|(body, header)| {
		StoredRecord::from_storage(body.0, header.metadata, header.index_fields)
	}))
}

mod metadata_mutation;
pub(crate) mod record_expiry;
