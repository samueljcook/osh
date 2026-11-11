//! Fixed one-key source Queue pause authority without savepoint or discovery.
//!
//! Keep cleans only this guard's new reservation. Native/source row locks remain until the
//! caller's transaction ends; this guard neither restores a savepoint nor releases row locks.

use crate::db::WriteTx;
use crate::queue::engine::QueuePauseScope;
use crate::queue::v10::source_group_storage::apply::{
	QueueSourceApplyRejectionError, QueueSourceGroupApplyError, QueueSourceGroupApplyPlan,
	QueueSourceGroupApplyResult, QueueSourcePreparationRejection,
};
use crate::queue::v10::source_group_storage::context::QueueSourceContext;
use crate::queue::v10::source_group_storage::{
	PgQueueSourceGroupStorage, QueueSourceGroupObservation, QueueSourceGroupRead,
	QueueSourceGroupReaders, QueueSourceGroupRequest, QueueSourceGroupStorageError,
	QueueSourceNativeRecords,
};
use crate::records::postgres::StateStorageError;
use crate::records::postgres::group::PgObservedExternalSourceGroupLock;
use crate::records::{RecordGroupRead, RecordIdentity};
use std::{collections::BTreeSet, num::NonZeroU32};

#[derive(Debug, thiserror::Error)]
pub(crate) enum QueueSourcePauseError {
	#[error("source Queue pause canonical storage failed")]
	Native(#[from] StateStorageError),
	#[error("source Queue pause protected observation failed")]
	Observation(#[from] QueueSourceGroupStorageError),
	#[error("source Queue pause required impossible discovery or non-pause assignment")]
	UnexpectedObservation {
		observation: Box<QueueSourceGroupObservation>,
	},
	#[error("source Queue pause unexpected observation and reservation cleanup both failed")]
	ObservationCleanup {
		observation: Box<QueueSourceGroupObservation>,
		#[source]
		cleanup: StateStorageError,
	},
	#[error("source Queue pause native/source application failed")]
	Application(#[from] QueueSourceGroupApplyError),
	#[error("source Queue pause preparation rejection and reservation cleanup both failed")]
	RejectionCleanup {
		rejection_error: Box<QueueSourceApplyRejectionError>,
		#[source]
		cleanup_error: StateStorageError,
	},
}
impl QueueSourcePauseError {
	/// Returns an actual retained driver cause without replacing the primary typed failure.
	pub(crate) fn database_error(&self) -> Option<&crate::db::DatabaseError> {
		match self {
			Self::Native(error)
			| Self::ObservationCleanup { cleanup: error, .. }
			| Self::RejectionCleanup {
				cleanup_error: error,
				..
			} => error.database_error(),
			Self::Observation(error) => error.database_error(),
			Self::Application(error) => error.database_error(),
			Self::UnexpectedObservation { .. } => None,
		}
	}

	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::Native(e)
			| Self::ObservationCleanup { cleanup: e, .. }
			| Self::RejectionCleanup {
				cleanup_error: e, ..
			} => e.database_error().is_some(),
			Self::Observation(e) => e.involves_database_failure(),
			Self::Application(e) => e.involves_database_failure(),
			Self::UnexpectedObservation { .. } => false,
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Native(e)
			| Self::ObservationCleanup { cleanup: e, .. }
			| Self::RejectionCleanup {
				cleanup_error: e, ..
			} => e
				.database_error()
				.is_some_and(crate::db::DatabaseError::commit_outcome_is_unknown),
			Self::Observation(e) => e.commit_outcome_is_unknown(),
			Self::Application(e) => e.commit_outcome_is_unknown(),
			Self::UnexpectedObservation { .. } => false,
		}
	}
}

pub(in crate::queue) struct PgQueueSourcePauseLock<'a, 'tx> {
	source: &'a PgQueueSourceGroupStorage,
	group_lock: PgObservedExternalSourceGroupLock<'a, 'tx>,
	observation: QueueSourceGroupRead,
}
impl<'a, 'tx> PgQueueSourcePauseLock<'a, 'tx> {
	pub(in crate::queue) async fn acquire(
		source: &'a PgQueueSourceGroupStorage,
		tx: &'a mut WriteTx<'tx>,
		contracts: QueueSourceContext<'_, '_>,
		readers: &QueueSourceGroupReaders<'_, '_, '_>,
		scope: QueuePauseScope,
		conflict_attempts: NonZeroU32,
	) -> Result<Self, QueueSourcePauseError> {
		contracts
			.require_queue_source_storage(source.descriptor(), source.native_storage())
			.map_err(QueueSourceGroupStorageError::from)?;
		readers.require_layout(source.descriptor().layout().queue_layout())?;
		let identity = RecordIdentity::new(source.native_storage().pauses().clone(), scope.key());
		let mut group_lock = source
			.native_storage()
			.storage()
			.lock_group_for_external_source(tx, BTreeSet::from([identity]), conflict_attempts)
			.await?;
		let request = QueueSourceGroupRequest::for_pause(source.native_storage(), scope);
		let observed = {
			let (records, source_tx) = group_lock.records_and_source_tx();
			source
				.observe_in_attempt(
					source_tx,
					QueueSourceNativeRecords::Acquired(records),
					contracts,
					request,
					readers,
				)
				.await?
		};
		let QueueSourceGroupObservation::Observed(observation) = observed else {
			if let Err(cleanup) = group_lock.abort_and_cleanup().await {
				return Err(QueueSourcePauseError::ObservationCleanup {
					observation: Box::new(observed),
					cleanup,
				});
			}
			return Err(QueueSourcePauseError::UnexpectedObservation {
				observation: Box::new(observed),
			});
		};
		let group_lock = group_lock.into_observed(observation.observed_at());
		Ok(Self {
			source,
			group_lock,
			observation,
		})
	}
	pub(in crate::queue) fn snapshot(&self) -> &RecordGroupRead {
		self.group_lock.snapshot()
	}
	pub(in crate::queue) fn observation(&self) -> &QueueSourceGroupRead {
		&self.observation
	}
	pub(in crate::queue) async fn keep(self) -> Result<(), StateStorageError> {
		self.group_lock.keep().await
	}
	pub(in crate::queue) async fn apply(
		self,
		preparation: Result<QueueSourceGroupApplyPlan, QueueSourcePreparationRejection>,
	) -> Result<
		Result<QueueSourceGroupApplyResult, QueueSourceApplyRejectionError>,
		QueueSourcePauseError,
	> {
		let prepared = match preparation {
			Err(rejection) => Err(rejection.into_rejection()),
			Ok(plan) => self.source.prepare_apply(
				self.group_lock.snapshot(),
				&self.observation,
				self.group_lock.inserted_reservations(),
				plan,
			),
		};
		let prepared = match prepared {
			Ok(prepared) => prepared,
			Err(rejection) => {
				if let Err(cleanup) = self.group_lock.keep().await {
					return Err(QueueSourcePauseError::RejectionCleanup {
						rejection_error: Box::new(rejection),
						cleanup_error: cleanup,
					});
				}
				return Ok(Err(rejection));
			}
		};
		let (native, cleanup, source_deletes) = prepared.into_parts();
		let mut applied = self
			.group_lock
			.apply_native_before_source(native, cleanup)
			.await?;
		let outcome = self
			.source
			.apply_source_deletes(applied.source_tx(), source_deletes)
			.await?;
		applied.cleanup_and_finish().await?;
		Ok(Ok(outcome))
	}
}
