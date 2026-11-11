//! Admitted raw KV source/native transport with primitive-owned opaque-byte semantics.
//!
//! This internal handle does not claim ownership of application payload schemas. Every
//! operation binds the installed raw KV semantic contract; bytes carry no inferred codec.
//! Handles retain immutable admission material rather than the scoped lifecycle owner.

mod initial_policy;
mod point;

use crate::records::metadata_mutation::{RecordMetadataMutationLock, RecordMetadataUpdateLock};
use std::{convert::Infallible, num::NonZeroU32, sync::Arc};

use crate::db::{
	Database, DatabaseError, TransactionFailure, WriteDatabase, run_transaction_read,
	run_transaction_write,
};
use crate::installation::ProtocolError;
use crate::installation::contract::{
	ComponentDeclaration, ComponentDeclarationFields, ComponentSettingsDeclaration,
	ComponentSettingsRequirement, SemanticContractDef,
};
use crate::records::postgres::v10::relocating_point::{
	PgRelocatingPointStorage, RelocatingPointBinding, RelocatingPointError,
};
use crate::records::postgres::v10::source_point::{PgSourcePointStorage, SourcePointError};
use crate::records::postgres::{PgPointStorage, StateStorageError};
use crate::records::{
	PointRecordLock, PointRecordRead, RecordIdentity, RecordKey, RecordNamespace,
	RelocatingPointRecordLock, RelocationRead,
};
use crate::runtime::admission::bindings::InstallationStorageRoute;
use crate::runtime::admission::{NativeAdmissionError, NativeOperationError, PgNativeAdmission};

use crate::kv::raw::core_component::CoreRawComponent;
use crate::kv::raw::state::{self, RecordMutationError};
use crate::kv::raw::state::{
	ConditionalRawWriteResult, RawLiveOrInitMutationResult, RawValueObservation, RawWriteResult,
};
use crate::kv::{
	AtomicLiveMutationCurrent, AtomicMutation, AtomicMutationCurrent, AtomicMutationResult, Key,
	KeyError, KeyLifetime, StateTimestampMicros, Ttl,
};

const RAW_KV_COMPONENT: &str = "paranoid.raw-kv";
const RAW_KV_SEMANTICS: &str = "paranoid.raw-kv.v1";

/// Actual opaque-byte transitions. The primitive has no application representation registry.
pub(crate) struct RawKvComponent;

impl RawKvComponent {
	pub(crate) fn new() -> Self {
		Self
	}

