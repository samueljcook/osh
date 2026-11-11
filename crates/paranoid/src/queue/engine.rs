//! Admitted Queue groups with exact schema and placement bindings.
//!
//! A pool owns its transaction; a caller WriteTx retains the final writes until its own
//! ending. Returned results are diagnostic and supply no worker acquisition authority. While
//! v10 writers remain, the operations run on the v10 Queue tables (see `coexistence`).

pub(in crate::queue) use super::auxiliary_state::QueuePauseScope;
use super::group_state::NativeEnqueueRequest;
use super::group_state::{
	BoundQueueGroup, DeadLetterRequeueTarget, DeadLetterTarget, PreparedQueueGroupOutcome,
	QueueAuxiliaryComponent, QueueGroupError, QueueGroupNamespaceFields, QueueGroupNamespaces,
	QueueGroupPreparation, QueueGroupSnapshot, QueueSourceFacts, SourceProbe, SourceRecord,
	StaleClaimRecoveryRequest,
};
pub(in crate::queue) use super::group_state::{NativeClaimCheckRequest, NativeClaimRequest};
pub(crate) use super::group_state::{
	NativeEnqueueRejection, QueueRelocationRecord, QueueSourceRelocationOutcome,
};
use super::{ClaimedJob, DeadLetterReason, JobId, QueueJobComponent};
use crate::db::{DatabaseError, TransactionFailure, WriteDatabase, WriteTx, run_transaction_write};
use crate::installation::creation::PgNativeQueueStorage;
use crate::records::postgres::group_attempt::PgRecordGroupAttemptBudget;
use crate::records::{RecordIdentity, RecordKey};
use crate::runtime::admission::bindings::InstallationStorageRoute;
use crate::runtime::admission::observation::PgOperationAdmission;
use crate::runtime::admission::{NativeAdmissionError, NativeOperationError};
use std::collections::BTreeSet;
use std::sync::Arc;

pub(crate) mod conversion;
mod error;
mod operation;
mod v10;
pub(in crate::queue) use error::NativeQueueErrorClass;
pub(crate) use error::{
	NativeQueueCleanupError, NativeQueueGroupError, NativeQueueGroupRejectionError,
};
use operation::GroupOperation;
use v10::coexistence;
pub(crate) use v10::source::NativeQueueSourceBindings;

pub(in crate::queue) use super::group_state::{PauseAction, QueueRequeueSchedule};
pub(in crate::queue) use super::record_transitions::OwnedJobMutation;
pub(in crate::queue) use super::record_transitions::{RequiredExecutionState, RetryConsumption};
pub(in crate::queue) use super::state::{QueueJobOperatorAction, QueueJobOperatorOutcome};

/// The actual shared implementation instances must also supply runtime registration support.
pub(crate) struct NativeQueueBindings {
	pub(crate) source: Option<Arc<NativeQueueSourceBindings>>,
	pub(crate) jobs: Arc<QueueJobComponent>,
	pub(crate) auxiliary: Arc<QueueAuxiliaryComponent>,
	pub(crate) storage: PgNativeQueueStorage,
	pub(crate) admission: PgOperationAdmission,
	pub(crate) attempt_budget: PgRecordGroupAttemptBudget,
}

#[derive(Clone, Copy)]
enum SelectionKind<'a> {
	Due {
		task_names: &'a [&'a str],
		limit: u32,
	},
	StaleRunning {
		limit: u32,
		stale_threshold_micros: i64,
	},
	RunningByOwner {
		owner: &'a str,
	},
	Finished {
		status: &'a str,
		retention_micros: i64,
		limit: u32,
	},
	ExpiredDeadLetters {
		retention_micros: i64,
		limit: u32,
	},
}

struct NativeQueueGroupInner {
	bindings: NativeQueueBindings,
	namespaces: QueueGroupNamespaces,
	automatic_recovery: tokio::sync::Mutex<Option<AutomaticRecoveryCompletion>>,
}

struct AutomaticRecoveryCompletion {
	finished_at: tokio::time::Instant,
	batch_limit: u32,
}

#[derive(Clone)]
pub(crate) struct PgNativeQueueGroup {
	inner: Arc<NativeQueueGroupInner>,
}

pub(in crate::queue) struct NativeQueueOwnedReconciliation<'a> {
	pub(in crate::queue) authority: &'a ClaimedJob,
	pub(in crate::queue) mutation: OwnedJobMutation,
}
pub(in crate::queue) struct NativeQueueStaleClaimRecovery {
	pub(in crate::queue) job_id: JobId,
	pub(in crate::queue) dead_letter_id: JobId,
}
pub(in crate::queue) struct NativeQueueDeadLetterMove {
	pub(in crate::queue) job_id: JobId,
	pub(in crate::queue) dead_letter_id: JobId,
	pub(in crate::queue) reason: DeadLetterReason,
}
pub(in crate::queue) struct NativeQueueDeadLetterDeletion {
	pub(in crate::queue) dead_letter_id: JobId,
}
pub(in crate::queue) struct NativeQueueDeadLetterRequeue {
	pub(in crate::queue) dead_letter_id: JobId,
	pub(in crate::queue) new_job_id: JobId,
	pub(in crate::queue) schedule: QueueRequeueSchedule,
}
pub(in crate::queue) struct NativeQueuePauseChange {
	pub(in crate::queue) scope: QueuePauseScope,
	pub(in crate::queue) action: PauseAction,
}

