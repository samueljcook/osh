//! Public coalescing cache operations of the native runtime.

use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::db::{Database, TransactionFailure, WriteDatabase, WritePool};
use crate::fleet::cache::engine::compute::NativeCacheComputeFields;
use crate::fleet::cache::engine::compute::error::NativeCacheComputeError;
use crate::fleet::cache::engine::compute::result::{
	NativeCacheFetchResult, NativeCachePublicationAccounting, NativeCachePublicationNotRequested,
};
use crate::fleet::cache::engine::options::{
	NativeCacheOptions, NativeCacheOptionsError, NativeCacheTiming,
};
use crate::fleet::cache::engine::{
	NativeCacheConstructionError, NativeCacheFreshReadFields, PgNativeCache,
};
use crate::fleet::cache::entry_state::{CacheEntryIdentity, CacheEntryIdentityFields};
use crate::fleet::cache::epoch::NativeCacheEpochError;
use crate::fleet::lease::handle::{PgLeaseRelease, PgLeaseRunOptions, PgLeaseTaskCtx};
use crate::fleet::lease::owned_driver::NativeLeaseDriverError;
use crate::fleet::lease::owned_driver::cache_publication::NativeCachePreparedPublication;
use crate::fleet::lease::owned_driver::cache_publication::error::NativeCachePublicationError;
use crate::fleet::lease::owned_driver::close::NativeLeaseCloseWait;
use crate::fleet::lease::owned_driver::loss::NativeLeaseCallerCtx;
use crate::fleet::payload::point::read::NativePayloadReadError;
use crate::fleet::{CoalescingCacheKey, HolderPrefix};
use crate::kv::{KeyError, Ttl};
use crate::scope::CacheScope;
use crate::scope::payload::PayloadSchemaDeclaration;

/// The inputs of a coalescing cache handle.
pub struct PgCoalescingCacheFields<'a, T, C> {
	/// The cache's key, which every entry of the cache shares.
	pub key: CoalescingCacheKey,
	/// The declared cache scope that holds the cache's values.
	pub scope: &'a CacheScope<T, C>,
	/// The cache's validated options.
	pub options: PgCoalescingCacheOptions,
}

/// The longest time that one computation of a value can run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PgCoalescingCacheComputeTimeout {
	/// A computation runs until it returns.
	NoLimit,
	/// A computation that runs longer than this positive duration fails.
	After(Duration),
}

/// The inputs of a cache's options.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PgCoalescingCacheOptionsFields {
	/// How long a stored value stays fresh.
	pub value_ttl: Ttl,
	/// The positive time that a fetch waits for another holder's computation, which holds the
	/// entry's lease, before the fetch fails.
	pub lease_wait_timeout: Duration,
	/// The longest time that one computation can run.
	pub compute_timeout: PgCoalescingCacheComputeTimeout,
	/// The lease options of a computation. A fetch that waits for another holder's computation
	/// retries with the acquire retry delays of these options.
	pub compute_lease_options: PgLeaseRunOptions,
}

/// Validated options of a cache.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PgCoalescingCacheOptions {
	timing: NativeCacheTiming,
	compute_lease_options: PgLeaseRunOptions,
}

impl PgCoalescingCacheOptions {
	/// Validates the options without database work.
	pub fn new(
		fields: PgCoalescingCacheOptionsFields,
	) -> Result<Self, PgCoalescingCacheError<std::convert::Infallible>> {
		let compute_timeout = match fields.compute_timeout {
			PgCoalescingCacheComputeTimeout::NoLimit => None,
			PgCoalescingCacheComputeTimeout::After(timeout) => Some(timeout),
		};
		let timing =
			NativeCacheTiming::parse(fields.value_ttl, fields.lease_wait_timeout, compute_timeout)
				.map_err(|error| {
					PgCoalescingCacheError::new(PgCoalescingCacheErrorCause::Options(error))
				})?;
		Ok(Self {
			timing,
			compute_lease_options: fields.compute_lease_options,
		})
	}

