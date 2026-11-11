//! Transaction-scoped relation pins for a configured native control record.
//!
//! Runtime integration supplies a validated dedicated control container. This adapter reads
//! physical records under relation locks; it establishes no semantic admission, registration,
//! policy generation, or participant authority. Effective changes retain an exclusive pin;
//! pending-work status updates use a restricted shared-pin row guard.
//! Application savepoint rollback can release pins; this adapter caches no admission state.

use std::marker::PhantomData;
use std::num::NonZeroU32;

use crate::db::bytea::Bytea;
use crate::db::{
	AuditedSql, DatabaseError, DatabaseOperationKind, SchemaError, Tx, WriteTx,
	owned_portable_query_as,
};
use crate::records::postgres::{
	PgPinnedControlRowLock, PgPointStorage, StateMutationCompletion, StateStorageError,
	decode_physical_record, invalid_record,
};
use crate::records::{EncodedRecord, PointRecordRead, RecordIdentity, StateTimestampMicros};

const READ_PINNED_SHARED_LABEL: &str = "db.state_storage.control.read_pinned_shared";
const READ_PINNED_SHARED_WITH_POINT_LABEL: &str =
	"db.state_storage.control.read_pinned_shared_with_point";
const READ_PINNED_EXCLUSIVE_LABEL: &str = "db.state_storage.control.read_pinned_exclusive";
const LOCK_EXCLUSIVE_LABEL: &str = "db.state_storage.control.lock_exclusive";
const LOCK_PROGRESS_LABEL: &str = "db.state_storage.control.lock_progress";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ControlAcquisitionPin {
	Exclusive,
	Progress,
}

type ControlRow = (
	Option<Bytea>,
	Option<i64>,
	Option<i64>,
	Option<String>,
	bool,
	i64,
	String,
);

type ControlWithPointRow = (
	Option<Bytea>,
	Option<Bytea>,
	Option<Bytea>,
	Option<i64>,
	Option<i64>,
	Option<String>,
	bool,
	i64,
	String,
	Option<Bytea>,
	Option<i64>,
	Option<i64>,
	Option<String>,
	bool,
);

/// A shared pin of the control record that also reads one record of another container in the
/// same statement, which means that both records come from one snapshot taken after the pin,
/// in one round trip.
#[derive(Clone, Debug)]
pub(crate) struct PgSharedPinWithPoint {
	query: AuditedSql,
	point: RecordIdentity,
}

/// The control record and the other record that one shared pin read.
pub(crate) struct PinnedControlWithPoint {
	control_digest: Option<Vec<u8>>,
	point_digest: Option<Vec<u8>>,
	pub(crate) control: PointRecordRead,
	pub(crate) point: PointRecordRead,
}

/// A failed control lookup does not expose stored state, key bytes, or ambient setting text.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ControlPinError {
	#[error("native control lookup database work failed")]
	Database(#[from] DatabaseError),
	#[error("native control record physical parsing failed")]
	Record(#[from] StateStorageError),
	#[error("native control pin requires a READ COMMITTED transaction")]
	UnsupportedIsolation,
	#[error("native control lookup must return exactly one row")]
	UnexpectedObservationCount,
	#[error("the record read with the native control pin failed physical parsing")]
	PointRecord(#[source] Box<StateStorageError>),
}

impl ControlPinError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Database(error) => Some(error),
			Self::Record(error) => error.database_error(),
			Self::PointRecord(error) => error.database_error(),
			Self::UnsupportedIsolation | Self::UnexpectedObservationCount => None,
		}
	}
}

/// Immutable configured identity in one already-validated dedicated native control relation.
/// Construction performs no catalog validation; installation code supplies that prerequisite.
#[derive(Clone, Debug)]
pub(crate) struct PgControlStorage {
	native: PgPointStorage,
	identity: RecordIdentity,
	shared: AuditedSql,
	exclusive: AuditedSql,
	exclusive_acquisition: AuditedSql,
	shared_acquisition: AuditedSql,
}

/// Canonical point authority under the owning relation pin in the exact write transaction.
/// The general write API is obtained through exclusive acquisition; progress retains it privately.
/// Writes consume this guard; snapshots cannot transfer authority to another transaction.
/// Failed, cancelled, dropped, or forgotten work requires rollback through the outer tracker.
pub(crate) struct PgControlRecordLock<'a, 'tx> {
	point: PgPinnedControlRowLock<'a, 'tx>,
	pin: ControlAcquisitionPin,
	completion: StateMutationCompletion,
}

