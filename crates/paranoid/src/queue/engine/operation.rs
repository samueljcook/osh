//! Actual planner dispatch and bounded whole-set continuation without caller callbacks.

use super::{
	BTreeSet, BoundQueueGroup, ClaimedJob, DeadLetterRequeueTarget, DeadLetterTarget, JobId,
	NativeQueueCleanupError, NativeQueueDeadLetterMove, NativeQueueDeadLetterRequeue,
	NativeQueueGroupError, NativeQueueGroupInner, NativeQueueGroupOutcome,
	NativeQueueGroupRejectionError, NativeQueueJobOperatorAction, NativeQueuePauseChange,
	NativeQueueStaleClaimRecovery, OwnedJobMutation, PgNativeQueueStorage, QueueGroupError,
	QueueGroupPreparation, QueueGroupSnapshot, QueuePauseScope, QueueRelocationRecord,
	QueueSourceFacts, RecordIdentity, RecordKey, SourceProbe, SourceRecord,
	StaleClaimRecoveryRequest, WriteTx,
};
use crate::records::postgres::group_attempt::PgRecordGroupAttempt;
use crate::records::{GroupMutationError, RecordGroupRead, RecordMutationGroup};

pub(super) enum GroupOperation<'a> {
	OperatorAction(NativeQueueJobOperatorAction),
	Relocate(QueueRelocationRecord),
	Owned {
		authority: &'a ClaimedJob,
		mutation: OwnedJobMutation,
	},
	RecoverStaleClaim(NativeQueueStaleClaimRecovery),
	MoveToDeadLetter(NativeQueueDeadLetterMove),
	DeleteDeadLetter(JobId),
	DeleteFinished {
		job_id: JobId,
		retention_micros: i64,
	},
	DeleteExpiredDeadLetter {
		dead_letter_id: JobId,
		retention_micros: i64,
	},
	RequeueDeadLetter(NativeQueueDeadLetterRequeue),
	Pause(NativeQueuePauseChange),
	Enqueue {
		request: &'a crate::queue::group_state::NativeEnqueueRequest,
		pauses: crate::queue::group_state::EnqueuePauses,
	},
	Claim {
		request: &'a crate::queue::group_state::NativeClaimRequest,
		pauses: crate::queue::group_state::ClaimPauses,
	},
	ClaimCheck(&'a crate::queue::group_state::NativeClaimCheckRequest),
}