	pub(crate) fn native(&self, holder_prefix: &HolderPrefix) -> NativeCacheOptions {
		NativeCacheOptions::new(
			self.timing,
			self.compute_lease_options.waiting,
			self.compute_lease_options.owner_options(holder_prefix),
		)
	}
}

/// One coalescing cache of the runtime's installation, with values of type `T`.
///
/// Each entry is named by the cache's key and a tuple of key parts. A fetch returns a fresh
/// stored value, or holds the entry's lease while the app computes the value and then stores
/// it so that concurrent fetches of one entry wait for one computation. Invalidating the cache
/// makes every stored value stale.
///
/// A computation can run more than once for one entry, for example when its lease ends before
/// the computation returns and another fetch computes the entry. Make a computation's effects
/// safe to repeat.
pub struct PgCoalescingCache<'a, T, C> {
	native: PgNativeCache<'a, 'a>,
	payload: PayloadSchemaDeclaration<'a, 'a, T, C>,
	pool: WritePool,
	close_wait: NativeLeaseCloseWait,
}

impl<'a, T, C> PgCoalescingCache<'a, T, C> {
	pub(crate) fn new(
		native: PgNativeCache<'a, 'a>,
		payload: PayloadSchemaDeclaration<'a, 'a, T, C>,
		pool: WritePool,
	) -> Self {
		let close_wait = native.close_wait();
		Self {
			native,
			payload,
			pool,
			close_wait,
		}
	}

	/// The cache's key.
	pub fn key(&self) -> &CoalescingCacheKey {
		self.native.key()
	}

	/// Reads one entry's value when it is stored and fresh. The read grants nothing.
	pub async fn fetch(
		&self,
		database: impl Database,
		fields: PgCoalescingCacheEntryFields,
	) -> Result<Option<T>, PgCoalescingCacheError<C>> {
		let cache_entry_identity = self.entry_identity(fields.key_parts)?;
		self.native
			.fetch_fresh(
				database,
				NativeCacheFreshReadFields {
					cache_entry_identity: &cache_entry_identity,
					payload: &self.payload,
				},
			)
			.await
			.map(|read| read.map(|read| read.value))
			.map_err(|error| PgCoalescingCacheError::new(PgCoalescingCacheErrorCause::Read(error)))
	}

	/// Returns one entry's fresh stored value, or computes the value with `compute_fn` while
	/// holding the entry's lease and then stores it.
	///
	/// An error means that `compute_fn` did not return: either it did not run, or it ran longer
	/// than the options' `compute_timeout`, and the fetch dropped it before it returned. When
	/// `compute_fn` returned, the result holds its own output, whether the value was stored,
	/// and whether the lease release was confirmed.
	pub fn fetch_or_compute<E, ComputeFn>(
		&self,
		fields: PgCoalescingCacheEntryFields,
		compute_fn: ComputeFn,
	) -> impl Future<Output = Result<PgCoalescingCacheFetchResult<T, E, C>, PgCoalescingCacheError<C>>>
	where
		ComputeFn: AsyncFnOnce(PgLeaseTaskCtx) -> Result<T, E>,
	{
		// The boxed task keeps its captured state out of the futures that run it.
		self.fetch_or_compute_boxed(fields, Box::new(compute_fn))
	}

	async fn fetch_or_compute_boxed<E, ComputeFn>(
		&self,
		fields: PgCoalescingCacheEntryFields,
		compute_fn: Box<ComputeFn>,
	) -> Result<PgCoalescingCacheFetchResult<T, E, C>, PgCoalescingCacheError<C>>
	where
		ComputeFn: AsyncFnOnce(PgLeaseTaskCtx) -> Result<T, E>,
	{
		let cache_entry_identity = self.entry_identity(fields.key_parts)?;
		let result = self
			.native
			.fetch_or_compute(NativeCacheComputeFields {
				pool: &self.pool,
				cache_entry_identity: &cache_entry_identity,
				payload: &self.payload,
				compute_fn: async move |ctx: NativeLeaseCallerCtx| {
					Box::pin(compute_fn(PgLeaseTaskCtx::from(ctx))).await
				},
				close_wait: &self.close_wait,
			})
			.await
			.map_err(|error| {
				PgCoalescingCacheError::new(PgCoalescingCacheErrorCause::Compute(error))
			})?;
		Ok(match result {
			NativeCacheFetchResult::Cached(cached) => {
				PgCoalescingCacheFetchResult::Cached(cached.value.value)
			}
			NativeCacheFetchResult::Computed(computed) => {
				PgCoalescingCacheFetchResult::Computed(PgCoalescingCacheComputeResult {
					task: computed.task,
					publication: PgCoalescingCachePublication::from_native(computed.publication),
					release: PgLeaseRelease::from_close_wait(&computed.release.wait),
				})
			}
		})
	}