pub(in crate::queue) struct NativeJobListRequest<'a> {
	pub(in crate::queue) statuses: &'a [super::JobStatus],
	pub(in crate::queue) task_name: Option<&'a super::TaskName>,
	pub(in crate::queue) after: Option<JobId>,
	pub(in crate::queue) limit: u32,
}

/// One page of listed jobs. `next_after` is present when the page is full.
pub(in crate::queue) struct NativeJobPage {
	pub(in crate::queue) jobs: Vec<super::Job>,
	pub(in crate::queue) next_after: Option<JobId>,
}

/// One page of dead letters. `next_after` is present when the page is full.
pub(in crate::queue) struct NativeDeadLetterPage {
	pub(in crate::queue) dead_letters: Vec<super::DeadLetterJob>,
	pub(in crate::queue) next_after: Option<JobId>,
}

pub(in crate::queue) struct NativeQueueJobOperatorAction {
	pub(in crate::queue) job_id: JobId,
	pub(in crate::queue) action: QueueJobOperatorAction,
}

/// A joined operation result is tentative until the caller's transaction commits.
/// Even a known committed result supplies no private acquisition material.
pub(in crate::queue) enum NativeQueueGroupOutcome {
	JobOperator(QueueJobOperatorOutcome),
	SourceRelocation(QueueSourceRelocationOutcome),
	Unchanged,
	Reconciled,
	JobDeleted,
	DeadLetterMoved { dead_letter_id: JobId },
	DeadLetterDeleted,
	DeadLetterRequeued { job_id: JobId },
	PauseChanged,
	Enqueued { job_id: JobId, deduplicated: bool },
	EnqueueRejected(super::group_state::NativeEnqueueRejection),
	Claimed { job: Box<super::Job> },
	ClaimSkipped,
	ClaimNotReturned,
	ClaimReturned,
}
impl std::fmt::Debug for NativeQueueGroupOutcome {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str(match self {
			Self::JobOperator(_) => "JobOperator { .. }",
			Self::SourceRelocation(QueueSourceRelocationOutcome::Moved) => {
				"SourceRelocation::Moved"
			}
			Self::SourceRelocation(QueueSourceRelocationOutcome::AlreadyNative) => {
				"SourceRelocation::AlreadyNative"
			}
			Self::SourceRelocation(QueueSourceRelocationOutcome::AbsentNow) => {
				"SourceRelocation::AbsentNow"
			}
			Self::Unchanged => "Unchanged",
			Self::Reconciled => "Reconciled",
			Self::JobDeleted => "JobDeleted",
			Self::DeadLetterMoved { .. } => "DeadLetterMoved { .. }",
			Self::DeadLetterDeleted => "DeadLetterDeleted",
			Self::DeadLetterRequeued { .. } => "DeadLetterRequeued { .. }",
			Self::PauseChanged => "PauseChanged",
			Self::Enqueued { .. } => "Enqueued { .. }",
			Self::EnqueueRejected(_) => "EnqueueRejected { .. }",
			Self::Claimed { .. } => "Claimed { .. }",
			Self::ClaimSkipped => "ClaimSkipped",
			Self::ClaimNotReturned => "ClaimNotReturned",
			Self::ClaimReturned => "ClaimReturned",
		})
	}
}
impl NativeQueueGroupOutcome {
	fn from_prepared(result: PreparedQueueGroupOutcome) -> Self {
		match result {
			PreparedQueueGroupOutcome::JobOperator(outcome) => Self::JobOperator(outcome),
			PreparedQueueGroupOutcome::SourceRelocation(outcome) => Self::SourceRelocation(outcome),
			PreparedQueueGroupOutcome::Reconciled => Self::Reconciled,
			PreparedQueueGroupOutcome::JobDeleted => Self::JobDeleted,
			PreparedQueueGroupOutcome::DeadLetterMoved { dead_letter_id } => {
				Self::DeadLetterMoved { dead_letter_id }
			}
			PreparedQueueGroupOutcome::DeadLetterDeleted => Self::DeadLetterDeleted,
			PreparedQueueGroupOutcome::DeadLetterRequeued { job_id } => {
				Self::DeadLetterRequeued { job_id }
			}
			PreparedQueueGroupOutcome::PauseChanged => Self::PauseChanged,
			PreparedQueueGroupOutcome::Enqueued {
				job_id,
				deduplicated,
			} => Self::Enqueued {
				job_id,
				deduplicated,
			},
			PreparedQueueGroupOutcome::EnqueueRejected(rejection) => {
				Self::EnqueueRejected(rejection)
			}
			PreparedQueueGroupOutcome::Claimed { job } => Self::Claimed { job },
			PreparedQueueGroupOutcome::ClaimSkipped => Self::ClaimSkipped,
			PreparedQueueGroupOutcome::ClaimNotReturned => Self::ClaimNotReturned,
			PreparedQueueGroupOutcome::ClaimReturned => Self::ClaimReturned,
		}
	}
}

impl PgNativeQueueGroup {
	/// Runs a fixed operator under canonical group acquisition, which may wait.
	/// A caller transaction retains the result tentatively until its own ending.
	pub(in crate::queue) async fn apply_job_operator_action(
		&self,
		database: impl WriteDatabase,
		request: NativeQueueJobOperatorAction,
	) -> Result<NativeQueueGroupOutcome, NativeQueueGroupError> {
		self.run(database, GroupOperation::OperatorAction(request))
			.await
	}