	pub(crate) fn declaration(&self) -> ComponentDeclaration<'_, Self> {
		ComponentDeclaration::new(ComponentDeclarationFields {
			identifier: RAW_KV_COMPONENT,
			fixed_schemas: Vec::new(),
			settings: ComponentSettingsDeclaration::Parameterless,
			semantics: vec![SemanticContractDef {
				identifier: RAW_KV_SEMANTICS,
				transitions: self,
				settings: ComponentSettingsRequirement::Parameterless,
			}],
		})
		.expect(
			"raw KV declares one actual opaque-byte semantic implementation and no payload schema",
		)
	}

	/// Retains the complete opaque source body and absolute metadata without interpretation.
	pub(crate) fn convert_source_record(
		&self,
		record: &crate::records::SourcePointRecord,
	) -> crate::records::EncodedRecord {
		state::convert_source_record(record)
	}

	pub(crate) fn get_bytes(&self, snapshot: &PointRecordRead) -> Option<Vec<u8>> {
		state::get_bytes(snapshot).map(<[u8]>::to_vec)
	}
	pub(in crate::kv) fn get_bytes_observation(
		&self,
		snapshot: &PointRecordRead,
	) -> RawValueObservation {
		state::observe_live_bytes(snapshot)
	}

	pub(crate) fn lifetime(
		&self,
		snapshot: &crate::records::RecordHeaderObservation,
	) -> Option<KeyLifetime> {
		state::lifetime(snapshot)
	}
	pub(crate) fn exists(&self, snapshot: &crate::records::RecordHeaderObservation) -> bool {
		snapshot.is_live()
	}
	pub(crate) async fn set_bytes<L: RecordMetadataMutationLock>(
		&self,
		lock: L,
		value: Vec<u8>,
		ttl: Ttl,
	) -> Result<RawWriteResult, RecordMutationError<Infallible, L::Error>> {
		state::set_bytes(lock, value, ttl).await
	}

	pub(crate) async fn mutate<L: PointRecordLock, C>(
		&self,
		lock: L,
		decide_mutation_fn: impl FnOnce(AtomicMutationCurrent<'_>) -> Result<AtomicMutation, C>,
	) -> Result<AtomicMutationResult, RecordMutationError<C, L::Error>> {
		state::mutate(lock, decide_mutation_fn).await
	}

	pub(in crate::kv) async fn set_bytes_if_absent<L: RecordMetadataMutationLock>(
		&self,
		lock: L,
		value: Vec<u8>,
		ttl: Ttl,
	) -> Result<ConditionalRawWriteResult, RecordMutationError<Infallible, L::Error>> {
		state::set_bytes_if_absent(lock, value, ttl).await
	}
	pub(in crate::kv) async fn delete_live<L: RecordMetadataMutationLock>(
		&self,
		lock: L,
	) -> Result<(), RecordMutationError<Infallible, L::Error>> {
		state::delete_live(lock).await
	}

	pub(in crate::kv) async fn mutate_live<L: PointRecordLock, C>(
		&self,
		lock: L,
		decide_mutation_fn: impl FnOnce(AtomicLiveMutationCurrent<'_>) -> Result<AtomicMutation, C>,
	) -> Result<AtomicMutationResult, RecordMutationError<C, L::Error>> {
		state::mutate_live(lock, decide_mutation_fn).await
	}

	pub(in crate::kv) async fn mutate_live_or_init<L: PointRecordLock, C>(
		&self,
		lock: L,
		initialize_value_fn: impl FnOnce(StateTimestampMicros) -> Result<(Vec<u8>, Ttl), C>,
		decide_mutation_fn: impl FnOnce(AtomicLiveMutationCurrent<'_>) -> Result<AtomicMutation, C>,
	) -> Result<RawLiveOrInitMutationResult, RecordMutationError<C, L::Error>> {
		state::mutate_live_or_init(lock, initialize_value_fn, decide_mutation_fn).await
	}

	fn get_relocated_bytes(&self, snapshot: &RelocationRead) -> Option<Vec<u8>> {
		state::get_relocated_bytes(snapshot).map(<[u8]>::to_vec)
	}
	async fn mutate_relocated<L: RelocatingPointRecordLock, C>(
		&self,
		lock: L,
		decide_mutation_fn: impl FnOnce(AtomicMutationCurrent<'_>) -> Result<AtomicMutation, C>,
	) -> Result<AtomicMutationResult, RecordMutationError<C, L::Error>> {
		state::mutate_relocated(lock, decide_mutation_fn).await
	}
	async fn mutate_live_relocated<L: RelocatingPointRecordLock, C>(
		&self,
		lock: L,
		decide_mutation_fn: impl FnOnce(AtomicLiveMutationCurrent<'_>) -> Result<AtomicMutation, C>,
	) -> Result<AtomicMutationResult, RecordMutationError<C, L::Error>> {
		state::mutate_live_relocated(lock, decide_mutation_fn).await
	}

	pub(in crate::kv) async fn update_metadata<L: RecordMetadataUpdateLock>(
		&self,
		lock: L,
		transition: MetadataTransition,
	) -> Result<(), RecordMutationError<Infallible, L::Error>> {
		match transition {
			MetadataTransition::Touch => state::touch(lock).await,
			MetadataTransition::SetTtl(ttl) => state::set_ttl(lock, ttl).await,
			MetadataTransition::Expire => state::expire(lock).await,
		}
	}
}

/// One actual declaration, physical mapping, namespace, and scoped runtime admission.
pub(crate) struct NativeRawKvBindings {
	pub(crate) component: CoreRawComponent,
	pub(crate) storage: PgPointStorage,
	pub(crate) source: Option<NativeRawKvSourceStorage>,
	pub(crate) admission: Arc<PgNativeAdmission>,
	pub(crate) conflict_attempts: NonZeroU32,
	pub(crate) namespace: RecordNamespace,
}

/// Both source modes derive from one exact catalog-validated source/native relation pair.
/// It supplies physical mappings, without admission, phase selection, or application codecs.
pub(crate) struct NativeRawKvSourceStorage {
	source: PgSourcePointStorage,
	relocation: PgRelocatingPointStorage,
}
pub(crate) struct NativeRawKvSourceStorageFields {
	pub(crate) relocation: PgRelocatingPointStorage,
}
/// The v10 location of the core namespace is absent or does not match the installation.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RawSourceRouteError {
	#[error("raw KV has no v10 location for the admitted v10 route")]
	SourceMappingAbsent,
	#[error("raw KV v10 location does not match the installation's recorded v10 location")]
	Admission(#[source] NativeAdmissionError),
	#[error("raw KV v10 location does not match the runtime's native container")]
	StorageMappingMismatch,
}
impl RawSourceRouteError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Admission(error) => error.database_error(),
			Self::SourceMappingAbsent | Self::StorageMappingMismatch => None,
		}
	}
	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::Admission(error) => error.involves_database_failure(),
			Self::SourceMappingAbsent | Self::StorageMappingMismatch => false,
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Admission(error) => error.commit_outcome_is_unknown(),
			Self::SourceMappingAbsent | Self::StorageMappingMismatch => false,
		}
	}
}