	/// Makes every stored value of the cache stale. The stored values stay in place until a
	/// computation replaces them.
	pub async fn invalidate_all(
		&self,
		database: impl WriteDatabase,
	) -> Result<(), PgCoalescingCacheError<std::convert::Infallible>> {
		self.native
			.invalidate_all(database)
			.await
			.map(|_| ())
			.map_err(|error| PgCoalescingCacheError::new(PgCoalescingCacheErrorCause::Epoch(error)))
	}

	fn entry_identity<E>(
		&self,
		key_parts: Vec<String>,
	) -> Result<CacheEntryIdentity, PgCoalescingCacheError<E>> {
		CacheEntryIdentity::new(CacheEntryIdentityFields {
			cache: self.native.key().clone(),
			key_parts,
		})
		.map_err(|error| PgCoalescingCacheError::new(PgCoalescingCacheErrorCause::KeyParts(error)))
	}
}

/// The name of one cache entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PgCoalescingCacheEntryFields {
	/// The entry's key parts after the cache's key.
	pub key_parts: Vec<String>,
}

/// The result of a fetch that can compute.
pub enum PgCoalescingCacheFetchResult<T, E, C> {
	/// The entry held a fresh stored value.
	Cached(T),
	/// This fetch computed the value.
	Computed(PgCoalescingCacheComputeResult<T, E, C>),
}

/// What a fetch reports about a computation that it ran.
#[non_exhaustive]
pub struct PgCoalescingCacheComputeResult<T, E, C> {
	/// The computation's own output.
	pub task: Result<T, E>,
	/// Whether the fetch stored the computed value.
	pub publication: PgCoalescingCachePublication<C>,
	/// Whether the fetch confirmed that the entry's lease was released.
	pub release: PgLeaseRelease,
}

/// Whether a fetch stored its computed value.
pub enum PgCoalescingCachePublication<C> {
	/// The fetch stored the value.
	Stored,
	/// The computation failed, so the fetch stored nothing.
	TaskFailed,
	/// The fetch no longer held the entry's lease, so it stored nothing.
	LeadershipLost,
	/// Storing the value failed. When the error's `commit_outcome_is_unknown` is true, the
	/// value may have been stored.
	Failed(PgCoalescingCacheError<C>),
}

impl<C> PgCoalescingCachePublication<C> {
	fn from_native(publication: NativeCachePublicationAccounting<C>) -> Self {
		match publication {
			NativeCachePublicationAccounting::NotRequested(
				NativeCachePublicationNotRequested::AppError,
			) => Self::TaskFailed,
			NativeCachePublicationAccounting::NotRequested(
				NativeCachePublicationNotRequested::AuthorityLost,
			) => Self::LeadershipLost,
			NativeCachePublicationAccounting::Completed(publication) => match publication {
				NativeCachePreparedPublication::ValueWrite(_) => Self::Stored,
				NativeCachePreparedPublication::Refused => Self::LeadershipLost,
			},
			NativeCachePublicationAccounting::HandoffFailed(source) => {
				Self::Failed(PgCoalescingCacheError::new(
					PgCoalescingCacheErrorCause::PublicationHandoff(source),
				))
			}
			NativeCachePublicationAccounting::Failed(source) => Self::Failed(
				PgCoalescingCacheError::new(PgCoalescingCacheErrorCause::Publication(source)),
			),
		}
	}
}