impl GroupOperation<'_> {
	pub(super) fn name(&self) -> &'static str {
		match self {
			Self::OperatorAction(request) => request.action.operation_name(),
			Self::Relocate(_) => "queue.native_group.relocate_source_record",
			Self::Owned { .. } => "queue.native_group.reconcile_owned",
			Self::RecoverStaleClaim(_) => "queue.native_group.recover_stale_claim",
			Self::MoveToDeadLetter(_) => "queue.native_group.move_to_dead_letter",
			Self::DeleteDeadLetter(_) => "queue.native_group.delete_dead_letter",
			Self::DeleteFinished { .. } => "queue.native_group.delete_finished_job",
			Self::DeleteExpiredDeadLetter { .. } => "queue.native_group.delete_expired_dead_letter",
			Self::RequeueDeadLetter(_) => "queue.native_group.requeue_dead_letter",
			Self::Pause(_) => "queue.native_group.change_pause",
			Self::Enqueue { .. } => "queue.native_group.enqueue",
			Self::Claim { .. } => "queue.native_group.claim",
			Self::ClaimCheck(_) => "queue.native_group.check_claim",
		}
	}
	pub(super) fn requires_source_relocation(&self) -> bool {
		matches!(self, Self::Relocate(_))
	}
	pub(super) fn pause_scope(&self) -> Option<&QueuePauseScope> {
		match self {
			Self::Pause(request) => Some(&request.scope),
			Self::Relocate(QueueRelocationRecord::Pause(scope)) => Some(scope),
			_ => None,
		}
	}

	pub(super) fn initial_identity(&self, storage: &PgNativeQueueStorage) -> RecordIdentity {
		let (namespace, id) = match self {
			Self::OperatorAction(request) => (storage.jobs(), request.job_id),
			Self::Relocate(QueueRelocationRecord::Job(id)) => (storage.jobs(), *id),
			Self::Relocate(QueueRelocationRecord::DeadLetter(id)) => (storage.dead_letters(), *id),
			Self::Relocate(QueueRelocationRecord::Pause(scope)) => {
				return RecordIdentity::new(storage.pauses().clone(), scope.key());
			}
			Self::Owned { authority, .. } => (storage.jobs(), authority.job_id()),
			Self::RecoverStaleClaim(request) => (storage.jobs(), request.job_id),
			Self::MoveToDeadLetter(request) => (storage.jobs(), request.job_id),
			Self::DeleteDeadLetter(id) => (storage.dead_letters(), *id),
			Self::DeleteFinished { job_id, .. } => (storage.jobs(), *job_id),
			Self::DeleteExpiredDeadLetter { dead_letter_id, .. } => {
				(storage.dead_letters(), *dead_letter_id)
			}
			Self::RequeueDeadLetter(request) => (storage.dead_letters(), request.dead_letter_id),
			Self::Pause(request) => {
				return RecordIdentity::new(storage.pauses().clone(), request.scope.key());
			}
			Self::Enqueue { request, .. } => (storage.jobs(), request.job_id),
			Self::Claim { request, .. } => (storage.jobs(), request.job_id),
			Self::ClaimCheck(request) => (storage.jobs(), request.job_id),
		};
		RecordIdentity::new(namespace.clone(), RecordKey::new(id.as_bytes().to_vec()))
	}
	pub(super) fn prepare(
		&self,
		bound: &BoundQueueGroup<'_, '_, '_>,
		snapshot: &QueueGroupSnapshot<'_>,
	) -> Result<QueueGroupPreparation, QueueGroupError> {
		match self {
			Self::OperatorAction(request) => {
				bound.prepare_job_operator_action(snapshot, request.job_id, request.action)
			}
			Self::Relocate(record) => bound.prepare_source_relocation(snapshot, record),
			Self::Owned {
				authority,
				mutation,
			} => bound.prepare_owned_reconciliation(snapshot, authority, mutation.clone()),
			Self::RecoverStaleClaim(request) => bound.prepare_stale_recovery(
				snapshot,
				StaleClaimRecoveryRequest {
					job_id: request.job_id,
					dead_letter_id: request.dead_letter_id,
				},
			),
			Self::MoveToDeadLetter(request) => bound.prepare_move_to_dead_letter(
				snapshot,
				request.job_id,
				DeadLetterTarget {
					dead_letter_id: request.dead_letter_id,
					reason: request.reason,
				},
			),
			Self::DeleteDeadLetter(id) => bound.prepare_delete_dead_letter(snapshot, *id),
			Self::DeleteFinished {
				job_id,
				retention_micros,
			} => bound.prepare_delete_finished_job(snapshot, *job_id, *retention_micros),
			Self::DeleteExpiredDeadLetter {
				dead_letter_id,
				retention_micros,
			} => bound.prepare_delete_expired_dead_letter(
				snapshot,
				*dead_letter_id,
				*retention_micros,
			),
			Self::RequeueDeadLetter(request) => bound.prepare_requeue_dead_letter(
				snapshot,
				request.dead_letter_id,
				DeadLetterRequeueTarget {
					job_id: request.new_job_id,
					schedule: request.schedule,
				},
			),
			Self::Pause(request) => {
				bound.prepare_pause(snapshot, request.scope.clone(), request.action)
			}
			Self::Enqueue { request, pauses } => bound.prepare_enqueue(snapshot, request, pauses),
			Self::Claim { request, pauses } => bound.prepare_claim(snapshot, request, pauses),
			Self::ClaimCheck(request) => bound.prepare_claim_check(snapshot, request),
		}
	}
}

