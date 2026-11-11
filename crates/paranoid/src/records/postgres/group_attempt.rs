//! Bounded whole-set acquisition with a private transaction savepoint.
//!
//! Admission pins precede this boundary. The exact borrowed transaction retains those pins
//! while known restoration releases this attempt's row locks and reservations.

use std::collections::BTreeSet;

use crate::records::{
	InvalidGroupMutationError, RecordGroupRead, RecordIdentity, RecordMutation,
	StateTimestampMicros,
};

use super::group::PreparedPgRecordGroupMutation;
use super::{PgPointStorage, StateMutationCompletion, StateStorageError, WriteTx};

use super::attempt_boundary::{GroupAttemptBoundaryPhase, RecordAttemptProgress, execute_boundary};
pub(crate) use super::attempt_boundary::{
	GroupAttemptError, GroupAttemptRejectionError, PgRecordGroupAttemptBudget,
};

/// One actual transaction owns every acquisition attempt and one sticky completion token.
/// The snapshot records which reservation rows the current attempt inserted.
pub(crate) struct PgRecordGroupAttempt<'a, 'tx> {
	storage: &'a PgPointStorage,
	tx: &'a mut WriteTx<'tx>,
	progress: RecordAttemptProgress<RecordIdentity>,
	snapshot: RecordGroupRead,
	completion: StateMutationCompletion,
}

pub(crate) struct PgAppliedGroupAttempt<'a, 'tx> {
	storage: &'a PgPointStorage,
	tx: &'a mut WriteTx<'tx>,
	cleanup: BTreeSet<RecordIdentity>,
	completion: StateMutationCompletion,
}

impl<'tx> PgAppliedGroupAttempt<'_, 'tx> {
	pub(crate) fn source_tx(&mut self) -> &mut WriteTx<'tx> {
		self.tx
	}

	pub(crate) async fn cleanup_and_release(mut self) -> Result<(), GroupAttemptError> {
		self.storage
			.cleanup_group_reservations(self.tx, &self.cleanup)
			.await?;
		execute_boundary(self.tx, GroupAttemptBoundaryPhase::Release).await?;
		self.completion.complete();
		Ok(())
	}
}

impl<'a, 'tx> PgRecordGroupAttempt<'a, 'tx> {
	pub(crate) fn shares_storage_container(&self, other: &PgPointStorage) -> bool {
		self.storage.shares_container(other)
	}

	pub(crate) fn tx_and_snapshot(&mut self) -> (&mut WriteTx<'tx>, &RecordGroupRead) {
		(self.tx, &self.snapshot)
	}

	pub(crate) fn refresh_observed_at(&mut self, observed_at: StateTimestampMicros) {
		self.snapshot.refresh_observed_at(observed_at);
	}

	/// Applies native group changes inside this attempt's savepoint, returning a linear guard
	/// to execute external v10 source deletes, clean up unused inserted reservations, and release
	/// the savepoint.
	pub(crate) async fn apply_native_before_source(
		self,
		native: PreparedPgRecordGroupMutation,
		cleanup: BTreeSet<RecordIdentity>,
	) -> Result<PgAppliedGroupAttempt<'a, 'tx>, StateStorageError> {
		self.storage.apply_group_changes(self.tx, native).await?;
		Ok(PgAppliedGroupAttempt {
			storage: self.storage,
			tx: self.tx,
			cleanup,
			completion: self.completion,
		})
	}

	/// Must follow actual admission; no snapshot or returned guard supplies that prerequisite.
	pub(crate) async fn acquire(
		storage: &'a PgPointStorage,
		tx: &'a mut WriteTx<'tx>,
		identities: BTreeSet<RecordIdentity>,
		budget: PgRecordGroupAttemptBudget,
	) -> Result<Result<Self, GroupAttemptRejectionError>, GroupAttemptError> {
		let progress = match RecordAttemptProgress::new(identities, budget) {
			Ok(progress) => progress,
			Err(rejection) => return Ok(Err(rejection)),
		};
		let completion = tx.track_state_mutation();
		execute_boundary(tx, GroupAttemptBoundaryPhase::Create).await?;
		let acquired = storage
			.acquire_group_identities(
				tx,
				progress.identities().clone(),
				progress.conflict_attempts(),
			)
			.await?;
		let observed_at = PgPointStorage::observe_time(tx).await?;
		Ok(Ok(Self {
			storage,
			tx,
			progress,
			snapshot: RecordGroupRead::new(acquired.records, observed_at),
			completion,
		}))
	}

	pub(crate) fn snapshot(&self) -> &RecordGroupRead {
		&self.snapshot
	}

	/// Restores the entire attempt before reacquiring a sorted union or returning a rejection.
	pub(crate) async fn reacquire(
		mut self,
		required: BTreeSet<RecordIdentity>,
	) -> Result<Result<Self, GroupAttemptRejectionError>, GroupAttemptError> {
		if let Err(rejection) = self.progress.restore_and_advance(self.tx, required).await? {
			self.completion.complete();
			return Ok(Err(rejection));
		}
		let acquired = self
			.storage
			.acquire_group_identities(
				self.tx,
				self.progress.identities().clone(),
				self.progress.conflict_attempts(),
			)
			.await?;
		let observed_at = PgPointStorage::observe_time(self.tx).await?;
		self.snapshot = RecordGroupRead::new(acquired.records, observed_at);
		Ok(Ok(self))
	}

	/// Restores all attempt changes before returning a pure rejection or unchanged result.
	pub(crate) async fn restore(mut self) -> Result<(), GroupAttemptError> {
		execute_boundary(self.tx, GroupAttemptBoundaryPhase::RestoreAndRelease).await?;
		self.completion.complete();
		Ok(())
	}

	/// Applies native-only actions and ends the boundary before completing the tracker.
	/// Source-enabled aggregates need their own owner through exact source deletion as well.
	pub(crate) async fn apply_native(
		mut self,
		actions: Vec<(RecordIdentity, RecordMutation)>,
	) -> Result<Result<(), InvalidGroupMutationError>, GroupAttemptError> {
		let prepared = match PreparedPgRecordGroupMutation::prepare(&self.snapshot, actions) {
			Ok(prepared) => prepared,
			Err(rejection) => {
				if let Err(cleanup) =
					execute_boundary(self.tx, GroupAttemptBoundaryPhase::RestoreAndRelease).await
				{
					return Err(GroupAttemptError::RejectionCleanup {
						rejection_error: rejection,
						cleanup_error: Box::new(cleanup),
					});
				}
				self.completion.complete();
				return Ok(Err(rejection));
			}
		};
		self.storage
			.apply_group_mutation(self.tx, prepared, &self.snapshot.inserted_reservations())
			.await?;
		execute_boundary(self.tx, GroupAttemptBoundaryPhase::Release).await?;
		self.completion.complete();
		Ok(Ok(()))
	}
}