impl<T: fmt::Debug, E: fmt::Debug, C: Error + 'static> fmt::Debug
	for PgCoalescingCacheFetchResult<T, E, C>
{
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Cached(value) => formatter.debug_tuple("Cached").field(value).finish(),
			Self::Computed(computed) => formatter.debug_tuple("Computed").field(computed).finish(),
		}
	}
}

impl<T: fmt::Debug, E: fmt::Debug, C: Error + 'static> fmt::Debug
	for PgCoalescingCacheComputeResult<T, E, C>
{
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("PgCoalescingCacheComputeResult")
			.field("task", &self.task)
			.field("publication", &self.publication)
			.field("release", &self.release)
			.finish()
	}
}

impl<C: Error + 'static> fmt::Debug for PgCoalescingCachePublication<C> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Stored => formatter.write_str("Stored"),
			Self::TaskFailed => formatter.write_str("TaskFailed"),
			Self::LeadershipLost => formatter.write_str("LeadershipLost"),
			Self::Failed(error) => formatter.debug_tuple("Failed").field(error).finish(),
		}
	}
}

enum PgCoalescingCacheErrorCause<C> {
	Construction(NativeCacheConstructionError),
	Options(NativeCacheOptionsError),
	KeyParts(KeyError),
	ScopeHandle(crate::runtime::PgScopeHandleError),
	Read(NativePayloadReadError<C>),
	Compute(NativeCacheComputeError<C>),
	PublicationHandoff(Arc<NativeLeaseDriverError>),
	Publication(NativeCachePublicationError<C>),
	Epoch(NativeCacheEpochError),
}

/// The class of a failed cache operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgCoalescingCacheErrorKind {
	/// The cache options are invalid.
	Options,
	/// The entry's key parts do not form a valid key.
	KeyParts,
	/// The installation does not declare the scope as a cache scope with these value and codec
	/// failure types, or the runtime could not build the cache from the declaration.
	Scope,
	/// The app's payload codec failed. The error's source chain holds the codec error.
	Codec,
	/// A database statement or the transaction's own control failed. The error's source chain
	/// holds the database error.
	Database,
	/// Paranoid's own work on the entry's lease exceeded its time limit, the fetch waited
	/// longer than the options' `lease_wait_timeout` for another holder's computation, or the
	/// computation ran longer than the options' compute timeout.
	Timeout,
	/// The runtime stopped accepting work.
	RuntimeStopped,
	/// The fetch lost the entry's lease. In an error that `fetch_or_compute` returns, the lease
	/// was lost before the computation started; in `PgCoalescingCachePublication::Failed`, it
	/// was lost after the computation returned.
	LeadershipLost,
	/// The runtime cannot operate on the cache under the installation's recorded policy.
	Admission,
	/// The stored entry or epoch could not be interpreted or changed.
	State,
	/// A local failure that the app cannot correct, such as a failed random source.
	Internal,
}

/// A failed cache operation, with its complete cause chain. `C` is the error type of the
/// app's payload codec.
pub struct PgCoalescingCacheError<C> {
	source: Box<PgCoalescingCacheErrorCause<C>>,
}

impl<C> PgCoalescingCacheError<C> {
	fn new(source: PgCoalescingCacheErrorCause<C>) -> Self {
		Self {
			source: Box::new(source),
		}
	}

	pub(crate) fn construction(source: NativeCacheConstructionError) -> Self {
		Self::new(PgCoalescingCacheErrorCause::Construction(source))
	}

	pub(crate) fn scope_handle(source: crate::runtime::PgScopeHandleError) -> Self {
		Self::new(PgCoalescingCacheErrorCause::ScopeHandle(source))
	}

