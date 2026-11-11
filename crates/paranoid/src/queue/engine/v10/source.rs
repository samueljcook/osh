//! Actual admitted source-relocation operators under one consuming native/source attempt.

use crate::db::WriteTx;
use crate::queue::QueueSourceImportDefs;
use crate::queue::engine::{
	GroupOperation, NativeQueueCleanupError, NativeQueueGroupError, NativeQueueGroupInner,
	NativeQueueGroupOutcome, NativeQueueGroupRejectionError,
};
use crate::queue::group_state::{BoundQueueGroup, QueueGroupPreparation, QueueGroupSnapshot};
use crate::queue::v10::source_group_storage::PgQueueSourceGroupStorage;
use std::collections::BTreeSet;
use std::sync::Arc;

/// Retained source storage and the actual assignment-bound import implementations.
/// Construction reuses a verified physical mapping and supplies no transaction admission.
pub(crate) struct NativeQueueSourceBindings {
	pub(crate) storage: PgQueueSourceGroupStorage,
	pub(crate) imports: Arc<QueueSourceImportDefs>,
}
impl NativeQueueSourceBindings {
	pub(crate) fn new(
		storage: PgQueueSourceGroupStorage,
		imports: Arc<QueueSourceImportDefs>,
	) -> Self {
		Self { storage, imports }
	}
}

use crate::queue::v10::source_group_storage::apply::{
	QueueSourceGroupApplyPlan, QueueSourcePreparationRejection,
};
use crate::queue::v10::source_group_storage::context::QueueSourceContext;
use crate::queue::v10::source_group_storage::{
	QueueSourceGroupObservation, QueueSourceGroupReaders, QueueSourceGroupRequest,
};
use crate::records::postgres::group_attempt::PgRecordGroupAttempt;

impl NativeQueueGroupInner {
	pub(in crate::queue::engine) async fn execute_source(
		&self,
		tx: &mut WriteTx<'_>,
		contracts: QueueSourceContext<'_, '_>,
		bound: &BoundQueueGroup<'_, '_, '_>,
		source: &NativeQueueSourceBindings,
		readers: &QueueSourceGroupReaders<'_, '_, '_>,
		operation: &GroupOperation<'_>,
	) -> Result<
		Result<NativeQueueGroupOutcome, NativeQueueGroupRejectionError>,
		NativeQueueGroupError,
	> {
		if operation.pause_scope().is_some() {
			return self
				.execute_source_pause(tx, contracts, bound, source, readers, operation)
				.await;
		}
		let primary = operation.initial_identity(&self.bindings.storage);
		let mut attempt = match PgRecordGroupAttempt::acquire(
			self.bindings.storage.storage(),
			tx,
			BTreeSet::from([primary.clone()]),
			self.bindings.attempt_budget,
		)
		.await?
		{
			Ok(attempt) => attempt,
			Err(rejection) => {
				return Ok(Err(NativeQueueGroupRejectionError::Acquisition(rejection)));
			}
		};
		loop {
			let request = match QueueSourceGroupRequest::from_held(
				attempt.snapshot(),
				&self.bindings.storage,
			) {
				Ok(request) => request,
				Err(error) => {
					let rejection = NativeQueueGroupRejectionError::SourceRequest(error);
					if let Err(cleanup) = attempt.restore().await {
						return Err(NativeQueueGroupError::RejectionCleanup {
							rejection_error: rejection,
							cleanup_error: Box::new(NativeQueueCleanupError::Attempt(cleanup)),
						});
					}
					return Ok(Err(rejection));
				}
			};
			let observed =
				crate::queue::v10::source_group_storage::attempt::PgQueueSourceObservationAttempt::observe(
					attempt,
					&source.storage,
					contracts,
					request,
					readers,
				)
				.await
				.map_err(NativeQueueGroupError::SourceObservation)?;
			let preparation = match observed.observation() {
				QueueSourceGroupObservation::RequiresReacquisition(identities) => {
					let identities = identities.clone();
					attempt = match observed.reacquire(identities).await? {
						Ok(attempt) => attempt,
						Err(rejection) => {
							return Ok(Err(NativeQueueGroupRejectionError::Acquisition(rejection)));
						}
					};
					continue;
				}

				QueueSourceGroupObservation::Observed(read) => {
					match operation.prepare(
						bound,
						&QueueGroupSnapshot {
							native: observed.snapshot(),
							source: read.facts(),
							observed_at: read.observed_at(),
						},
					) {
						Err(error) => {
							let identity = error
								.record_identity()
								.cloned()
								.unwrap_or_else(|| primary.clone());
							Err(QueueSourcePreparationRejection::new(identity, error))
						}
						Ok(QueueGroupPreparation::Unchanged) => {
							observed.restore().await?;
							return Ok(Ok(NativeQueueGroupOutcome::Unchanged));
						}
						Ok(QueueGroupPreparation::RequiresReacquisition(identities)) => {
							attempt = match observed.reacquire(identities).await? {
								Ok(attempt) => attempt,
								Err(rejection) => {
									return Ok(Err(NativeQueueGroupRejectionError::Acquisition(
										rejection,
									)));
								}
							};
							continue;
						}
						Ok(QueueGroupPreparation::Prepared(plan)) => {
							Ok(QueueSourceGroupApplyPlan::from_prepared(plan))
						}
					}
				}
			};
			return observed
				.apply_queue_source(preparation)
				.await
				.map_err(NativeQueueGroupError::SourceApply)
				.map(|result| {
					result
						.map(|outcome| {
							NativeQueueGroupOutcome::from_prepared(outcome.into_outcome())
						})
						.map_err(NativeQueueGroupRejectionError::Source)
				});
		}
	}
}

