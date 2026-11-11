//! Bounded upgrade pages share admission, progress locking, and owned transaction completion.

use crate::upgrade::wait::{require_valid_wait, tokio_instant};
use primitives_rs::time::Deadline;
use std::{num::NonZeroU32, sync::Arc, time::Duration};

use crate::db::transaction::run_registered_transaction_write;
use crate::db::{DatabaseError, TransactionFailure};
use crate::installation::ProtocolError;
use crate::kv::document::schema::IndexContractIdentity;
use crate::runtime::admission::upgrade::{NativeUpgradeAdmissionError, NativeUpgradeInvocation};
use crate::runtime::targets::ResolvedNativeRuntimeTarget;
use crate::scope::ScopeIdentity;
use crate::upgrade::observation::NativeUpgradeObservedControl;
use crate::upgrade::progress::document_conversion::{
	DocumentConversionPageError, DocumentConversionPageFields,
};
use crate::upgrade::progress::document_verification::{
	DocumentVerificationPageError, DocumentVerificationPageFields,
};
use crate::upgrade::progress::payload_conversion::{
	PayloadConversionPageError, PayloadConversionPageFields,
};
use crate::upgrade::progress::v10::source_fleet::{
	SourceFleetConversionError, SourceFleetConversionFields,
};
use crate::upgrade::progress::v10::source_lease::{
	SourceLeaseConversionError, SourceLeaseConversionFields,
};
use crate::upgrade::progress::v10::source_queue::{
	SourceQueueBatchKind, SourceQueueConversionError, SourceQueueConversionFields,
};
use crate::upgrade::progress::v10::source_raw::{
	SourceRawConversionError, SourceRawConversionFields,
};
use crate::upgrade::progress::{
	ControlProgressError, PgControlProgressLock, PgControlProgressStorage, SourceConversionBatch,
};
use crate::upgrade::work::UpgradeWork;
use crate::upgrade::work::v10::source_checkpoint::PgV10ConversionProcedure;

use super::PgNativeUpgradeRequests;
use crate::upgrade::progress::queue_job_conversion::{
	QueueJobConversionPageError, QueueJobConversionPageFields,
};

#[derive(Clone, Copy)]
pub(in crate::upgrade) enum NativeUpgradeBatchOperation<'a> {
	ConvertQueueJobs,
	Source(PgV10ConversionProcedure),
	ConvertDocument {
		scope: &'a ScopeIdentity,
	},
	VerifyDocument {
		scope: &'a ScopeIdentity,
		index: &'a IndexContractIdentity,
	},
	ConvertPayload {
		scope: &'a ScopeIdentity,
	},
}

pub(in crate::upgrade) struct NativeUpgradeBatchRequest<'a> {
	pub(crate) invocation: NativeUpgradeInvocation<'a>,
	pub(crate) resolved: &'a ResolvedNativeRuntimeTarget,
	pub(crate) operation: NativeUpgradeBatchOperation<'a>,
	/// Bounds awaited batch work after BEGIN and before owned transaction ending.
	pub(crate) batch_wait_timeout: Duration,
	/// The most records that one batch reads.
	pub(crate) limit: NonZeroU32,
}

