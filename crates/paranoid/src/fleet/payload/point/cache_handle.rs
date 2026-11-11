//! Borrowed actual Cache entry reads; computation and epoch freshness are separate operations.

use super::PayloadPointFamily;
use super::cache::{
	CacheEntryDecodeFields, CachePointReadFields, CacheValueRead, decode_cache_entry,
	observe_cache_entry,
};
use super::factory::{
	NativePayloadConstructionError, require_retained_payload, retained_point_code,
};
use super::read::{
	NativePayloadOperationFailureFields, NativePayloadReadCause, NativePayloadReadContextFields,
	NativePayloadReadError, NativePayloadReadErrorFields,
};
use crate::db::{Database, run_transaction_read};
use crate::fleet::cache::entry_state::CacheEntryIdentity;
use crate::fleet::cache::epoch::{CacheEpochReadBindingFields, PgNativeCacheEpoch};
use crate::fleet::payload::scope::cache::CacheEntryScopeDeclaration;
use crate::fleet::payload::scope::retained::RetainedCacheEntryScopeCode;
use crate::records::RecordIdentity;
use crate::records::postgres::PgPointStorage;
use crate::runtime::admission::payload::PayloadAdmissionFields;
use crate::runtime::admission::{NativeOperationError, PgNativeAdmission};
use crate::scope::managed::ManagedScopeRegistry;
use crate::scope::payload::PayloadSchemaDeclaration;
use std::sync::Arc;

