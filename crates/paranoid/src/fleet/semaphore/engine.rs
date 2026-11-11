//! Authenticated native Semaphore control and owned task execution with retained scoped code.

use std::{num::NonZeroU32, sync::Arc};

use crate::db::{
	Database, DatabaseError, TransactionFailure, WriteDatabase, run_transaction_read,
	run_transaction_write,
};
use crate::installation::ProtocolError;
use crate::records::postgres::{PgPointStorage, StateStorageError};
use crate::records::{PointRecordLock, RecordIdentity, RecordMetadata};
use crate::runtime::admission::{NativeAdmissionError, NativeOperationError, PgNativeAdmission};

use super::state::{SemaphoreSettings, SemaphoreStateError};
use crate::fleet::primitive_scope::{
	PrimitiveScopeBindingError, RetainedSemaphoreScopeCode, SemaphoreResourceReadFields,
	SemaphoreResourceWriteFields,
};
use crate::fleet::{SemaphoreKey, SemaphoreStatus};

mod delivery;
mod driver;
mod execution;
mod options;
mod pending;
pub(crate) use options::{
	NativeSemaphoreAcquisitionChoice, NativeSemaphoreStartOptions,
	NativeSemaphoreStartOptionsFields, NativeSemaphoreWaitingPolicy,
	NativeSemaphoreWaitingPolicyFields,
};
// The runtime factory supplies actual control-pool/timing composition in its own slice.
pub(crate) use execution::{
	NativeSemaphoreExecutionBindings, NativeSemaphoreExecutionBindingsFields,
};
pub(crate) use execution::{
	SemaphoreExecutionTiming, SemaphoreExecutionTimingFields, SemaphoreTimingError,
};
pub(crate) use options::{SemaphoreDriverTiming, SemaphoreDriverTimingFields};
pub(crate) use pending::{
	NativeSemaphoreAcquireFields, NativeSemaphoreAcquisitionObservation,
	NativeSemaphoreCleanupObservation, NativeSemaphoreCompletionObservation,
	NativeSemaphoreDiagnosticError, NativeSemaphoreOperation, NativeSemaphoreReleaseObservation,
};

