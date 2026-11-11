//! Runtime-bound native counter operations using installed policy and composed transactions.
//!
//! A handle retains admission and storage bindings without retaining lifecycle ownership.
//! Dropping or shutting down the owning runtime therefore fences retained handles.

use crate::runtime::admission::observation::PgOperationAdmission;
use std::{num::NonZeroU32, sync::Arc};

use crate::db::{
	Database, DatabaseError, TransactionFailure, WriteDatabase, run_transaction_read,
	run_transaction_write,
};
use crate::fleet::scalar_codec::ValueError;
use crate::installation::ProtocolError;
use crate::installation::contract::{ContractStateReadError, StateContractError};
use crate::records::postgres::v10::relocating_point::{
	PgRelocatingPointStorage, RelocatingPointBinding, RelocatingPointError,
};
use crate::records::postgres::v10::source_point::{PgSourcePointStorage, SourcePointError};
use crate::records::postgres::{PgPointStorage, StateStorageError};
use crate::records::{RecordIdentity, RecordNamespace};
use crate::runtime::admission::bindings::InstallationStorageRoute;
use crate::runtime::admission::{NativeAdmissionError, NativeOperationError};

use super::state::{CounterComponent, CounterOperationError, CounterRepresentationError};

/// Immutable counter code, explicit physical mapping, and the same runtime admission.
pub(crate) struct NativeCounterBindings {
	pub(crate) component: CounterComponent,
	pub(crate) storage: PgPointStorage,
	pub(crate) source: Option<NativeCounterSourceStorage>,
	pub(crate) admission: PgOperationAdmission,
	pub(crate) conflict_attempts: NonZeroU32,
	pub(crate) namespace: RecordNamespace,
}

/// Source and relocation operations derive from one exact catalog-validated relation pair.
pub(crate) struct NativeCounterSourceStorage {
	source: PgSourcePointStorage,
	relocation: PgRelocatingPointStorage,
}

impl NativeCounterSourceStorage {
	pub(crate) fn relocation(&self) -> &PgRelocatingPointStorage {
		&self.relocation
	}
	pub(crate) fn new(relocation: PgRelocatingPointStorage) -> Self {
		Self {
			source: relocation.source_point_storage(),
			relocation,
		}
	}
}

/// A configured logical identity, without a copy of private participation authority.
#[derive(Clone)]
pub(crate) struct PgNativeCounter {
	bindings: Arc<NativeCounterBindings>,
	identity: RecordIdentity,
}

impl PgNativeCounter {
	pub(crate) fn new(bindings: Arc<NativeCounterBindings>, identity: RecordIdentity) -> Self {
		Self { bindings, identity }
	}

	fn source(
		&self,
	) -> Result<(&NativeCounterSourceStorage, RelocatingPointBinding), NativeCounterError> {
		let source = self
			.bindings
			.source
			.as_ref()
			.ok_or(NativeCounterError::SourceMappingAbsent)?;
		let binding = self
			.bindings
			.admission
			.counter_source_binding(&self.identity)
			.map_err(NativeCounterError::Admission)?;
		if !source
			.relocation
			.native_storage()
			.shares_container(&self.bindings.storage)
		{
			return Err(NativeCounterError::StorageMappingMismatch);
		}
		self.bindings
			.admission
			.verify_counter_source_storage(&source.relocation, &binding)
			.map_err(NativeCounterError::Admission)?;
		Ok((source, binding))
	}

	pub(crate) async fn fetch_value(
		&self,
		database: impl Database,
	) -> Result<i64, NativeCounterError> {
		run_transaction_read(database, "fleet.native_counter.fetch_value", async |tx| {
			let handle = self.clone();
			self.bindings
				.admission
				.execute_read_with_contracts(tx, async move |contracts, tx| {
					let declaration = handle.bindings.component.declaration();
					let contract = contracts
						.bind(&declaration)
						.map_err(NativeOperationError::Admission)?;
					let result = async {
						let schema = contract.transitions().bind_schema(&contract)?;
						match contracts.storage_route() {
							InstallationStorageRoute::NativeFresh
							| InstallationStorageRoute::NativeImported => {
								schema.require_native_writer()?;
								let snapshot =
									handle.bindings.storage.read(tx, &handle.identity).await?;
								contract
									.transitions()
									.decode_value(&snapshot, &schema)
									.map_err(NativeCounterError::Read)
							}
							InstallationStorageRoute::SourceCoexistence => {
								schema.require_source_writer()?;
								let (source, binding) = handle.source()?;
								let snapshot = source.source.read(tx, binding.source_key()).await?;
								contract
									.transitions()
									.decode_source_value(&snapshot, &schema)
									.map_err(NativeCounterError::Read)
							}
							InstallationStorageRoute::SourceRelocation => {
								schema.require_native_writer()?;
								let (source, binding) = handle.source()?;
								let snapshot = source.relocation.read(tx, &binding).await?;
								contract
									.transitions()
									.decode_relocated_value(&snapshot, &schema)
									.map_err(NativeCounterError::Read)
							}
						}
					}
					.await;
					result.map_err(NativeOperationError::Operation)
				})
				.await
				.map_err(NativeCounterError::from_operation)
		})
		.await
	}