	/// Binds exact immutable physical placement without creating schemas or admitting a Tx.
	pub(crate) fn new(bindings: NativeQueueBindings) -> Result<Self, NativeQueueGroupError> {
		bindings.admission.verify_queue_storage(&bindings.storage)?;
		if let Some(source) = &bindings.source {
			bindings.admission.verify_queue_source_storage(
				source.storage.descriptor(),
				source.storage.native_storage(),
			)?;
		}
		let namespaces = QueueGroupNamespaces::new(QueueGroupNamespaceFields {
			jobs: bindings.storage.jobs().clone(),
			dedupe: bindings.storage.active_dedupe().clone(),
			dead_letters: bindings.storage.dead_letters().clone(),
			originals: bindings.storage.dead_letter_originals().clone(),
			pauses: bindings.storage.pauses().clone(),
		})
		.map_err(NativeQueueGroupError::PolicyBinding)?;
		Ok(Self {
			inner: Arc::new(NativeQueueGroupInner {
				bindings,
				namespaces,
				automatic_recovery: tokio::sync::Mutex::new(None),
			}),
		})
	}

	/// This method coordinates automatic recovery among workers of this runtime. A completed
	/// pass can satisfy another worker's interval only when its batch was at least as large.
	/// Failed or cancelled passes do not postpone the next attempt; explicit recovery
	/// remains independent of this local scheduling state.
	pub(in crate::queue) async fn recover_stale_claims_automatically(
		&self,
		pool: &crate::db::WritePool,
		interval: std::time::Duration,
		limit: u32,
	) -> Result<(), super::RecoverStaleClaimsError> {
		let mut completed = self.inner.automatic_recovery.lock().await;
		if completed
			.as_ref()
			.is_some_and(|pass| pass.batch_limit >= limit && pass.finished_at.elapsed() < interval)
		{
			return Ok(());
		}
		self.recover_stale_claims(pool, limit).await?;
		*completed = Some(AutomaticRecoveryCompletion {
			finished_at: tokio::time::Instant::now(),
			batch_limit: limit,
		});
		Ok(())
	}

	pub(in crate::queue) async fn reconcile_owned(
		&self,
		database: impl WriteDatabase,
		request: NativeQueueOwnedReconciliation<'_>,
	) -> Result<NativeQueueGroupOutcome, NativeQueueGroupError> {
		self.run(
			database,
			GroupOperation::Owned {
				authority: request.authority,
				mutation: request.mutation,
			},
		)
		.await
	}
	pub(in crate::queue) async fn recover_stale_claim(
		&self,
		database: impl WriteDatabase,
		request: NativeQueueStaleClaimRecovery,
	) -> Result<NativeQueueGroupOutcome, NativeQueueGroupError> {
		self.run(database, GroupOperation::RecoverStaleClaim(request))
			.await
	}
	pub(in crate::queue) async fn delete_finished_job(
		&self,
		database: impl WriteDatabase,
		job_id: JobId,
		retention_micros: i64,
	) -> Result<NativeQueueGroupOutcome, NativeQueueGroupError> {
		self.run(
			database,
			GroupOperation::DeleteFinished {
				job_id,
				retention_micros,
			},
		)
		.await
	}
	pub(in crate::queue) async fn delete_expired_dead_letter(
		&self,
		database: impl WriteDatabase,
		dead_letter_id: JobId,
		retention_micros: i64,
	) -> Result<NativeQueueGroupOutcome, NativeQueueGroupError> {
		self.run(
			database,
			GroupOperation::DeleteExpiredDeadLetter {
				dead_letter_id,
				retention_micros,
			},
		)
		.await
	}
	pub(in crate::queue) async fn move_to_dead_letter(
		&self,
		database: impl WriteDatabase,
		request: NativeQueueDeadLetterMove,
	) -> Result<NativeQueueGroupOutcome, NativeQueueGroupError> {
		self.run(database, GroupOperation::MoveToDeadLetter(request))
			.await
	}
	pub(in crate::queue) async fn delete_dead_letter(
		&self,
		database: impl WriteDatabase,
		request: NativeQueueDeadLetterDeletion,
	) -> Result<NativeQueueGroupOutcome, NativeQueueGroupError> {
		self.run(
			database,
			GroupOperation::DeleteDeadLetter(request.dead_letter_id),
		)
		.await
	}
	pub(in crate::queue) async fn requeue_dead_letter(
		&self,
		database: impl WriteDatabase,
		request: NativeQueueDeadLetterRequeue,
	) -> Result<NativeQueueGroupOutcome, NativeQueueGroupError> {
		self.run(database, GroupOperation::RequeueDeadLetter(request))
			.await
	}
	/// Inserts one pending job unless the queue or task is paused, or returns the active job
	/// that already owns the request's dedupe key.
	pub(in crate::queue) async fn enqueue(
		&self,
		database: impl WriteDatabase,
		request: &NativeEnqueueRequest,
	) -> Result<NativeQueueGroupOutcome, NativeQueueGroupError> {
		self.run(
			database,
			GroupOperation::Enqueue {
				request,
				pauses: super::group_state::EnqueuePauses::Held,
			},
		)
		.await
	}
	/// Claims one candidate job when it is still pending, due, and not paused. The returned job
	/// is tentative until the caller's transaction commits.
	pub(in crate::queue) async fn claim(
		&self,
		database: impl WriteDatabase,
		request: &NativeClaimRequest,
	) -> Result<NativeQueueGroupOutcome, NativeQueueGroupError> {
		self.run(
			database,
			GroupOperation::Claim {
				request,
				pauses: super::group_state::ClaimPauses::Held,
			},
		)
		.await
	}
	/// Returns a job to pending when the claim that its owner and token identify holds the job
	/// and the job's execution has not started.
	pub(in crate::queue) async fn check_claim(
		&self,
		database: impl WriteDatabase,
		request: &NativeClaimCheckRequest,
	) -> Result<NativeQueueGroupOutcome, NativeQueueGroupError> {
		self.run(database, GroupOperation::ClaimCheck(request))
			.await
	}
	pub(in crate::queue) async fn change_pause(
		&self,
		database: impl WriteDatabase,
		request: NativeQueuePauseChange,
	) -> Result<NativeQueueGroupOutcome, NativeQueueGroupError> {
		self.run(database, GroupOperation::Pause(request)).await
	}

