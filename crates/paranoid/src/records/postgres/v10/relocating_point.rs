//! Point relocation from published v10 KV/Fleet state after source writer retirement.
//!
//! Every operation MUST run under post-retirement admission. This adapter does not install
//! that admission or prove that v10 writers have been excluded. Source bytes remain opaque;
//! primitive code selects codecs and preserves source metadata for pure conversion.

use std::{fmt, num::NonZeroU32};

use super::super::{
	AuditedSql, Bytea, DatabaseError, DatabaseOperationKind, PgPointStorage, PgQualifiedTableName,
	ReservationOwnership, SchemaError, StateMutationCompletion, StateStorageError, Tx, WriteTx,
	decode_physical_record, encode_index_fields, owned_portable_query, owned_portable_query_as,
};
use crate::db::AuditedSqlBuilder;
use crate::records::v10_key::Key;
use crate::records::{
	EncodedMetadataUpdate, EncodedRecord, PointRecordOrigin, RecordHeaderObservation,
	RecordIdentity, RelocatingPointRecordLock, RelocationRead, RelocationRecord, SourcePointRecord,
	StateTimestampMicros, StoredRecord,
};

const VALIDATE_LABEL: &str = "db.state_storage.relocating_point.validate";
const READ_LABEL: &str = "db.state_storage.relocating_point.read";
const LOCK_SOURCE_LABEL: &str = "db.state_storage.relocating_point.lock_source";
const DELETE_SOURCE_LABEL: &str = "db.state_storage.relocating_point.delete_source";

/// Explicit library-owned mapping from one parsed source key to one canonical identity.
/// Construction selects no namespace, key convention, primitive, or source decoder.
#[derive(Clone)]
pub(crate) struct RelocatingPointBinding {
	source_key: Key,
	identity: RecordIdentity,
}

impl RelocatingPointBinding {
	pub(crate) fn new(source_key: Key, identity: RecordIdentity) -> Self {
		Self {
			source_key,
			identity,
		}
	}
	pub(crate) fn source_key(&self) -> &Key {
		&self.source_key
	}
	pub(crate) fn identity(&self) -> &RecordIdentity {
		&self.identity
	}
}

impl fmt::Debug for RelocatingPointBinding {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("RelocatingPointBinding")
			.field("source_key_byte_len", &self.source_key.as_str().len())
			.field("identity", &self.identity)
			.finish()
	}
}

pub(crate) use super::source_point::SourceTimestampField;
use super::source_point::{
	SourcePhysicalError, SourcePhysicalHeaderFields, SourcePhysicalRow, SourceSnapshotQueryKind,
	decode_source_header, decode_source_record, source_delete_query, source_snapshot_query,
	validate_source_existing,
};

/// Default formatting redacts key bytes and all source/native payloads.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RelocatingPointError {
	#[error("relocating point database work failed")]
	Database(#[from] DatabaseError),
	#[error("relocating point physical layout is incompatible")]
	Schema(#[from] SchemaError),
	#[error("relocating point native storage work failed")]
	Native(#[from] StateStorageError),
	#[error("source timestamp {field:?} is not representable for record {identity:?}")]
	SourceTimestampUnrepresentable {
		identity: RecordIdentity,
		field: SourceTimestampField,
	},
	#[error("source record {identity:?} is invalid: {reason}")]
	InvalidSource {
		identity: RecordIdentity,
		reason: &'static str,
	},
	#[error("source and native both contain record {identity:?}")]
	DuplicatePhysicalRecords { identity: RecordIdentity },
	#[error("source record {identity:?} lost its acquired row")]
	SourceMutationRowMissing { identity: RecordIdentity },
}

impl RelocatingPointError {
	/// Returns an actual retained driver cause without replacing the primary typed failure.
	pub(crate) fn database_error(&self) -> Option<&crate::db::DatabaseError> {
		match self {
			Self::Database(error) => Some(error),
			Self::Schema(error) => error.database_error(),
			Self::Native(error) => error.database_error(),

			_ => None,
		}
	}

	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::Database(_) => true,
			Self::Schema(error) => crate::db::TransactionFailure::involves_database_failure(error),
			Self::Native(error) => error.database_error().is_some(),

			_ => false,
		}
	}

	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Database(error) => error.commit_outcome_is_unknown(),
			Self::Schema(error) => crate::db::TransactionFailure::commit_outcome_is_unknown(error),
			Self::Native(error) => error
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),

			_ => false,
		}
	}
}

