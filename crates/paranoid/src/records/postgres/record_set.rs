//! One canonical known set across actual native point containers.
//!
//! The fixed caller establishes installation admission and semantic/schema binding first.
//! This guard owns one transaction borrow, locks every placed identity once, and observes one
//! clock after all waits. It grants no source, publication, lease or discovery authority.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::num::NonZeroU32;

use crate::db::PgQualifiedTableName;
use crate::records::{
	InvalidGroupMutationError, PointRecordRead, RecordIdentity, RecordMutation,
	StateTimestampMicros, StoredRecord,
};

use super::attempt_boundary::{
	GroupAttemptBoundaryPhase, GroupAttemptRejectionError, RecordAttemptProgress, execute_boundary,
};
use super::group::PgRecordGroupStorageError;
use super::{PgPointStorage, StateMutationCompletion, StateStorageError, WriteTx};

mod error;
mod observation;
mod plan;
use super::acquisition::{
	AcquiredNativeRecord, NativeRecordAcquisitionFields, NativeRecordProjection,
};
pub(crate) use error::{RecordSetError, RecordSetRejectionError};
use observation::HeldRecord;
use plan::PreparedRecordSetMutation;

/// Canonical order is schema/table bytes, namespace bytes, then key bytes.
/// The table is derived from an actual target, rather than independently supplied.
#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct PgPlacedRecordIdentity {
	table: PgQualifiedTableName,
	record: RecordIdentity,
}
impl PgPlacedRecordIdentity {
	pub(crate) fn table(&self) -> &PgQualifiedTableName {
		&self.table
	}
	pub(crate) fn record(&self) -> &RecordIdentity {
		&self.record
	}
}
impl fmt::Debug for PgPlacedRecordIdentity {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("PgPlacedRecordIdentity")
			.field("record", &self.record)
			.finish_non_exhaustive()
	}
}

pub(crate) struct PgRecordTargetFields<'storage> {
	pub(crate) storage: &'storage PgPointStorage,
	pub(crate) record: RecordIdentity,
}
/// Physical association only; construction neither admits a transaction nor validates shape.
pub(crate) struct PgRecordTarget<'storage> {
	storage: &'storage PgPointStorage,
	identity: PgPlacedRecordIdentity,
	projection: NativeRecordProjection,
}
impl<'storage> PgRecordTarget<'storage> {
	/// Validates the stored header while retaining authority to replace or delete the value.
	pub(crate) fn header(fields: PgRecordTargetFields<'storage>) -> Self {
		let mut target = Self::new(fields);
		target.projection = NativeRecordProjection::Header;
		target
	}

	pub(crate) fn new(fields: PgRecordTargetFields<'storage>) -> Self {
		Self {
			storage: fields.storage,
			identity: PgPlacedRecordIdentity {
				table: fields.storage.table().clone(),
				record: fields.record,
			},
			projection: NativeRecordProjection::Full,
		}
	}
	pub(crate) fn identity(&self) -> &PgPlacedRecordIdentity {
		&self.identity
	}
}

pub(crate) struct PgRecordSetLockFields<'a, 'tx> {
	pub(crate) tx: &'a mut WriteTx<'tx>,
	pub(crate) targets: Vec<PgRecordTarget<'a>>,
	pub(crate) conflict_attempts: NonZeroU32,
}

struct AcquiringContainer<'storage> {
	storage: &'storage PgPointStorage,
	records: BTreeMap<RecordIdentity, AcquiredNativeRecord>,
}
impl AcquiringContainer<'_> {
	/// The identities for which this acquisition inserted a reservation row.
	fn inserted_reservations(&self) -> BTreeSet<RecordIdentity> {
		self.records
			.iter()
			.filter(|(_, record)| record.is_inserted_reservation())
			.map(|(identity, _)| identity.clone())
			.collect()
	}
}
struct HeldContainer<'storage> {
	storage: &'storage PgPointStorage,
	records: BTreeMap<RecordIdentity, HeldRecord>,
}
impl HeldContainer<'_> {
	/// The identities for which this acquisition inserted a reservation row.
	fn inserted_reservations(&self) -> BTreeSet<RecordIdentity> {
		self.records
			.iter()
			.filter(|(_, record)| record.is_inserted_reservation())
			.map(|(identity, _)| identity.clone())
			.collect()
	}
}

