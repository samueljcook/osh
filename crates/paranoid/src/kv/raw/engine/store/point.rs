//! Core point adapters bind actual current raw semantics before selecting a physical route.
use super::{
	AtomicLiveMutationCurrent, AtomicMutation, ConditionalRawWriteResult, Database, Infallible,
	InstallationStorageRoute, NativeOperationError, NativeRawKvError, PgNativeRawKvStore,
	PointRecordLock, RawKvComponent, RawLiveOrInitMutationResult, RawValueObservation,
	RawWriteResult, RecordIdentity, RecordKey, RecordMetadataMutationLock, RecordMutationError,
	RecordNamespace, RelocatingPointRecordLock, RelocationRead, StateTimestampMicros, Ttl,
	WriteDatabase, mutation_error, run_transaction_write, state,
};

use crate::kv::raw::engine::slot::NativeRawSlotError;
use crate::kv::raw::model::slot::RawSlotResult;
use crate::kv::raw::model::slot_preparation::EncryptedRawSlotRequest;

impl PgNativeRawKvStore {
	/// Borrows the actual configured core namespace without supplying access authority.
	pub(crate) fn namespace(&self) -> &RecordNamespace {
		&self.bindings.namespace
	}
	/// Retains live opaque bytes, absolute metadata and time from one physical observation.
	pub(in crate::kv) async fn get_bytes_observation(
		&self,
		database: impl Database,
		key: &RecordKey,
	) -> Result<RawValueObservation, NativeRawKvError<Infallible>> {
		self.read(
			database,
			key,
			"kv.native_store.get_bytes_observation",
			RawKvComponent::get_bytes_observation,
			RawKvComponent::observe_relocated_bytes,
		)
		.await
	}
	pub(in crate::kv) async fn get_bytes_observation_with_lock(
		&self,
		tx: &mut crate::db::WriteTx<'_>,
		key: &RecordKey,
	) -> Result<RawValueObservation, NativeRawKvError<Infallible>> {
		let identity = RecordIdentity::new(self.bindings.namespace.clone(), key.clone());
		run_transaction_write(
			tx,
			"kv.native_store.get_bytes_observation_with_lock",
			async |tx| {
				self.bindings
					.admission
					.execute_write_with_contracts(tx, async |contracts, tx| {
						let declaration = self.bindings.component.declaration();
						let contract = contracts
							.bind(&declaration)
							.map_err(NativeOperationError::Admission)?;
						let result: Result<_, NativeRawKvError<Infallible>> = async {
							match contracts.storage_route() {
								InstallationStorageRoute::NativeFresh
								| InstallationStorageRoute::NativeImported => {
									let snapshot =
										self.bindings.storage.read_locked(tx, &identity).await?;
									Ok(contract.transitions().get_bytes_observation(&snapshot))
								}
								InstallationStorageRoute::SourceCoexistence => {
									let (source, binding) = self.source(key)?;
									let lock = source
										.source
										.lock(
											tx,
											binding.source_key(),
											self.bindings.conflict_attempts,
										)
										.await?;
									let observation = contract
										.transitions()
										.get_bytes_observation(lock.snapshot());
									PointRecordLock::keep(lock).await?;
									Ok(observation)
								}
								InstallationStorageRoute::SourceRelocation => {
									let (source, binding) = self.source(key)?;
									let lock = source
										.relocation
										.lock(tx, &binding, self.bindings.conflict_attempts)
										.await?;
									let observation = contract
										.transitions()
										.observe_relocated_bytes(lock.snapshot());
									RelocatingPointRecordLock::keep(lock).await?;
									Ok(observation)
								}
							}
						}
						.await;
						result.map_err(NativeOperationError::Operation)
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
			"kv.native_store.exists",
			RawKvComponent::exists,
		)
		.await
	}
	/// Replaces bytes without returning or copying the previous opaque body.
	pub(crate) async fn set_bytes(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		value: Vec<u8>,
		ttl: Ttl,
	) -> Result<RawWriteResult, NativeRawKvError<Infallible>> {
		self.change_blind(database, key, SetBytes { value, ttl })
			.await
	}
	/// Tests liveness without returning or copying an existing opaque body.
	pub(in crate::kv) async fn set_bytes_if_absent(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		value: Vec<u8>,
		ttl: Ttl,
	) -> Result<ConditionalRawWriteResult, NativeRawKvError<Infallible>> {
		self.change_blind(database, key, SetBytesIfAbsent { value, ttl })
			.await
	}
	pub(in crate::kv) async fn delete_live(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
	) -> Result<(), NativeRawKvError<Infallible>> {
		self.change_blind(database, key, DeleteLive).await
	}
	async fn change_blind<B: BlindChange>(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		change: B,
	) -> Result<B::Output, NativeRawKvError<Infallible>> {
		let identity = RecordIdentity::new(self.bindings.namespace.clone(), key.clone());
		run_transaction_write(database, B::OPERATION, async |tx| {
			self.bindings
				.admission
				.execute_write_with_contracts(tx, async |contracts, tx| {
					let declaration = self.bindings.component.declaration();
					let contract = contracts
						.bind(&declaration)
						.map_err(NativeOperationError::Admission)?;
					let result = async {
						match contracts.storage_route() {
							InstallationStorageRoute::NativeFresh
							| InstallationStorageRoute::NativeImported => {
								let lock = self
									.bindings
									.storage
									.lock_header(tx, &identity, self.bindings.conflict_attempts)
									.await?;
								change
									.apply(contract.transitions(), lock)
									.await
									.map_err(mutation_error)
							}
							InstallationStorageRoute::SourceCoexistence => {
								let (source, binding) = self.source(key)?;
								let lock = source
									.source
									.lock_header(
										tx,
										binding.source_key(),
										self.bindings.conflict_attempts,
									)
									.await?;
								change
									.apply(contract.transitions(), lock)
									.await
									.map_err(mutation_error)
							}
							InstallationStorageRoute::SourceRelocation => {
								let (source, binding) = self.source(key)?;
								let lock = source
									.relocation
									.lock_header(tx, &binding, self.bindings.conflict_attempts)
									.await?;
								change
									.apply(contract.transitions(), lock)
									.await
									.map_err(mutation_error)
							}
						}
					}
					.await;
					Ok(result)
				})
				.await
				.map_err(NativeRawKvError::from_operation)?
		})
		.await
	}
	/// Initializes only absence/expiry and calls one synchronous decision under the same lock.
	pub(in crate::kv) async fn mutate_live_or_init<C>(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		initialize_value_fn: impl FnOnce(StateTimestampMicros) -> Result<(Vec<u8>, Ttl), C>,
		decide_mutation_fn: impl for<'current> FnOnce(
			AtomicLiveMutationCurrent<'current>,
		) -> Result<AtomicMutation, C>,
	) -> Result<RawLiveOrInitMutationResult, NativeRawKvError<C>> {
		let identity = RecordIdentity::new(self.bindings.namespace.clone(), key.clone());
		run_transaction_write(
			database,
			"kv.native_store.mutate_live_or_init",
			async |tx| {
				self.bindings
					.admission
					.execute_write_with_contracts(tx, async |contracts, tx| {
						let declaration = self.bindings.component.declaration();
						let contract = contracts
							.bind(&declaration)
							.map_err(NativeOperationError::Admission)?;
						let result = async {
							match contracts.storage_route() {
								InstallationStorageRoute::NativeFresh
								| InstallationStorageRoute::NativeImported => {
									let lock = self
										.bindings
										.storage
										.lock(tx, &identity, self.bindings.conflict_attempts)
										.await?;
									contract
										.transitions()
										.mutate_live_or_init(
											lock,
											initialize_value_fn,
											decide_mutation_fn,
										)
										.await
										.map_err(mutation_error)
								}
								InstallationStorageRoute::SourceCoexistence => {
									let (source, binding) = self.source(key)?;
									let lock = source
										.source
										.lock(
											tx,
											binding.source_key(),
											self.bindings.conflict_attempts,
										)
										.await?;
									contract
										.transitions()
										.mutate_live_or_init(
											lock,
											initialize_value_fn,
											decide_mutation_fn,
										)
										.await
										.map_err(mutation_error)
								}
								InstallationStorageRoute::SourceRelocation => {
									let (source, binding) = self.source(key)?;
									let lock = source
										.relocation
										.lock(tx, &binding, self.bindings.conflict_attempts)
										.await?;
									contract
										.transitions()
										.mutate_live_or_init_relocated(
											lock,
											initialize_value_fn,
											decide_mutation_fn,
										)
										.await
										.map_err(mutation_error)
								}
							}
						}
						.await;
						Ok(result)
					})
					.await
					.map_err(NativeRawKvError::from_operation)?
			},
		)
		.await
	}
	pub(in crate::kv) async fn acquire_encrypted_slot(
		&self,
		database: impl WriteDatabase,
		request: EncryptedRawSlotRequest<'_>,
	) -> Result<RawSlotResult, NativeRawSlotError> {
		crate::kv::raw::engine::slot::acquire_encrypted_core(&self.bindings, database, request)
			.await
	}
}

/// A write that does not read the record's value, with its operation name and its result.
trait BlindChange {
	type Output;
	const OPERATION: &'static str;
	fn apply<L: RecordMetadataMutationLock>(
		self,
		component: &RawKvComponent,
		lock: L,
	) -> impl Future<Output = Result<Self::Output, RecordMutationError<Infallible, L::Error>>> + Send;
}
struct SetBytes {
	value: Vec<u8>,
	ttl: Ttl,
}
impl BlindChange for SetBytes {
	type Output = RawWriteResult;
	const OPERATION: &'static str = "kv.native_store.set_bytes";
	fn apply<L: RecordMetadataMutationLock>(
		self,
		component: &RawKvComponent,
		lock: L,
	) -> impl Future<Output = Result<RawWriteResult, RecordMutationError<Infallible, L::Error>>> + Send
	{
		component.set_bytes(lock, self.value, self.ttl)
	}
}
struct SetBytesIfAbsent {
	value: Vec<u8>,
	ttl: Ttl,
}
impl BlindChange for SetBytesIfAbsent {
	type Output = ConditionalRawWriteResult;
	const OPERATION: &'static str = "kv.native_store.set_bytes_if_absent";
	fn apply<L: RecordMetadataMutationLock>(
		self,
		component: &RawKvComponent,
		lock: L,
	) -> impl Future<
		Output = Result<ConditionalRawWriteResult, RecordMutationError<Infallible, L::Error>>,
	> + Send {
		component.set_bytes_if_absent(lock, self.value, self.ttl)
	}
}
struct DeleteLive;
impl BlindChange for DeleteLive {
	type Output = ();
	const OPERATION: &'static str = "kv.native_store.delete_live";
	fn apply<L: RecordMetadataMutationLock>(
		self,
		component: &RawKvComponent,
		lock: L,
	) -> impl Future<Output = Result<(), RecordMutationError<Infallible, L::Error>>> + Send {
		component.delete_live(lock)
	}
}
impl RawKvComponent {
	fn observe_relocated_bytes(&self, snapshot: &RelocationRead) -> RawValueObservation {
		state::observe_relocated_bytes(snapshot)
	}
	async fn mutate_live_or_init_relocated<L: RelocatingPointRecordLock, C>(
		&self,
		lock: L,
		initialize_value_fn: impl FnOnce(StateTimestampMicros) -> Result<(Vec<u8>, Ttl), C>,
		decide_mutation_fn: impl FnOnce(AtomicLiveMutationCurrent<'_>) -> Result<AtomicMutation, C>,
	) -> Result<RawLiveOrInitMutationResult, RecordMutationError<C, L::Error>> {
		state::mutate_live_or_init_relocated(lock, initialize_value_fn, decide_mutation_fn).await
	}
}
