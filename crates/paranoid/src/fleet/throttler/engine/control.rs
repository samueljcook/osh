//! Ordinary authenticated Throttler status and activity control, without managed grant ownership.

use super::{
	DatabaseError, PgNativeThrottler, PrimitiveScopeBindingError, ProtocolError, RecordIdentity,
	ThrottlerResourceWriteFields, TransactionFailure,
};
use crate::db::{Database, WriteDatabase, run_transaction_read, run_transaction_write};
use crate::fleet::ThrottlerStatus;
use crate::fleet::primitive_scope::ThrottlerResourceReadFields;
use crate::fleet::throttler::arithmetic::semantic_settings::ThrottlerCircuitSettings;
use crate::fleet::throttler::state::{
	NativeThrottlerState, NativeThrottlerStateFields, ThrottlerCircuitAction,
	ThrottlerCircuitInput, ThrottlerMutationDecision, ThrottlerStateError, resource_status,
};
use crate::records::postgres::StateStorageError;
use crate::records::{PointRecordLock, RecordMetadata};
use crate::runtime::admission::{NativeAdmissionError, NativeOperationError};

#[derive(Clone, Copy)]
enum ControlAction {
	ResetActivity,
	/// Opens or closes the Throttler circuit identified by `circuit` in its settings.
	Circuit {
		action: ThrottlerCircuitAction,
		circuit: ThrottlerCircuitSettings,
	},
}
impl PgNativeThrottler {
	/// Reads actual live activity using one database-clock point observation and the retained
	/// reader.
	pub(crate) async fn fetch_status(
		&self,
		database: impl Database,
	) -> Result<ThrottlerStatus, NativeThrottlerPointError> {
		run_transaction_read(
			database,
			"fleet.native_throttler.fetch_status",
			async |tx| {
				self.bindings
					.admission
					.execute_read_with_contracts(tx, async |contracts, tx| {
						let declaration = self.bindings.code.declaration();
						let reader = contracts
							.bind_throttler_reader(&declaration)
							.map_err(NativeOperationError::Admission)?;
						let result = async {
							let snapshot = self
								.bindings
								.storage
								.read(tx, self.identity.record())
								.await?;
							match snapshot.record() {
								Some(record) => reader
									.status(
										ThrottlerResourceReadFields {
											identity: self.identity.record(),
											key: self.identity.resource(),
											settings: &self.settings,
											record,
										},
										snapshot.observed_at(),
									)
									.map_err(NativeThrottlerPointError::from),
								None => Ok(resource_status(
									&NativeThrottlerState::new(NativeThrottlerStateFields {
										key: self.identity.resource().clone(),
										settings: self.settings,
									}),
									snapshot.observed_at(),
								)),
							}
						}
						.await
						.map_err(|error| NativeThrottlerPointError::Record {
							identity: self.identity.record().clone(),
							source: Box::new(error),
						});
						result.map_err(NativeOperationError::Operation)
					})
					.await
					.map_err(NativeThrottlerPointError::from_operation)
			},
		)
		.await
	}
	/// Clears activity while leaving permanent resource settings and outcome anchors intact.
	pub(crate) async fn reset(
		&self,
		database: impl WriteDatabase,
	) -> Result<(), NativeThrottlerPointError> {
		self.control(database, ControlAction::ResetActivity).await
	}
	pub(crate) async fn open_circuit(
		&self,
		database: impl WriteDatabase,
	) -> Result<(), NativeThrottlerPointError> {
		let action = self.circuit_action(ThrottlerCircuitAction::Open)?;
		self.control(database, action).await
	}
	pub(crate) async fn close_circuit(
		&self,
		database: impl WriteDatabase,
	) -> Result<(), NativeThrottlerPointError> {
		let action = self.circuit_action(ThrottlerCircuitAction::Close)?;
		self.control(database, action).await
	}
	/// The settings decide without database work, so a circuit operation on a Throttler without a
	/// circuit starts no transaction and leaves a caller's transaction usable.
	fn circuit_action(
		&self,
		action: ThrottlerCircuitAction,
	) -> Result<ControlAction, NativeThrottlerPointError> {
		let circuit = self
			.settings
			.circuit()
			.ok_or(NativeThrottlerPointError::CircuitAbsent)?;
		Ok(ControlAction::Circuit { action, circuit })
	}
	async fn control(
		&self,
		database: impl WriteDatabase,
		action: ControlAction,
	) -> Result<(), NativeThrottlerPointError> {
		let label = match action {
			ControlAction::ResetActivity => "fleet.native_throttler.reset",
			ControlAction::Circuit {
				action: ThrottlerCircuitAction::Open,
				..
			} => "fleet.native_throttler.open_circuit",
			ControlAction::Circuit {
				action: ThrottlerCircuitAction::Close,
				..
			} => "fleet.native_throttler.close_circuit",
		};
		run_transaction_write(database, label, async |tx| {
			self.bindings
				.admission
				.execute_write_with_contracts(tx, async |contracts, tx| {
					let declaration = self.bindings.code.declaration();
					let writer = contracts
						.bind_throttler_writer(&declaration)
						.map_err(NativeOperationError::Admission)?;
					let result = async {
						let resource = writer.bind_resource(ThrottlerResourceWriteFields {
							identity: self.identity.record(),
							key: self.identity.resource(),
							settings: &self.settings,
						})?;
						let lock = self
							.bindings
							.storage
							.lock(
								tx,
								self.identity.record(),
								self.bindings.group_budget.conflict_attempts,
							)
							.await?;
						let observed_at = lock.snapshot().observed_at();
						let mut state = match lock.snapshot().record() {
							Some(record) => resource.decode_record(record)?,
							None if matches!(action, ControlAction::ResetActivity) => {
								lock.keep().await?;
								return Ok(());
							}
							None => NativeThrottlerState::new(NativeThrottlerStateFields {
								key: self.identity.resource().clone(),
								settings: self.settings,
							}),
						};
						let decision = match action {
							ControlAction::ResetActivity => resource.reset(&mut state)?,
							ControlAction::Circuit { action, circuit } => resource.set_circuit(
								&mut state,
								circuit,
								ThrottlerCircuitInput {
									action,
									observed_at,
								},
							)?,
						};
						match decision {
							ThrottlerMutationDecision::Keep => lock.keep().await?,
							ThrottlerMutationDecision::Write => {
								lock.write(resource.encode_record(
									&state,
									RecordMetadata {
										expires_at: None,
										updated_at: observed_at,
									},
								)?)
								.await?
							}
						}
						Ok::<_, NativeThrottlerPointError>(())
					}
					.await
					.map_err(|error| NativeThrottlerPointError::Record {
						identity: self.identity.record().clone(),
						source: Box::new(error),
					});
					// A completed point rejection preserves prior caller work; unfinished guards retain sticky failure.
					Ok(result)
				})
				.await
				.map_err(NativeThrottlerPointError::from_operation)?
		})
		.await
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeThrottlerPointError {
	#[error("native Throttler admission failed")]
	Admission(#[source] Box<NativeAdmissionError>),
	#[error("native Throttler scoped binding failed")]
	Scope(#[from] PrimitiveScopeBindingError),
	#[error("native Throttler record state failed")]
	State(#[from] ThrottlerStateError),
	#[error("native Throttler record storage failed")]
	Storage(#[source] Box<StateStorageError>),
	#[error("native Throttler settings have no circuit to open or close")]
	CircuitAbsent,
	#[error("native Throttler record operation failed")]
	Record {
		identity: RecordIdentity,
		#[source]
		source: Box<Self>,
	},
	#[error("native Throttler database work failed")]
	Database(#[source] Box<DatabaseError>),
	#[error("native Throttler transaction protocol failed")]
	Protocol(#[source] ProtocolError),
	#[error("native Throttler operation failed and rollback also failed")]
	RollbackFailed {
		#[source]
		operation_error: Box<Self>,
		rollback_error: Box<DatabaseError>,
	},
}
impl From<StateStorageError> for NativeThrottlerPointError {
	fn from(error: StateStorageError) -> Self {
		Self::Storage(Box::new(error))
	}
}
impl NativeThrottlerPointError {
	fn from_operation(error: NativeOperationError<Self>) -> Self {
		match error {
			NativeOperationError::Admission(error) => Self::Admission(Box::new(error)),
			NativeOperationError::Operation(error) => error,
		}
	}
}
impl TransactionFailure for NativeThrottlerPointError {
	fn from_protocol_error(error: ProtocolError) -> Self {
		Self::Protocol(error)
	}
	fn from_database_error(error: DatabaseError) -> Self {
		Self::Database(Box::new(error))
	}
	fn from_rollback_failure(operation_error: Self, rollback_error: DatabaseError) -> Self {
		Self::RollbackFailed {
			operation_error: Box::new(operation_error),
			rollback_error: Box::new(rollback_error),
		}
	}
	fn involves_database_failure(&self) -> bool {
		match self {
			Self::Admission(error) => error.involves_database_failure(),
			Self::Storage(error) => error.database_error().is_some(),
			Self::Record { source, .. } => source.involves_database_failure(),
			Self::Database(_) | Self::RollbackFailed { .. } => true,
			Self::CircuitAbsent | Self::Scope(_) | Self::State(_) | Self::Protocol(_) => false,
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Admission(error) => error.commit_outcome_is_unknown(),
			Self::Storage(error) => error
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
			Self::Record { source, .. } => source.commit_outcome_is_unknown(),
			Self::Database(error) => error.commit_outcome_is_unknown(),
			// The rollback after a failed operation sends no `COMMIT`, so only the operation's
			// own failure can leave a commit outcome unknown.
			Self::RollbackFailed {
				operation_error, ..
			} => operation_error.commit_outcome_is_unknown(),
			Self::CircuitAbsent | Self::Scope(_) | Self::State(_) | Self::Protocol(_) => false,
		}
	}
}
