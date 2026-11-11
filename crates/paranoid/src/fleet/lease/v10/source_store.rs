//! Coherent published v10 lease import after source-writer retirement.
//!
//! Reads require library-owned source-aware admission; mutations require writer retirement. Physical
//! validation and Source observations supply no retirement proof or private Claim grant.
//! Canonical native key authority covers absent source rows; source locks follow claim,
//! then fencing counter order. The same transaction retains every lock through completion.

use std::num::NonZeroU32;

use sqlx::{FromRow, Row, postgres::PgRow};

use super::queries::{LeaseSqlTables, Queries};
use super::source_bridge::{
	LeaseSourceBridge, LeaseSourceClaim, LeaseSourceCounter, LeaseSourceImport,
	LeaseSourceImportError, LeaseSourceRows, LeaseSourceTimestamp,
};
use crate::db::bytea::Bytea;
use crate::db::{
	AuditedSql, AuditedSqlBuilder, DatabaseError, DatabaseOperationKind, PgQualifiedTableName,
	SchemaError, TransactionFailure, Tx, WriteTx, owned_portable_query, owned_portable_query_as,
	owned_portable_query_scalar,
};
use crate::fleet::lease::state::{LeaseRepresentationError, LeaseSchemaBinding};
use crate::fleet::lease::{HolderId, LeaseKey, StoreConfig, Token};
use crate::installation::contract::BoundComponentContract;
use crate::records::postgres::v10::source_point::{
	PgExternalSourcePointLock, validate_source_heap,
};
use crate::records::postgres::v10::source_timestamp::{
	PgSourceTimestamp, append_timestamp_projection, parse_source_timestamp,
};
use crate::records::postgres::{PgPointStorage, StateStorageError, decode_physical_record};
use crate::records::{
	EncodedMetadataUpdate, EncodedRecord, PointRecordLock, PointRecordRead, RecordIdentity,
	RecordKey, RecordNamespace, StateTimestampMicros, StoredRecord,
};

const VALIDATE_LABEL: &str = "lease.source.validate";
const READ_LABEL: &str = "lease.source.read";
const LOCK_LABEL: &str = "lease.source.lock";
const DELETE_LABEL: &str = "lease.source.delete";

#[derive(Clone, Copy)]
enum SourceRowSet {
	CounterOnly,
	CounterAndClaim,
}

enum PointOrigin {
	Absent(PointRecordRead),
	Native(PointRecordRead),
	Source {
		rows: SourceRowSet,
		snapshot: PointRecordRead,
	},
}

/// Source values are typed SQL facts, never an invented byte/JSON source representation.
/// Source observations require the actual bound bridge to check aggregate semantic domains.
#[derive(Debug)]
pub(crate) enum LeaseSourceOrigin {
	Source(LeaseSourceRows),
	Native(StoredRecord),
	Absent,
}

#[derive(Debug)]
pub(crate) struct LeaseSourceRead {
	origin: LeaseSourceOrigin,
	observed_at: StateTimestampMicros,
}

impl LeaseSourceRead {
	pub(crate) fn origin(&self) -> &LeaseSourceOrigin {
		&self.origin
	}
	pub(crate) fn observed_at(&self) -> StateTimestampMicros {
		self.observed_at
	}
	pub(crate) fn into_origin(self) -> LeaseSourceOrigin {
		self.origin
	}
}

/// Typed primary and cleanup failures retain their exact causes without source values.
#[derive(Debug)]
pub(crate) enum LeaseSourceStorageError {
	Database(DatabaseError),
	Schema(SchemaError),
	Native(StateStorageError),
	Import(LeaseSourceImportError),
	Representation(LeaseRepresentationError),
	InvalidPhysical {
		reason: &'static str,
	},
	DuplicatePhysicalRecords {
		identity: RecordIdentity,
	},
	BindingKeyMismatch,
	DurableHistoryDeletionUnsupported,
	MetadataNativeOriginRequired,
	SourceMutationCount {
		expected: u64,
		actual: u64,
	},
	CleanupFailed {
		operation_error: Box<Self>,
		cleanup_error: Box<Self>,
	},
}