impl NativeRawKvBindings {
	/// The core namespace's v10 storage on a route that still uses it. Paranoid first checks
	/// that the storage matches the installation's recorded v10 location and the runtime's
	/// native container.
	pub(super) fn source_storage(&self) -> Result<&NativeRawKvSourceStorage, RawSourceRouteError> {
		let source = self
			.source
			.as_ref()
			.ok_or(RawSourceRouteError::SourceMappingAbsent)?;
		self.admission
			.verify_raw_source_storage(&source.relocation, &self.namespace)
			.map_err(RawSourceRouteError::Admission)?;
		if !source
			.relocation
			.native_storage()
			.shares_container(&self.storage)
		{
			return Err(RawSourceRouteError::StorageMappingMismatch);
		}
		Ok(source)
	}
}

impl NativeRawKvSourceStorage {
	pub(super) fn source(&self) -> &PgSourcePointStorage {
		&self.source
	}
	pub(crate) fn relocation(&self) -> &PgRelocatingPointStorage {
		&self.relocation
	}

	pub(crate) fn new(fields: NativeRawKvSourceStorageFields) -> Self {
		Self {
			source: fields.relocation.source_point_storage(),
			relocation: fields.relocation,
		}
	}
}

/// Opaque raw KV access in one explicitly configured native namespace.
#[derive(Clone)]
pub(crate) struct PgNativeRawKvStore {
	bindings: Arc<NativeRawKvBindings>,
}

impl PgNativeRawKvStore {
	/// Attempts one raw slot while preserving unselected neutral seed history.
	pub(in crate::kv) async fn acquire_slot(
		&self,
		database: impl WriteDatabase,
		request: crate::kv::raw::model::slot::RawSlotRequest,
	) -> Result<crate::kv::raw::model::slot::RawSlotResult, super::slot::NativeRawSlotError> {
		super::slot::acquire_core(&self.bindings, database, request).await
	}

	/// Deletes the selection in one transaction through an actual observed-exhaustion point.
	pub(in crate::kv) async fn delete_selection_atomically(
		&self,
		database: impl WriteDatabase,
		request: &crate::kv::raw::model::namespace::RecordSelectionDeleteRequest,
	) -> Result<
		crate::kv::raw::model::namespace::RecordSelectionDeleteResult,
		super::delete::NativeRawSelectionDeleteError,
	> {
		super::delete::delete_selection_core(&self.bindings, database, request).await
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
		super::delete::cleanup_core(self, pool, request).await
	}