/// A catalog-validated binding to an explicit source table and native container.
/// Installation must validate it under post-retirement admission before operations.
#[derive(Clone, Debug)]
pub(crate) struct PgRelocatingPointStorage {
	native: PgPointStorage,
	source_table: PgQualifiedTableName,
	queries: RelocatingPointQueries,
}

#[cfg(test)]
impl PgRelocatingPointStorage {
	pub(crate) fn for_tests(native: PgPointStorage, source_table: PgQualifiedTableName) -> Self {
		let queries = RelocatingPointQueries::new(native.table(), &source_table);
		Self {
			native,
			source_table,
			queries,
		}
	}
}

#[derive(Clone, Debug)]
struct RelocatingPointQueries {
	lock_source: AuditedSql,
	lock_source_header: AuditedSql,
	read_dual: AuditedSql,
	delete_source: AuditedSql,
	relocate_metadata: AuditedSql,
}

type SourceRow = SourcePhysicalRow;
type DualRow = (
	bool,
	Option<Bytea>,
	Option<i64>,
	Option<i64>,
	Option<String>,
	bool,
	Option<Bytea>,
	Option<i64>,
	Option<i64>,
	bool,
	bool,
	i64,
);

enum RelocationAuthority {
	Native {
		identity: RecordIdentity,
		reservation: ReservationOwnership,
	},
	Source {
		binding: RelocatingPointBinding,
		reservation: ReservationOwnership,
	},
}
impl RelocationAuthority {
	fn identity(&self) -> &RecordIdentity {
		match self {
			Self::Native { identity, .. } => identity,
			Self::Source { binding, .. } => binding.identity(),
		}
	}
	fn source_binding(&self) -> Option<&RelocatingPointBinding> {
		match self {
			Self::Native { .. } => None,
			Self::Source { binding, .. } => Some(binding),
		}
	}
	fn reservation(&self) -> &ReservationOwnership {
		match self {
			Self::Native { reservation, .. } | Self::Source { reservation, .. } => reservation,
		}
	}
}

/// One outer tracker covers canonical native locking, source locking, and both mutations.
pub(crate) struct PgRelocatingPointRecordLock<'a, 'tx, Observation = RelocationRead> {
	storage: &'a PgRelocatingPointStorage,
	tx: &'a mut WriteTx<'tx>,
	authority: RelocationAuthority,
	snapshot: Observation,
	completion: StateMutationCompletion,
}

impl PgRelocatingPointStorage {
	/// Reuses this adapter's exact source catalog proof and fixed source operation builders.
	pub(crate) fn source_point_storage(&self) -> super::source_point::PgSourcePointStorage {
		super::source_point::PgSourcePointStorage::from_validated_table(self.source_table.clone())
	}

	pub(crate) fn native_storage(&self) -> &PgPointStorage {
		&self.native
	}

	pub(crate) fn source_table(&self) -> &PgQualifiedTableName {
		&self.source_table
	}