/// Completing releases the Rust borrow, while actual transaction locks remain held.
/// Drop, cancellation, forgotten guards or partial application prevent commit through the
/// transaction's unresolved-mutation tracker. No asynchronous destructor performs cleanup.
pub(crate) struct PgRecordSetLock<'a, 'tx> {
	tx: &'a mut WriteTx<'tx>,
	containers: BTreeMap<PgQualifiedTableName, HeldContainer<'a>>,
	observed_at: StateTimestampMicros,
	completion: StateMutationCompletion,
}
impl<'a, 'tx> PgRecordSetLock<'a, 'tx> {
	/// Deduplicates complete physical identities and acquires only the supplied known set.
	/// The conflict budget bounds attempts per identity, not database lock wait duration.
	pub(crate) async fn acquire(
		fields: PgRecordSetLockFields<'a, 'tx>,
	) -> Result<Self, RecordSetError> {
		if fields.targets.is_empty() {
			return Err(RecordSetError::EmptySet);
		}
		let mut acquiring = PgAcquiringRecordSet::begin(fields.tx);
		acquiring
			.acquire_known(fields.targets, fields.conflict_attempts)
			.await?;
		acquiring.finish().await
	}

	pub(crate) fn observed_at(&self) -> StateTimestampMicros {
		self.observed_at
	}
	/// Requires an actual Full target; missing identities and Header projections are typed
	/// rejections.
	pub(crate) fn snapshot(
		&self,
		identity: &PgPlacedRecordIdentity,
	) -> Result<&PointRecordRead, RecordSetRejectionError> {
		self.held_record(identity)?.full_snapshot()
	}

	fn held_record(
		&self,
		identity: &PgPlacedRecordIdentity,
	) -> Result<&HeldRecord, RecordSetRejectionError> {
		self.containers
			.get(identity.table())
			.and_then(|container| container.records.get(identity.record()))
			.ok_or_else(|| {
				RecordSetRejectionError::mutation(InvalidGroupMutationError::UnknownIdentity {
					identity: identity.record().clone(),
				})
			})
	}

	/// Omitted targets mean Keep. Full validation, projections and cleanup subsets precede writes.
	/// Pure rejection completes only after every new reservation is cleaned; locks remain held.
	pub(crate) async fn apply(
		self,
		actions: Vec<(PgPlacedRecordIdentity, RecordMutation)>,
	) -> Result<Result<(), RecordSetRejectionError>, RecordSetError> {
		self.apply_with_ending(actions, RecordSetEnding::RetainLocks)
			.await
	}

	async fn apply_with_ending(
		mut self,
		actions: Vec<(PgPlacedRecordIdentity, RecordMutation)>,
		ending: RecordSetEnding,
	) -> Result<Result<(), RecordSetRejectionError>, RecordSetError> {
		let prepared = match PreparedRecordSetMutation::prepare(&self.containers, actions) {
			Ok(prepared) => prepared,
			Err(rejection) => {
				let cleanup = match ending {
					RecordSetEnding::RetainLocks => self.cleanup_new_reservations().await,
					RecordSetEnding::ReleaseAttempt => {
						execute_boundary(self.tx, GroupAttemptBoundaryPhase::RestoreAndRelease)
							.await
							.map_err(RecordSetError::from)
					}
				};
				if let Err(cleanup) = cleanup {
					return Err(RecordSetError::RejectionCleanup {
						rejection_error: Box::new(rejection),
						cleanup_error: Box::new(cleanup),
					});
				}
				self.completion.complete();
				return Ok(Err(rejection));
			}
		};
		for (table, changes) in prepared.changes {
			self.containers[&table]
				.storage
				.apply_group_changes_identified(self.tx, changes)
				.await
				.map_err(|error| placed_failure(&table, error))?;
		}
		for (table, cleanup) in prepared.cleanup {
			self.containers[&table]
				.storage
				.cleanup_group_reservations_identified(self.tx, &cleanup)
				.await
				.map_err(|error| placed_failure(&table, error))?;
		}
		if let RecordSetEnding::ReleaseAttempt = ending {
			execute_boundary(self.tx, GroupAttemptBoundaryPhase::Release).await?;
		}
		self.completion.complete();
		Ok(Ok(()))
	}

	/// Removes all new reservations and preserves every valued row and preexisting reservation.
	pub(crate) async fn keep(mut self) -> Result<(), RecordSetError> {
		self.cleanup_new_reservations().await?;
		self.completion.complete();
		Ok(())
	}