impl std::fmt::Display for LeaseSourceStorageError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Database(_) => f.write_str("lease source database work failed"),
			Self::Schema(_) => f.write_str("lease source physical layout is incompatible"),
			Self::Native(_) => f.write_str("lease source native storage work failed"),
			Self::Import(_) => f.write_str("lease source aggregate cannot be imported"),
			Self::Representation(_) => f.write_str("lease source native representation failed"),
			Self::InvalidPhysical { reason } => {
				write!(f, "lease source physical observation is invalid: {reason}")
			}
			Self::DuplicatePhysicalRecords { identity } => {
				write!(f, "lease source and native both contain {identity:?}")
			}
			Self::BindingKeyMismatch => f.write_str("lease source binding belongs to another key"),
			Self::DurableHistoryDeletionUnsupported => {
				f.write_str("lease source point mutation cannot delete durable fencing history")
			}
			Self::MetadataNativeOriginRequired => {
				f.write_str("lease source metadata-only update requires a physical native value")
			}
			Self::SourceMutationCount { expected, actual } => write!(
				f,
				"lease source mutation affected {actual} rows; expected {expected}"
			),
			Self::CleanupFailed { .. } => {
				f.write_str("lease source rejection and reservation cleanup both failed")
			}
		}
	}
}
impl std::error::Error for LeaseSourceStorageError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Database(e) => Some(e),
			Self::Schema(e) => Some(e),
			Self::Native(e) => Some(e),
			Self::Import(e) => Some(e),
			Self::Representation(e) => Some(e),
			Self::CleanupFailed {
				operation_error, ..
			} => Some(operation_error.as_ref()),
			_ => None,
		}
	}
}
impl From<DatabaseError> for LeaseSourceStorageError {
	fn from(e: DatabaseError) -> Self {
		Self::Database(e)
	}
}
impl From<SchemaError> for LeaseSourceStorageError {
	fn from(e: SchemaError) -> Self {
		Self::Schema(e)
	}
}
impl From<StateStorageError> for LeaseSourceStorageError {
	fn from(e: StateStorageError) -> Self {
		Self::Native(e)
	}
}

impl LeaseSourceStorageError {
	/// Returns an actual retained driver cause without replacing the primary typed failure.
	pub(crate) fn database_error(&self) -> Option<&crate::db::DatabaseError> {
		match self {
			Self::Database(error) => Some(error),
			Self::Schema(error) => error.database_error(),
			Self::Native(error) => error.database_error(),
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
			Self::Native(error) => error.database_error().is_some(),
			Self::CleanupFailed {
				operation_error,
				cleanup_error,
			} => {
				operation_error.involves_database_failure()
					|| cleanup_error.involves_database_failure()
			}
			_ => false,
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Database(error) => error.commit_outcome_is_unknown(),
			Self::Schema(error) => error.commit_outcome_is_unknown(),
			Self::Native(error) => error
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
			Self::CleanupFailed {
				operation_error,
				cleanup_error,
			} => {
				operation_error.commit_outcome_is_unknown()
					|| cleanup_error.commit_outcome_is_unknown()
			}
			_ => false,
		}
	}
}

#[derive(Clone)]
pub(crate) struct PgLeaseSourceStorage {
	native: PgPointStorage,
	namespace: RecordNamespace,
	claim_table: PgQualifiedTableName,
	fencing_counter_table: PgQualifiedTableName,
	queries: Queries,
	read: AuditedSql,
	lock_claim: AuditedSql,
	lock_counter: AuditedSql,
	delete_claim: AuditedSql,
	delete_counter: AuditedSql,
}

/// The single outer completion tracks acquisition, preparation, and both physical mutations.
/// Dropping, failing, or cancelling this guard requires rollback; no asynchronous Drop runs.
pub(crate) struct PgLeaseSourceImportLock<'a, 'tx> {
	storage: &'a PgLeaseSourceStorage,
	native_lock: PgExternalSourcePointLock<'a, 'tx>,
	key: LeaseKey,
	snapshot: LeaseSourceRead,
}

