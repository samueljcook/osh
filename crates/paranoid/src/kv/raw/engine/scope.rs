//! Opaque-byte operations in explicit raw application scopes of the document engine.
//!
//! Every operation authenticates current scope ownership and registered access roles.
//! Published source values require their explicit adoption procedure; this handle supplies
//! no source interpretation or alternate path around typed document ownership.

use std::{convert::Infallible, num::NonZeroU32, sync::Arc};

use crate::db::{Database, WriteDatabase, run_transaction_read, run_transaction_write};
use crate::records::postgres::{PgPointRecordHeaderLock, PgPointRecordLock, PgPointStorage};
use crate::records::{PointRecordRead, RecordIdentity, RecordKey};
use crate::runtime::admission::{NativeAdmissionError, NativeOperationError, PgNativeAdmission};
use crate::scope::{RawRole, RawScopeDeclaration};

use super::store::{MetadataTransition, NativeRawKvError, RawKvComponent, mutation_error};
use crate::kv::raw::state::{
	ConditionalRawWriteResult, RawLiveOrInitMutationResult, RawValueObservation, RawWriteResult,
};
use crate::kv::{
	AtomicLiveMutationCurrent, AtomicMutation, AtomicMutationCurrent, AtomicMutationResult,
	KeyLifetime, StateTimestampMicros, Ttl,
};

pub(crate) struct PgNativeRawScope<'a> {
	declaration: &'a RawScopeDeclaration,
	storage: PgPointStorage,
	admission: Arc<PgNativeAdmission>,
	conflict_attempts: NonZeroU32,
}
impl<'a> PgNativeRawScope<'a> {
	/// Borrows the exact logical storage and namespace of this declaration.
	pub(crate) fn scope(&self) -> &crate::scope::ScopeIdentity {
		self.declaration.identity()
	}
	/// Uses the same scoped Writer admission and fixed selected-key encryption owner.
	pub(in crate::kv) async fn acquire_encrypted_slot(
		&self,
		database: impl WriteDatabase,
		request: crate::kv::raw::model::slot_preparation::EncryptedRawSlotRequest<'_>,
	) -> Result<crate::kv::raw::model::slot::RawSlotResult, super::slot::NativeRawSlotError> {
		super::slot::acquire_encrypted_scope(
			super::slot::NativeRawScopeSlotFields {
				declaration: self.declaration,
				storage: &self.storage,
				admission: &self.admission,
				conflict_attempts: self.conflict_attempts,
			},
			database,
			request,
		)
		.await
	}

	/// Attempts one native raw slot under this exact scope's current Writer policy.
	pub(in crate::kv) async fn acquire_slot(
		&self,
		database: impl WriteDatabase,
		request: crate::kv::raw::model::slot::RawSlotRequest,
	) -> Result<crate::kv::raw::model::slot::RawSlotResult, super::slot::NativeRawSlotError> {
		super::slot::acquire_scope(
			super::slot::NativeRawScopeSlotFields {
				declaration: self.declaration,
				storage: &self.storage,
				admission: &self.admission,
				conflict_attempts: self.conflict_attempts,
			},
			database,
			request,
		)
		.await
	}

	/// Deletes the selected raw scope records atomically without predicate exclusion.
	pub(in crate::kv) async fn delete_selection_atomically(
		&self,
		database: impl WriteDatabase,
		request: &crate::kv::raw::model::namespace::RecordSelectionDeleteRequest,
	) -> Result<
		crate::kv::raw::model::namespace::RecordSelectionDeleteResult,
		super::delete::NativeRawSelectionDeleteError,
	> {
		super::delete::delete_selection_scope(
			super::delete::NativeRawScopeDeleteFields {
				declaration: self.declaration,
				storage: &self.storage,
				admission: &self.admission,
			},
			database,
			request,
		)
		.await
	}
	/// Cleans expired windows in separate pool transactions and reports an honest underfilled stop.
	/// Earlier successful batches remain committed if a later batch, pacing, or caller cancellation
	/// stops the run.
	pub(in crate::kv) async fn cleanup_expired_with_delay(
		&self,
		pool: &crate::db::WritePool,
		request: &crate::kv::raw::model::cleanup::RawExpiredCleanupRunRequest,
	) -> Result<
		crate::kv::raw::model::cleanup::RawExpiredCleanupRunResult,
		crate::kv::raw::model::cleanup::RawExpiredCleanupRunError,
	> {
		super::delete::cleanup_scope(self, pool, request).await
	}

