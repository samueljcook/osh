//! One actual native point acquisition loop selects full bytes or validated physical headers.

use std::num::NonZeroU32;

use crate::records::{RecordIdentity, StoredRecord};

use super::{
	DatabaseOperationKind, LOCK_POINT_LABEL, NativePhysicalHeaderFields, PgPointStorage,
	PhysicalPointRow, RESERVE_POINT_LABEL, StateStorageError, WriteTx, decode_physical_header,
	decode_physical_record, owned_portable_query_as,
};

const LOCK_POINT_HEADER_LABEL: &str = "db.state_storage.lock_point_header";
type HeaderPointRow = (bool, Option<i64>, Option<i64>, String);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum NativeRecordProjection {
	Full,
	Header,
}

pub(super) enum AcquiredNativeRecord {
	Full(HeldNativeRow<StoredRecord>),
	Header(HeldNativeRow<super::NativePhysicalHeader>),
}
/// The row that holds one acquired native identity.
pub(crate) enum HeldNativeRow<V> {
	/// A stored value, which the acquisition found and locked.
	Value(V),
	/// A reservation row, which the acquisition found and locked or inserted.
	Reservation(ReservationOwnership),
}
impl<V> HeldNativeRow<V> {
	pub(crate) fn value(&self) -> Option<&V> {
		match self {
			Self::Value(value) => Some(value),
			Self::Reservation(_) => None,
		}
	}
	pub(crate) fn is_inserted_reservation(&self) -> bool {
		matches!(self, Self::Reservation(ReservationOwnership::Inserted))
	}
	/// The value and the row's ownership, as a point guard keeps them. The acquisition found
	/// and locked every value row.
	pub(crate) fn into_point(self) -> (Option<V>, ReservationOwnership) {
		match self {
			Self::Value(value) => (Some(value), ReservationOwnership::Preexisting),
			Self::Reservation(ownership) => (None, ownership),
		}
	}
	pub(super) fn without_value(&self) -> HeldNativeRow<()> {
		match self {
			Self::Value(_) => HeldNativeRow::Value(()),
			Self::Reservation(ownership) => HeldNativeRow::Reservation(*ownership),
		}
	}
	/// An inserted reservation row has no value, so `value` is `None` for it.
	fn from_decoded(value: Option<V>, ownership: ReservationOwnership) -> Self {
		match value {
			Some(value) => Self::Value(value),
			None => Self::Reservation(ownership),
		}
	}
}
/// Whether this acquisition inserted the reservation row that holds an identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReservationOwnership {
	Preexisting,
	Inserted,
}
impl ReservationOwnership {
	pub(super) fn from_inserted(inserted: bool) -> Self {
		if inserted {
			Self::Inserted
		} else {
			Self::Preexisting
		}
	}
}
pub(super) struct NativeRecordAcquisitionFields<'a, 'tx> {
	pub(super) tx: &'a mut WriteTx<'tx>,
	pub(super) identity: &'a RecordIdentity,
	pub(super) conflict_attempts: NonZeroU32,
	pub(super) projection: NativeRecordProjection,
}
enum ProjectedPointRow {
	Full(PhysicalPointRow),
	Header(HeaderPointRow),
}

impl PgPointStorage {
	/// The caller owns tracking and its eventual post-acquisition clock observation.
	pub(in crate::records) async fn acquire_identity(
		&self,
		tx: &mut WriteTx<'_>,
		identity: &RecordIdentity,
		conflict_attempts: NonZeroU32,
	) -> Result<HeldNativeRow<StoredRecord>, StateStorageError> {
		let acquired = self
			.acquire_projected_identity(NativeRecordAcquisitionFields {
				tx,
				identity,
				conflict_attempts,
				projection: NativeRecordProjection::Full,
			})
			.await?;
		match acquired {
			AcquiredNativeRecord::Full(row) => Ok(row),
			AcquiredNativeRecord::Header(_) => {
				unreachable!("Full acquisition selects full physical rows")
			}
		}
	}