impl PgLeaseSourceStorage {
	/// Validates exact source lease associations and an existing native container.
	/// No relation is created, prepared, repaired, or inferred from a bootstrap name.
	pub(crate) async fn validate_existing(
		config: &crate::fleet::lease::StoreConfig,
		native: PgPointStorage,
		namespace: RecordNamespace,
		tx: &mut Tx<'_>,
	) -> Result<Self, LeaseSourceStorageError> {
		let mut completion = tx.track_state_mutation();
		if native.table() == &config.claim_table || native.table() == &config.fencing_counter_table
		{
			return Err(SchemaError::mismatch(
				"native lease container must differ from both source tables",
			)
			.into());
		}
		// Relation pins use native, claim, counter order; row authority follows the same order.
		let pin = crate::audited_sql!(
			"LOCK TABLE ONLY {native} IN ACCESS SHARE MODE",
			native = native.table()
		);
		tx.record_database_operation(
			DatabaseOperationKind::Execute,
			VALIDATE_LABEL,
			Some(pin.as_str()),
		);
		owned_portable_query(pin)
			.execute(tx.inner.as_mut())
			.await
			.map_err(|e| e.with_operation_label(VALIDATE_LABEL))?;
		native.validate_existing(tx).await?;
		validate_source_heap(&config.claim_table, tx).await?;
		validate_source_heap(&config.fencing_counter_table, tx).await?;
		super::schema::validate_schema_in_current_transaction(tx, config).await?;
		let storage = Self {
			read: aggregate_query(native.table(), config),
			lock_claim: crate::audited_sql!(
				"SELECT key FROM ONLY {table} WHERE key OPERATOR(pg_catalog.=) $1 FOR UPDATE",
				table = &config.claim_table
			),
			lock_counter: crate::audited_sql!(
				"SELECT key FROM ONLY {table} WHERE key OPERATOR(pg_catalog.=) $1 FOR UPDATE",
				table = &config.fencing_counter_table
			),
			delete_claim: crate::audited_sql!(
				"DELETE FROM ONLY {table} WHERE key OPERATOR(pg_catalog.=) $1",
				table = &config.claim_table
			),
			delete_counter: crate::audited_sql!(
				"DELETE FROM ONLY {table} WHERE key OPERATOR(pg_catalog.=) $1",
				table = &config.fencing_counter_table
			),
			queries: Queries::for_tables(LeaseSqlTables {
				claim_table: &config.claim_table,
				fencing_counter_table: &config.fencing_counter_table,
			}),
			claim_table: config.claim_table.clone(),
			fencing_counter_table: config.fencing_counter_table.clone(),
			native,
			namespace,
		};
		completion.complete();
		Ok(storage)
	}

	pub(crate) fn native_storage(&self) -> &PgPointStorage {
		&self.native
	}
	pub(crate) fn namespace(&self) -> &RecordNamespace {
		&self.namespace
	}
	pub(crate) fn claim_table(&self) -> &PgQualifiedTableName {
		&self.claim_table
	}
	pub(crate) fn fencing_counter_table(&self) -> &PgQualifiedTableName {
		&self.fencing_counter_table
	}
	/// The coexistence statements of this storage's lease tables.
	pub(super) fn queries(&self) -> &Queries {
		&self.queries
	}

	async fn delete_source_rows(
		&self,
		tx: &mut WriteTx<'_>,
		key: &LeaseKey,
		rows: SourceRowSet,
	) -> Result<(), LeaseSourceStorageError> {
		let queries = match rows {
			SourceRowSet::CounterOnly => [None, Some(&self.delete_counter)],
			SourceRowSet::CounterAndClaim => [Some(&self.delete_claim), Some(&self.delete_counter)],
		};
		for query in queries.into_iter().flatten() {
			tx.record_database_operation(
				DatabaseOperationKind::Execute,
				DELETE_LABEL,
				Some(query.as_str()),
			);
			let actual = owned_portable_query(query.clone())
				.bind_validated(key.as_str())
				.execute(tx.inner.as_mut())
				.await
				.map_err(|e| e.with_operation_label(DELETE_LABEL))?
				.rows_affected();
			if actual != 1 {
				return Err(LeaseSourceStorageError::SourceMutationCount {
					expected: 1,
					actual,
				});
			}
		}
		Ok(())
	}