	/// Deletes only a bounded acquired candidate set; underfill does not establish emptiness.
	pub(in crate::kv) async fn delete_batch(
		&self,
		database: impl WriteDatabase,
		request: &crate::kv::raw::model::delete::RecordDeleteBatchRequest,
	) -> Result<
		crate::kv::raw::model::delete::RecordDeleteBatchResult,
		super::delete::NativeRawDeleteError,
	> {
		super::delete::delete_core(&self.bindings, database, request).await
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
		super::aggregate::count_core(&self.bindings, database, selection).await
	}

	/// Replaces the complete parsed set atomically; empty input performs no SQL.
	pub(in crate::kv) async fn set_many(
		&self,
		database: impl WriteDatabase,
		request: crate::kv::raw::model::many::RawManySetRequest,
	) -> Result<crate::kv::raw::model::many::RawManyWriteResult, super::many::NativeRawManyError> {
		super::many::write_core(&self.bindings, database, request).await
	}

	/// Observes every unique requested key in caller order; empty input performs no SQL.
	pub(in crate::kv) async fn get_many(
		&self,
		database: impl Database,
		request: &crate::kv::raw::model::many::RawManyReadRequest,
	) -> Result<crate::kv::raw::model::many::RawManyReadResult, super::many::NativeRawManyError> {
		super::many::read_core(&self.bindings, database, request).await
	}

	/// Reads one independent admitted value page without mutation or coverage authority.
	pub(in crate::kv) async fn read_page(
		&self,
		database: impl Database,
		request: &crate::kv::raw::model::page::RecordPageRequest,
	) -> Result<crate::kv::raw::model::page::RawValuePage, super::page::NativeRawPageError> {
		super::page::read_core::<super::page::Values>(&self.bindings, database, request).await
	}

	/// Reads one admitted body-free page of valued keys and original physical headers.
	pub(in crate::kv) async fn read_keys(
		&self,
		database: impl Database,
		request: &crate::kv::raw::model::page::RecordPageRequest,
	) -> Result<crate::kv::raw::model::page::RecordKeyPage, super::page::NativeRawPageError> {
		super::page::read_core::<super::page::Keys>(&self.bindings, database, request).await
	}

	pub(crate) fn new(bindings: Arc<NativeRawKvBindings>) -> Self {
		Self { bindings }
	}

	fn source<C>(
		&self,
		key: &RecordKey,
	) -> Result<(&NativeRawKvSourceStorage, RelocatingPointBinding), NativeRawKvError<C>> {
		let source = self
			.bindings
			.source_storage()
			.map_err(NativeRawKvError::Route)?;
		let text =
			std::str::from_utf8(key.as_bytes()).map_err(NativeRawKvError::SourceKeyNotUtf8)?;
		let source_key = Key::from_persisted_text(text.to_owned())
			.map_err(NativeRawKvError::SourceKeyNotCanonical)?;
		let identity = RecordIdentity::new(self.bindings.namespace.clone(), key.clone());
		Ok((source, RelocatingPointBinding::new(source_key, identity)))
	}

	/// Returns live opaque bytes; absent and expired identities return None.
	pub(crate) async fn get_bytes(
		&self,
		database: impl Database,
		key: &RecordKey,
	) -> Result<Option<Vec<u8>>, NativeRawKvError<Infallible>> {
		self.read(
			database,
			key,
			"kv.native_store.get_bytes",
			RawKvComponent::get_bytes,
			RawKvComponent::get_relocated_bytes,
		)
		.await
	}