/// Called only after this transaction proves that source data need not be consulted.
/// A held native absence is exact for that namespace/key; no page or count supplies it.
fn native_only_source_facts(
	snapshot: &RecordGroupRead,
	storage: &PgNativeQueueStorage,
) -> QueueSourceFacts {
	let identities = snapshot.snapshots().keys();
	QueueSourceFacts {
		records: identities
			.clone()
			.map(|identity| (identity.clone(), SourceRecord::Absent))
			.collect(),
		active_dedupe: identities
			.clone()
			.filter(|identity| identity.namespace() == storage.active_dedupe())
			.map(|identity| (identity.clone(), SourceProbe::Absent))
			.collect(),
		originals: identities
			.filter(|identity| identity.namespace() == storage.dead_letter_originals())
			.map(|identity| (identity.clone(), SourceProbe::Absent))
			.collect(),
	}
}

fn preparation(
	operation: &GroupOperation<'_>,
	bound: &BoundQueueGroup<'_, '_, '_>,
	snapshot: &RecordGroupRead,
	storage: &PgNativeQueueStorage,
) -> Result<QueueGroupPreparation, NativeQueueGroupRejectionError> {
	let source = native_only_source_facts(snapshot, storage);
	let result = operation
		.prepare(
			bound,
			&QueueGroupSnapshot {
				native: snapshot,
				source: &source,
				observed_at: snapshot.observed_at(),
			},
		)
		.map_err(NativeQueueGroupRejectionError::Preparation)?;
	if let QueueGroupPreparation::Prepared(plan) = &result
		&& !plan.source_deletes.is_empty()
	{
		return Err(NativeQueueGroupRejectionError::SourceDeletionInNativePlan);
	}
	Ok(result)
}

impl NativeQueueGroupInner {
	pub(super) async fn execute_native(
		&self,
		tx: &mut WriteTx<'_>,
		bound: &BoundQueueGroup<'_, '_, '_>,
		operation: &GroupOperation<'_>,
	) -> Result<
		Result<NativeQueueGroupOutcome, NativeQueueGroupRejectionError>,
		NativeQueueGroupError,
	> {
		let identity = operation.initial_identity(&self.bindings.storage);
		if matches!(operation, GroupOperation::Pause(_)) {
			return self.execute_pause(tx, bound, operation, identity).await;
		}
		// On native storage a pause is always a native record, so enqueue and claim read pauses
		// without holding them, and concurrent enqueues and claims do not wait for each other.
		let unlocked;
		let operation = match operation {
			GroupOperation::Enqueue { request, .. } => {
				let (queue_paused, task_paused) =
					self.observe_enqueue_pauses(tx, bound, request).await?;
				unlocked = GroupOperation::Enqueue {
					request,
					pauses: crate::queue::group_state::EnqueuePauses::Observed {
						queue_paused,
						task_paused,
					},
				};
				&unlocked
			}
			GroupOperation::Claim { request, .. } => {
				unlocked = GroupOperation::Claim {
					request,
					pauses: crate::queue::group_state::ClaimPauses::ExcludedBySelection,
				};
				&unlocked
			}
			operation => operation,
		};
		let mut attempt = match PgRecordGroupAttempt::acquire(
			self.bindings.storage.storage(),
			tx,
			BTreeSet::from([identity]),
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
			match preparation(operation, bound, attempt.snapshot(), &self.bindings.storage) {
				Err(rejection) => {
					if let Err(cleanup) = attempt.restore().await {
						return Err(NativeQueueGroupError::RejectionCleanup {
							rejection_error: rejection,
							cleanup_error: Box::new(NativeQueueCleanupError::Attempt(cleanup)),
						});
					}
					return Ok(Err(rejection));
				}
				Ok(QueueGroupPreparation::Unchanged) => {
					attempt.restore().await?;
					return Ok(Ok(NativeQueueGroupOutcome::Unchanged));
				}
				Ok(QueueGroupPreparation::RequiresReacquisition(identities)) => {
					attempt = match attempt.reacquire(identities).await? {
						Ok(attempt) => attempt,
						Err(rejection) => {
							return Ok(Err(NativeQueueGroupRejectionError::Acquisition(rejection)));
						}
					};
				}
				Ok(QueueGroupPreparation::Prepared(plan)) => {
					let actions = plan.native.into_actions().into_iter().collect();
					return Ok(attempt
						.apply_native(actions)
						.await?
						.map(|()| NativeQueueGroupOutcome::from_prepared(plan.result))
						.map_err(NativeQueueGroupRejectionError::Plan));
				}
			}
		}
	}