	/// Reuses the exact validated source relation and validates only a distinct native container.
	/// The owning startup retains its source/control pins and source relation lock throughout.
	pub(crate) async fn rebind_existing_native(
		&self,
		native: PgPointStorage,
		tx: &mut Tx<'_>,
	) -> Result<Self, RelocatingPointError> {
		if self.native.shares_container(&native) {
			return Ok(self.clone());
		}
		if native.table() == &self.source_table {
			return Err(
				SchemaError::mismatch("source and native point tables must be distinct").into(),
			);
		}
		let mut completion = tx.track_state_mutation();
		let lock = crate::audited_sql!(
			"LOCK TABLE ONLY {table} IN ACCESS SHARE MODE",
			table = native.table()
		);
		tx.record_database_operation(
			DatabaseOperationKind::Execute,
			VALIDATE_LABEL,
			Some(lock.as_str()),
		);
		owned_portable_query(lock)
			.execute(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(VALIDATE_LABEL))?;
		native.validate_existing(tx).await?;
		let rebound = Self {
			queries: RelocatingPointQueries::new(native.table(), &self.source_table),
			native,
			source_table: self.source_table.clone(),
		};
		completion.complete();
		Ok(rebound)
	}

	/// Validates existing shapes without creating or repairing either table.
	/// Catalog validation does not establish source writer retirement.
	pub(crate) async fn validate_existing(
		native: PgPointStorage,
		source_table: PgQualifiedTableName,
		tx: &mut Tx<'_>,
	) -> Result<Self, RelocatingPointError> {
		let mut completion = tx.track_state_mutation();
		if native.table == source_table {
			return Err(
				SchemaError::mismatch("source and native point tables must be distinct").into(),
			);
		}
		let lock_tables = crate::audited_sql!(
			"LOCK TABLE ONLY {native}, ONLY {source} IN ACCESS SHARE MODE",
			native = &native.table,
			source = &source_table
		);
		tx.record_database_operation(
			DatabaseOperationKind::Execute,
			VALIDATE_LABEL,
			Some(lock_tables.as_str()),
		);
		owned_portable_query(lock_tables)
			.execute(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(VALIDATE_LABEL))?;
		native.validate_existing(tx).await?;
		validate_source_existing(&source_table, tx).await?;
		let queries = RelocatingPointQueries::new(&native.table, &source_table);
		completion.complete();
		Ok(Self {
			native,
			source_table,
			queries,
		})
	}

	/// One SQL snapshot covers both representations, so a committed move cannot look absent.
	/// No source codec is selected on a native decoding failure.
	pub(crate) async fn read(
		&self,
		tx: &mut Tx<'_>,
		binding: &RelocatingPointBinding,
	) -> Result<RelocationRead, RelocatingPointError> {
		let mut completion = tx.track_state_mutation();
		tx.record_database_operation(
			DatabaseOperationKind::FetchOne,
			READ_LABEL,
			Some(self.queries.read_dual.as_str()),
		);
		let (
			native_exists,
			body,
			expires,
			updated,
			indexes,
			source_exists,
			source_body,
			source_expires,
			source_updated,
			expires_at_valid,
			updated_at_valid,
			observed_at,
		) = owned_portable_query_as::<DualRow>(self.queries.read_dual.clone())
			.bind_validated(binding.identity.namespace().as_bytes())
			.bind_validated(binding.identity.key().as_bytes())
			.bind_validated(binding.source_key.as_str())
			.fetch_one(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(READ_LABEL))?;
		let native = if native_exists {
			let indexes = indexes.ok_or_else(|| StateStorageError::InvalidRecord {
				identity: binding.identity.clone(),
				reason: "index map is null",
			})?;
			decode_physical_record(&binding.identity, (body, expires, updated, indexes))?
		} else {
			None
		};
		let source = if source_exists {
			Some(decode_source(
				binding,
				(
					source_body,
					source_expires,
					source_updated,
					expires_at_valid,
					updated_at_valid,
				),
			)?)
		} else {
			None
		};
		let snapshot = select_origin(
			binding,
			native,
			source,
			StateTimestampMicros::new(observed_at),
		)?;
		completion.complete();
		Ok(snapshot)
	}