/// Constructed only after the owned transaction has a known successful COMMIT.
pub(in crate::upgrade) struct CommittedUpgradeBatch {
	pub(crate) observed: NativeUpgradeObservedControl,
	pub(crate) work: UpgradeWork,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeUpgradeBatchError {
	#[error("Queue job conversion page failed")]
	QueueJobConversion(#[from] QueueJobConversionPageError),
	#[error("upgrade batch runtime admission failed")]
	Admission(#[from] NativeUpgradeAdmissionError),
	#[error("upgrade batch code or immutable target differs from its invocation")]
	BindingMismatch,
	#[error("upgrade batch wait must be positive and fit the local clock")]
	InvalidBatchWaitTimeout,
	#[error("upgrade batch wait elapsed before owned transaction ending")]
	BatchWaitElapsed,
	#[error("upgrade batch progress guard failed")]
	Progress(#[from] ControlProgressError),
	#[error("source raw batch fixed conversion failed")]
	Raw(#[from] SourceRawConversionError),
	#[error("source Fleet batch fixed conversion failed")]
	Fleet(#[from] SourceFleetConversionError),
	#[error("source lease batch fixed conversion failed")]
	Lease(#[from] SourceLeaseConversionError),
	#[error("source Queue batch fixed conversion failed")]
	Queue(#[from] SourceQueueConversionError),
	#[error("document conversion fixed page failed")]
	DocumentConversion(#[from] DocumentConversionPageError),
	#[error("document verification fixed page failed")]
	DocumentVerification(#[source] Box<DocumentVerificationPageError>),
	#[error("payload conversion fixed page failed")]
	PayloadConversion(#[source] Box<PayloadConversionPageError>),
	#[error("upgrade batch owned database work failed")]
	Database(#[source] DatabaseError),
	#[error("upgrade batch transaction protocol failed")]
	Protocol(#[source] ProtocolError),
	#[error("upgrade batch operation and rollback both failed")]
	RollbackFailed {
		#[source]
		operation_error: Box<Self>,
		rollback_error: DatabaseError,
	},
}

impl From<DocumentVerificationPageError> for NativeUpgradeBatchError {
	fn from(error: DocumentVerificationPageError) -> Self {
		Self::DocumentVerification(Box::new(error))
	}
}

impl From<PayloadConversionPageError> for NativeUpgradeBatchError {
	fn from(error: PayloadConversionPageError) -> Self {
		Self::PayloadConversion(Box::new(error))
	}
}

impl TransactionFailure for NativeUpgradeBatchError {
	fn from_protocol_error(error: ProtocolError) -> Self {
		Self::Protocol(error)
	}
	fn from_database_error(error: DatabaseError) -> Self {
		Self::Database(error)
	}
	fn from_rollback_failure(operation_error: Self, rollback_error: DatabaseError) -> Self {
		Self::RollbackFailed {
			operation_error: Box::new(operation_error),
			rollback_error,
		}
	}
	fn involves_database_failure(&self) -> bool {
		match self {
			Self::Admission(error) => error.involves_database_failure(),
			Self::Progress(error) => error.involves_database_failure(),
			Self::Raw(error) => error.involves_database_failure(),
			Self::Fleet(error) => error.involves_database_failure(),
			Self::Lease(error) => error.involves_database_failure(),
			Self::Queue(error) => error.involves_database_failure(),
			Self::QueueJobConversion(error) => error.involves_database_failure(),
			Self::DocumentConversion(error) => error.involves_database_failure(),
			Self::DocumentVerification(error) => error.involves_database_failure(),
			Self::PayloadConversion(error) => error.involves_database_failure(),
			Self::Database(_) | Self::RollbackFailed { .. } => true,
			_ => false,
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Admission(error) => error.commit_outcome_is_unknown(),
			Self::Progress(error) => error.commit_outcome_is_unknown(),
			Self::Raw(error) => error.commit_outcome_is_unknown(),
			Self::Fleet(error) => error.commit_outcome_is_unknown(),
			Self::Lease(error) => error.commit_outcome_is_unknown(),
			Self::Queue(error) => error.commit_outcome_is_unknown(),
			Self::QueueJobConversion(error) => error.commit_outcome_is_unknown(),
			Self::DocumentConversion(error) => error.commit_outcome_is_unknown(),
			Self::DocumentVerification(error) => error.commit_outcome_is_unknown(),
			Self::PayloadConversion(error) => error.commit_outcome_is_unknown(),
			Self::Database(error) => error.commit_outcome_is_unknown(),
			Self::RollbackFailed {
				operation_error,
				rollback_error,
			} => {
				operation_error.commit_outcome_is_unknown()
					|| rollback_error.commit_outcome_is_unknown()
			}
			_ => false,
		}
	}
}

impl PgNativeUpgradeRequests {
	/// Runs one fixed page and commits its data and progress atomically.
	/// Row limits do not bound bytes or preempt synchronous application code.
	pub(in crate::upgrade) async fn execute_batch(
		&self,
		mut request: NativeUpgradeBatchRequest<'_>,
	) -> Result<CommittedUpgradeBatch, NativeUpgradeBatchError> {
		request.invocation.require_bindings(
			&self.config.installation_binding,
			&self.config.control_storage,
			&self.config.administration,
		)?;
		if !Arc::ptr_eq(
			request.resolved.code().administration(),
			&self.config.administration,
		) || !Arc::ptr_eq(
			request.resolved.code().installation(),
			&self.config.installation_binding,
		) || !request
			.resolved
			.target()
			.matches_target(request.invocation.target())
		{
			return Err(NativeUpgradeBatchError::BindingMismatch);
		}
		require_valid_wait(
			request.batch_wait_timeout,
			NativeUpgradeBatchError::InvalidBatchWaitTimeout,
		)?;
		let progress = PgControlProgressStorage::new(
			&self.config.installation_binding,
			&self.config.control_storage,
			&self.config.administration,
		)?;
		let registration = request.invocation.prepare_transaction()?;
		let (observed, work) = run_registered_transaction_write(
			&self.config.control_pool,
			request.operation.label(),
			registration,
			async |tx| {
				request.invocation.require_transaction(tx)?;
				let started_at = tokio::time::Instant::now().into_std();
				let deadline = Deadline::new_from_base(started_at, request.batch_wait_timeout)
					.map_err(|_| NativeUpgradeBatchError::InvalidBatchWaitTimeout)?;
				let take_until =
					Deadline::new_from_base(started_at, request.batch_wait_timeout / 2)
						.map_err(|_| NativeUpgradeBatchError::InvalidBatchWaitTimeout)?;
				let timing = BatchPageTiming {
					deadline,
					take_until,
				};
				let interruption = tx.statement_interruption_marker();
				tokio::time::timeout_at(tokio_instant(deadline), async {
					let lock = progress
						.lock(
							tx,
							self.config.conflict_attempts,
							self.config.control_pin_wait_timeout,
						)
						.await?;
					request
						.execute_page(lock, self.config.conflict_attempts, timing)
						.await
				})
				.await
				.map_err(|_| {
					interruption.mark_interrupted();
					NativeUpgradeBatchError::BatchWaitElapsed
				})?
			},
		)
		.await?;
		Ok(CommittedUpgradeBatch { observed, work })
	}
}

#[derive(Clone, Copy)]
struct BatchPageTiming {
	deadline: Deadline,
	take_until: Deadline,
}

impl NativeUpgradeBatchRequest<'_> {
	async fn execute_page(
		&self,
		lock: PgControlProgressLock<'_, '_>,
		conflict_attempts: NonZeroU32,
		timing: BatchPageTiming,
	) -> Result<(NativeUpgradeObservedControl, UpgradeWork), NativeUpgradeBatchError> {
		let result = match self.operation {
			NativeUpgradeBatchOperation::ConvertQueueJobs => {
				let result = lock
					.convert_queue_job_page(QueueJobConversionPageFields {
						invocation: &self.invocation,
						resolved: self.resolved,
						limit: self.limit,
						conflict_attempts,
					})
					.await?;
				(result.observed, result.work)
			}
			NativeUpgradeBatchOperation::Source(procedure) => {
				// Half of the protected wait remains for the last key and progress write.
				let batch = SourceConversionBatch {
					limit: self.limit,
					take_until: timing.take_until,
				};
				match procedure {
					PgV10ConversionProcedure::RawBytesRelocationV1 => {
						let result = lock
							.convert_source_raw_page(SourceRawConversionFields {
								invocation: &self.invocation,
								resolved: self.resolved,
								conflict_attempts,
								batch,
							})
							.await?;
						(result.observed, result.work)
					}
					PgV10ConversionProcedure::FleetStateRelocationV1 => {
						let result = lock
							.convert_source_fleet_page(SourceFleetConversionFields {
								invocation: &self.invocation,
								resolved: self.resolved,
								conflict_attempts,
								batch,
							})
							.await?;
						(result.observed, result.work)
					}
					PgV10ConversionProcedure::LeaseAggregateRelocationV1 => {
						let result = lock
							.convert_source_lease_page(SourceLeaseConversionFields {
								invocation: &self.invocation,
								resolved: self.resolved,
								conflict_attempts,
								batch,
							})
							.await?;
						(result.observed, result.work)
					}
					PgV10ConversionProcedure::QueueJobRelocationV1
					| PgV10ConversionProcedure::QueueDeadLetterRelocationV1
					| PgV10ConversionProcedure::QueuePauseRelocationV1 => {
						let kind = match procedure {
							PgV10ConversionProcedure::QueueJobRelocationV1 => {
								SourceQueueBatchKind::Jobs
							}
							PgV10ConversionProcedure::QueueDeadLetterRelocationV1 => {
								SourceQueueBatchKind::DeadLetters
							}
							PgV10ConversionProcedure::QueuePauseRelocationV1 => {
								SourceQueueBatchKind::Pauses
							}
							_ => unreachable!("the Queue branch selects only Queue procedures"),
						};
						let result = lock
							.convert_source_queue_page(SourceQueueConversionFields {
								invocation: &self.invocation,
								resolved: self.resolved,
								kind,
								batch,
							})
							.await?;
						(result.observed, result.work)
					}
				}
			}
			NativeUpgradeBatchOperation::ConvertDocument { scope } => {
				let result = lock
					.convert_document_page(DocumentConversionPageFields {
						invocation: &self.invocation,
						resolved: self.resolved,
						scope,
						limit: self.limit,
						conflict_attempts,
					})
					.await?;
				(result.observed, result.work)
			}
			NativeUpgradeBatchOperation::VerifyDocument { scope, index } => {
				let result = lock
					.verify_document_page(DocumentVerificationPageFields {
						invocation: &self.invocation,
						resolved: self.resolved,
						scope,
						index,
						limit: self.limit,
						deadline: timing.deadline,
					})
					.await?;
				(result.observed, result.work)
			}
			NativeUpgradeBatchOperation::ConvertPayload { scope } => {
				let result = lock
					.convert_payload_page(PayloadConversionPageFields {
						invocation: &self.invocation,
						candidate: self.resolved.candidate(),
						registry: self.resolved.code().scopes(),
						scope,
						limit: self.limit,
						conflict_attempts,
						deadline: timing.deadline,
					})
					.await?;
				(result.observed, result.work)
			}
		};
		Ok::<_, NativeUpgradeBatchError>(result)
	}
}

impl NativeUpgradeBatchError {
	/// Returns an actual retained driver cause without replacing the primary typed failure.
	pub(crate) fn database_error(&self) -> Option<&crate::db::DatabaseError> {
		match self {
			Self::Admission(error) => error.database_error(),
			Self::Progress(error) => error.database_error(),
			Self::Raw(error) => error.database_error(),
			Self::Fleet(error) => error.database_error(),
			Self::Lease(error) => error.database_error(),
			Self::Queue(error) => error.database_error(),
			Self::QueueJobConversion(error) => error.database_error(),
			Self::DocumentConversion(error) => error.database_error(),
			Self::DocumentVerification(error) => error.database_error(),
			Self::PayloadConversion(error) => error.database_error(),
			Self::Database(error) => Some(error),
			Self::RollbackFailed {
				operation_error,
				rollback_error,
			} => operation_error.database_error().or(Some(rollback_error)),
			_ => None,
		}
	}
}

impl NativeUpgradeBatchOperation<'_> {
	fn label(self) -> &'static str {
		match self {
			Self::ConvertQueueJobs => "db.native_upgrade_requests.convert_queue_job_batch",
			Self::ConvertDocument { .. } => "db.native_upgrade_requests.convert_document_batch",
			Self::VerifyDocument { .. } => "db.native_upgrade_requests.verify_document_batch",
			Self::ConvertPayload { .. } => "db.native_upgrade_requests.convert_payload_batch",
			Self::Source(procedure) => match procedure {
				PgV10ConversionProcedure::RawBytesRelocationV1 => {
					"db.native_upgrade_requests.convert_source_raw_batch"
				}
				PgV10ConversionProcedure::FleetStateRelocationV1 => {
					"db.native_upgrade_requests.convert_source_fleet_batch"
				}
				PgV10ConversionProcedure::LeaseAggregateRelocationV1 => {
					"db.native_upgrade_requests.convert_source_lease_batch"
				}
				PgV10ConversionProcedure::QueueJobRelocationV1
				| PgV10ConversionProcedure::QueueDeadLetterRelocationV1
				| PgV10ConversionProcedure::QueuePauseRelocationV1 => {
					"db.native_upgrade_requests.convert_source_queue_batch"
				}
			},
		}
	}
}
