//! Canonical group operations shared by internal aggregate owners.
//!
//! The caller retains the same transaction, admission pins, and unfinished mutation token.
//! These helpers neither observe time nor complete an aggregate after partial application.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;

use crate::records::{
	EncodedRecord, InvalidGroupMutationError, RecordGroupRead, RecordIdentity, RecordMutation,
	RecordMutationPlan, StateTimestampMicros, StoredRecord,
};

use super::{
	HeldNativeRow, PgPointStorage, StateMutationCompletion, StateStorageError, WriteTx,
	encode_index_fields,
};

pub(in crate::records) struct AcquiredPgRecordGroup {
	pub(in crate::records) records: BTreeMap<RecordIdentity, HeldNativeRow<StoredRecord>>,
}
impl AcquiredPgRecordGroup {
	/// The identities for which this acquisition inserted a reservation row.
	pub(in crate::records) fn inserted_reservations(&self) -> BTreeSet<RecordIdentity> {
		self.records
			.iter()
			.filter(|(_, row)| row.is_inserted_reservation())
			.map(|(identity, _)| identity.clone())
			.collect()
	}
}

/// The actual current member remains identified when its driver error has no record key.
#[derive(Debug, thiserror::Error)]
#[error("native group storage failed for record {identity:?}")]
pub(crate) struct PgRecordGroupStorageError {
	pub(crate) identity: RecordIdentity,
	#[source]
	pub(crate) source: StateStorageError,
}

enum PreparedPgMutation {
	Write {
		encoded: EncodedRecord,
		index_fields: String,
	},
	Delete,
	Keep,
}

/// Every held identity has one prepared action, including omitted Keep actions.
pub(crate) struct PreparedPgRecordGroupMutation {
	actions: BTreeMap<RecordIdentity, PreparedPgMutation>,
}
impl PreparedPgRecordGroupMutation {
	pub(crate) fn prepare(
		snapshot: &RecordGroupRead,
		actions: Vec<(RecordIdentity, RecordMutation)>,
	) -> Result<Self, InvalidGroupMutationError> {
		Self::prepare_records(snapshot.snapshots(), actions)
	}
	/// Prepares actual held identity maps, including body-free physical observations.
	pub(super) fn prepare_records<R>(
		records: &BTreeMap<RecordIdentity, R>,
		actions: Vec<(RecordIdentity, RecordMutation)>,
	) -> Result<Self, InvalidGroupMutationError> {
		let plan = RecordMutationPlan::prepare_records(records, actions)?;
		let mut actions: BTreeMap<_, _> = plan
			.into_actions()
			.into_iter()
			.map(|(identity, action)| {
				let action = match action {
					RecordMutation::Write(encoded) => PreparedPgMutation::Write {
						index_fields: encode_index_fields(encoded.record().index_fields()),
						encoded,
					},
					RecordMutation::Delete => PreparedPgMutation::Delete,
					RecordMutation::Keep => PreparedPgMutation::Keep,
				};
				(identity, action)
			})
			.collect();
		for identity in records.keys() {
			actions
				.entry(identity.clone())
				.or_insert(PreparedPgMutation::Keep);
		}
		Ok(Self { actions })
	}
	/// Derives cleanup from actual prepared Keep actions and this acquisition's new reservations.
	pub(crate) fn inserted_keep_reservations(
		&self,
		inserted: &BTreeSet<RecordIdentity>,
	) -> BTreeSet<RecordIdentity> {
		inserted
			.iter()
			.filter(|identity| {
				matches!(self.actions.get(*identity), Some(PreparedPgMutation::Keep))
			})
			.cloned()
			.collect()
	}
}

/// Canonical native group authority paired with an external v10 source observation.
pub(crate) struct PgExternalSourceGroupLock<'a, 'tx> {
	storage: &'a PgPointStorage,
	tx: &'a mut WriteTx<'tx>,
	acquired: AcquiredPgRecordGroup,
	completion: StateMutationCompletion,
}

impl<'a, 'tx> PgExternalSourceGroupLock<'a, 'tx> {
	pub(crate) fn records_and_source_tx(
		&mut self,
	) -> (
		&BTreeMap<RecordIdentity, HeldNativeRow<StoredRecord>>,
		&mut WriteTx<'tx>,
	) {
		(&self.acquired.records, self.tx)
	}

	pub(crate) fn into_observed(
		self,
		observed_at: StateTimestampMicros,
	) -> PgObservedExternalSourceGroupLock<'a, 'tx> {
		let inserted = self.acquired.inserted_reservations();
		let snapshot = RecordGroupRead::new(self.acquired.records, observed_at);
		PgObservedExternalSourceGroupLock {
			storage: self.storage,
			tx: self.tx,
			snapshot,
			inserted,
			completion: self.completion,
		}
	}

	pub(crate) async fn abort_and_cleanup(mut self) -> Result<(), StateStorageError> {
		let inserted = self.acquired.inserted_reservations();
		self.storage
			.cleanup_group_reservations(self.tx, &inserted)
			.await?;
		self.completion.complete();
		Ok(())
	}
}