/// Evidence that the full exclusive fetch completed; it grants no semantic admission.
/// Its physical ordering remains established even when the fetched record cannot be parsed.
#[derive(Debug)]
pub(crate) struct ExclusiveControlOrderingProof {
	_identity: RecordIdentity,
}

pub(crate) struct ExclusiveControlPhysicalObservation {
	proof: ExclusiveControlOrderingProof,
	record: Result<PointRecordRead, ControlPinError>,
}

impl ExclusiveControlPhysicalObservation {
	pub(crate) fn into_parts(
		self,
	) -> (
		ExclusiveControlOrderingProof,
		Result<PointRecordRead, ControlPinError>,
	) {
		(self.proof, self.record)
	}
}

impl PgControlStorage {
	/// The actual configured point container, for installation registration and creation.
	pub(in crate::installation) fn point_storage(&self) -> &PgPointStorage {
		&self.native
	}

	pub(crate) fn shares_container(&self, storage: &PgPointStorage) -> bool {
		self.native.shares_container(storage)
	}

	pub(crate) fn matches_storage(&self, other: &Self) -> bool {
		self.identity == other.identity && self.native.shares_container(&other.native)
	}

	pub(crate) async fn validate_existing(&self, tx: &mut Tx<'_>) -> Result<(), SchemaError> {
		self.native.validate_existing(tx).await
	}