	/// Locks native identity first and source row second, then observes authoritative time.
	/// Every after-retirement writer must use this same canonical identity mapping.
	pub(crate) async fn lock<'a, 'tx>(
		&'a self,
		tx: &'a mut WriteTx<'tx>,
		binding: &RelocatingPointBinding,
		conflict_attempts: NonZeroU32,
	) -> Result<PgRelocatingPointRecordLock<'a, 'tx>, RelocatingPointError> {
		let completion = tx.track_state_mutation();
		let (native, reservation) = self
			.native
			.acquire_identity(tx, &binding.identity, conflict_attempts)
			.await?
			.into_point();
		tx.record_database_operation(
			DatabaseOperationKind::FetchOptional,
			LOCK_SOURCE_LABEL,
			Some(self.queries.lock_source.as_str()),
		);
		let source = owned_portable_query_as::<SourceRow>(self.queries.lock_source.clone())
			.bind_validated(binding.source_key.as_str())
			.fetch_optional(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(LOCK_SOURCE_LABEL))?
			.map(|row| decode_source(binding, row))
			.transpose()?;
		let authority = match &source {
			Some(_) => RelocationAuthority::Source {
				binding: binding.clone(),
				reservation,
			},
			None => RelocationAuthority::Native {
				identity: binding.identity.clone(),
				reservation,
			},
		};
		let observed_at = PgPointStorage::observe_time(tx).await?;
		let snapshot = select_origin(binding, native, source, observed_at)?;
		Ok(PgRelocatingPointRecordLock {
			storage: self,
			tx,
			authority,
			snapshot,
			completion,
		})
	}

	async fn delete_source(
		&self,
		tx: &mut WriteTx<'_>,
		binding: &RelocatingPointBinding,
	) -> Result<(), RelocatingPointError> {
		tx.record_database_operation(
			DatabaseOperationKind::Execute,
			DELETE_SOURCE_LABEL,
			Some(self.queries.delete_source.as_str()),
		);
		let affected = owned_portable_query(self.queries.delete_source.clone())
			.bind_validated(binding.source_key.as_str())
			.execute(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(DELETE_SOURCE_LABEL))?
			.rows_affected();
		if affected != 1 {
			return Err(RelocatingPointError::SourceMutationRowMissing {
				identity: binding.identity.clone(),
			});
		}
		Ok(())
	}
}

pub(crate) type PgRelocatingPointRecordHeaderLock<'a, 'tx> =
	PgRelocatingPointRecordLock<'a, 'tx, RecordHeaderObservation>;