	pub(crate) async fn add(
		&self,
		database: impl WriteDatabase,
		delta: i64,
	) -> Result<i64, NativeCounterError> {
		run_transaction_write(database, "fleet.native_counter.add", async |tx| {
			let handle = self.clone();
			self.bindings
				.admission
				.execute_write_with_contracts(tx, async move |contracts, tx| {
					let declaration = handle.bindings.component.declaration();
					let contract = contracts
						.bind(&declaration)
						.map_err(NativeOperationError::Admission)?;
					let result = async {
						let schema = contract.transitions().bind_schema(&contract)?;
						match contracts.storage_route() {
							InstallationStorageRoute::NativeFresh
							| InstallationStorageRoute::NativeImported => {
								schema.require_native_writer()?;
								let lock = handle
									.bindings
									.storage
									.lock(tx, &handle.identity, handle.bindings.conflict_attempts)
									.await?;
								contract
									.transitions()
									.add(lock, delta, &schema)
									.await
									.map_err(mutation_error)
							}
							InstallationStorageRoute::SourceCoexistence => {
								schema.require_source_writer()?;
								let (source, binding) = handle.source()?;
								let lock = source
									.source
									.lock(
										tx,
										binding.source_key(),
										handle.bindings.conflict_attempts,
									)
									.await?;
								contract
									.transitions()
									.add_source(lock, delta, &schema)
									.await
									.map_err(mutation_error)
							}
							InstallationStorageRoute::SourceRelocation => {
								schema.require_native_writer()?;
								let (source, binding) = handle.source()?;
								let lock = source
									.relocation
									.lock(tx, &binding, handle.bindings.conflict_attempts)
									.await?;
								contract
									.transitions()
									.add_relocated(lock, delta, &schema)
									.await
									.map_err(mutation_error)
							}
						}
					}
					.await;
					Ok(result)
				})
				.await
				.map_err(NativeCounterError::from_operation)?
		})
		.await
	}

	pub(crate) async fn set_value(
		&self,
		database: impl WriteDatabase,
		value: i64,
	) -> Result<(), NativeCounterError> {
		run_transaction_write(database, "fleet.native_counter.set_value", async |tx| {
			let handle = self.clone();
			self.bindings
				.admission
				.execute_write_with_contracts(tx, async move |contracts, tx| {
					let declaration = handle.bindings.component.declaration();
					let contract = contracts
						.bind(&declaration)
						.map_err(NativeOperationError::Admission)?;
					let result = async {
						let schema = contract.transitions().bind_schema(&contract)?;
						match contracts.storage_route() {
							InstallationStorageRoute::NativeFresh
							| InstallationStorageRoute::NativeImported => {
								schema.require_native_writer()?;
								let lock = handle
									.bindings
									.storage
									.lock(tx, &handle.identity, handle.bindings.conflict_attempts)
									.await?;
								contract
									.transitions()
									.set_value(lock, value, &schema)
									.await
									.map_err(mutation_error)
							}
							InstallationStorageRoute::SourceCoexistence => {
								schema.require_source_writer()?;
								let (source, binding) = handle.source()?;
								let lock = source
									.source
									.lock(
										tx,
										binding.source_key(),
										handle.bindings.conflict_attempts,
									)
									.await?;
								contract
									.transitions()
									.set_source_value(lock, value, &schema)
									.await
									.map_err(mutation_error)
							}
							InstallationStorageRoute::SourceRelocation => {
								schema.require_native_writer()?;
								let (source, binding) = handle.source()?;
								let lock = source
									.relocation
									.lock(tx, &binding, handle.bindings.conflict_attempts)
									.await?;
								contract
									.transitions()
									.set_relocated_value(lock, value, &schema)
									.await
									.map_err(mutation_error)
							}
						}
					}
					.await;
					Ok(result)
				})
				.await
				.map_err(NativeCounterError::from_operation)?
		})
		.await
	}
}