	/// Binds an existing validated container and explicit identity without database work.
	/// The relation is dedicated to control; effective changes use exclusive pins and pending-work
	/// status updates use the restricted shared-pin progress boundary.
	pub(crate) fn new(storage: PgPointStorage, identity: RecordIdentity) -> Self {
		let shared = crate::audited_sql!(
			r#"
LOCK TABLE ONLY {table} IN ACCESS SHARE MODE;
SELECT record.body, record.expires_at, record.updated_at,
    record.index_fields::pg_catalog.text, record.namespace IS NOT NULL,
    pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.clock_timestamp())
        OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8,
    pg_catalog.current_setting('transaction_isolation')
FROM (SELECT 1) observation
LEFT JOIN ONLY {table} record
    ON record.namespace OPERATOR(pg_catalog.=) $1
   AND record.key OPERATOR(pg_catalog.=) $2
LIMIT 2
"#,
			table = storage.table()
		);
		let exclusive = crate::audited_sql!(
			r#"
LOCK TABLE ONLY {table} IN ACCESS EXCLUSIVE MODE;
SELECT record.body, record.expires_at, record.updated_at,
    record.index_fields::pg_catalog.text, record.namespace IS NOT NULL,
    pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.clock_timestamp())
        OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8,
    pg_catalog.current_setting('transaction_isolation')
FROM (SELECT 1) observation
LEFT JOIN ONLY {table} record
    ON record.namespace OPERATOR(pg_catalog.=) $1
   AND record.key OPERATOR(pg_catalog.=) $2
LIMIT 2
"#,
			table = storage.table()
		);
		let exclusive_acquisition = crate::audited_sql!(
			"LOCK TABLE ONLY {table} IN ACCESS EXCLUSIVE MODE; \
             SELECT pg_catalog.current_setting('transaction_isolation')",
			table = storage.table()
		);
		let shared_acquisition = crate::audited_sql!(
			"LOCK TABLE ONLY {table} IN ACCESS SHARE MODE; \
             SELECT pg_catalog.current_setting('transaction_isolation')",
			table = storage.table()
		);
		Self {
			native: storage,
			identity,
			shared,
			exclusive,
			exclusive_acquisition,
			shared_acquisition,
		}
	}

	/// Returns the exact configured lookup identity for control-record semantic decoding.
	pub(crate) fn identity(&self) -> &RecordIdentity {
		&self.identity
	}

	/// Pins ordinary reads, including read-only transactions, against controller changes.
	/// ACCESS SHARE conflicts with controller ACCESS EXCLUSIVE and requires no row-share lock.
	/// The relation lock belongs to the transaction, not this returned snapshot or Rust borrow.
	pub(crate) async fn read_pinned_shared(
		&self,
		tx: &mut Tx<'_>,
	) -> Result<PointRecordRead, ControlPinError> {
		self.read_with_pin(tx, &self.shared, READ_PINNED_SHARED_LABEL)
			.await
	}

	/// Pins controller reads against ordinary pins and other controller transactions.
	/// This returned snapshot is a physical observation, not mutation authority.
	/// Controller writes use `lock_exclusive`, retaining the exact transaction in its guard.
	pub(crate) async fn read_pinned_exclusive(
		&self,
		tx: &mut WriteTx<'_>,
	) -> Result<PointRecordRead, ControlPinError> {
		self.read_pinned_exclusive_physical(tx).await?.record
	}

	/// Publishes physical ordering only after the complete exclusive fetch succeeds.
	/// Failed, partial, or cancelled requests establish no proof through this interface.
	pub(crate) async fn read_pinned_exclusive_physical(
		&self,
		tx: &mut WriteTx<'_>,
	) -> Result<ExclusiveControlPhysicalObservation, ControlPinError> {
		let mut completion = tx.track_state_mutation();
		let rows = self
			.fetch_with_pin(tx, &self.exclusive, READ_PINNED_EXCLUSIVE_LABEL)
			.await?;
		let record = self.parse_rows(rows);
		if record.is_ok() {
			completion.complete();
		}
		Ok(ExclusiveControlPhysicalObservation {
			proof: ExclusiveControlOrderingProof {
				_identity: self.identity.clone(),
			},
			record,
		})
	}

	/// Acquires exclusive relation authority before canonical point locking/reservation.
	/// Initial creation remains an installer decision; an absent snapshot creates no value.
	/// One outer tracker covers relation pinning, point acquisition, and final mutation/cleanup.
	pub(crate) async fn lock_exclusive<'a, 'tx>(
		&'a self,
		tx: &'a mut WriteTx<'tx>,
		conflict_attempts: NonZeroU32,
	) -> Result<PgControlRecordLock<'a, 'tx>, ControlPinError> {
		self.lock_with_pin(tx, conflict_attempts, ControlAcquisitionPin::Exclusive)
			.await
	}

	/// Acquires shared relation authority and serialized row lock for pending-upgrade progress.
	pub(crate) async fn lock_progress<'a, 'tx>(
		&'a self,
		tx: &'a mut WriteTx<'tx>,
		conflict_attempts: NonZeroU32,
	) -> Result<PgControlRecordLock<'a, 'tx>, ControlPinError> {
		self.lock_with_pin(tx, conflict_attempts, ControlAcquisitionPin::Progress)
			.await
	}

	async fn lock_with_pin<'a, 'tx>(
		&'a self,
		tx: &'a mut WriteTx<'tx>,
		conflict_attempts: NonZeroU32,
		pin: ControlAcquisitionPin,
	) -> Result<PgControlRecordLock<'a, 'tx>, ControlPinError> {
		let completion = tx.track_state_mutation();
		let (query, operation) = match pin {
			ControlAcquisitionPin::Exclusive => (&self.exclusive_acquisition, LOCK_EXCLUSIVE_LABEL),
			ControlAcquisitionPin::Progress => (&self.shared_acquisition, LOCK_PROGRESS_LABEL),
		};
		tx.record_database_operation(
			DatabaseOperationKind::FetchAll,
			operation,
			Some(query.as_str()),
		);
		let rows = owned_portable_query_as::<(String,)>(query.clone())
			.fetch_all(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(operation))?;
		let [(isolation,)] = <[(String,); 1]>::try_from(rows)
			.map_err(|_| ControlPinError::UnexpectedObservationCount)?;
		if isolation != "read committed" {
			return Err(ControlPinError::UnsupportedIsolation);
		}
		let point = self
			.native
			.lock_pinned_control_row(tx, &self.identity, conflict_attempts)
			.await?;
		Ok(PgControlRecordLock {
			point,
			pin,
			completion,
		})
	}

	/// Prepares a shared pin that also reads `point` from `point_storage`, without database
	/// work.
	pub(crate) fn shared_pin_with_point(
		&self,
		point_storage: &PgPointStorage,
		point: RecordIdentity,
	) -> PgSharedPinWithPoint {
		// PostgreSQL core has no built-in BLAKE3 function, so this query uses `pg_catalog.sha256`
		// solely to compare cached row bytes against PostgreSQL's built-in `pg_catalog.sha256(bytea)`
		// SQL function without requiring PostgreSQL extensions.
		let query = crate::audited_sql!(
			r#"
LOCK TABLE ONLY {control} IN ACCESS SHARE MODE;
WITH observed AS MATERIALIZED (
SELECT record AS control_record, point AS point_record,
    pg_catalog.sha256(record.body) AS control_digest,
    pg_catalog.sha256(point.body) AS point_digest
FROM (SELECT 1) observation
LEFT JOIN ONLY {control} record
    ON record.namespace OPERATOR(pg_catalog.=) $1
   AND record.key OPERATOR(pg_catalog.=) $2
LEFT JOIN ONLY {point_table} point
    ON point.namespace OPERATOR(pg_catalog.=) $3
   AND point.key OPERATOR(pg_catalog.=) $4
LIMIT 2
)
SELECT control_digest, point_digest,
    CASE WHEN control_digest OPERATOR(pg_catalog.=) $5::pg_catalog.bytea
        THEN NULL ELSE (control_record).body END, (control_record).expires_at, (control_record).updated_at,
    (control_record).index_fields::pg_catalog.text, (control_record).namespace IS NOT NULL,
    pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.clock_timestamp())
        OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8,
    pg_catalog.current_setting('transaction_isolation'),
    CASE WHEN point_digest OPERATOR(pg_catalog.=) $6::pg_catalog.bytea
        THEN NULL ELSE (point_record).body END, (point_record).expires_at, (point_record).updated_at,
    (point_record).index_fields::pg_catalog.text, (point_record).namespace IS NOT NULL
FROM observed

"#,
			control = self.native.table(),
			point_table = point_storage.table()
		);
		PgSharedPinWithPoint { query, point }
	}

	/// Pins the control record against controller writes and reads it together with the
	/// prepared point record, as `read_pinned_shared` and a point read would in two round
	/// trips.
	pub(crate) async fn read_pinned_shared_with_point(
		&self,
		tx: &mut Tx<'_>,
		pin: &PgSharedPinWithPoint,
		cached: Option<&PinnedControlWithPoint>,
	) -> Result<PinnedControlWithPoint, ControlPinError> {
		let mut completion = tx.track_state_mutation();
		tx.record_database_operation(
			DatabaseOperationKind::FetchAll,
			READ_PINNED_SHARED_WITH_POINT_LABEL,
			Some(pin.query.as_str()),
		);
		let rows = owned_portable_query_as::<ControlWithPointRow>(pin.query.clone())
			.bind_validated(self.identity.namespace().as_bytes())
			.bind_validated(self.identity.key().as_bytes())
			.bind_validated(pin.point.namespace().as_bytes())
			.bind_validated(pin.point.key().as_bytes())
			.bind_validated(cached.and_then(|c| c.control_digest.as_deref()))
			.bind_validated(cached.and_then(|c| c.point_digest.as_deref()))
			.fetch_all(tx.inner.as_mut())
			.await
			.map_err(|error| {
				ControlPinError::Database(
					error.with_operation_label(READ_PINNED_SHARED_WITH_POINT_LABEL),
				)
			})?;
		let [row] = <[ControlWithPointRow; 1]>::try_from(rows)
			.map_err(|_| ControlPinError::UnexpectedObservationCount)?;
		let (
			control_digest,
			point_digest,
			body,
			expires_at,
			updated_at,
			index_fields,
			exists,
			observed_at,
			isolation,
			point_body,
			point_expires_at,
			point_updated_at,
			point_index_fields,
			point_exists,
		) = row;
		// The request retains the exact cached snapshots that supplied its digests. Concurrent
		// readers may replace the runtime cache while this query is in progress.
		let control_digest = control_digest.map(|value| value.0);
		let point_digest = point_digest.map(|value| value.0);
		let body = restore_matching_body(
			body,
			&control_digest,
			cached.map(|c| (&c.control_digest, &c.control)),
		);
		let point_body = restore_matching_body(
			point_body,
			&point_digest,
			cached.map(|c| (&c.point_digest, &c.point)),
		);
		let control = self.parse_rows(vec![(
			body,
			expires_at,
			updated_at,
			index_fields,
			exists,
			observed_at,
			isolation,
		)])?;
		let point = if point_exists {
			let index_fields = point_index_fields.ok_or_else(|| {
				ControlPinError::PointRecord(Box::new(invalid_record(
					&pin.point,
					"index map is null",
				)))
			})?;
			decode_physical_record(
				&pin.point,
				(point_body, point_expires_at, point_updated_at, index_fields),
			)
			.map_err(|error| ControlPinError::PointRecord(Box::new(error)))?
		} else {
			None
		};
		completion.complete();
		Ok(PinnedControlWithPoint {
			control_digest,
			point_digest,
			control,
			point: PointRecordRead::new(point, StateTimestampMicros::new(observed_at)),
		})
	}

	async fn read_with_pin(
		&self,
		tx: &mut Tx<'_>,
		query: &AuditedSql,
		operation: &'static str,
	) -> Result<PointRecordRead, ControlPinError> {
		let mut completion = tx.track_state_mutation();
		let rows = self.fetch_with_pin(tx, query, operation).await?;
		let snapshot = self.parse_rows(rows)?;
		completion.complete();
		Ok(snapshot)
	}

	async fn fetch_with_pin(
		&self,
		tx: &mut Tx<'_>,
		query: &AuditedSql,
		operation: &'static str,
	) -> Result<Vec<ControlRow>, ControlPinError> {
		tx.record_database_operation(
			DatabaseOperationKind::FetchAll,
			operation,
			Some(query.as_str()),
		);
		owned_portable_query_as::<ControlRow>(query.clone())
			.bind_validated(self.identity.namespace().as_bytes())
			.bind_validated(self.identity.key().as_bytes())
			.fetch_all(tx.inner.as_mut())
			.await
			.map_err(|error| ControlPinError::Database(error.with_operation_label(operation)))
	}

	fn parse_rows(&self, rows: Vec<ControlRow>) -> Result<PointRecordRead, ControlPinError> {
		let [row] = <[ControlRow; 1]>::try_from(rows)
			.map_err(|_| ControlPinError::UnexpectedObservationCount)?;
		let (body, expires_at, updated_at, index_fields, exists, observed_at, isolation) = row;
		// An older transaction snapshot can predate a generation change that finished during the wait.
		if isolation != "read committed" {
			return Err(ControlPinError::UnsupportedIsolation);
		}
		let record = if exists {
			let index_fields =
				index_fields.ok_or_else(|| invalid_record(&self.identity, "index map is null"))?;
			decode_physical_record(&self.identity, (body, expires_at, updated_at, index_fields))?
		} else {
			None
		};
		let snapshot = PointRecordRead::new(record, StateTimestampMicros::new(observed_at));
		Ok(snapshot)
	}
}

