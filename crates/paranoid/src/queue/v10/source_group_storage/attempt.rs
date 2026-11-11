//! Bounded Queue v10 source observation and atomic native-plus-source application under a savepoint.

use std::collections::BTreeSet;

use crate::records::postgres::group_attempt::{
	GroupAttemptError, GroupAttemptRejectionError, PgRecordGroupAttempt,
};
use crate::records::{RecordGroupRead, RecordIdentity};

use super::apply::{
	QueueSourceApplyRejectionError, QueueSourceGroupApplyError, QueueSourceGroupApplyPlan,
	QueueSourceGroupApplyResult, QueueSourcePreparationRejection,
};
use super::context::QueueSourceContext;
use super::{
	PgQueueSourceGroupStorage, QueueSourceGroupObservation, QueueSourceGroupReaders,
	QueueSourceGroupRequest, QueueSourceGroupStorageError, QueueSourceNativeRecords,
};

/// One observed source/native group retains whole-set authority and exact source storage.
/// Restoring or reacquiring consumes all source facts; no native-only apply or transaction escape exists.
pub(in crate::queue) struct PgQueueSourceObservationAttempt<'a, 'tx> {
	attempt: PgRecordGroupAttempt<'a, 'tx>,
	observation: QueueSourceGroupObservation,
	source: &'a PgQueueSourceGroupStorage,
}

impl<'a, 'tx> PgQueueSourceObservationAttempt<'a, 'tx> {
	/// Executes one descriptor-bound source observation inside the admitted group attempt.
	/// Source errors or cancellation consume the guard and retain mandatory full rollback.
	pub(in crate::queue) async fn observe(
		mut attempt: PgRecordGroupAttempt<'a, 'tx>,
		storage: &'a PgQueueSourceGroupStorage,
		contracts: QueueSourceContext<'_, '_>,
		request: QueueSourceGroupRequest,
		readers: &QueueSourceGroupReaders<'_, '_, '_>,
	) -> Result<Self, QueueSourceGroupStorageError> {
		if !attempt.shares_storage_container(storage.native_storage().storage()) {
			return Err(QueueSourceGroupStorageError::AttemptContainerMismatch);
		}
		let observation = {
			let (tx, snapshot) = attempt.tx_and_snapshot();
			storage
				.observe_in_attempt(
					tx,
					QueueSourceNativeRecords::Group(snapshot),
					contracts,
					request,
					readers,
				)
				.await?
		};
		if let QueueSourceGroupObservation::Observed(read) = &observation {
			attempt.refresh_observed_at(read.observed_at());
		}
		Ok(Self {
			attempt,
			observation,
			source: storage,
		})
	}

	pub(in crate::queue) fn snapshot(&self) -> &RecordGroupRead {
		self.attempt.snapshot()
	}

	pub(in crate::queue) fn observation(&self) -> &QueueSourceGroupObservation {
		&self.observation
	}

	/// Applies only a completed source observation through the retained storage and transaction.
	/// Every pure rejection restores the whole attempt; any later failure leaves full rollback.
	pub(in crate::queue) async fn apply_queue_source(
		self,
		preparation: Result<QueueSourceGroupApplyPlan, QueueSourcePreparationRejection>,
	) -> Result<
		Result<QueueSourceGroupApplyResult, QueueSourceApplyRejectionError>,
		QueueSourceGroupApplyError,
	> {
		let prepared = match preparation {
			Err(rejection) => Err(rejection.into_rejection()),
			Ok(plan) => match &self.observation {
				QueueSourceGroupObservation::Observed(read) => self.source.prepare_apply(
					self.attempt.snapshot(),
					read,
					&self.attempt.snapshot().inserted_reservations(),
					plan,
				),
				QueueSourceGroupObservation::RequiresReacquisition(_) => {
					Err(QueueSourceApplyRejectionError::ObservationIncomplete)
				}
			},
		};
		let prepared = match prepared {
			Ok(prepared) => prepared,
			Err(rejection) => {
				if let Err(cleanup) = self.attempt.restore().await {
					return Err(QueueSourceGroupApplyError::RejectionCleanup {
						rejection_error: Box::new(rejection),
						cleanup_error: Box::new(cleanup),
					});
				}
				return Ok(Err(rejection));
			}
		};
		let (native, cleanup, source_deletes) = prepared.into_parts();
		let mut applied = self
			.attempt
			.apply_native_before_source(native, cleanup)
			.await?;
		let outcome = self
			.source
			.apply_source_deletes(applied.source_tx(), source_deletes)
			.await?;
		applied.cleanup_and_release().await?;
		Ok(Ok(outcome))
	}

	pub(in crate::queue) async fn restore(self) -> Result<(), GroupAttemptError> {
		self.attempt.restore().await
	}

	/// Restores all attempt locks before reacquiring the complete sorted union; prior source facts
	/// are discarded.
	pub(in crate::queue) async fn reacquire(
		self,
		required: BTreeSet<RecordIdentity>,
	) -> Result<Result<PgRecordGroupAttempt<'a, 'tx>, GroupAttemptRejectionError>, GroupAttemptError>
	{
		self.attempt.reacquire(required).await
	}
}