	/// Returns at most `limit` due pending jobs of `task_names`, earliest first, under the
	/// caller's admitted transaction. Selection skips jobs that another transaction holds, such
	/// as another worker's claim in progress, which means that concurrent workers select
	/// different jobs. It keeps no lock and grants no claim. While data retirement moves the v10 jobs, the jobs
	/// that are still in the v10 tables come first, and their selection skips no held job, so
	/// a concurrent claim of the same job finds it claimed and skips it.
	pub(in crate::queue) async fn select_due(
		&self,
		tx: &mut WriteTx<'_>,
		task_names: &[&str],
		limit: u32,
	) -> Result<Vec<JobId>, NativeQueueGroupError> {
		self.select_job_ids(tx, SelectionKind::Due { task_names, limit }, Some(limit))
			.await
	}

	/// Returns stale running jobs, the oldest heartbeat first, under the caller's admitted
	/// transaction: one group of at most `limit` for each store that the selection reads (see
	/// `select_job_id_groups`), each in heartbeat order. Selection holds no lock and recovers
	/// nothing.
	pub(in crate::queue) async fn select_stale_running(
		&self,
		tx: &mut WriteTx<'_>,
		limit: u32,
	) -> Result<Vec<Vec<JobId>>, NativeQueueGroupError> {
		self.inner
			.bindings
			.admission
			.execute_write_with_contracts(tx, async |contracts, tx| {
				let declaration = self.inner.bindings.jobs.declaration();
				let contract = contracts
					.bind(&declaration)
					.map_err(NativeOperationError::Admission)?;
				let policy = self
					.inner
					.bindings
					.jobs
					.installed_recovery_policy(&contract)
					.map_err(|error| {
						NativeOperationError::Operation(NativeQueueGroupError::PolicyBinding(
							QueueGroupError::JobBinding(error),
						))
					})?;
				self.select_admitted_job_id_groups(
					tx,
					&contracts,
					SelectionKind::StaleRunning {
						limit,
						stale_threshold_micros: policy.stale_threshold_micros(),
					},
				)
				.await
				.map_err(|error| *error)
			})
			.await
			.map_err(NativeQueueGroupError::from_operation)
	}

	/// Returns running jobs with a claim that carries the owner label `owner`, under the caller's
	/// admitted transaction. The label alone grants nothing.
	pub(in crate::queue) async fn select_running_by_owner(
		&self,
		tx: &mut WriteTx<'_>,
		owner: &str,
	) -> Result<Vec<JobId>, NativeQueueGroupError> {
		self.select_job_ids(tx, SelectionKind::RunningByOwner { owner }, None)
			.await
	}

	/// Returns at most `limit` jobs with `status` that finished more than `retention_micros` ago
	/// by the database clock, the earliest finish first.
	pub(in crate::queue) async fn select_finished(
		&self,
		tx: &mut WriteTx<'_>,
		status: super::JobStatus,
		retention_micros: i64,
		limit: u32,
	) -> Result<Vec<JobId>, NativeQueueGroupError> {
		self.select_job_ids(
			tx,
			SelectionKind::Finished {
				status: status.as_str(),
				retention_micros,
				limit,
			},
			Some(limit),
		)
		.await
	}

	/// Returns at most `limit` dead letters created more than `retention_micros` ago by the
	/// database clock, the oldest first.
	pub(in crate::queue) async fn select_expired_dead_letters(
		&self,
		tx: &mut WriteTx<'_>,
		retention_micros: i64,
		limit: u32,
	) -> Result<Vec<JobId>, NativeQueueGroupError> {
		self.select_job_ids(
			tx,
			SelectionKind::ExpiredDeadLetters {
				retention_micros,
				limit,
			},
			Some(limit),
		)
		.await
	}