	/// Deletes only a bounded acquired candidate set under current Raw Writer admission.
	pub(in crate::kv) async fn delete_batch(
		&self,
		database: impl WriteDatabase,
		request: &crate::kv::raw::model::delete::RecordDeleteBatchRequest,
	) -> Result<
		crate::kv::raw::model::delete::RecordDeleteBatchResult,
		super::delete::NativeRawDeleteError,
	> {
		super::delete::delete_scope(
			super::delete::NativeRawScopeDeleteFields {
				declaration: self.declaration,
				storage: &self.storage,
				admission: &self.admission,
			},
			database,
			request,
		)
		.await
	}

	/// Counts live raw identities in one statement without enumerating bodies or granting
	/// authority.
	pub(in crate::kv) async fn count_live(
		&self,
		database: impl Database,
		selection: &crate::kv::raw::model::page::RecordKeySelection,
	) -> Result<
		crate::kv::raw::model::aggregate::RecordLiveCount,
		super::aggregate::NativeRawAggregateError,
	> {
		super::aggregate::count_scope(
			super::aggregate::NativeRawScopeAggregateFields {
				declaration: self.declaration,
				storage: &self.storage,
				admission: &self.admission,
			},
			database,
			selection,
		)
		.await
	}

	/// Replaces the complete parsed set atomically; empty input performs no SQL.
	pub(in crate::kv) async fn set_many(
		&self,
		database: impl WriteDatabase,
		request: crate::kv::raw::model::many::RawManySetRequest,
	) -> Result<crate::kv::raw::model::many::RawManyWriteResult, super::many::NativeRawManyError> {
		super::many::write_scope(
			super::many::NativeRawScopeManyFields {
				declaration: self.declaration,
				storage: &self.storage,
				admission: &self.admission,
			},
			database,
			request,
		)
		.await
	}

	/// Observes a complete native key set with current Raw Reader access; empty input uses no SQL.
	pub(in crate::kv) async fn get_many(
		&self,
		database: impl Database,
		request: &crate::kv::raw::model::many::RawManyReadRequest,
	) -> Result<crate::kv::raw::model::many::RawManyReadResult, super::many::NativeRawManyError> {
		super::many::read_scope(
			super::many::NativeRawScopeManyFields {
				declaration: self.declaration,
				storage: &self.storage,
				admission: &self.admission,
			},
			database,
			request,
		)
		.await
	}

	/// Reads one native application namespace with actual current Raw Reader admission.
	pub(in crate::kv) async fn read_page(
		&self,
		database: impl Database,
		request: &crate::kv::raw::model::page::RecordPageRequest,
	) -> Result<crate::kv::raw::model::page::RawValuePage, super::page::NativeRawPageError> {
		super::page::read_scope::<super::page::Values>(
			super::page::NativeRawScopePageFields {
				declaration: self.declaration,
				storage: &self.storage,
				admission: &self.admission,
			},
			database,
			request,
		)
		.await
	}

	/// Reads one body-free native application page under actual Raw Reader admission.
	pub(in crate::kv) async fn read_keys(
		&self,
		database: impl Database,
		request: &crate::kv::raw::model::page::RecordPageRequest,
	) -> Result<crate::kv::raw::model::page::RecordKeyPage, super::page::NativeRawPageError> {
		super::page::read_scope::<super::page::Keys>(
			super::page::NativeRawScopePageFields {
				declaration: self.declaration,
				storage: &self.storage,
				admission: &self.admission,
			},
			database,
			request,
		)
		.await
	}