	fn identity(&self, key: &LeaseKey) -> RecordIdentity {
		RecordIdentity::new(
			self.namespace.clone(),
			RecordKey::new(key.as_str().as_bytes().to_vec()),
		)
	}

	/// One statement reads native plus both source rows, so an atomic move cannot look absent.
	pub(crate) async fn read(
		&self,
		tx: &mut Tx<'_>,
		key: &LeaseKey,
	) -> Result<LeaseSourceRead, LeaseSourceStorageError> {
		let mut completion = tx.track_state_mutation();
		let result = self.read_physical(tx, key, &self.identity(key)).await?;
		completion.complete();
		Ok(result)
	}

	async fn read_physical(
		&self,
		tx: &mut Tx<'_>,
		key: &LeaseKey,
		identity: &RecordIdentity,
	) -> Result<LeaseSourceRead, LeaseSourceStorageError> {
		tx.record_database_operation(
			DatabaseOperationKind::FetchOne,
			READ_LABEL,
			Some(self.read.as_str()),
		);
		let row = owned_portable_query_as::<AggregateRow>(self.read.clone())
			.bind_validated(identity.namespace().as_bytes())
			.bind_validated(identity.key().as_bytes())
			.bind_validated(key.as_str())
			.fetch_one(tx.inner.as_mut())
			.await
			.map_err(|e| e.with_operation_label(READ_LABEL))?;
		decode_aggregate(row, key, identity)
	}

	/// Acquires one canonical native key, then claim and counter row locks before the snapshot.
	/// Source absence is protected by native authority only after verified writer retirement.
	pub(crate) async fn lock<'a, 'tx>(
		&'a self,
		tx: &'a mut WriteTx<'tx>,
		key: LeaseKey,
		conflict_attempts: NonZeroU32,
	) -> Result<PgLeaseSourceImportLock<'a, 'tx>, LeaseSourceStorageError> {
		let identity = self.identity(&key);
		let mut native_lock = self
			.native
			.lock_for_external_source_import(tx, identity.clone(), conflict_attempts)
			.await?;
		for query in [&self.lock_claim, &self.lock_counter] {
			let source_tx = native_lock.source_tx();
			source_tx.record_database_operation(
				DatabaseOperationKind::FetchOptional,
				LOCK_LABEL,
				Some(query.as_str()),
			);
			let locked_key = owned_portable_query_scalar::<String>(query.clone())
				.bind_validated(key.as_str())
				.fetch_optional(source_tx.inner.as_mut())
				.await
				.map_err(|e| e.with_operation_label(LOCK_LABEL))?;
			if locked_key
				.as_deref()
				.is_some_and(|stored| stored != key.as_str())
			{
				return Err(LeaseSourceStorageError::InvalidPhysical {
					reason: "locked source key mismatch",
				});
			}
		}
		let snapshot = self
			.read_physical(native_lock.source_tx(), &key, &identity)
			.await?;
		Ok(PgLeaseSourceImportLock {
			storage: self,
			native_lock,
			key,
			snapshot,
		})
	}
}

impl<'a, 'tx> PgLeaseSourceImportLock<'a, 'tx> {
	pub(crate) fn snapshot(&self) -> &LeaseSourceRead {
		&self.snapshot
	}

	/// Keeps source and preexisting reservations; removes only this guard's new reservation.
	pub(crate) async fn keep(self) -> Result<(), LeaseSourceStorageError> {
		self.native_lock.keep().await?;
		Ok(())
	}

	async fn reject(self, operation_error: LeaseSourceStorageError) -> LeaseSourceStorageError {
		match self.keep().await {
			Ok(()) => operation_error,
			Err(cleanup_error) => LeaseSourceStorageError::CleanupFailed {
				operation_error: Box::new(operation_error),
				cleanup_error: Box::new(cleanup_error),
			},
		}
	}