/// Completed native-plus-source group lock ready to keep or atomically apply native and source changes.
pub(crate) struct PgObservedExternalSourceGroupLock<'a, 'tx> {
	storage: &'a PgPointStorage,
	tx: &'a mut WriteTx<'tx>,
	snapshot: RecordGroupRead,
	inserted: BTreeSet<RecordIdentity>,
	completion: StateMutationCompletion,
}

pub(crate) struct PgAppliedExternalSourceGroup<'a, 'tx> {
	storage: &'a PgPointStorage,
	tx: &'a mut WriteTx<'tx>,
	cleanup: BTreeSet<RecordIdentity>,
	completion: StateMutationCompletion,
}

impl<'tx> PgAppliedExternalSourceGroup<'_, 'tx> {
	pub(crate) fn source_tx(&mut self) -> &mut WriteTx<'tx> {
		self.tx
	}

	pub(crate) async fn cleanup_and_finish(mut self) -> Result<(), StateStorageError> {
		self.storage
			.cleanup_group_reservations(self.tx, &self.cleanup)
			.await?;
		self.completion.complete();
		Ok(())
	}
}

impl<'a, 'tx> PgObservedExternalSourceGroupLock<'a, 'tx> {
	pub(crate) fn snapshot(&self) -> &RecordGroupRead {
		&self.snapshot
	}

	pub(crate) fn inserted_reservations(&self) -> &BTreeSet<RecordIdentity> {
		&self.inserted
	}

	pub(crate) async fn keep(mut self) -> Result<(), StateStorageError> {
		self.storage
			.cleanup_group_reservations(self.tx, &self.inserted)
			.await?;
		self.completion.complete();
		Ok(())
	}

	pub(crate) async fn apply_native_before_source(
		self,
		native: PreparedPgRecordGroupMutation,
		cleanup: BTreeSet<RecordIdentity>,
	) -> Result<PgAppliedExternalSourceGroup<'a, 'tx>, StateStorageError> {
		self.storage.apply_group_changes(self.tx, native).await?;
		Ok(PgAppliedExternalSourceGroup {
			storage: self.storage,
			tx: self.tx,
			cleanup,
			completion: self.completion,
		})
	}
}

/// Canonical native group lock for bounded upgrade conversion batches.
pub(crate) struct PgRecordConversionBatchLock<'a, 'tx> {
	storage: &'a PgPointStorage,
	tx: &'a mut WriteTx<'tx>,
	records: BTreeMap<RecordIdentity, HeldNativeRow<StoredRecord>>,
	unused_reservations: BTreeSet<RecordIdentity>,
	completion: StateMutationCompletion,
}

pub(crate) struct PgWrittenConversionBatch<'a, 'tx> {
	storage: &'a PgPointStorage,
	tx: &'a mut WriteTx<'tx>,
	unused_reservations: BTreeSet<RecordIdentity>,
	completion: StateMutationCompletion,
}

impl<'a, 'tx> PgRecordConversionBatchLock<'a, 'tx> {
	pub(crate) fn into_records_and_guard(
		self,
	) -> (
		BTreeMap<RecordIdentity, HeldNativeRow<StoredRecord>>,
		PgRecordConversionWriteGuard<'a, 'tx>,
	) {
		(
			self.records,
			PgRecordConversionWriteGuard {
				storage: self.storage,
				tx: self.tx,
				unused_reservations: self.unused_reservations,
				completion: self.completion,
			},
		)
	}
}

pub(crate) struct PgRecordConversionWriteGuard<'a, 'tx> {
	storage: &'a PgPointStorage,
	tx: &'a mut WriteTx<'tx>,
	unused_reservations: BTreeSet<RecordIdentity>,
	completion: StateMutationCompletion,
}

impl<'a, 'tx> PgRecordConversionWriteGuard<'a, 'tx> {
	pub(crate) async fn write_batch(
		mut self,
		prepared: Vec<(RecordIdentity, EncodedRecord)>,
	) -> Result<PgWrittenConversionBatch<'a, 'tx>, StateStorageError> {
		for (identity, _) in &prepared {
			self.unused_reservations.remove(identity);
		}
		self.storage.write_records(self.tx, prepared).await?;
		Ok(PgWrittenConversionBatch {
			storage: self.storage,
			tx: self.tx,
			unused_reservations: self.unused_reservations,
			completion: self.completion,
		})
	}

	pub(crate) async fn write_each_and_cleanup(
		mut self,
		prepared: Vec<(RecordIdentity, EncodedRecord, String)>,
	) -> Result<(), StateStorageError> {
		for (identity, encoded, projections) in prepared {
			self.storage
				.write_record(self.tx, &identity, &encoded, &projections)
				.await?;
			self.unused_reservations.remove(&identity);
		}
		self.storage
			.cleanup_group_reservations(self.tx, &self.unused_reservations)
			.await?;
		self.completion.complete();
		Ok(())
	}
}