	pub(crate) fn new(
		declaration: &'a RawScopeDeclaration,
		admission: Arc<PgNativeAdmission>,
		conflict_attempts: NonZeroU32,
	) -> Result<Self, NativeAdmissionError> {
		let storage = admission.app_storage(declaration.identity())?;
		Ok(Self {
			declaration,
			storage,
			admission,
			conflict_attempts,
		})
	}
	fn identity(&self, key: &RecordKey) -> RecordIdentity {
		RecordIdentity::new(self.declaration.identity().namespace().clone(), key.clone())
	}
	pub(crate) async fn get_bytes(
		&self,
		database: impl Database,
		key: &RecordKey,
	) -> Result<Option<Vec<u8>>, NativeRawKvError<Infallible>> {
		self.read(
			database,
			key,
			"kv.native_scope.get_bytes",
			RawKvComponent::get_bytes,
		)
		.await
	}
	pub(crate) async fn lifetime(
		&self,
		database: impl Database,
		key: &RecordKey,
	) -> Result<Option<KeyLifetime>, NativeRawKvError<Infallible>> {
		self.read_header(
			database,
			key,
			"kv.native_scope.lifetime",
			RawKvComponent::lifetime,
		)
		.await
	}
	/// Returns live bytes, envelope metadata, and the clock from one admitted point observation.
	pub(in crate::kv) async fn get_bytes_observation(
		&self,
		database: impl Database,
		key: &RecordKey,
	) -> Result<RawValueObservation, NativeRawKvError<Infallible>> {
		self.read(
			database,
			key,
			"kv.native_scope.get_bytes_observation",
			RawKvComponent::get_bytes_observation,
		)
		.await
	}
	pub(in crate::kv) async fn get_bytes_observation_with_lock(
		&self,
		tx: &mut crate::db::WriteTx<'_>,
		key: &RecordKey,
	) -> Result<RawValueObservation, NativeRawKvError<Infallible>> {
		let identity = self.identity(key);
		run_transaction_write(
			tx,
			"kv.native_scope.get_bytes_observation_with_lock",
			async |tx| {
				self.admission
					.execute_write_with_contracts(tx, async |contracts, tx| {
						let declaration = self.declaration.declaration();
						let bound = contracts
							.bind_raw_scope(self.declaration, &declaration, RawRole::Reader)
							.map_err(NativeOperationError::Admission)?;
						let snapshot = self
							.storage
							.read_locked(tx, &identity)
							.await
							.map_err(NativeRawKvError::from)
							.map_err(NativeOperationError::Operation)?;
						Ok(bound.transitions().get_bytes_observation(&snapshot))
					})
					.await
					.map_err(NativeRawKvError::from_operation)
			},
		)
		.await
	}

	pub(crate) async fn exists(
		&self,
		database: impl Database,
		key: &RecordKey,
	) -> Result<bool, NativeRawKvError<Infallible>> {
		self.read_header(
			database,
			key,
			"kv.native_scope.exists",
			RawKvComponent::exists,
		)
		.await
	}
	async fn read<T>(
		&self,
		database: impl Database,
		key: &RecordKey,
		operation: &'static str,
		read_fn: fn(&RawKvComponent, &PointRecordRead) -> T,
	) -> Result<T, NativeRawKvError<Infallible>> {
		let identity = self.identity(key);
		run_transaction_read(database, operation, async |tx| {
			self.admission
				.execute_read_with_contracts(tx, async |contracts, tx| {
					let declaration = self.declaration.declaration();
					let bound = contracts
						.bind_raw_scope(self.declaration, &declaration, RawRole::Reader)
						.map_err(NativeOperationError::Admission)?;
					let snapshot = self
						.storage
						.read(tx, &identity)
						.await
						.map_err(NativeRawKvError::from)
						.map_err(NativeOperationError::Operation)?;
					Ok(read_fn(bound.transitions(), &snapshot))
				})
				.await
				.map_err(NativeRawKvError::from_operation)
		})
		.await
	}

	async fn read_header<T>(
		&self,
		database: impl Database,
		key: &RecordKey,
		operation: &'static str,
		read_fn: fn(&RawKvComponent, &crate::records::RecordHeaderObservation) -> T,
	) -> Result<T, NativeRawKvError<Infallible>> {
		let identity = self.identity(key);
		run_transaction_read(database, operation, async |tx| {
			self.admission
				.execute_read_with_contracts(tx, async |contracts, tx| {
					let declaration = self.declaration.declaration();
					let bound = contracts
						.bind_raw_scope(self.declaration, &declaration, RawRole::Reader)
						.map_err(NativeOperationError::Admission)?;
					let snapshot = self
						.storage
						.read_header(tx, &identity)
						.await
						.map_err(NativeRawKvError::from)
						.map_err(NativeOperationError::Operation)?;
					Ok(read_fn(bound.transitions(), &snapshot))
				})
				.await
				.map_err(NativeRawKvError::from_operation)
		})
		.await
	}