	/// Returns the candidates of one selection, in one group for each store that the
	/// selection reads, each group in the selection's order.
	///
	/// While v10 writers remain, the jobs are in the v10 Queue tables. While data retirement
	/// moves them, a job can be in either store, so the selection reads the v10 tables first and
	/// Paranoid's storage after them, and returns one group for each. A job that moved between
	/// the two reads appears in both groups, and none is missed: when the first read finds no
	/// job, the move committed before it, so the second read finds the moved job. A candidate is
	/// only a candidate: the group operation that acts on it reads both stores under its own
	/// locks.
	async fn select_job_id_groups(
		&self,
		tx: &mut WriteTx<'_>,
		kind: SelectionKind<'_>,
	) -> Result<Vec<Vec<JobId>>, NativeQueueGroupError> {
		self.inner
			.bindings
			.admission
			.execute_write_with_contracts(tx, async |contracts, tx| {
				self.select_admitted_job_id_groups(tx, &contracts, kind)
					.await
					.map_err(|error| *error)
			})
			.await
			.map_err(NativeQueueGroupError::from_operation)
	}

	async fn select_admitted_job_id_groups(
		&self,
		tx: &mut WriteTx<'_>,
		contracts: &crate::runtime::admission::NativeAdmittedContracts,
		kind: SelectionKind<'_>,
	) -> Result<Vec<Vec<JobId>>, Box<NativeOperationError<NativeQueueGroupError>>> {
		match contracts.storage_route() {
			InstallationStorageRoute::NativeFresh | InstallationStorageRoute::NativeImported => {
				Ok(vec![
					self.select_native_job_ids(tx, kind)
						.await
						.map_err(NativeOperationError::Operation)?,
				])
			}
			InstallationStorageRoute::SourceCoexistence => {
				let tables = self.inner.coexistence_tables(contracts)?;
				Ok(vec![
					coexistence::coexistence_select(tx, &tables, kind)
						.await
						.map_err(NativeOperationError::Operation)?,
				])
			}
			InstallationStorageRoute::SourceRelocation => {
				let tables = self.inner.relocation_tables(contracts)?;
				let source = coexistence::coexistence_select(tx, &tables, kind)
					.await
					.map_err(NativeOperationError::Operation)?;
				let native = self
					.select_native_job_ids(tx, kind)
					.await
					.map_err(NativeOperationError::Operation)?;
				Ok(vec![source, native])
			}
		}
	}

	/// Returns the candidates of one selection as one list: each group in order, without a
	/// job that an earlier group already holds, and at most `limit` of them.
	async fn select_job_ids(
		&self,
		tx: &mut WriteTx<'_>,
		kind: SelectionKind<'_>,
		limit: Option<u32>,
	) -> Result<Vec<JobId>, NativeQueueGroupError> {
		let groups = self.select_job_id_groups(tx, kind).await?;
		let mut seen = BTreeSet::new();
		let mut candidates = groups
			.into_iter()
			.flatten()
			.filter(|job_id| seen.insert(*job_id))
			.collect::<Vec<_>>();
		if let Some(limit) = limit {
			candidates.truncate(usize::try_from(limit).expect("a u32 limit fits usize"));
		}
		Ok(candidates)
	}

	async fn select_native_job_ids(
		&self,
		tx: &mut WriteTx<'_>,
		kind: SelectionKind<'_>,
	) -> Result<Vec<JobId>, NativeQueueGroupError> {
		let storage = self.inner.bindings.storage.storage();
		let jobs = self.inner.bindings.storage.jobs();
		let keys = match kind {
			SelectionKind::Due { task_names, limit } => {
				storage
					.select_due_queue_job_keys(
						tx,
						jobs,
						self.inner.bindings.storage.pauses(),
						task_names,
						limit,
					)
					.await
			}
			SelectionKind::StaleRunning {
				limit,
				stale_threshold_micros,
			} => {
				storage
					.select_running_queue_job_keys(tx, jobs, limit, stale_threshold_micros)
					.await
			}
			SelectionKind::RunningByOwner { owner } => {
				storage
					.select_running_queue_job_keys_by_owner(tx, jobs, owner)
					.await
			}
			SelectionKind::Finished {
				status,
				retention_micros,
				limit,
			} => {
				storage
					.select_finished_queue_job_keys(tx, jobs, status, retention_micros, limit)
					.await
			}
			SelectionKind::ExpiredDeadLetters {
				retention_micros,
				limit,
			} => {
				storage
					.select_expired_queue_dead_letter_keys(
						tx,
						self.inner.bindings.storage.dead_letters(),
						retention_micros,
						limit,
					)
					.await
			}
		}
		.map_err(NativeQueueGroupError::Database)?;
		keys.iter()
			.map(|key| JobId::from_bytes(key.as_bytes()).ok())
			.collect::<Option<Vec<_>>>()
			.ok_or(NativeQueueGroupError::InvalidJobKey)
	}

	/// Reads the stale-claim recovery settings that the installation records for its Queue.
	pub(crate) async fn installed_recovery_policy(
		&self,
		database: impl crate::db::Database,
	) -> Result<super::QueueRecoveryPolicy, NativeQueueGroupError> {
		crate::db::run_transaction_read(
			database,
			"queue.native_group.installed_recovery_policy",
			async |tx| {
				self.inner
					.bindings
					.admission
					.execute_read_with_contracts(tx, async |contracts, _tx| {
						let declaration = self.inner.bindings.jobs.declaration();
						let contract = contracts
							.bind(&declaration)
							.map_err(NativeOperationError::Admission)?;
						self.inner
							.bindings
							.jobs
							.installed_recovery_policy(&contract)
							.map_err(|error| {
								NativeOperationError::Operation(
									NativeQueueGroupError::PolicyBinding(
										QueueGroupError::JobBinding(error),
									),
								)
							})
					})
					.await
					.map_err(NativeQueueGroupError::from_operation)
			},
		)
		.await
	}