	async fn cleanup_new_reservations(&mut self) -> Result<(), RecordSetError> {
		for (table, container) in &self.containers {
			container
				.storage
				.cleanup_group_reservations_identified(self.tx, &container.inserted_reservations())
				.await
				.map_err(|error| placed_failure(table, error))?;
		}
		Ok(())
	}
}

#[derive(Clone, Copy)]
enum RecordSetEnding {
	RetainLocks,
	ReleaseAttempt,
}

/// Only fixed storage owners can discover a later identity or restore this un-timed set.
/// No snapshot claims a clock until finish consumes this owner.
pub(crate) struct PgAcquiringRecordSet<'a, 'tx> {
	tx: &'a mut WriteTx<'tx>,
	containers: BTreeMap<PgQualifiedTableName, AcquiringContainer<'a>>,
	greatest: Option<PgPlacedRecordIdentity>,
	completion: StateMutationCompletion,
}
impl<'a, 'tx> PgAcquiringRecordSet<'a, 'tx> {
	pub(crate) fn begin(tx: &'a mut WriteTx<'tx>) -> Self {
		let completion = tx.track_state_mutation();
		Self {
			tx,
			containers: BTreeMap::new(),
			greatest: None,
			completion,
		}
	}
	pub(crate) async fn begin_attempt(
		tx: &'a mut WriteTx<'tx>,
	) -> Result<PgAcquiringRecordSetAttempt<'a, 'tx>, RecordSetError> {
		let acquiring = Self::begin(tx);
		execute_boundary(acquiring.tx, GroupAttemptBoundaryPhase::Create).await?;
		Ok(PgAcquiringRecordSetAttempt { acquiring })
	}
	/// Every new target must follow every existing target; already held identities are skipped.
	pub(crate) async fn acquire_known(
		&mut self,
		targets: Vec<PgRecordTarget<'a>>,
		conflict_attempts: NonZeroU32,
	) -> Result<(), RecordSetError> {
		let mut parsed = BTreeMap::new();
		for target in targets {
			let entry = parsed
				.entry(target.identity)
				.or_insert((target.storage, target.projection));
			entry.0 = target.storage;
			if target.projection == NativeRecordProjection::Full {
				entry.1 = NativeRecordProjection::Full;
			}
		}
		let targets = parsed;

		if targets.is_empty() {
			return Err(RecordSetError::EmptySet);
		}
		for (identity, (_, requested)) in &targets {
			if let Some(held) = self.projection(identity) {
				if held == NativeRecordProjection::Header
					&& *requested == NativeRecordProjection::Full
				{
					return Err(RecordSetError::HeaderToFullUpgrade {
						identity: Box::new(identity.clone()),
					});
				}
				continue;
			}
			if self
				.greatest
				.as_ref()
				.is_some_and(|greatest| identity <= greatest)
			{
				return Err(RecordSetError::NonCanonicalExtension {
					identity: Box::new(identity.clone()),
				});
			}
		}
		for (identity, (storage, projection)) in targets {
			if self.contains(&identity) {
				continue;
			}
			let acquired = storage
				.acquire_projected_identity(NativeRecordAcquisitionFields {
					tx: &mut *self.tx,
					identity: &identity.record,
					conflict_attempts,
					projection,
				})
				.await
				.map_err(|error| RecordSetError::Storage {
					identity: Box::new(identity.clone()),
					source: Box::new(error),
				})?;
			self.greatest = Some(identity.clone());
			let container =
				self.containers
					.entry(identity.table)
					.or_insert_with(|| AcquiringContainer {
						storage,
						records: BTreeMap::new(),
					});
			container.records.insert(identity.record, acquired);
		}
		Ok(())
	}
	fn projection(&self, identity: &PgPlacedRecordIdentity) -> Option<NativeRecordProjection> {
		self.containers
			.get(identity.table())
			.and_then(|container| container.records.get(identity.record()))
			.map(AcquiredNativeRecord::projection)
	}
	fn contains(&self, identity: &PgPlacedRecordIdentity) -> bool {
		self.containers
			.get(identity.table())
			.is_some_and(|container| container.records.contains_key(identity.record()))
	}
	pub(crate) fn record(
		&self,
		identity: &PgPlacedRecordIdentity,
	) -> Result<Option<&StoredRecord>, RecordSetRejectionError> {
		self.containers
			.get(identity.table())
			.and_then(|container| container.records.get(identity.record()))
			.ok_or_else(|| {
				RecordSetRejectionError::mutation(InvalidGroupMutationError::UnknownIdentity {
					identity: identity.record().clone(),
				})
			})?
			.full_record()
	}