	/// Only owning point/set guards call this while retaining their actual unresolved token.
	pub(super) async fn acquire_projected_identity(
		&self,
		fields: NativeRecordAcquisitionFields<'_, '_>,
	) -> Result<AcquiredNativeRecord, StateStorageError> {
		let NativeRecordAcquisitionFields {
			tx,
			identity,
			conflict_attempts,
			projection,
		} = fields;
		for _ in 0..conflict_attempts.get() {
			let existing = match projection {
				NativeRecordProjection::Full => {
					tx.record_database_operation(
						DatabaseOperationKind::FetchOptional,
						LOCK_POINT_LABEL,
						Some(self.queries.lock.as_str()),
					);
					owned_portable_query_as::<PhysicalPointRow>(self.queries.lock.clone())
						.bind_validated(identity.namespace().as_bytes())
						.bind_validated(identity.key().as_bytes())
						.fetch_optional(tx.inner.as_mut())
						.await
						.map_err(|error| error.with_operation_label(LOCK_POINT_LABEL))?
						.map(ProjectedPointRow::Full)
				}
				NativeRecordProjection::Header => {
					tx.record_database_operation(
						DatabaseOperationKind::FetchOptional,
						LOCK_POINT_HEADER_LABEL,
						Some(self.queries.lock_header.as_str()),
					);
					owned_portable_query_as::<HeaderPointRow>(self.queries.lock_header.clone())
						.bind_validated(identity.namespace().as_bytes())
						.bind_validated(identity.key().as_bytes())
						.fetch_optional(tx.inner.as_mut())
						.await
						.map_err(|error| error.with_operation_label(LOCK_POINT_HEADER_LABEL))?
						.map(ProjectedPointRow::Header)
				}
			};
			let (row, reservation) = match existing {
				Some(row) => (row, ReservationOwnership::Preexisting),
				None => {
					tx.record_database_operation(
						DatabaseOperationKind::FetchOptional,
						RESERVE_POINT_LABEL,
						Some(self.queries.reserve.as_str()),
					);
					let inserted =
						owned_portable_query_as::<PhysicalPointRow>(self.queries.reserve.clone())
							.bind_validated(identity.namespace().as_bytes())
							.bind_validated(identity.key().as_bytes())
							.fetch_optional(tx.inner.as_mut())
							.await
							.map_err(|error| error.with_operation_label(RESERVE_POINT_LABEL))?;
					let Some(row) = inserted else {
						continue;
					};
					(ProjectedPointRow::Full(row), ReservationOwnership::Inserted)
				}
			};
			let record = match (projection, row) {
				(NativeRecordProjection::Full, ProjectedPointRow::Full(row)) => {
					AcquiredNativeRecord::Full(HeldNativeRow::from_decoded(
						decode_physical_record(identity, row)?,
						reservation,
					))
				}
				(
					NativeRecordProjection::Header,
					ProjectedPointRow::Full((body, expires_at, updated_at, index_fields)),
				) => {
					let header = decode_physical_header(NativePhysicalHeaderFields {
						identity,
						body_present: body.is_some(),
						expires_at,
						updated_at,
						index_fields: &index_fields,
					})?;
					AcquiredNativeRecord::Header(HeldNativeRow::from_decoded(header, reservation))
				}
				(
					NativeRecordProjection::Header,
					ProjectedPointRow::Header((body_present, expires_at, updated_at, index_fields)),
				) => {
					let header = decode_physical_header(NativePhysicalHeaderFields {
						identity,
						body_present,
						expires_at,
						updated_at,
						index_fields: &index_fields,
					})?;
					AcquiredNativeRecord::Header(HeldNativeRow::from_decoded(header, reservation))
				}
				(NativeRecordProjection::Full, ProjectedPointRow::Header(_)) => {
					unreachable!("Full acquisition selects full physical rows")
				}
			};
			return Ok(record);
		}
		Err(StateStorageError::ConflictBudgetExhausted)
	}
}