	/// Reads one job. The read holds no lock and grants nothing. While data retirement moves
	/// the v10 jobs, it reads the v10 tables first and Paranoid's storage after them so that it
	/// finds a job that moves between the two reads.
	pub(in crate::queue) async fn fetch_job(
		&self,
		database: impl crate::db::Database,
		job_id: JobId,
	) -> Result<Option<super::Job>, NativeQueueGroupError> {
		crate::db::run_transaction_read(database, "queue.native_group.fetch_job", async |tx| {
			self.inner
				.bindings
				.admission
				.execute_read_with_contracts(tx, async |contracts, tx| {
					let route = contracts.storage_route();
					match route {
						InstallationStorageRoute::NativeFresh
						| InstallationStorageRoute::NativeImported => {}
						InstallationStorageRoute::SourceCoexistence => {
							let tables = self
								.inner
								.coexistence_tables(&contracts)
								.map_err(|error| *error)?;
							return coexistence::coexistence_fetch(tx, &tables, job_id)
								.await
								.map_err(NativeOperationError::Operation);
						}
						InstallationStorageRoute::SourceRelocation => {
							let tables = self
								.inner
								.relocation_tables(&contracts)
								.map_err(|error| *error)?;
							if let Some(job) = coexistence::coexistence_fetch(tx, &tables, job_id)
								.await
								.map_err(NativeOperationError::Operation)?
							{
								return Ok(Some(job));
							}
						}
					}
					let fail = |error| NativeOperationError::Operation(error);
					let declaration = self.inner.bindings.jobs.declaration();
					let contract = contracts
						.bind(&declaration)
						.map_err(NativeOperationError::Admission)?;
					let identity = RecordIdentity::new(
						self.inner.bindings.storage.jobs().clone(),
						RecordKey::new(job_id.as_bytes().to_vec()),
					);
					let binding = self
						.inner
						.bindings
						.jobs
						.bind_schema(&contract, job_id)
						.map_err(|error| {
							fail(NativeQueueGroupError::PolicyBinding(
								QueueGroupError::JobBinding(error),
							))
						})?;
					let storage = self.inner.bindings.storage.storage();
					let snapshots = if binding.separate_payload_key_prefix().is_some() {
						let keys = [
							identity.key().clone(),
							super::state::QueueJobSchemaBinding::payload_record_key(job_id),
						];
						storage
							.read_many(
								tx,
								crate::records::postgres::record_many::RecordManyReadFields {
									namespace: identity.namespace(),
									keys: &keys,
								},
							)
							.await
							.map_err(|error| fail(NativeQueueGroupError::RecordBatchRead(error)))?
							.records
					} else {
						vec![
							storage
								.read(tx, &identity)
								.await
								.map_err(|error| fail(NativeQueueGroupError::Storage(error)))?,
						]
					};
					let Some(record) = snapshots[0].record() else {
						return Ok(None);
					};
					let state = binding
						.inspect_stored_record(record)
						.map_err(|error| fail(NativeQueueGroupError::JobRecord(error)))?;
					binding
						.public_job(
							&state,
							snapshots.get(1).and_then(|snapshot| snapshot.record()),
						)
						.map(Some)
						.map_err(|error| fail(NativeQueueGroupError::JobRecord(error)))
				})
				.await
				.map_err(NativeQueueGroupError::from_operation)
		})
		.await
	}