	/// Prepares a selected native working snapshot under retained aggregate authority.
	/// It performs no physical value write or source deletion. Existing lease kernels decide
	/// the final mutation; keep leaves source placement and durable history unchanged.
	pub(crate) async fn into_point_lock(
		mut self,
		bridge: &BoundComponentContract<'_, '_, LeaseSourceBridge>,
		native: &LeaseSchemaBinding<'_>,
	) -> Result<PgLeaseSourcePointLock<'a, 'tx>, LeaseSourceStorageError> {
		if native.key() != &self.key {
			return Err(self
				.reject(LeaseSourceStorageError::BindingKeyMismatch)
				.await);
		}
		let origin = std::mem::replace(&mut self.snapshot.origin, LeaseSourceOrigin::Absent);
		let observed_at = self.snapshot.observed_at;
		let origin = match origin {
			LeaseSourceOrigin::Absent => {
				PointOrigin::Absent(PointRecordRead::new(None, observed_at))
			}
			LeaseSourceOrigin::Native(record) => {
				PointOrigin::Native(PointRecordRead::new(Some(record), observed_at))
			}
			LeaseSourceOrigin::Source(rows) => {
				let (encoded, rows) = match encode_source_record(&self.key, rows, bridge, native) {
					Ok(prepared) => prepared,
					Err(error) => return Err(self.reject(error).await),
				};
				PointOrigin::Source {
					rows,
					snapshot: PointRecordRead::new(Some(encoded.into_record()), observed_at),
				}
			}
		};
		Ok(PgLeaseSourcePointLock {
			aggregate: self,
			origin,
		})
	}

	/// Prepares with the actual bound typed source transition and selected native writer.
	/// A successful write/deletion is tentative until the owning controller commits.
	/// This operation never extracts or delivers preserved private Claim authority.
	pub(crate) async fn convert(
		mut self,
		bridge: &BoundComponentContract<'_, '_, LeaseSourceBridge>,
		native: &LeaseSchemaBinding<'_>,
	) -> Result<bool, LeaseSourceStorageError> {
		if native.key() != &self.key {
			return Err(self
				.reject(LeaseSourceStorageError::BindingKeyMismatch)
				.await);
		}
		let origin = std::mem::replace(&mut self.snapshot.origin, LeaseSourceOrigin::Absent);
		let rows = match origin {
			LeaseSourceOrigin::Absent => {
				self.keep().await?;
				return Ok(false);
			}
			LeaseSourceOrigin::Native(record) => {
				let snapshot = PointRecordRead::new(Some(record), self.snapshot.observed_at);
				if let Err(error) = native.decode_record(&snapshot) {
					return Err(self
						.reject(LeaseSourceStorageError::Representation(error))
						.await);
				}
				self.keep().await?;
				return Ok(false);
			}
			LeaseSourceOrigin::Source(rows) => rows,
		};
		let (encoded, rows) = match encode_source_record(&self.key, rows, bridge, native) {
			Ok(prepared) => prepared,
			Err(error) => return Err(self.reject(error).await),
		};
		let mut written = self
			.native_lock
			.write_native_before_source_retirement(encoded)
			.await?;
		self.storage
			.delete_source_rows(written.source_tx(), &self.key, rows)
			.await?;
		written.finish();
		Ok(true)
	}
}

/// The actual typed transition and selected writer prepare one native record before any write.
fn encode_source_record(
	key: &LeaseKey,
	rows: LeaseSourceRows,
	bridge: &BoundComponentContract<'_, '_, LeaseSourceBridge>,
	native: &LeaseSchemaBinding<'_>,
) -> Result<(EncodedRecord, SourceRowSet), LeaseSourceStorageError> {
	let source_rows = if rows.claim.is_some() {
		SourceRowSet::CounterAndClaim
	} else {
		SourceRowSet::CounterOnly
	};
	let imported = bridge
		.transitions()
		.import(key, rows)
		.map_err(LeaseSourceStorageError::Import)?;
	let LeaseSourceImport::Present { state, metadata } = imported else {
		return Err(LeaseSourceStorageError::InvalidPhysical {
			reason: "present source imported as absence",
		});
	};
	let encoded = native
		.encode_record(&state, metadata)
		.map_err(LeaseSourceStorageError::Representation)?;
	Ok((encoded, source_rows))
}