	/// Whether the operation's own `COMMIT` failed, which means that the value may or may not
	/// have been stored, or the invalidation may or may not have taken effect.
	pub fn commit_outcome_is_unknown(&self) -> bool {
		match self.source.as_ref() {
			PgCoalescingCacheErrorCause::Publication(source) => source.commit_outcome_is_unknown(),
			PgCoalescingCacheErrorCause::Epoch(source) => {
				crate::db::TransactionFailure::commit_outcome_is_unknown(source)
			}
			PgCoalescingCacheErrorCause::Construction(_)
			| PgCoalescingCacheErrorCause::Options(_)
			| PgCoalescingCacheErrorCause::KeyParts(_)
			| PgCoalescingCacheErrorCause::ScopeHandle(_)
			| PgCoalescingCacheErrorCause::Read(_)
			| PgCoalescingCacheErrorCause::Compute(_)
			| PgCoalescingCacheErrorCause::PublicationHandoff(_) => false,
		}
	}
}

impl<C: Error + 'static> PgCoalescingCacheError<C> {
	/// The class of the failure.
	pub fn kind(&self) -> PgCoalescingCacheErrorKind {
		use crate::fleet::lease::handle::HandleFailureClass as Class;
		match self.source.as_ref() {
			PgCoalescingCacheErrorCause::Options(_) => return PgCoalescingCacheErrorKind::Options,
			PgCoalescingCacheErrorCause::KeyParts(_) => {
				return PgCoalescingCacheErrorKind::KeyParts;
			}
			PgCoalescingCacheErrorCause::Construction(_)
			| PgCoalescingCacheErrorCause::ScopeHandle(_) => {
				return PgCoalescingCacheErrorKind::Scope;
			}
			PgCoalescingCacheErrorCause::Compute(source) if source.timed_out() => {
				return PgCoalescingCacheErrorKind::Timeout;
			}
			PgCoalescingCacheErrorCause::Compute(source) if source.leadership_lost() => {
				return PgCoalescingCacheErrorKind::LeadershipLost;
			}
			_ => {}
		}
		let mut current = Some(self.source_error());
		while let Some(error) = current {
			if error.downcast_ref::<C>().is_some() {
				return PgCoalescingCacheErrorKind::Codec;
			}
			current = error.source();
		}
		match crate::fleet::lease::handle::classify_handle_failure(self.source_error()) {
			Class::Database => PgCoalescingCacheErrorKind::Database,
			Class::Timeout => PgCoalescingCacheErrorKind::Timeout,
			Class::RuntimeStopped => PgCoalescingCacheErrorKind::RuntimeStopped,
			Class::LeadershipLost => PgCoalescingCacheErrorKind::LeadershipLost,
			Class::Admission => PgCoalescingCacheErrorKind::Admission,
			Class::State => PgCoalescingCacheErrorKind::State,
			Class::Internal => PgCoalescingCacheErrorKind::Internal,
		}
	}

	fn source_error(&self) -> &(dyn Error + 'static) {
		match self.source.as_ref() {
			PgCoalescingCacheErrorCause::Construction(source) => source,
			PgCoalescingCacheErrorCause::Options(source) => source,
			PgCoalescingCacheErrorCause::KeyParts(source) => source,
			PgCoalescingCacheErrorCause::ScopeHandle(source) => source,
			PgCoalescingCacheErrorCause::Read(source) => source,
			PgCoalescingCacheErrorCause::Compute(source) => source,
			PgCoalescingCacheErrorCause::PublicationHandoff(source) => source.as_ref(),
			PgCoalescingCacheErrorCause::Publication(source) => source,
			PgCoalescingCacheErrorCause::Epoch(source) => source,
		}
	}
}

impl<C: Error + 'static> fmt::Display for PgCoalescingCacheError<C> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("cache operation failed")
	}
}

impl<C: Error + 'static> fmt::Debug for PgCoalescingCacheError<C> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(formatter, "PgCoalescingCacheError: {self}")?;
		let mut source = self.source();
		while let Some(error) = source {
			write!(formatter, ": {error}")?;
			source = error.source();
		}
		Ok(())
	}
}

impl<C: Error + 'static> Error for PgCoalescingCacheError<C> {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		Some(self.source_error())
	}
}