	/// Lists dead letters in identifier order after `after`. The listing holds no lock and grants
	/// nothing. While v10 deployments still run, it lists the v10 dead-letter table. While data
	/// retirement moves the v10 records, it lists the v10 table first and Paranoid's storage after
	/// it and merges the two pages by identifier, as `list_jobs` does.
	pub(in crate::queue) async fn list_dead_letters(
		&self,
		database: impl crate::db::Database,
		after: Option<JobId>,
		limit: std::num::NonZeroU32,
	) -> Result<NativeDeadLetterPage, NativeQueueGroupError> {
		crate::db::run_transaction_read(
			database,
			"queue.native_group.list_dead_letters",
			async |tx| {
				self.inner
					.bindings
					.admission
					.execute_read_with_contracts(tx, async |contracts, tx| {
						let source_page = match contracts.storage_route() {
							InstallationStorageRoute::NativeFresh
							| InstallationStorageRoute::NativeImported => None,
							InstallationStorageRoute::SourceCoexistence => {
								let tables = self
									.inner
									.coexistence_tables(&contracts)
									.map_err(|error| *error)?;
								return coexistence::coexistence_list_dead_letters(
									tx, &tables, after, limit,
								)
								.await
								.map_err(NativeOperationError::Operation);
							}
							InstallationStorageRoute::SourceRelocation => {
								let tables = self
									.inner
									.relocation_tables(&contracts)
									.map_err(|error| *error)?;
								Some(
									coexistence::coexistence_list_dead_letters(
										tx, &tables, after, limit,
									)
									.await
									.map_err(NativeOperationError::Operation)?,
								)
							}
						};
						let fail = |error| NativeOperationError::Operation(error);
						let declaration = self.inner.bindings.auxiliary.declaration();
						let contract = contracts
							.bind(&declaration)
							.map_err(NativeOperationError::Admission)?;
						let dead_letters = self.inner.bindings.storage.dead_letters();
						let page = self
							.inner
							.bindings
							.storage
							.storage()
							.read_page(
								tx,
								&crate::records::NamespacePageRequest::new(
									dead_letters.clone(),
									after.map(|id| RecordKey::new(id.as_bytes().to_vec())),
									limit,
								),
							)
							.await
							.map_err(|error| fail(NativeQueueGroupError::DeadLetterPage(error)))?;
						let mut page_dead_letters = Vec::with_capacity(page.entries().len());
						for entry in page.entries() {
							let dead_letter_id =
								JobId::from_bytes(entry.identity().key().as_bytes())
									.map_err(|_| fail(NativeQueueGroupError::InvalidJobKey))?;
							let binding = self
								.inner
								.bindings
								.auxiliary
								.schemas()
								.bind_dead_letter(&contract, dead_letters.clone(), dead_letter_id)
								.map_err(|error| {
									fail(NativeQueueGroupError::DeadLetterRecord(error))
								})?;
							let state = binding
								.decode_stored_record(entry.identity(), entry.record())
								.map_err(|error| {
									fail(NativeQueueGroupError::DeadLetterRecord(error))
								})?;
							page_dead_letters.push(state.dead_letter().clone());
						}
						let limit = usize::try_from(limit.get()).expect("a u32 limit fits usize");
						let page_dead_letters = match source_page {
							None => page_dead_letters,
							Some(source_page) => {
								let mut merged = std::collections::BTreeMap::new();
								for dead_letter in source_page
									.dead_letters
									.into_iter()
									.chain(page_dead_letters)
								{
									merged.insert(dead_letter.id.as_bytes().to_vec(), dead_letter);
								}
								merged.into_values().take(limit).collect()
							}
						};
						let next_after = if page_dead_letters.len() == limit {
							page_dead_letters.last().map(|dead_letter| dead_letter.id)
						} else {
							None
						};
						Ok(NativeDeadLetterPage {
							dead_letters: page_dead_letters,
							next_after,
						})
					})
					.await
					.map_err(NativeQueueGroupError::from_operation)
			},
		)
		.await
	}

	/// Lists jobs in job-identifier order. The listing holds no lock and grants nothing. While
	/// data retirement moves the v10 jobs, it lists the v10 tables first and Paranoid's storage
	/// after them and merges the two pages by job identifier. Each store's page holds that
	/// store's first jobs after the cursor, so the merged page holds the first jobs of both; a job
	/// that moved between the two reads appears once, as Paranoid's storage holds it.
	pub(in crate::queue) async fn list_jobs(
		&self,
		database: impl crate::db::Database,
		request: NativeJobListRequest<'_>,
	) -> Result<NativeJobPage, NativeQueueGroupError> {
		let statuses: Vec<&str> = request
			.statuses
			.iter()
			.map(|status| status.as_str())
			.collect();
		let after = request
			.after
			.map(|id| RecordKey::new(id.as_bytes().to_vec()));
		crate::db::run_transaction_read(database, "queue.native_group.list_jobs", async |tx| {
			self.inner
				.bindings
				.admission
				.execute_read_with_contracts(tx, async |contracts, tx| {
					let source_page = match contracts.storage_route() {
						InstallationStorageRoute::NativeFresh
						| InstallationStorageRoute::NativeImported => None,
						InstallationStorageRoute::SourceCoexistence => {
							let tables = self
								.inner
								.coexistence_tables(&contracts)
								.map_err(|error| *error)?;
							return coexistence::coexistence_list(tx, &tables, &request)
								.await
								.map_err(NativeOperationError::Operation);
						}
						InstallationStorageRoute::SourceRelocation => {
							let tables = self
								.inner
								.relocation_tables(&contracts)
								.map_err(|error| *error)?;
							Some(
								coexistence::coexistence_list(tx, &tables, &request)
									.await
									.map_err(NativeOperationError::Operation)?,
							)
						}
					};
					let declaration = self.inner.bindings.jobs.declaration();
					let contract = contracts
						.bind(&declaration)
						.map_err(NativeOperationError::Admission)?;
					let fail = |error| NativeOperationError::Operation(error);
					let records = self
						.inner
						.bindings
						.storage
						.storage()
						.list_queue_job_records(
							tx,
							crate::queue::postgres::due::QueueJobListRequest {
								jobs: self.inner.bindings.storage.jobs(),
								statuses: &statuses,
								task_name: request.task_name.map(super::TaskName::as_str),
								after: after.as_ref(),
								limit: request.limit,
								separate_payload_key_prefix: self
									.inner
									.bindings
									.jobs
									.separate_payload_key_prefix(&contract)
									.map_err(|error| {
										fail(NativeQueueGroupError::PolicyBinding(
											QueueGroupError::JobBinding(error),
										))
									})?,
							},
						)
						.await
						.map_err(|error| fail(NativeQueueGroupError::Storage(error)))?;
					let mut jobs = Vec::with_capacity(records.len());
					for row in &records {
						let key = &row.key;
						let record = &row.job;
						let job_id = JobId::from_bytes(key.as_bytes())
							.map_err(|_| fail(NativeQueueGroupError::InvalidJobKey))?;
						let binding = self
							.inner
							.bindings
							.jobs
							.bind_schema(&contract, job_id)
							.map_err(|error| {
								fail(NativeQueueGroupError::PolicyBinding(
									QueueGroupError::JobBinding(error),
								))
							})?;
						let state = binding
							.inspect_stored_record(record)
							.map_err(|error| fail(NativeQueueGroupError::JobRecord(error)))?;
						jobs.push(
							binding
								.public_job(&state, row.payload.as_ref())
								.map_err(|error| fail(NativeQueueGroupError::JobRecord(error)))?,
						);
					}
					let limit = usize::try_from(request.limit).expect("a u32 limit fits usize");
					let jobs = match source_page {
						None => jobs,
						Some(source_page) => {
							let mut merged = std::collections::BTreeMap::new();
							for job in source_page.jobs.into_iter().chain(jobs) {
								merged.insert(job.id.as_bytes().to_vec(), job);
							}
							merged.into_values().take(limit).collect()
						}
					};
					let next_after = if jobs.len() == limit {
						jobs.last().map(|job| job.id)
					} else {
						None
					};
					Ok(NativeJobPage { jobs, next_after })
				})
				.await
				.map_err(NativeQueueGroupError::from_operation)
		})
		.await
	}