pub(crate) use delivery::{
	NativeSemaphoreCallerCtx, NativeSemaphoreClient, NativeSemaphoreStartResult,
};
pub(crate) use driver::{
	NativeSemaphoreAccountingObservation, NativeSemaphoreLocalObservation, NativeSemaphoreObserver,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeSemaphoreCancellationReason {
	ClientClosed,
	RuntimeStopped(crate::runtime::local_admission::RuntimeAdmissionStop),
}

pub(crate) struct NativeSemaphoreBindingsFields {
	pub(crate) code: Arc<RetainedSemaphoreScopeCode>,
	pub(crate) admission: Arc<PgNativeAdmission>,
	pub(crate) conflict_attempts: NonZeroU32,
}
pub(crate) struct NativeSemaphoreBindings {
	code: Arc<RetainedSemaphoreScopeCode>,
	admission: Arc<PgNativeAdmission>,
	storage: PgPointStorage,
	conflict_attempts: NonZeroU32,
}
impl NativeSemaphoreBindings {
	pub(crate) fn new(fields: NativeSemaphoreBindingsFields) -> Result<Self, NativeSemaphoreError> {
		let storage = fields
			.admission
			.app_storage(fields.code.declaration().identity())?;
		Ok(Self {
			code: fields.code,
			admission: fields.admission,
			storage,
			conflict_attempts: fields.conflict_attempts,
		})
	}
}

pub(crate) struct NativeSemaphoreFields {
	pub(crate) bindings: Arc<NativeSemaphoreBindings>,
	pub(crate) key: SemaphoreKey,
	pub(crate) settings: SemaphoreSettings,
}
#[derive(Clone)]
pub(crate) struct PgNativeSemaphore {
	bindings: Arc<NativeSemaphoreBindings>,
	key: SemaphoreKey,
	settings: SemaphoreSettings,
	identity: RecordIdentity,
}
impl PgNativeSemaphore {
	pub(crate) fn scope(&self) -> &crate::scope::ScopeIdentity {
		self.bindings.code.declaration().identity()
	}
	pub(crate) fn key(&self) -> &SemaphoreKey {
		&self.key
	}
	pub(crate) fn settings(&self) -> &SemaphoreSettings {
		&self.settings
	}
	pub(crate) fn new(fields: NativeSemaphoreFields) -> Self {
		let identity = fields
			.bindings
			.code
			.declaration()
			.record_identity(&fields.key);
		Self {
			bindings: fields.bindings,
			key: fields.key,
			settings: fields.settings,
			identity,
		}
	}
	pub(crate) async fn fetch_status(
		&self,
		database: impl Database,
	) -> Result<SemaphoreStatus, NativeSemaphoreError> {
		run_transaction_read(
			database,
			"fleet.native_semaphore.fetch_status",
			async |tx| {
				self.bindings
					.admission
					.execute_read_with_contracts(tx, async |contracts, tx| {
						let declaration = self.bindings.code.declaration();
						let reader = contracts
							.bind_semaphore_reader(&declaration)
							.map_err(NativeOperationError::Admission)?;
						let result = async {
							let snapshot = self.bindings.storage.read(tx, &self.identity).await?;
							match snapshot.record() {
								Some(record) => reader
									.status(
										SemaphoreResourceReadFields {
											identity: &self.identity,
											key: &self.key,
											settings: &self.settings,
											record,
										},
										snapshot.observed_at(),
									)
									.map_err(NativeSemaphoreError::from),
								None => Ok(SemaphoreStatus {
									current_count: 0,
									capacity: self.settings.capacity(),
								}),
							}
						}
						.await;
						result.map_err(NativeOperationError::Operation)
					})
					.await
					.map_err(NativeSemaphoreError::from_operation)
			},
		)
		.await
	}
	/// Clears all retained slots without deleting permanent resource settings or stopping tasks.
	pub(crate) async fn reset(
		&self,
		database: impl WriteDatabase,
	) -> Result<u64, NativeSemaphoreError> {
		run_transaction_write(database, "fleet.native_semaphore.reset", async |tx| {
			self.bindings
				.admission
				.execute_write_with_contracts(tx, async |contracts, tx| {
					let declaration = self.bindings.code.declaration();
					let writer = contracts
						.bind_semaphore_writer(&declaration)
						.map_err(NativeOperationError::Admission)?;
					let result = async {
						let resource = writer.bind_resource(SemaphoreResourceWriteFields {
							identity: &self.identity,
							key: &self.key,
							settings: &self.settings,
						})?;
						let lock = self
							.bindings
							.storage
							.lock(tx, &self.identity, self.bindings.conflict_attempts)
							.await?;
						let Some(record) = lock.snapshot().record() else {
							lock.keep().await?;
							return Ok(0);
						};
						let mut state = resource.decode_record(record)?;
						let count = resource.reset(&mut state)?;
						if count == 0 {
							lock.keep().await?;
						} else {
							let metadata = RecordMetadata {
								expires_at: None,
								updated_at: lock.snapshot().observed_at(),
							};
							lock.write(resource.encode_record(&state, metadata)?)
								.await?;
						}
						Ok::<_, NativeSemaphoreError>(count)
					}
					.await;
					Ok(result)
				})
				.await
				.map_err(NativeSemaphoreError::from_operation)?
		})
		.await
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeSemaphoreError {
	#[error("native Semaphore registered admission failed")]
	RegisteredAdmission(
		#[from] crate::runtime::admission::owned_primitive::OwnedPrimitiveAdmissionError,
	),
	#[error("native Semaphore local ownership failed")]
	Local(#[from] crate::runtime::local_admission::RuntimeAdmissionError),
	#[error("native Semaphore managed acquisition was cancelled")]
	AcquisitionCancelled {
		reason: NativeSemaphoreCancellationReason,
	},
	#[error("native Semaphore fresh queued delivery was rejected")]
	DeliveryRejected,
	#[error("native Semaphore managed driver ended without a response")]
	DriverClosed,
	#[error("native Semaphore waits must be positive and fit the local clock")]
	InvalidTiming,
	#[error("native Semaphore operation is not in the required private phase")]
	OperationState,
	#[error("native Semaphore protected work exceeded its configured wait")]
	ProtectedTimeout,
	#[error("native Semaphore ending or local continuation remains pending")]
	EndingPending,
	#[error("native Semaphore record preparation failed")]
	Record {
		identity: RecordIdentity,
		#[source]
		source: Box<Self>,
	},
	#[error("native Semaphore preparation and point restoration failed")]
	PreparationCleanup {
		#[source]
		preparation_error: Box<Self>,
		cleanup_error: Box<StateStorageError>,
	},
	#[error("native Semaphore physical proof and local settlement failed")]
	PhysicalSettlementFailed {
		#[source]
		operation_error: Box<Self>,
		settlement_error: Box<Self>,
	},
	#[error("native Semaphore cause retention also failed")]
	DiagnosticRetention {
		#[source]
		operation_error: Arc<Self>,
		retention_error: crate::runtime::local_admission::RuntimeAdmissionError,
	},

	#[error("native Semaphore admission failed")]
	Admission(#[from] NativeAdmissionError),
	#[error("native Semaphore scoped binding failed")]
	Scope(#[from] PrimitiveScopeBindingError),
	#[error("native Semaphore state operation failed")]
	State(#[from] SemaphoreStateError),
	#[error("native Semaphore record storage failed")]
	Storage(#[from] StateStorageError),
	#[error("native Semaphore database work failed")]
	Database(#[source] DatabaseError),
	#[error("native Semaphore transaction protocol failed")]
	Protocol(#[source] ProtocolError),
	/// The operation failed, and the rollback of its transaction failed in the database. The
	/// source is the operation's failure.
	#[error("native Semaphore operation failed and rollback also failed")]
	RollbackFailed {
		#[source]
		operation_error: Box<Self>,
		rollback_error: DatabaseError,
	},
}
impl NativeSemaphoreError {
	fn from_operation(error: NativeOperationError<Self>) -> Self {
		match error {
			NativeOperationError::Admission(error) => Self::Admission(error),
			NativeOperationError::Operation(error) => error,
		}
	}
}
impl TransactionFailure for NativeSemaphoreError {
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
			Self::RegisteredAdmission(error) => error.involves_database_failure(),
			Self::Record { source, .. } => source.involves_database_failure(),
			Self::PreparationCleanup {
				preparation_error,
				cleanup_error,
			} => {
				preparation_error.involves_database_failure()
					|| cleanup_error.database_error().is_some()
			}
			Self::PhysicalSettlementFailed {
				operation_error,
				settlement_error,
			} => {
				operation_error.involves_database_failure()
					|| settlement_error.involves_database_failure()
			}
			Self::DiagnosticRetention {
				operation_error, ..
			} => operation_error.involves_database_failure(),
			Self::Admission(error) => error.involves_database_failure(),
			Self::Storage(error) => error.database_error().is_some(),
			Self::Database(_) | Self::RollbackFailed { .. } => true,
			Self::Local(_)
			| Self::InvalidTiming
			| Self::AcquisitionCancelled { .. }
			| Self::DeliveryRejected
			| Self::DriverClosed
			| Self::OperationState
			| Self::ProtectedTimeout
			| Self::EndingPending
			| Self::Scope(_)
			| Self::State(_)
			| Self::Protocol(_) => false,
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::RegisteredAdmission(error) => error.commit_outcome_is_unknown(),
			Self::Record { source, .. } => source.commit_outcome_is_unknown(),
			Self::PreparationCleanup {
				preparation_error,
				cleanup_error,
			} => {
				preparation_error.commit_outcome_is_unknown()
					|| cleanup_error
						.database_error()
						.is_some_and(DatabaseError::commit_outcome_is_unknown)
			}
			Self::PhysicalSettlementFailed {
				operation_error,
				settlement_error,
			} => {
				operation_error.commit_outcome_is_unknown()
					|| settlement_error.commit_outcome_is_unknown()
			}
			Self::DiagnosticRetention {
				operation_error, ..
			} => operation_error.commit_outcome_is_unknown(),
			Self::Admission(error) => error.commit_outcome_is_unknown(),
			Self::Storage(error) => error
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
			Self::Database(error) => error.commit_outcome_is_unknown(),
			Self::RollbackFailed {
				operation_error, ..
			} => operation_error.commit_outcome_is_unknown(),
			Self::Local(_)
			| Self::InvalidTiming
			| Self::AcquisitionCancelled { .. }
			| Self::DeliveryRejected
			| Self::DriverClosed
			| Self::OperationState
			| Self::ProtectedTimeout
			| Self::EndingPending
			| Self::Scope(_)
			| Self::State(_)
			| Self::Protocol(_) => false,
		}
	}
}