impl NativeQueueGroupInner {
	async fn execute_source_pause(
		&self,
		tx: &mut WriteTx<'_>,
		contracts: QueueSourceContext<'_, '_>,
		bound: &BoundQueueGroup<'_, '_, '_>,
		source: &NativeQueueSourceBindings,
		readers: &QueueSourceGroupReaders<'_, '_, '_>,
		operation: &GroupOperation<'_>,
	) -> Result<
		Result<NativeQueueGroupOutcome, NativeQueueGroupRejectionError>,
		NativeQueueGroupError,
	> {
		let scope = operation
			.pause_scope()
			.expect("fixed source pause dispatch requires its exact known scope");
		let guard = crate::queue::v10::source_pause::PgQueueSourcePauseLock::acquire(
			&source.storage,
			tx,
			contracts,
			readers,
			scope.clone(),
			self.bindings.attempt_budget.conflict_attempts,
		)
		.await
		.map_err(NativeQueueGroupError::SourcePause)?;
		let primary = operation.initial_identity(&self.bindings.storage);
		let prepared = match operation.prepare(
			bound,
			&QueueGroupSnapshot {
				native: guard.snapshot(),
				source: guard.observation().facts(),
				observed_at: guard.observation().observed_at(),
			},
		) {
			Ok(QueueGroupPreparation::Unchanged) => {
				guard.keep().await?;
				return Ok(Ok(NativeQueueGroupOutcome::Unchanged));
			}
			Ok(QueueGroupPreparation::Prepared(plan)) => {
				Ok(QueueSourceGroupApplyPlan::from_prepared(plan))
			}
			Ok(QueueGroupPreparation::RequiresReacquisition(_)) => {
				let rejection = NativeQueueGroupRejectionError::UnexpectedPauseDiscovery;
				if let Err(cleanup) = guard.keep().await {
					return Err(NativeQueueGroupError::RejectionCleanup {
						rejection_error: rejection,
						cleanup_error: Box::new(NativeQueueCleanupError::Storage(cleanup)),
					});
				}
				return Ok(Err(rejection));
			}
			Err(error) => {
				let identity = error.record_identity().cloned().unwrap_or(primary);
				Err(QueueSourcePreparationRejection::new(identity, error))
			}
		};
		guard
			.apply(prepared)
			.await
			.map_err(NativeQueueGroupError::SourcePause)
			.map(|result| {
				result
					.map(|outcome| NativeQueueGroupOutcome::from_prepared(outcome.into_outcome()))
					.map_err(NativeQueueGroupRejectionError::Source)
			})
	}
}