impl PgRelocatingPointStorage {
	pub(crate) async fn lock_header<'a, 'tx>(
		&'a self,
		tx: &'a mut WriteTx<'tx>,
		binding: &RelocatingPointBinding,
		conflict_attempts: NonZeroU32,
	) -> Result<PgRelocatingPointRecordHeaderLock<'a, 'tx>, RelocatingPointError> {
		use super::super::acquisition::{
			AcquiredNativeRecord, NativeRecordAcquisitionFields, NativeRecordProjection,
		};
		let completion = tx.track_state_mutation();
		let native = self
			.native
			.acquire_projected_identity(NativeRecordAcquisitionFields {
				tx,
				identity: &binding.identity,
				conflict_attempts,
				projection: NativeRecordProjection::Header,
			})
			.await?;
		let AcquiredNativeRecord::Header(header) = native else {
			unreachable!("header acquisition returns a header")
		};
		let (header, reservation) = header.into_point();
		tx.record_database_operation(
			DatabaseOperationKind::FetchOptional,
			LOCK_SOURCE_LABEL,
			Some(self.queries.lock_source_header.as_str()),
		);
		let source = owned_portable_query_as::<(bool, Option<i64>, Option<i64>, bool, bool)>(
			self.queries.lock_source_header.clone(),
		)
		.bind_validated(binding.source_key.as_str())
		.fetch_optional(tx.inner.as_mut())
		.await
		.map_err(|error| error.with_operation_label(LOCK_SOURCE_LABEL))?
		.map(
			|(body_present, expires_at, updated_at, expires_at_valid, updated_at_valid)| {
				decode_source_header(SourcePhysicalHeaderFields {
					body_present,
					expires_at,
					updated_at,
					expires_at_valid,
					updated_at_valid,
				})
				.map_err(|error| source_error(binding, error))
			},
		)
		.transpose()?;
		if header.is_some() && source.is_some() {
			return Err(RelocatingPointError::DuplicatePhysicalRecords {
				identity: binding.identity.clone(),
			});
		}
		let authority = match source {
			Some(_) => RelocationAuthority::Source {
				binding: binding.clone(),
				reservation,
			},
			None => RelocationAuthority::Native {
				identity: binding.identity.clone(),
				reservation,
			},
		};
		let observed_at = PgPointStorage::observe_time(tx).await?;
		Ok(PgRelocatingPointRecordLock {
			storage: self,
			tx,
			authority,
			snapshot: RecordHeaderObservation::new(
				header.map(|h| h.metadata).or(source),
				observed_at,
			),
			completion,
		})
	}
}
impl PgRelocatingPointRecordHeaderLock<'_, '_> {
	pub(crate) fn header(&self) -> RecordHeaderObservation {
		self.snapshot
	}
	pub(crate) async fn delete(self) -> Result<bool, RelocatingPointError> {
		let existed = self.snapshot.metadata().is_some();
		self.delete_value().await?;
		Ok(existed)
	}

	pub(in crate::records) async fn update_value_metadata(
		mut self,
		update: EncodedMetadataUpdate,
	) -> Result<(), RelocatingPointError> {
		if self.snapshot.metadata().is_none() {
			return Err(StateStorageError::MetadataValueAbsent {
				identity: self.authority.identity().clone(),
			}
			.into());
		}
		if let Some(binding) = self.authority.source_binding() {
			const LABEL: &str = "db.state_storage.relocating_point.relocate_metadata";
			let query = self.storage.queries.relocate_metadata.clone();
			self.tx.record_database_operation(
				DatabaseOperationKind::Execute,
				LABEL,
				Some(query.as_str()),
			);
			let metadata = update.metadata();
			let indexes = encode_index_fields(update.index_fields());
			let affected = owned_portable_query(query)
				.bind_validated(self.authority.identity().namespace().as_bytes())
				.bind_validated(self.authority.identity().key().as_bytes())
				.bind_validated(binding.source_key.as_str())
				.bind_validated(metadata.expires_at.map(StateTimestampMicros::as_i64))
				.bind_validated(metadata.updated_at.as_i64())
				.bind_validated(indexes.as_str())
				.execute(self.tx.inner.as_mut())
				.await
				.map_err(|error| error.with_operation_label(LABEL))?
				.rows_affected();
			PgPointStorage::require_mutation_row(self.authority.identity(), affected)?;
			self.storage.delete_source(self.tx, binding).await?;
		} else {
			self.storage
				.native
				.update_record_metadata(self.tx, self.authority.identity(), &update)
				.await?;
		}
		self.completion.complete();
		Ok(())
	}
}

impl RelocatingPointRecordLock for PgRelocatingPointRecordLock<'_, '_> {
	type Error = RelocatingPointError;
	fn snapshot(&self) -> &RelocationRead {
		&self.snapshot
	}
	async fn write(self, record: EncodedRecord) -> Result<(), Self::Error> {
		self.write_value(record).await
	}
	async fn delete(self) -> Result<bool, Self::Error> {
		let existed = self.snapshot.record().origin() != PointRecordOrigin::Absent;
		self.delete_value().await?;
		Ok(existed)
	}
	async fn keep(self) -> Result<(), Self::Error> {
		self.keep_value().await
	}
}