impl PgWrittenConversionBatch<'_, '_> {
	pub(crate) async fn cleanup_reservations(mut self) -> Result<(), StateStorageError> {
		self.storage
			.cleanup_group_reservations(self.tx, &self.unused_reservations)
			.await?;
		self.completion.complete();
		Ok(())
	}
}

impl PgPointStorage {
	pub(crate) async fn lock_group_for_external_source<'a, 'tx>(
		&'a self,
		tx: &'a mut WriteTx<'tx>,
		identities: BTreeSet<RecordIdentity>,
		conflict_attempts: NonZeroU32,
	) -> Result<PgExternalSourceGroupLock<'a, 'tx>, StateStorageError> {
		let completion = tx.track_state_mutation();
		let acquired = self
			.acquire_group_identities(tx, identities, conflict_attempts)
			.await?;
		Ok(PgExternalSourceGroupLock {
			storage: self,
			tx,
			acquired,
			completion,
		})
	}

	pub(crate) async fn lock_conversion_batch<'a, 'tx>(
		&'a self,
		tx: &'a mut WriteTx<'tx>,
		identities: BTreeSet<RecordIdentity>,
		conflict_attempts: NonZeroU32,
	) -> Result<PgRecordConversionBatchLock<'a, 'tx>, StateStorageError> {
		let completion = tx.track_state_mutation();
		let acquired = self
			.acquire_group_identities(tx, identities, conflict_attempts)
			.await?;
		let unused_reservations = acquired.inserted_reservations();
		Ok(PgRecordConversionBatchLock {
			storage: self,
			tx,
			records: acquired.records,
			unused_reservations,
			completion,
		})
	}

	/// Acquires only the supplied complete set, in bytewise order, before source-row locks.
	pub(in crate::records) async fn acquire_group_identities(
		&self,
		tx: &mut WriteTx<'_>,
		identities: BTreeSet<RecordIdentity>,
		conflict_attempts: NonZeroU32,
	) -> Result<AcquiredPgRecordGroup, StateStorageError> {
		let mut records = BTreeMap::new();
		for identity in identities {
			let row = self
				.acquire_identity(tx, &identity, conflict_attempts)
				.await?;
			records.insert(identity, row);
		}
		Ok(AcquiredPgRecordGroup { records })
	}

	/// Applies a fully prepared plan without completing the caller's aggregate token.
	pub(in crate::records) async fn apply_group_mutation(
		&self,
		tx: &mut WriteTx<'_>,
		prepared: PreparedPgRecordGroupMutation,
		inserted_reservations: &BTreeSet<RecordIdentity>,
	) -> Result<(), StateStorageError> {
		let cleanup = prepared.inserted_keep_reservations(inserted_reservations);
		self.apply_group_changes(tx, prepared).await?;
		self.cleanup_group_reservations(tx, &cleanup).await
	}

	/// Applies native changes while retaining new Keep reservations for an owning source aggregate.
	/// The same owner must clean its exact prepared subset before completing its mutation token.
	pub(in crate::records) async fn apply_group_changes(
		&self,
		tx: &mut WriteTx<'_>,
		prepared: PreparedPgRecordGroupMutation,
	) -> Result<(), StateStorageError> {
		self.apply_group_changes_identified(tx, prepared)
			.await
			.map_err(|error| error.source)
	}

	/// Uses the same mutation loop while retaining the exact member for aggregate diagnostics.
	pub(in crate::records) async fn apply_group_changes_identified(
		&self,
		tx: &mut WriteTx<'_>,
		prepared: PreparedPgRecordGroupMutation,
	) -> Result<(), PgRecordGroupStorageError> {
		for (identity, action) in prepared.actions {
			let result = match action {
				PreparedPgMutation::Write {
					encoded,
					index_fields,
				} => {
					self.write_record(tx, &identity, &encoded, &index_fields)
						.await
				}
				PreparedPgMutation::Delete => self.delete_record(tx, &identity, false).await,
				PreparedPgMutation::Keep => Ok(()),
			};
			result.map_err(|source| PgRecordGroupStorageError { identity, source })?;
		}
		Ok(())
	}

	/// Cleans only this attempt's new reservations; the caller still owns its token and locks.
	pub(in crate::records) async fn cleanup_group_reservations(
		&self,
		tx: &mut WriteTx<'_>,
		inserted_reservations: &BTreeSet<RecordIdentity>,
	) -> Result<(), StateStorageError> {
		self.cleanup_group_reservations_identified(tx, inserted_reservations)
			.await
			.map_err(|error| error.source)
	}

	/// Retains exact reservation identity without changing cleanup semantics or query count.
	pub(in crate::records) async fn cleanup_group_reservations_identified(
		&self,
		tx: &mut WriteTx<'_>,
		inserted_reservations: &BTreeSet<RecordIdentity>,
	) -> Result<(), PgRecordGroupStorageError> {
		for identity in inserted_reservations {
			self.delete_record(tx, identity, true)
				.await
				.map_err(|source| PgRecordGroupStorageError {
					identity: identity.clone(),
					source,
				})?;
		}
		Ok(())
	}
}