	/// Observes a live value's remaining lifetime at the database clock.
	pub(crate) async fn lifetime(
		&self,
		database: impl Database,
		key: &RecordKey,
	) -> Result<Option<KeyLifetime>, NativeRawKvError<Infallible>> {
		self.read_header(
			database,
			key,
			"kv.native_store.lifetime",
			RawKvComponent::lifetime,
		)
		.await
	}

	async fn read<T>(
		&self,
		database: impl Database,
		key: &RecordKey,
		operation: &'static str,
		read_fn: fn(&RawKvComponent, &PointRecordRead) -> T,
		read_relocated_fn: fn(&RawKvComponent, &RelocationRead) -> T,
	) -> Result<T, NativeRawKvError<Infallible>> {
		let identity = RecordIdentity::new(self.bindings.namespace.clone(), key.clone());
		run_transaction_read(database, operation, async |tx| {
			let bindings = Arc::clone(&self.bindings);
			self.bindings
				.admission
				.execute_read_with_contracts(tx, async move |contracts, tx| {
					let declaration = bindings.component.declaration();
					let contract = contracts
						.bind(&declaration)
						.map_err(NativeOperationError::Admission)?;
					let result = async {
						match contracts.storage_route() {
							InstallationStorageRoute::NativeFresh
							| InstallationStorageRoute::NativeImported => {
								let snapshot = bindings.storage.read(tx, &identity).await?;
								Ok(read_fn(contract.transitions(), &snapshot))
							}
							InstallationStorageRoute::SourceCoexistence => {
								let (source, binding) = self.source(key)?;
								let snapshot = source.source.read(tx, binding.source_key()).await?;
								Ok(read_fn(contract.transitions(), &snapshot))
							}
							InstallationStorageRoute::SourceRelocation => {
								let (source, binding) = self.source(key)?;
								let snapshot = source.relocation.read(tx, &binding).await?;
								Ok(read_relocated_fn(contract.transitions(), &snapshot))
							}
						}
					}
					.await;
					result.map_err(NativeOperationError::Operation)
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
		let identity = RecordIdentity::new(self.bindings.namespace.clone(), key.clone());
		run_transaction_read(database, operation, async |tx| {
			let bindings = Arc::clone(&self.bindings);
			self.bindings
				.admission
				.execute_read_with_contracts(tx, async move |contracts, tx| {
					let declaration = bindings.component.declaration();
					let contract = contracts
						.bind(&declaration)
						.map_err(NativeOperationError::Admission)?;
					let result = async {
						match contracts.storage_route() {
							InstallationStorageRoute::NativeFresh
							| InstallationStorageRoute::NativeImported => {
								let snapshot = bindings.storage.read_header(tx, &identity).await?;
								Ok(read_fn(contract.transitions(), &snapshot))
							}
							InstallationStorageRoute::SourceCoexistence => {
								let (source, binding) = self.source(key)?;
								let snapshot =
									source.source.read_header(tx, binding.source_key()).await?;
								Ok(read_fn(contract.transitions(), &snapshot))
							}
							InstallationStorageRoute::SourceRelocation => {
								let (source, binding) = self.source(key)?;
								let snapshot = source.relocation.read_header(tx, &binding).await?;
								Ok(read_fn(contract.transitions(), &snapshot))
							}
						}
					}
					.await;
					result.map_err(NativeOperationError::Operation)
				})
				.await
				.map_err(NativeRawKvError::from_operation)
		})
		.await
	}

	/// Calls a synchronous raw-byte decision once after admitted point locking.
	pub(crate) async fn mutate<C, F>(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		decide_mutation_fn: F,
	) -> Result<AtomicMutationResult, NativeRawKvError<C>>
	where
		F: for<'current> FnOnce(AtomicMutationCurrent<'current>) -> Result<AtomicMutation, C>,
	{
		let identity = RecordIdentity::new(self.bindings.namespace.clone(), key.clone());
		run_transaction_write(database, "kv.native_store.mutate", async |tx| {
			let bindings = Arc::clone(&self.bindings);
			self.bindings
				.admission
				.execute_write_with_contracts(tx, async move |contracts, tx| {
					let declaration = bindings.component.declaration();
					let contract = contracts
						.bind(&declaration)
						.map_err(NativeOperationError::Admission)?;
					let result = async {
						match contracts.storage_route() {
							InstallationStorageRoute::NativeFresh
							| InstallationStorageRoute::NativeImported => {
								let lock = bindings
									.storage
									.lock(tx, &identity, bindings.conflict_attempts)
									.await?;
								contract
									.transitions()
									.mutate(lock, decide_mutation_fn)
									.await
									.map_err(mutation_error)
							}
							InstallationStorageRoute::SourceCoexistence => {
								let (source, binding) = self.source(key)?;
								let lock = source
									.source
									.lock(tx, binding.source_key(), bindings.conflict_attempts)
									.await?;
								contract
									.transitions()
									.mutate(lock, decide_mutation_fn)
									.await
									.map_err(mutation_error)
							}
							InstallationStorageRoute::SourceRelocation => {
								let (source, binding) = self.source(key)?;
								let lock = source
									.relocation
									.lock(tx, &binding, bindings.conflict_attempts)
									.await?;
								contract
									.transitions()
									.mutate_relocated(lock, decide_mutation_fn)
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

	/// Requires a live value before calling the synchronous raw-byte decision once.
	pub(in crate::kv) async fn mutate_live<C, F>(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		decide_mutation_fn: F,
	) -> Result<AtomicMutationResult, NativeRawKvError<C>>
	where
		F: for<'current> FnOnce(AtomicLiveMutationCurrent<'current>) -> Result<AtomicMutation, C>,
	{
		let identity = RecordIdentity::new(self.bindings.namespace.clone(), key.clone());
		run_transaction_write(database, "kv.native_store.mutate_live", async |tx| {
			let bindings = Arc::clone(&self.bindings);
			self.bindings
				.admission
				.execute_write_with_contracts(tx, async move |contracts, tx| {
					let declaration = bindings.component.declaration();
					let contract = contracts
						.bind(&declaration)
						.map_err(NativeOperationError::Admission)?;
					let result = async {
						match contracts.storage_route() {
							InstallationStorageRoute::NativeFresh
							| InstallationStorageRoute::NativeImported => {
								let lock = bindings
									.storage
									.lock(tx, &identity, bindings.conflict_attempts)
									.await?;
								contract
									.transitions()
									.mutate_live(lock, decide_mutation_fn)
									.await
									.map_err(mutation_error)
							}
							InstallationStorageRoute::SourceCoexistence => {
								let (source, binding) = self.source(key)?;
								let lock = source
									.source
									.lock(tx, binding.source_key(), bindings.conflict_attempts)
									.await?;
								contract
									.transitions()
									.mutate_live(lock, decide_mutation_fn)
									.await
									.map_err(mutation_error)
							}
							InstallationStorageRoute::SourceRelocation => {
								let (source, binding) = self.source(key)?;
								let lock = source
									.relocation
									.lock(tx, &binding, bindings.conflict_attempts)
									.await?;
								contract
									.transitions()
									.mutate_live_relocated(lock, decide_mutation_fn)
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

	/// Changes a live value's modification time while preserving its opaque bytes.
	pub(crate) async fn touch(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
	) -> Result<(), NativeRawKvError<Infallible>> {
		self.update_metadata(database, key, MetadataTransition::Touch)
			.await
	}

	/// Changes a live value's expiry while preserving its opaque bytes.
	pub(crate) async fn set_ttl(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		ttl: Ttl,
	) -> Result<(), NativeRawKvError<Infallible>> {
		self.update_metadata(database, key, MetadataTransition::SetTtl(ttl))
			.await
	}

	/// Expires a live value while preserving its opaque bytes.
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
		let identity = RecordIdentity::new(self.bindings.namespace.clone(), key.clone());
		run_transaction_write(database, transition.operation(), async |tx| {
			let bindings = Arc::clone(&self.bindings);
			self.bindings
				.admission
				.execute_write_with_contracts(tx, async move |contracts, tx| {
					let declaration = bindings.component.declaration();
					let contract = contracts
						.bind(&declaration)
						.map_err(NativeOperationError::Admission)?;
					let result = async {
						match contracts.storage_route() {
							InstallationStorageRoute::NativeFresh
							| InstallationStorageRoute::NativeImported => {
								let lock = bindings
									.storage
									.lock_header(tx, &identity, bindings.conflict_attempts)
									.await?;
								contract
									.transitions()
									.update_metadata(lock, transition)
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
										bindings.conflict_attempts,
									)
									.await?;
								contract
									.transitions()
									.update_metadata(lock, transition)
									.await
									.map_err(mutation_error)
							}
							InstallationStorageRoute::SourceRelocation => {
								let (source, binding) = self.source(key)?;
								let lock = source
									.relocation
									.lock_header(tx, &binding, bindings.conflict_attempts)
									.await?;
								contract
									.transitions()
									.update_metadata(lock, transition)
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
}

#[derive(Clone, Copy)]
pub(in crate::kv) enum MetadataTransition {
	Touch,
	SetTtl(Ttl),
	Expire,
}

impl MetadataTransition {
	pub(crate) fn operation(self) -> &'static str {
		match self {
			Self::Touch => "kv.native_store.touch",
			Self::SetTtl(_) => "kv.native_store.set_ttl",
			Self::Expire => "kv.native_store.expire",
		}
	}
}

/// The callback's original error and actual storage/transaction failures remain separately typed.
#[derive(Debug)]
pub(crate) enum NativeRawKvError<C> {
	Route(RawSourceRouteError),
	SourceKeyNotUtf8(std::str::Utf8Error),
	SourceKeyNotCanonical(KeyError),
	Admission(NativeAdmissionError),
	Mutation(RecordMutationError<C, RawKvStorageError>),
	Storage(RawKvStorageError),
	Database(DatabaseError),
	Protocol(ProtocolError),
	RollbackFailed {
		operation_error: Box<Self>,
		rollback_error: DatabaseError,
	},
}

impl<C> NativeRawKvError<C> {
	pub(crate) fn from_operation(error: NativeOperationError<Self>) -> Self {
		match error {
			NativeOperationError::Admission(error) => Self::Admission(error),
			NativeOperationError::Operation(error) => error,
		}
	}
}

impl<C> From<StateStorageError> for NativeRawKvError<C> {
	fn from(error: StateStorageError) -> Self {
		Self::Storage(error.into())
	}
}

impl<C> std::fmt::Display for NativeRawKvError<C> {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str(match self {
			Self::Route(_) => "raw KV v10 route failed",
			Self::SourceKeyNotUtf8(_) => "raw KV key is not UTF-8 for the admitted source route",
			Self::SourceKeyNotCanonical(_) => "raw KV key does not match published source grammar",
			Self::Admission(_) => "native raw KV admission failed",
			Self::Mutation(_) => "native raw KV transition failed",
			Self::Storage(_) => "native raw KV storage work failed",
			Self::Database(_) => "native raw KV database work failed",
			Self::Protocol(_) => "native raw KV transaction protocol failed",
			Self::RollbackFailed { .. } => "native raw KV work failed and rollback also failed",
		})
	}
}

impl<C: std::error::Error + 'static> std::error::Error for NativeRawKvError<C> {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Route(error) => Some(error),
			Self::SourceKeyNotUtf8(error) => Some(error),
			Self::SourceKeyNotCanonical(error) => Some(error),
			Self::Admission(error) => Some(error),
			Self::Mutation(error) => Some(error),
			Self::Storage(error) => Some(error),
			Self::Database(error) => Some(error),
			Self::Protocol(error) => Some(error),
			Self::RollbackFailed {
				operation_error, ..
			} => Some(operation_error.as_ref()),
		}
	}
}

impl<C> TransactionFailure for NativeRawKvError<C> {
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
			Self::Mutation(error) => mutation_involves_database_failure(error),
			Self::Storage(error) => error.involves_database_failure(),
			Self::Route(error) => error.involves_database_failure(),
			Self::Database(_) | Self::RollbackFailed { .. } => true,
			Self::Protocol(_) | Self::SourceKeyNotUtf8(_) | Self::SourceKeyNotCanonical(_) => false,
		}
	}

	fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Database(error) => error.commit_outcome_is_unknown(),
			Self::RollbackFailed {
				operation_error,
				rollback_error,
			} => {
				operation_error.commit_outcome_is_unknown()
					|| rollback_error.commit_outcome_is_unknown()
			}
			Self::Admission(error) => error.commit_outcome_is_unknown(),
			Self::Mutation(error) => mutation_commit_outcome_is_unknown(error),
			Self::Storage(error) => error.commit_outcome_is_unknown(),
			Self::Route(error) => error.commit_outcome_is_unknown(),
			Self::Protocol(_) | Self::SourceKeyNotUtf8(_) | Self::SourceKeyNotCanonical(_) => false,
		}
	}
}

fn mutation_involves_database_failure<C>(
	error: &RecordMutationError<C, RawKvStorageError>,
) -> bool {
	match error {
		RecordMutationError::Storage(error) => error.involves_database_failure(),
		RecordMutationError::CleanupFailed {
			operation_error,
			cleanup_error,
		} => {
			mutation_involves_database_failure(operation_error)
				|| cleanup_error.involves_database_failure()
		}
		RecordMutationError::Callback(_)
		| RecordMutationError::KeyNotFound
		| RecordMutationError::ExpirationOverflow => false,
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RawKvStorageError {
	#[error("native raw KV storage failed")]
	Native(#[from] StateStorageError),
	#[error("source raw KV storage failed")]
	Source(#[from] SourcePointError),
	#[error("raw KV relocation storage failed")]
	Relocation(#[from] RelocatingPointError),
}
impl RawKvStorageError {
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
impl<C> From<SourcePointError> for NativeRawKvError<C> {
	fn from(error: SourcePointError) -> Self {
		Self::Storage(error.into())
	}
}
impl<C> From<RelocatingPointError> for NativeRawKvError<C> {
	fn from(error: RelocatingPointError) -> Self {
		Self::Storage(error.into())
	}
}
pub(crate) fn mutation_error<C, S: Into<RawKvStorageError>>(
	error: RecordMutationError<C, S>,
) -> NativeRawKvError<C> {
	NativeRawKvError::Mutation(map_mutation_error(error))
}
fn map_mutation_error<C, S: Into<RawKvStorageError>>(
	error: RecordMutationError<C, S>,
) -> RecordMutationError<C, RawKvStorageError> {
	match error {
		RecordMutationError::Callback(error) => RecordMutationError::Callback(error),
		RecordMutationError::KeyNotFound => RecordMutationError::KeyNotFound,
		RecordMutationError::ExpirationOverflow => RecordMutationError::ExpirationOverflow,
		RecordMutationError::Storage(error) => RecordMutationError::Storage(error.into()),
		RecordMutationError::CleanupFailed {
			operation_error,
			cleanup_error,
		} => RecordMutationError::CleanupFailed {
			operation_error: Box::new(map_mutation_error(*operation_error)),
			cleanup_error: cleanup_error.into(),
		},
	}
}
fn mutation_commit_outcome_is_unknown<C>(
	error: &RecordMutationError<C, RawKvStorageError>,
) -> bool {
	match error {
		RecordMutationError::Storage(error) => error.commit_outcome_is_unknown(),
		RecordMutationError::CleanupFailed {
			operation_error,
			cleanup_error,
		} => {
			mutation_commit_outcome_is_unknown(operation_error)
				|| cleanup_error.commit_outcome_is_unknown()
		}
		RecordMutationError::Callback(_)
		| RecordMutationError::KeyNotFound
		| RecordMutationError::ExpirationOverflow => false,
	}
}