/// Native working state and one retained native→claim→counter authority/completion token.
/// The snapshot may be encoded in memory while physical source rows still own placement.
pub(crate) struct PgLeaseSourcePointLock<'a, 'tx> {
	aggregate: PgLeaseSourceImportLock<'a, 'tx>,
	origin: PointOrigin,
}
impl PointRecordLock for PgLeaseSourcePointLock<'_, '_> {
	type Error = LeaseSourceStorageError;
	fn snapshot(&self) -> &PointRecordRead {
		match &self.origin {
			PointOrigin::Absent(snapshot)
			| PointOrigin::Native(snapshot)
			| PointOrigin::Source { snapshot, .. } => snapshot,
		}
	}
	async fn observe_time(&mut self) -> Result<StateTimestampMicros, Self::Error> {
		Ok(self.aggregate.native_lock.observe_time().await?)
	}
	async fn write(self, encoded: EncodedRecord) -> Result<(), Self::Error> {
		let PgLeaseSourceImportLock {
			storage,
			native_lock,
			key,
			..
		} = self.aggregate;
		if let PointOrigin::Source { rows, .. } = self.origin {
			let mut written = native_lock
				.write_native_before_source_retirement(encoded)
				.await?;
			storage
				.delete_source_rows(written.source_tx(), &key, rows)
				.await?;
			written.finish();
		} else {
			native_lock.write_native_only(encoded).await?;
		}
		Ok(())
	}
	async fn update_metadata(self, update: EncodedMetadataUpdate) -> Result<(), Self::Error> {
		if !matches!(self.origin, PointOrigin::Native(_)) {
			return Err(self
				.aggregate
				.reject(LeaseSourceStorageError::MetadataNativeOriginRequired)
				.await);
		}
		self.aggregate.native_lock.update_metadata(update).await?;
		Ok(())
	}
	async fn delete(self) -> Result<bool, Self::Error> {
		Err(self
			.aggregate
			.reject(LeaseSourceStorageError::DurableHistoryDeletionUnsupported)
			.await)
	}
	async fn keep(self) -> Result<(), Self::Error> {
		self.aggregate.keep().await
	}
}

struct AggregateRow {
	native_exists: bool,
	body: Option<Bytea>,
	expires: Option<i64>,
	updated: Option<i64>,
	indexes: Option<String>,
	counter_exists: bool,
	last_fence: Option<i64>,
	counter_updated: Option<String>,
	claim_exists: bool,
	holder: Option<String>,
	fence: Option<i64>,
	token: Option<Bytea>,
	claim_expires: Option<String>,
	claim_updated: Option<String>,
	observed_at: i64,
	isolation: String,
}
impl FromRow<'_, PgRow> for AggregateRow {
	fn from_row(row: &PgRow) -> Result<Self, sqlx::Error> {
		Ok(Self {
			native_exists: row.try_get("native_exists")?,
			body: row.try_get("body")?,
			expires: row.try_get("expires")?,
			updated: row.try_get("updated")?,
			indexes: row.try_get("indexes")?,
			counter_exists: row.try_get("counter_exists")?,
			last_fence: row.try_get("last_fence")?,
			counter_updated: row.try_get("counter_updated")?,
			claim_exists: row.try_get("claim_exists")?,
			holder: row.try_get("holder")?,
			fence: row.try_get("fence")?,
			token: row.try_get("token")?,
			claim_expires: row.try_get("claim_expires")?,
			claim_updated: row.try_get("claim_updated")?,
			observed_at: row.try_get("observed_at")?,
			isolation: row.try_get("isolation")?,
		})
	}
}

fn required<T>(value: Option<T>) -> Result<T, LeaseSourceStorageError> {
	value.ok_or(LeaseSourceStorageError::InvalidPhysical {
		reason: "present source has a null required field",
	})
}