	/// Blind byte replacement requires writer access and exposes no previous bytes.
	pub(crate) async fn set_bytes(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		value: Vec<u8>,
		ttl: Ttl,
	) -> Result<RawWriteResult, NativeRawKvError<Infallible>> {
		self.write_header(
			database,
			key,
			"kv.native_scope.set_bytes",
			async |component, lock| {
				component
					.set_bytes(lock, value, ttl)
					.await
					.map_err(mutation_error)
			},
		)
		.await
	}
	/// A conditional writer observes liveness without receiving the existing opaque value.
	pub(in crate::kv) async fn set_bytes_if_absent(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		value: Vec<u8>,
		ttl: Ttl,
	) -> Result<ConditionalRawWriteResult, NativeRawKvError<Infallible>> {
		self.write_header(
			database,
			key,
			"kv.native_scope.set_bytes_if_absent",
			async |component, lock| {
				component
					.set_bytes_if_absent(lock, value, ttl)
					.await
					.map_err(mutation_error)
			},
		)
		.await
	}
	/// Deletes a live value with writer access; a missing or expired value is an identified error.
	pub(in crate::kv) async fn delete_live(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
	) -> Result<(), NativeRawKvError<Infallible>> {
		self.write_header(
			database,
			key,
			"kv.native_scope.delete_live",
			async |component, lock| component.delete_live(lock).await.map_err(mutation_error),
		)
		.await
	}
	pub(crate) async fn mutate<C>(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		decide_mutation_fn: impl for<'current> FnOnce(
			AtomicMutationCurrent<'current>,
		) -> Result<AtomicMutation, C>,
	) -> Result<AtomicMutationResult, NativeRawKvError<C>> {
		self.write(
			database,
			key,
			"kv.native_scope.mutate",
			async |component, lock| {
				component
					.mutate(lock, decide_mutation_fn)
					.await
					.map_err(mutation_error)
			},
		)
		.await
	}
	/// Reads a prerequisite and mutates one record under one current reader/writer admission.
	/// The prerequisite is observed without locking; a concurrent update can commit before mutation.
	pub(crate) async fn mutate_after_read<T, C>(
		&self,
		database: impl WriteDatabase,
		read_key: &RecordKey,
		mutation_key: &RecordKey,
		parse_prerequisite_fn: impl FnOnce(Option<&[u8]>) -> Result<T, C>,
		decide_mutation_fn: impl FnOnce(AtomicMutationCurrent<'_>, T) -> Result<AtomicMutation, C>,
	) -> Result<AtomicMutationResult, NativeRawKvError<C>> {
		let read_identity = self.identity(read_key);
		let mutation_identity = self.identity(mutation_key);
		run_transaction_write(database, "kv.native_scope.mutate_after_read", async |tx| {
			self.admission
				.execute_write_with_contracts(tx, async |contracts, tx| {
					let declaration = self.declaration.declaration();
					let bound = contracts
						.bind_raw_scope(self.declaration, &declaration, RawRole::Writer)
						.map_err(NativeOperationError::Admission)?;
					contracts
						.bind_raw_scope(self.declaration, &declaration, RawRole::Reader)
						.map_err(NativeOperationError::Admission)?;
					let result = async {
						let snapshot = self.storage.read(tx, &read_identity).await?;
						let prerequisite =
							parse_prerequisite_fn(crate::kv::raw::state::get_bytes(&snapshot))
								.map_err(|error| {
									NativeRawKvError::Mutation(
										crate::kv::raw::state::RecordMutationError::Callback(error),
									)
								})?;
						let lock = self
							.storage
							.lock(tx, &mutation_identity, self.conflict_attempts)
							.await?;
						bound
							.transitions()
							.mutate(lock, |current| decide_mutation_fn(current, prerequisite))
							.await
							.map_err(mutation_error)
					}
					.await;
					Ok(result)
				})
				.await
				.map_err(NativeRawKvError::from_operation)?
		})
		.await
	}

	pub(in crate::kv) async fn mutate_live<C>(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		decide_mutation_fn: impl for<'current> FnOnce(
			AtomicLiveMutationCurrent<'current>,
		) -> Result<AtomicMutation, C>,
	) -> Result<AtomicMutationResult, NativeRawKvError<C>> {
		self.write(
			database,
			key,
			"kv.native_scope.mutate_live",
			async |component, lock| {
				component
					.mutate_live(lock, decide_mutation_fn)
					.await
					.map_err(mutation_error)
			},
		)
		.await
	}
	/// Requires Reader and Writer access before initialization or a caller-owned decision.
	pub(in crate::kv) async fn mutate_live_or_init<C>(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		initialize_value_fn: impl FnOnce(StateTimestampMicros) -> Result<(Vec<u8>, Ttl), C>,
		decide_mutation_fn: impl for<'current> FnOnce(
			AtomicLiveMutationCurrent<'current>,
		) -> Result<AtomicMutation, C>,
	) -> Result<RawLiveOrInitMutationResult, NativeRawKvError<C>> {
		self.write(
			database,
			key,
			"kv.native_scope.mutate_live_or_init",
			async |component, lock| {
				component
					.mutate_live_or_init(lock, initialize_value_fn, decide_mutation_fn)
					.await
					.map_err(mutation_error)
			},
		)
		.await
	}

	pub(crate) async fn touch(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
	) -> Result<(), NativeRawKvError<Infallible>> {
		self.update_metadata(database, key, MetadataTransition::Touch)
			.await
	}
	pub(crate) async fn set_ttl(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		ttl: Ttl,
	) -> Result<(), NativeRawKvError<Infallible>> {
		self.update_metadata(database, key, MetadataTransition::SetTtl(ttl))
			.await
	}
	pub(crate) async fn expire(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
	) -> Result<(), NativeRawKvError<Infallible>> {
		self.update_metadata(database, key, MetadataTransition::Expire)
			.await
	}
	async fn update_metadata(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		transition: MetadataTransition,
	) -> Result<(), NativeRawKvError<Infallible>> {
		self.write_header(
			database,
			key,
			transition.operation(),
			async |component, lock| {
				component
					.update_metadata(lock, transition)
					.await
					.map_err(mutation_error)
			},
		)
		.await
	}
	async fn write_header<T, C>(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		operation: &'static str,
		operation_fn: impl for<'component, 'lock, 'tx> AsyncFnOnce(
			&'component RawKvComponent,
			PgPointRecordHeaderLock<'lock, 'tx>,
		) -> Result<T, NativeRawKvError<C>>,
	) -> Result<T, NativeRawKvError<C>> {
		let identity = self.identity(key);
		run_transaction_write(database, operation, async |tx| {
			self.admission
				.execute_write_with_contracts(tx, async |contracts, tx| {
					let declaration = self.declaration.declaration();
					let bound = contracts
						.bind_raw_scope(self.declaration, &declaration, RawRole::Writer)
						.map_err(NativeOperationError::Admission)?;
					let result = async {
						let lock = self
							.storage
							.lock_header(tx, &identity, self.conflict_attempts)
							.await?;
						operation_fn(bound.transitions(), lock).await
					}
					.await;
					Ok(result)
				})
				.await
				.map_err(NativeRawKvError::from_operation)?
		})
		.await
	}
	async fn write<T, C>(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		operation: &'static str,
		operation_fn: impl for<'component, 'lock, 'tx> AsyncFnOnce(
			&'component RawKvComponent,
			PgPointRecordLock<'lock, 'tx>,
		) -> Result<T, NativeRawKvError<C>>,
	) -> Result<T, NativeRawKvError<C>> {
		let identity = self.identity(key);
		run_transaction_write(database, operation, async |tx| {
			self.admission
				.execute_write_with_contracts(tx, async |contracts, tx| {
					let declaration = self.declaration.declaration();
					let bound = contracts
						.bind_raw_scope(self.declaration, &declaration, RawRole::Writer)
						.map_err(NativeOperationError::Admission)?;
					contracts
						.bind_raw_scope(self.declaration, &declaration, RawRole::Reader)
						.map_err(NativeOperationError::Admission)?;
					let result = async {
						let lock = self
							.storage
							.lock(tx, &identity, self.conflict_attempts)
							.await?;
						operation_fn(bound.transitions(), lock).await
					}
					.await;
					Ok(result)
				})
				.await
				.map_err(NativeRawKvError::from_operation)?
		})
		.await
	}
}