	pub(crate) async fn finish(self) -> Result<PgRecordSetLock<'a, 'tx>, RecordSetError> {
		let observed_at = PgPointStorage::observe_time(self.tx)
			.await
			.map_err(RecordSetError::Clock)?;
		let containers = self
			.containers
			.into_iter()
			.map(|(table, container)| {
				(
					table,
					HeldContainer {
						storage: container.storage,
						records: container
							.records
							.into_iter()
							.map(|(identity, record)| (identity, record.finish(observed_at)))
							.collect(),
					},
				)
			})
			.collect();
		Ok(PgRecordSetLock {
			tx: self.tx,
			containers,
			observed_at,
			completion: self.completion,
		})
	}
	pub(crate) async fn keep(mut self) -> Result<(), RecordSetError> {
		for (table, container) in &self.containers {
			container
				.storage
				.cleanup_group_reservations_identified(self.tx, &container.inserted_reservations())
				.await
				.map_err(|error| placed_failure(table, error))?;
		}
		self.completion.complete();
		Ok(())
	}
}

/// This owner retains the actual savepoint and unresolved mutation token during acquisition.
pub(crate) struct PgAcquiringRecordSetAttempt<'a, 'tx> {
	acquiring: PgAcquiringRecordSet<'a, 'tx>,
}
impl<'a, 'tx> PgAcquiringRecordSetAttempt<'a, 'tx> {
	pub(crate) fn records(&self) -> &PgAcquiringRecordSet<'a, 'tx> {
		&self.acquiring
	}
	pub(crate) async fn acquire_known(
		&mut self,
		targets: Vec<PgRecordTarget<'a>>,
		conflict_attempts: NonZeroU32,
	) -> Result<(), RecordSetError> {
		self.acquiring
			.acquire_known(targets, conflict_attempts)
			.await
	}
	/// Known restoration discards old records and reservations before sorted reacquisition.
	/// Budget rejection completes only after restoration; any error retains the token.
	pub(crate) async fn restore_and_advance(
		mut self,
		progress: &mut RecordAttemptProgress<PgPlacedRecordIdentity>,
		required: BTreeSet<PgPlacedRecordIdentity>,
	) -> Result<Result<Self, GroupAttemptRejectionError>, RecordSetError> {
		if let Err(rejection) = progress
			.restore_and_advance(self.acquiring.tx, required)
			.await?
		{
			self.acquiring.completion.complete();
			return Ok(Err(rejection));
		}
		self.acquiring.containers.clear();
		self.acquiring.greatest = None;
		Ok(Ok(self))
	}
	pub(crate) async fn finish(self) -> Result<PgRecordSetAttempt<'a, 'tx>, RecordSetError> {
		Ok(PgRecordSetAttempt {
			set: self.acquiring.finish().await?,
		})
	}
	pub(crate) async fn restore(mut self) -> Result<(), RecordSetError> {
		execute_boundary(
			self.acquiring.tx,
			GroupAttemptBoundaryPhase::RestoreAndRelease,
		)
		.await?;
		self.acquiring.completion.complete();
		Ok(())
	}
}

/// A complete observed record set retains its actual attempt until application or restoration.
pub(crate) struct PgRecordSetAttempt<'a, 'tx> {
	set: PgRecordSetLock<'a, 'tx>,
}
impl PgRecordSetAttempt<'_, '_> {
	pub(crate) fn observed_at(&self) -> StateTimestampMicros {
		self.set.observed_at()
	}
	pub(crate) async fn apply(
		self,
		actions: Vec<(PgPlacedRecordIdentity, RecordMutation)>,
	) -> Result<Result<(), RecordSetRejectionError>, RecordSetError> {
		self.set
			.apply_with_ending(actions, RecordSetEnding::ReleaseAttempt)
			.await
	}
	pub(crate) async fn restore(mut self) -> Result<(), RecordSetError> {
		execute_boundary(self.set.tx, GroupAttemptBoundaryPhase::RestoreAndRelease).await?;
		self.set.completion.complete();
		Ok(())
	}
}

fn placed_failure(
	table: &PgQualifiedTableName,
	error: PgRecordGroupStorageError,
) -> RecordSetError {
	RecordSetError::Storage {
		identity: Box::new(PgPlacedRecordIdentity {
			table: table.clone(),
			record: error.identity,
		}),
		source: Box::new(error.source),
	}
}