fn decode_source(
	binding: &RelocatingPointBinding,
	row: SourceRow,
) -> Result<SourcePointRecord, RelocatingPointError> {
	decode_source_record(row)
		.map(|record| record.into_source())
		.map_err(|error| source_error(binding, error))
}

fn select_origin(
	binding: &RelocatingPointBinding,
	native: Option<StoredRecord>,
	source: Option<SourcePointRecord>,
	observed_at: StateTimestampMicros,
) -> Result<RelocationRead, RelocatingPointError> {
	let record = match (native, source) {
		(Some(_), Some(_)) => {
			return Err(RelocatingPointError::DuplicatePhysicalRecords {
				identity: binding.identity.clone(),
			});
		}
		(Some(record), None) => RelocationRecord::Native(record),
		(None, Some(record)) => RelocationRecord::Source(record),
		(None, None) => RelocationRecord::Absent,
	};
	Ok(RelocationRead::new(record, observed_at))
}

impl RelocatingPointQueries {
	fn new(native: &PgQualifiedTableName, source: &PgQualifiedTableName) -> Self {
		let mut relocate_metadata = AuditedSqlBuilder::new();
		relocate_metadata.push_text("UPDATE ONLY ");
		relocate_metadata.push_argument(native);
		relocate_metadata.push_text(" AS native SET body = source.value, expires_at = $4, updated_at = $5, index_fields = $6::pg_catalog.jsonb FROM ONLY ");
		relocate_metadata.push_argument(source);
		relocate_metadata.push_text(" AS source WHERE native.namespace OPERATOR(pg_catalog.=) $1 AND native.key OPERATOR(pg_catalog.=) $2 AND source.key OPERATOR(pg_catalog.=) $3");
		Self {
			lock_source: source_snapshot_query(source, SourceSnapshotQueryKind::RelocationLock),
			lock_source_header: source_snapshot_query(
				source,
				SourceSnapshotQueryKind::RelocationHeaderLock,
			),
			read_dual: source_snapshot_query(
				source,
				SourceSnapshotQueryKind::RelocationRead { native },
			),
			delete_source: source_delete_query(source),
			relocate_metadata: relocate_metadata.finish(),
		}
	}
}

pub(in crate::records::postgres) fn source_error(
	binding: &RelocatingPointBinding,
	error: SourcePhysicalError,
) -> RelocatingPointError {
	match error {
		SourcePhysicalError::TimestampUnrepresentable { field } => {
			RelocatingPointError::SourceTimestampUnrepresentable {
				identity: binding.identity.clone(),
				field,
			}
		}
		SourcePhysicalError::Invalid { reason } => RelocatingPointError::InvalidSource {
			identity: binding.identity.clone(),
			reason,
		},
	}
}

impl<Observation> PgRelocatingPointRecordLock<'_, '_, Observation> {
	pub(in crate::records) async fn write_value(
		mut self,
		record: EncodedRecord,
	) -> Result<(), RelocatingPointError> {
		let indexes = encode_index_fields(record.record().index_fields());
		self.storage
			.native
			.write_record(self.tx, self.authority.identity(), &record, &indexes)
			.await?;
		if let Some(binding) = self.authority.source_binding() {
			self.storage.delete_source(self.tx, binding).await?;
		}
		self.completion.complete();
		Ok(())
	}
	pub(in crate::records) async fn keep_value(mut self) -> Result<(), RelocatingPointError> {
		if matches!(self.authority.reservation(), ReservationOwnership::Inserted) {
			self.storage
				.native
				.delete_record(self.tx, self.authority.identity(), true)
				.await?;
		}
		self.completion.complete();
		Ok(())
	}
	pub(in crate::records) async fn delete_value(mut self) -> Result<(), RelocatingPointError> {
		self.storage
			.native
			.delete_record(self.tx, self.authority.identity(), false)
			.await?;
		if let Some(binding) = self.authority.source_binding() {
			self.storage.delete_source(self.tx, binding).await?;
		}
		self.completion.complete();
		Ok(())
	}
}