/// Non-duplicable proof that the caller holds a live [`PgControlRecordLock`] in the borrowed
/// write transaction.
pub(crate) struct ControlLockProof<'lock>(PhantomData<&'lock mut ()>);

impl<'a, 'tx> PgControlRecordLock<'a, 'tx> {
	pub(crate) fn matches_exclusive_control_storage(
		&self,
		control_storage: &PgControlStorage,
	) -> bool {
		self.pin == ControlAcquisitionPin::Exclusive
			&& self.point.identity() == &control_storage.identity
			&& self.point.shares_container(&control_storage.native)
	}

	pub(crate) fn identity(&self) -> &RecordIdentity {
		self.point.identity()
	}

	pub(crate) fn companion_tx(&mut self) -> &mut WriteTx<'tx> {
		self.point.companion_tx()
	}

	pub(crate) fn companion_tx_with_proof(&mut self) -> (&mut WriteTx<'tx>, ControlLockProof<'_>) {
		(self.point.companion_tx(), ControlLockProof(PhantomData))
	}

	pub(crate) fn snapshot(&self) -> &PointRecordRead {
		self.point.snapshot()
	}

	/// Delegates the fresh clock's nested cancellation/error tracking without finishing authority.
	pub(crate) async fn observe_time(&mut self) -> Result<StateTimestampMicros, ControlPinError> {
		self.point
			.observe_time()
			.await
			.map_err(ControlPinError::Record)
	}

	/// Persists one explicit control value through the existing point mutation path.
	/// The outer tracker completes only after the point write has fully succeeded.
	pub(crate) async fn write(mut self, record: EncodedRecord) -> Result<(), ControlPinError> {
		self.point.write(record).await?;
		self.completion.complete();
		Ok(())
	}

	/// Preserves values/preexisting reservations and removes only a new absent reservation.
	pub(crate) async fn keep(mut self) -> Result<(), ControlPinError> {
		self.point.keep().await?;
		self.completion.complete();
		Ok(())
	}
}

/// This function restores a cached body only when its SHA-256 digest matched in this observation.
/// Physical metadata and database time always come from the fresh query result.
fn restore_matching_body(
	body: Option<Bytea>,
	digest: &Option<Vec<u8>>,
	cached: Option<(&Option<Vec<u8>>, &PointRecordRead)>,
) -> Option<Bytea> {
	body.or_else(|| {
		let (cached_digest, cached_snapshot) = cached?;
		(digest.is_some() && digest == cached_digest)
			.then(|| {
				cached_snapshot
					.record()
					.map(|record| Bytea(record.body().to_vec()))
			})
			.flatten()
	})
}