/// Typed operation and transaction-control failures retain exact rollback failure causes.
#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeCounterError {
	#[error("counter has no source mapping for the admitted source route")]
	SourceMappingAbsent,
	#[error("counter source/native mapping does not match the runtime container or namespace")]
	StorageMappingMismatch,
	#[error("native counter representation selection failed")]
	Representation(#[from] CounterRepresentationError),
	#[error("native counter admission failed")]
	Admission(#[source] NativeAdmissionError),
	#[error("native counter schema binding failed")]
	Contract(#[from] StateContractError),
	#[error("native counter state read failed")]
	Read(#[source] ContractStateReadError<ValueError>),
	#[error("native counter mutation failed")]
	Mutation(#[source] CounterOperationError<CounterStorageError>),
	#[error("native counter storage work failed")]
	Storage(#[from] CounterStorageError),
	#[error("native counter database work failed")]
	Database(#[source] DatabaseError),
	#[error("native counter transaction protocol failed")]
	Protocol(#[source] ProtocolError),
	#[error("native counter work failed and rollback also failed")]
	RollbackFailed {
		#[source]
		operation_error: Box<Self>,
		rollback_error: DatabaseError,
	},
}

impl NativeCounterError {
	fn from_operation(error: NativeOperationError<Self>) -> Self {
		match error {
			NativeOperationError::Admission(error) => Self::Admission(error),
			NativeOperationError::Operation(error) => error,
		}
	}
}

impl TransactionFailure for NativeCounterError {
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
			Self::Database(_) | Self::RollbackFailed { .. } => true,
			Self::Admission(error) => error.involves_database_failure(),
			Self::Storage(error) => error.involves_database_failure(),
			Self::Mutation(error) => mutation_involves_database_failure(error),
			Self::SourceMappingAbsent
			| Self::StorageMappingMismatch
			| Self::Representation(_)
			| Self::Contract(_)
			| Self::Read(_)
			| Self::Protocol(_) => false,
		}
	}

	fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Database(error) => error.commit_outcome_is_unknown(),
			Self::RollbackFailed {
				operation_error, ..
			} => operation_error.commit_outcome_is_unknown(),
			Self::Admission(error) => error.commit_outcome_is_unknown(),
			Self::Mutation(error) => mutation_commit_outcome_is_unknown(error),
			Self::Storage(error) => error.commit_outcome_is_unknown(),
			Self::SourceMappingAbsent
			| Self::StorageMappingMismatch
			| Self::Representation(_)
			| Self::Contract(_)
			| Self::Read(_)
			| Self::Protocol(_) => false,
		}
	}
}

fn mutation_involves_database_failure(error: &CounterOperationError<CounterStorageError>) -> bool {
	match error {
		CounterOperationError::Storage(error) => error.involves_database_failure(),
		CounterOperationError::CleanupFailed {
			operation_error,
			cleanup_error,
		} => {
			mutation_involves_database_failure(operation_error)
				|| cleanup_error.involves_database_failure()
		}
		CounterOperationError::Representation(_) | CounterOperationError::ArithmeticOverflow => {
			false
		}
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CounterStorageError {
	#[error("native counter record storage failed")]
	Native(#[from] StateStorageError),
	#[error("source counter record storage failed")]
	Source(#[from] SourcePointError),
	#[error("counter source relocation storage failed")]
	Relocation(#[from] RelocatingPointError),
}

impl CounterStorageError {
	fn involves_database_failure(&self) -> bool {
		match self {
			Self::Native(error) => error.database_error().is_some(),
			Self::Source(error) => error.involves_database_failure(),
			Self::Relocation(error) => error.involves_database_failure(),
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Native(error) => error
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
			Self::Source(error) => error.commit_outcome_is_unknown(),
			Self::Relocation(error) => error.commit_outcome_is_unknown(),
		}
	}
}

impl From<StateStorageError> for NativeCounterError {
	fn from(error: StateStorageError) -> Self {
		Self::Storage(error.into())
	}
}
impl From<SourcePointError> for NativeCounterError {
	fn from(error: SourcePointError) -> Self {
		Self::Storage(error.into())
	}
}
impl From<RelocatingPointError> for NativeCounterError {
	fn from(error: RelocatingPointError) -> Self {
		Self::Storage(error.into())
	}
}

fn mutation_error<E: Into<CounterStorageError>>(
	error: CounterOperationError<E>,
) -> NativeCounterError {
	NativeCounterError::Mutation(map_mutation_error(error))
}
fn map_mutation_error<E: Into<CounterStorageError>>(
	error: CounterOperationError<E>,
) -> CounterOperationError<CounterStorageError> {
	match error {
		CounterOperationError::Representation(error) => {
			CounterOperationError::Representation(error)
		}
		CounterOperationError::ArithmeticOverflow => CounterOperationError::ArithmeticOverflow,
		CounterOperationError::Storage(error) => CounterOperationError::Storage(error.into()),
		CounterOperationError::CleanupFailed {
			operation_error,
			cleanup_error,
		} => CounterOperationError::CleanupFailed {
			operation_error: Box::new(map_mutation_error(*operation_error)),
			cleanup_error: cleanup_error.into(),
		},
	}
}
fn mutation_commit_outcome_is_unknown(error: &CounterOperationError<CounterStorageError>) -> bool {
	match error {
		CounterOperationError::Storage(error) => error.commit_outcome_is_unknown(),
		CounterOperationError::CleanupFailed {
			operation_error,
			cleanup_error,
		} => {
			mutation_commit_outcome_is_unknown(operation_error)
				|| cleanup_error.commit_outcome_is_unknown()
		}
		CounterOperationError::Representation(_) | CounterOperationError::ArithmeticOverflow => {
			false
		}
	}
}