	/// Reads the queue's and the task's pause records in one statement that takes no lock.
	async fn observe_enqueue_pauses(
		&self,
		tx: &mut WriteTx<'_>,
		bound: &BoundQueueGroup<'_, '_, '_>,
		request: &crate::queue::group_state::NativeEnqueueRequest,
	) -> Result<(bool, bool), NativeQueueGroupError> {
		let scopes = [
			QueuePauseScope::Global,
			QueuePauseScope::Task(request.task_name.clone()),
		];
		let keys = scopes.iter().map(QueuePauseScope::key).collect::<Vec<_>>();
		let read = self
			.bindings
			.storage
			.storage()
			.read_many(
				tx,
				crate::records::postgres::record_many::RecordManyReadFields {
					namespace: self.bindings.storage.pauses(),
					keys: &keys,
				},
			)
			.await
			.map_err(NativeQueueGroupError::RecordBatchRead)?;
		let [queue, task] = &read.records[..] else {
			return Err(NativeQueueGroupError::RecordBatchRead(
				crate::records::postgres::record_many::RecordManyStorageError::Coverage,
			));
		};
		Ok((
			bound
				.pause_is_present(&scopes[0], queue)
				.map_err(NativeQueueGroupError::PolicyBinding)?,
			bound
				.pause_is_present(&scopes[1], task)
				.map_err(NativeQueueGroupError::PolicyBinding)?,
		))
	}

	/// A pause has one known identity; no whole-set discovery requires a savepoint roundtrip.
	async fn execute_pause(
		&self,
		tx: &mut WriteTx<'_>,
		bound: &BoundQueueGroup<'_, '_, '_>,
		operation: &GroupOperation<'_>,
		identity: RecordIdentity,
	) -> Result<
		Result<NativeQueueGroupOutcome, NativeQueueGroupRejectionError>,
		NativeQueueGroupError,
	> {
		let group = self
			.bindings
			.storage
			.storage()
			.lock_group(
				tx,
				vec![identity],
				self.bindings.attempt_budget.conflict_attempts,
			)
			.await?;
		let prepared = match preparation(operation, bound, group.snapshot(), &self.bindings.storage)
		{
			Ok(QueueGroupPreparation::RequiresReacquisition(_)) => {
				Err(NativeQueueGroupRejectionError::UnexpectedPauseDiscovery)
			}
			other => other,
		};
		match prepared {
			Err(rejection) => {
				if let Err(cleanup) = group.keep().await {
					return Err(NativeQueueGroupError::RejectionCleanup {
						rejection_error: rejection,
						cleanup_error: Box::new(NativeQueueCleanupError::Storage(cleanup)),
					});
				}
				Ok(Err(rejection))
			}
			Ok(QueueGroupPreparation::Unchanged) => {
				group.keep().await?;
				Ok(Ok(NativeQueueGroupOutcome::Unchanged))
			}
			Ok(QueueGroupPreparation::Prepared(plan)) => {
				let actions = plan.native.into_actions().into_iter().collect();
				match group.apply(actions).await {
					Ok(()) => Ok(Ok(NativeQueueGroupOutcome::from_prepared(plan.result))),
					Err(GroupMutationError::Rejected(rejection)) => {
						Ok(Err(NativeQueueGroupRejectionError::Plan(rejection)))
					}
					Err(error) => Err(NativeQueueGroupError::GroupMutation(error)),
				}
			}
			Ok(QueueGroupPreparation::RequiresReacquisition(_)) => {
				unreachable!("pause discovery was converted to a rejection")
			}
		}
	}
}