pub(crate) struct NativeCacheEntryFields<'scope, 'code, 'registry> {
	pub(crate) declaration: CacheEntryScopeDeclaration<'scope, 'code>,
	pub(crate) registry: &'registry ManagedScopeRegistry,
	pub(crate) admission: Arc<PgNativeAdmission>,
}
pub(crate) struct PgNativeCacheEntry<'scope, 'code> {
	declaration: CacheEntryScopeDeclaration<'scope, 'code>,
	retained: Option<Arc<RetainedCacheEntryScopeCode>>,
	storage: PgPointStorage,
	admission: Arc<PgNativeAdmission>,
}
/// What one Cache entry read reports when it finds no fresh entry.
trait CacheReadPurpose: Copy {
	/// The report of a read that finds no fresh entry.
	type Miss;
	/// The label of the read's transaction.
	const LABEL: &'static str;
	/// The report of a read that finds no entry, when the read reports it without reading the
	/// epoch, or `None` when the read reads the epoch first.
	fn miss_without_epoch(self) -> Option<Self::Miss>;
	/// The report of a read that finds no fresh entry at `captured_epoch`. A computation start
	/// first runs `preflight_publication_fn`, which checks that the runtime can publish a value.
	fn miss_at_epoch<E>(
		self,
		captured_epoch: i64,
		preflight_publication_fn: impl FnOnce() -> Result<(), E>,
	) -> Result<Self::Miss, E>;
}
/// A read of a fresh value, which reports an absent or stale entry as `()`.
#[derive(Clone, Copy)]
struct FreshRead;
impl CacheReadPurpose for FreshRead {
	type Miss = ();
	const LABEL: &'static str = "fleet.native_cache.fetch_fresh";
	fn miss_without_epoch(self) -> Option<()> {
		Some(())
	}
	fn miss_at_epoch<E>(
		self,
		_captured_epoch: i64,
		_preflight_publication_fn: impl FnOnce() -> Result<(), E>,
	) -> Result<(), E> {
		Ok(())
	}
}
/// The start of a computation, which reports an absent or stale entry as the epoch that the
/// computation captures.
#[derive(Clone, Copy)]
struct ComputationStartRead;
impl CacheReadPurpose for ComputationStartRead {
	type Miss = i64;
	const LABEL: &'static str = "fleet.native_cache.observe_computation_start";
	fn miss_without_epoch(self) -> Option<i64> {
		None
	}
	fn miss_at_epoch<E>(
		self,
		captured_epoch: i64,
		preflight_publication_fn: impl FnOnce() -> Result<(), E>,
	) -> Result<i64, E> {
		preflight_publication_fn()?;
		Ok(captured_epoch)
	}
}
pub(in crate::fleet) enum CacheComputationRead<T> {
	Cached(CacheValueRead<T>),
	Compute { captured_epoch: i64 },
}
enum CacheReadResult<T, M> {
	Cached(CacheValueRead<T>),
	Miss(M),
}
impl<'scope, 'code> PgNativeCacheEntry<'scope, 'code> {
	pub(crate) fn new(
		fields: NativeCacheEntryFields<'scope, 'code, '_>,
	) -> Result<Self, NativePayloadConstructionError> {
		let retained = retained_point_code(fields.registry, &fields.declaration)?;
		let storage = fields
			.admission
			.app_storage(fields.declaration.identity())
			.map_err(NativePayloadConstructionError::admission)?;
		Ok(Self {
			declaration: fields.declaration,
			retained,
			storage,
			admission: fields.admission,
		})
	}
	pub(in crate::fleet) fn require_payload_declaration<T, C>(
		&self,
		declaration: &PayloadSchemaDeclaration<'_, '_, T, C>,
	) -> Result<(), crate::scope::ScopeError> {
		require_retained_payload(
			self.retained.as_ref().map(|code| code.payload_code()),
			declaration,
		)
	}
	pub(in crate::fleet) fn declaration(&self) -> &CacheEntryScopeDeclaration<'scope, 'code> {
		&self.declaration
	}
	pub(in crate::fleet) fn storage(&self) -> &PgPointStorage {
		&self.storage
	}
	pub(in crate::fleet) fn scope(&self) -> &crate::scope::ScopeIdentity {
		self.declaration.identity()
	}
	pub(in crate::fleet) fn record_identity(
		&self,
		cache_entry_identity: &CacheEntryIdentity,
	) -> RecordIdentity {
		self.declaration.record_identity(cache_entry_identity)
	}
	pub(in crate::fleet) fn shares_admission(&self, admission: &PgNativeAdmission) -> bool {
		std::ptr::eq(self.admission.as_ref(), admission)
	}
	fn failure<C>(
		&self,
		identity: &RecordIdentity,
		source: NativePayloadReadCause<C>,
	) -> NativePayloadReadError<C> {
		NativePayloadReadError::new(NativePayloadReadErrorFields {
			scope: self.declaration.identity(),
			identity,
			family: PayloadPointFamily::CacheEntry,
			source,
		})
	}
	pub(in crate::fleet) async fn fetch_fresh<T, C>(
		&self,
		database: impl Database,
		cache_entry_identity: &CacheEntryIdentity,
		declaration: &PayloadSchemaDeclaration<'_, '_, T, C>,
		epoch: &PgNativeCacheEpoch,
	) -> Result<Option<CacheValueRead<T>>, NativePayloadReadError<C>> {
		if cache_entry_identity.cache() != epoch.key() {
			return Err(self.failure(
				&self.declaration.record_identity(cache_entry_identity),
				NativePayloadReadCause::DifferentCache,
			));
		}
		Ok(
			match self
				.read_value(
					database,
					cache_entry_identity,
					declaration,
					epoch,
					FreshRead,
				)
				.await?
			{
				CacheReadResult::Cached(value) => Some(value),
				CacheReadResult::Miss(()) => None,
			},
		)
	}
	/// Reads the seed even on a miss and preflights publication only on that miss.
	pub(in crate::fleet) async fn observe_computation_start<T, C>(
		&self,
		database: impl Database,
		cache_entry_identity: &CacheEntryIdentity,
		declaration: &PayloadSchemaDeclaration<'_, '_, T, C>,
		epoch: &PgNativeCacheEpoch,
	) -> Result<CacheComputationRead<T>, NativePayloadReadError<C>> {
		if cache_entry_identity.cache() != epoch.key() {
			return Err(self.failure(
				&self.declaration.record_identity(cache_entry_identity),
				NativePayloadReadCause::DifferentCache,
			));
		}
		match self
			.read_value(
				database,
				cache_entry_identity,
				declaration,
				epoch,
				ComputationStartRead,
			)
			.await?
		{
			CacheReadResult::Cached(value) => Ok(CacheComputationRead::Cached(value)),
			CacheReadResult::Miss(captured_epoch) => {
				Ok(CacheComputationRead::Compute { captured_epoch })
			}
		}
	}
	async fn read_value<T, C, P: CacheReadPurpose>(
		&self,
		database: impl Database,
		cache_entry_identity: &CacheEntryIdentity,
		declaration: &PayloadSchemaDeclaration<'_, '_, T, C>,
		epoch: &PgNativeCacheEpoch,
		purpose: P,
	) -> Result<CacheReadResult<T, P::Miss>, NativePayloadReadError<C>> {
		let identity = self.declaration.record_identity(cache_entry_identity);
		run_transaction_read(database, P::LABEL, async |tx| {
			self.admission
				.execute_read_with_contracts(tx, async |contracts, tx| {
					let reader = contracts
						.bind_cache_entry_reader(&self.declaration)
						.map_err(NativeOperationError::Admission)?;
					require_retained_payload(
						self.retained.as_ref().map(|code| code.payload_code()),
						declaration,
					)
					.map_err(|error| {
						NativeOperationError::Operation(
							self.failure(&identity, NativePayloadReadCause::Scope(error)),
						)
					})?;
					let payload = contracts
						.bind_payload_reader(PayloadAdmissionFields {
							actual_scope: self.declaration.identity(),
							declaration,
						})
						.map_err(NativeOperationError::Admission)?;
					payload.require_declaration(declaration).map_err(|error| {
						NativeOperationError::Operation(
							self.failure(&identity, NativePayloadReadCause::Scope(error)),
						)
					})?;
					reader.require_payload_reader(&payload).map_err(|error| {
						NativeOperationError::Operation(self.failure(
							&identity,
							NativePayloadReadCause::Read(super::PayloadPointError::new(
								super::PayloadPointContext {
									family: PayloadPointFamily::CacheEntry,
									action: super::PayloadPointAction::ObserveValue,
								},
								super::PayloadPointCause::Envelope(error),
							)),
						))
					})?;
					let epoch_declaration = epoch.declaration();
					let epoch_component = contracts
						.bind(&epoch_declaration)
						.map_err(NativeOperationError::Admission)?;
					let epoch_reader =
						epoch
							.bind_read(CacheEpochReadBindingFields {
								contracts: &contracts,
								component: &epoch_component,
							})
							.map_err(|error| {
								NativeOperationError::Operation(self.failure(
									&identity,
									NativePayloadReadCause::Epoch(Box::new(error)),
								))
							})?;
					let snapshot = self.storage.read(tx, &identity).await.map_err(|error| {
						NativeOperationError::Operation(
							self.failure(&identity, NativePayloadReadCause::Storage(error)),
						)
					})?;
					let entry = observe_cache_entry(
						CachePointReadFields {
							identity: &identity,
							cache_entry_identity,
							snapshot: &snapshot,
						},
						&reader,
					)
					.map_err(|error| {
						NativeOperationError::Operation(
							self.failure(&identity, NativePayloadReadCause::EnvelopeRead(error)),
						)
					})?;
					if entry.is_none()
						&& let Some(miss) = purpose.miss_without_epoch()
					{
						return Ok(CacheReadResult::Miss(miss));
					}
					let current_epoch = epoch_reader.read(tx).await.map_err(|error| {
						NativeOperationError::Operation(
							self.failure(&identity, NativePayloadReadCause::Epoch(Box::new(error))),
						)
					})?;
					let fresh = entry.filter(|entry| entry.state.epoch() >= current_epoch);
					let Some(entry) = fresh else {
						return purpose
							.miss_at_epoch(current_epoch, || {
								let _writer = contracts
									.bind_cache_entry_writer(&self.declaration)
									.map_err(NativeOperationError::Admission)?;
								let producer = contracts
									.bind_payload_producer(PayloadAdmissionFields {
										actual_scope: self.declaration.identity(),
										declaration,
									})
									.map_err(NativeOperationError::Admission)?;
								producer.require_declaration(declaration).map_err(|error| {
									NativeOperationError::Operation(
										self.failure(
											&identity,
											NativePayloadReadCause::Scope(error),
										),
									)
								})
							})
							.map(CacheReadResult::Miss);
					};
					decode_cache_entry(CacheEntryDecodeFields { entry }, &payload)
						.map(CacheReadResult::Cached)
						.map_err(|error| {
							NativeOperationError::Operation(
								self.failure(&identity, NativePayloadReadCause::Read(error)),
							)
						})
				})
				.await
				.map_err(|error| {
					NativePayloadReadError::from_operation(NativePayloadOperationFailureFields {
						scope: self.declaration.identity(),
						identity: &identity,
						family: PayloadPointFamily::CacheEntry,
						error,
					})
				})
		})
		.await
		.map_err(|error: NativePayloadReadError<C>| {
			error.with_context_if_absent(NativePayloadReadContextFields {
				scope: self.declaration.identity(),
				identity: &identity,
				family: PayloadPointFamily::CacheEntry,
			})
		})
	}
}