fn decode_aggregate(
	row: AggregateRow,
	key: &LeaseKey,
	identity: &RecordIdentity,
) -> Result<LeaseSourceRead, LeaseSourceStorageError> {
	if row.isolation != "read committed" {
		return Err(LeaseSourceStorageError::InvalidPhysical {
			reason: "source import requires READ COMMITTED",
		});
	}
	if row.body.is_some() && (row.counter_exists || row.claim_exists) {
		return Err(LeaseSourceStorageError::DuplicatePhysicalRecords {
			identity: identity.clone(),
		});
	}
	let native = if row.native_exists {
		let indexes = required(row.indexes)?;
		decode_physical_record(identity, (row.body, row.expires, row.updated, indexes))?
	} else {
		None
	};
	let origin = if let Some(native) = native {
		LeaseSourceOrigin::Native(native)
	} else if row.counter_exists || row.claim_exists {
		let counter = if row.counter_exists {
			Some(LeaseSourceCounter {
				last_fencing_token: required(row.last_fence)?,
				updated_at: parse_timestamp(required(row.counter_updated)?.as_str())?,
			})
		} else {
			None
		};
		let claim = if row.claim_exists {
			Some(LeaseSourceClaim {
				holder_id: HolderId::new(required(row.holder)?).map_err(|_| {
					LeaseSourceStorageError::InvalidPhysical {
						reason: "invalid source holder",
					}
				})?,
				fencing_token: required(row.fence)?,
				lease_token: Token::from_persisted_bytes(required(row.token)?.0).map_err(|_| {
					LeaseSourceStorageError::InvalidPhysical {
						reason: "invalid source private token",
					}
				})?,
				expires_at: parse_timestamp(required(row.claim_expires)?.as_str())?,
				updated_at: parse_timestamp(required(row.claim_updated)?.as_str())?,
			})
		} else {
			None
		};
		LeaseSourceOrigin::Source(LeaseSourceRows {
			key: key.clone(),
			counter,
			claim,
		})
	} else {
		LeaseSourceOrigin::Absent
	};
	Ok(LeaseSourceRead {
		origin,
		observed_at: StateTimestampMicros::new(row.observed_at),
	})
}

/// Adapts the exact physical timestamp without narrowing before lease import.
fn parse_timestamp(value: &str) -> Result<LeaseSourceTimestamp, LeaseSourceStorageError> {
	parse_source_timestamp(value)
		.map(|timestamp| match timestamp {
			PgSourceTimestamp::NegativeInfinity => LeaseSourceTimestamp::NegativeInfinity,
			PgSourceTimestamp::FiniteUnixMicros(value) => {
				LeaseSourceTimestamp::FiniteUnixMicros(value)
			}
			PgSourceTimestamp::PositiveInfinity => LeaseSourceTimestamp::PositiveInfinity,
		})
		.map_err(|_| LeaseSourceStorageError::InvalidPhysical {
			reason: "invalid exact source timestamp",
		})
}

fn aggregate_query(native: &crate::db::PgQualifiedTableName, config: &StoreConfig) -> AuditedSql {
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("SELECT n.namespace IS NOT NULL AS native_exists, n.body, n.expires_at AS expires, n.updated_at AS updated, n.index_fields::pg_catalog.text AS indexes, c.key IS NOT NULL AS counter_exists, c.last_fencing_token AS last_fence, ");
	append_timestamp_projection(&mut sql, "c.updated_at");
	sql.push_text(" AS counter_updated, l.key IS NOT NULL AS claim_exists, l.holder_id AS holder, l.fencing_token AS fence, l.lease_token AS token, ");
	append_timestamp_projection(&mut sql, "l.expires_at");
	sql.push_text(" AS claim_expires, ");
	append_timestamp_projection(&mut sql, "l.updated_at");
	sql.push_text(" AS claim_updated, pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.clock_timestamp()) OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8 AS observed_at, pg_catalog.current_setting('transaction_isolation') AS isolation FROM (SELECT 1) observation LEFT JOIN ONLY ");
	sql.push_argument(native);
	sql.push_text(
		" n ON n.namespace OPERATOR(pg_catalog.=) $1 AND n.key OPERATOR(pg_catalog.=) $2 LEFT JOIN ONLY ",
	);
	sql.push_argument(&config.claim_table);
	sql.push_text(" l ON l.key OPERATOR(pg_catalog.=) $3 LEFT JOIN ONLY ");
	sql.push_argument(&config.fencing_counter_table);
	sql.push_text(" c ON c.key OPERATOR(pg_catalog.=) $3");
	sql.finish()
}