	async fn run(
		&self,
		database: impl WriteDatabase,
		operation: GroupOperation<'_>,
	) -> Result<NativeQueueGroupOutcome, NativeQueueGroupError> {
		run_transaction_write(database, operation.name(), async |tx| {
			let result = self
				.inner
				.bindings
				.admission
				.execute_write_with_contracts(tx, async |contracts, tx| {
					let route = contracts.storage_route();
					if operation.requires_source_relocation()
						&& route != InstallationStorageRoute::SourceRelocation
					{
						return Err(NativeOperationError::Admission(NativeAdmissionError::StorageRouteUnsupported { route }));
					}
					if route == InstallationStorageRoute::SourceCoexistence {
						let tables = self.inner.coexistence_tables(&contracts).map_err(|error| *error)?;
						let job_declaration = self.inner.bindings.jobs.declaration();
						let job_contract = contracts
							.bind(&job_declaration)
							.map_err(NativeOperationError::Admission)?;
						let recovery = self
							.inner
							.bindings
							.jobs
							.installed_recovery_policy(&job_contract)
							.map_err(|error| {
								NativeOperationError::Operation(NativeQueueGroupError::PolicyBinding(
									QueueGroupError::JobBinding(error),
								))
							})?;
						return self
							.inner
							.execute_coexistence(tx, &tables, &recovery, &operation)
							.await
							.map(Ok)
							.map_err(NativeOperationError::Operation);
					}
					let job_declaration = self.inner.bindings.jobs.declaration();
					let auxiliary_declaration = self.inner.bindings.auxiliary.declaration();
					let job_contract = contracts
						.bind(&job_declaration)
						.map_err(NativeOperationError::Admission)?;
					let auxiliary_contract = contracts
						.bind(&auxiliary_declaration)
						.map_err(NativeOperationError::Admission)?;
					let bound = self
						.inner
						.bindings
						.auxiliary
						.bind(
							&auxiliary_contract,
							&self.inner.bindings.jobs,
							&job_contract,
							&self.inner.namespaces,
						)
						.map_err(|error| {
							NativeOperationError::Operation(NativeQueueGroupError::PolicyBinding(
								error,
							))
						})?;
					match route {
						InstallationStorageRoute::NativeFresh | InstallationStorageRoute::NativeImported => self.inner.execute_native(tx, &bound, &operation).await.map_err(NativeOperationError::Operation),
						InstallationStorageRoute::SourceRelocation => {
							let source = self.inner.bindings.source.as_ref().ok_or(NativeOperationError::Operation(NativeQueueGroupError::SourceMappingAbsent))?;
							contracts.require_queue_source_storage(source.storage.descriptor(), source.storage.native_storage()).map_err(|e| NativeOperationError::Operation(NativeQueueGroupError::SourceObservation(e.into())))?;
							let job_source_declaration = source.imports.job_declaration();
							let auxiliary_source_declaration = source.imports.auxiliary_declaration();
							let source_job_contract = contracts.bind(&job_source_declaration).map_err(NativeOperationError::Admission)?;
							let source_auxiliary_contract = contracts.bind(&auxiliary_source_declaration).map_err(NativeOperationError::Admission)?;
							let readers = crate::queue::v10::source_group_storage::QueueSourceGroupReaders {
								jobs: &self.inner.bindings.jobs, job_contract: &job_contract,
								imports: &source.imports, source_job_contract: &source_job_contract,
								auxiliary: &self.inner.bindings.auxiliary, auxiliary_contract: &auxiliary_contract,
								source_auxiliary_contract: &source_auxiliary_contract,
							};
							readers.require_layout(source.storage.descriptor().layout().queue_layout()).map_err(|e| NativeOperationError::Operation(NativeQueueGroupError::SourceObservation(e)))?;
							self.inner.execute_source(tx, crate::queue::v10::source_group_storage::context::QueueSourceContext::Ordinary(&contracts), &bound, source, &readers, &operation).await.map_err(NativeOperationError::Operation)
						}
						InstallationStorageRoute::SourceCoexistence => unreachable!("source coexistence was rejected before bindings and protected data"),
					}
				})
				.await
				.map_err(NativeQueueGroupError::from_operation)?;
			// Only a known restored/cleaned semantic rejection reaches this inner result.
			result.map_err(NativeQueueGroupError::Rejected)
		})
		.await
	}
}
