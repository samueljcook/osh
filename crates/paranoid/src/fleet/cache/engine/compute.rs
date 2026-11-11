//! Caller-owned computation under the actual mapped Lease and fixed Cache publication.

mod acquire;
pub(crate) mod error;
pub(crate) mod result;

use super::{NativeCacheFreshReadFields, PgNativeCache};
use crate::db::WritePool;
use crate::fleet::cache::entry_state::CacheEntryIdentity;
use crate::fleet::lease::PgNativeLease;
use crate::fleet::lease::owned_driver::cache_publication::execution::NativeCacheEntryPublicationFields;
use crate::fleet::lease::owned_driver::{
	NativeOwnedLeaseClient,
	cache_publication::{NativeLeaseCachePublicationFields, NativeLeaseCallerScope},
	close::NativeLeaseCloseWait,
	loss::{NativeLeaseAuthorityObservation, NativeLeaseCallerCtx},
};
use crate::fleet::payload::point::cache_handle::CacheComputationRead;
use crate::scope::payload::PayloadSchemaDeclaration;
use acquire::{CacheAcquireError, CacheAcquireFields, CacheAcquisition, acquire};
use error::{NativeCacheComputeCause as Cause, NativeCacheComputeError};
use result::{
	NativeCacheCached, NativeCacheComputed, NativeCacheFetchResult,
	NativeCachePublicationAccounting, NativeCachePublicationNotRequested,
	NativeCacheReleaseAccounting,
};
use tokio::time::{Instant, timeout_at};

pub(crate) struct NativeCacheComputeFields<'a, 'scope, 'code, T, C, ComputeFn> {
	pub(crate) pool: &'a WritePool,
	pub(crate) cache_entry_identity: &'a CacheEntryIdentity,
	pub(crate) payload: &'a PayloadSchemaDeclaration<'scope, 'code, T, C>,
	pub(crate) compute_fn: ComputeFn,
	pub(crate) close_wait: &'a NativeLeaseCloseWait,
}

impl PgNativeCache<'_, '_> {
	/// Application output stays in this caller future through publication and close observation.
	pub(crate) async fn fetch_or_compute<T, E, C, ComputeFn>(
		&self,
		fields: NativeCacheComputeFields<'_, '_, '_, T, C, ComputeFn>,
	) -> Result<NativeCacheFetchResult<T, E, C>, NativeCacheComputeError<C>>
	where
		ComputeFn: AsyncFnOnce(NativeLeaseCallerCtx) -> Result<T, E>,
	{
		let fail = NativeCacheComputeError::new;
		if let Some(value) = self
			.fetch_fresh(
				fields.pool,
				NativeCacheFreshReadFields {
					cache_entry_identity: fields.cache_entry_identity,
					payload: fields.payload,
				},
			)
			.await
			.map_err(|error| fail(Cause::Read(error)))?
		{
			return Ok(NativeCacheFetchResult::Cached(NativeCacheCached { value }));
		}
		let identity = self
			.publication_identity(fields.cache_entry_identity)
			.map_err(|error| fail(Cause::Identity(error)))?;
		let lease = PgNativeLease::new_native_only(
			identity.lease_bindings().clone(),
			identity.mutex_key().clone(),
		);
		let deadline = Instant::now()
			.checked_add(self.options.lease_wait_timeout())
			.ok_or_else(|| fail(Cause::LocalClockDomain))?;
		let (pool, cache_entry_identity, payload) =
			(fields.pool, fields.cache_entry_identity, fields.payload);
		let mut client = match acquire(CacheAcquireFields {
			lease: &lease,
			options: self.options.lease(),
			deadline,
			waiting: self.options.waiting(),
			read_fresh: move || {
				self.fetch_fresh(
					pool,
					NativeCacheFreshReadFields {
						cache_entry_identity,
						payload,
					},
				)
			},
		})
		.await
		{
			Ok(CacheAcquisition::Acquired(client)) => client,
			Ok(CacheAcquisition::Fresh { value, observer }) => {
				NativeCacheReleaseAccounting::observe(observer, fields.close_wait).await;
				return Ok(NativeCacheFetchResult::Cached(NativeCacheCached { value }));
			}
			Err(CacheAcquireError::Read { source, observer }) => {
				NativeCacheReleaseAccounting::observe(observer, fields.close_wait).await;
				return Err(fail(Cause::Read(source)));
			}
			Err(CacheAcquireError::Acquire(failure)) => {
				if let Some(observer) = failure.observer {
					NativeCacheReleaseAccounting::observe(observer, fields.close_wait).await;
				}
				return Err(fail(Cause::Acquire(failure.source)));
			}
		};
		let caller = match client.begin_caller_work().await {
			Ok(caller) => caller,
			Err(source) => {
				NativeCacheReleaseAccounting::observe(client.close(), fields.close_wait).await;
				return Err(fail(Cause::Caller(source)));
			}
		};
		let read = self
			.entry
			.observe_computation_start(
				fields.pool,
				fields.cache_entry_identity,
				fields.payload,
				&self.epoch,
			)
			.await;
		let captured_epoch = match read {
			// The value is fresh whether or not the release is confirmed; an unconfirmed release
			// ends when the claim expires, and a later fetch reads the value before it tries the
			// lease.
			Ok(CacheComputationRead::Cached(value)) => {
				close_caller(client, caller, fields.close_wait).await;
				return Ok(NativeCacheFetchResult::Cached(NativeCacheCached { value }));
			}
			Ok(CacheComputationRead::Compute { captured_epoch }) => captured_epoch,
			Err(source) => {
				close_caller(client, caller, fields.close_wait).await;
				return Err(fail(Cause::Read(source)));
			}
		};
		let loss = client.loss_observer();
		let holder = match loss.observe() {
			NativeLeaseAuthorityObservation::ConfirmedLocally { holder, .. } => holder,
			NativeLeaseAuthorityObservation::Lost { .. } => {
				close_caller(client, caller, fields.close_wait).await;
				return Err(fail(Cause::AuthorityLost));
			}
		};
		let compute_deadline = match self.options.compute_timeout() {
			Some(duration) => match Instant::now().checked_add(duration) {
				Some(deadline) => Some(deadline),
				None => {
					close_caller(client, caller, fields.close_wait).await;
					return Err(fail(Cause::LocalClockDomain));
				}
			},
			None => None,
		};
		let computed = {
			let future = (fields.compute_fn)(NativeLeaseCallerCtx {
				holder,
				loss: loss.clone(),
			});
			match compute_deadline {
				Some(deadline) => timeout_at(deadline, future).await.map_err(|_| ()),
				None => Ok(future.await),
			}
		};
		let task = match computed {
			Ok(task) => task,
			Err(()) => {
				close_caller(client, caller, fields.close_wait).await;
				return Err(fail(Cause::ComputeTimeout));
			}
		};
		let authority_at_completion = loss.observe();
		let not_requested = if task.is_err() {
			Some(NativeCachePublicationNotRequested::AppError)
		} else if matches!(
			authority_at_completion,
			NativeLeaseAuthorityObservation::Lost { .. }
		) {
			Some(NativeCachePublicationNotRequested::AuthorityLost)
		} else {
			None
		};
		let (publication, observer) = if let Some(reason) = not_requested {
			drop(caller);
			(
				NativeCachePublicationAccounting::NotRequested(reason),
				client.close(),
			)
		} else {
			match client
				.into_cache_publication(NativeLeaseCachePublicationFields {
					identity,
					caller,
					requested_at: Instant::now(),
				})
				.await
			{
				Ok(publication) => match publication
					.publish_cache_entry(NativeCacheEntryPublicationFields {
						cache: self,
						cache_entry_identity: fields.cache_entry_identity,
						declaration: fields.payload,
						value: match &task {
							Ok(value) => value,
							Err(_) => unreachable!("only a successful task requests publication"),
						},
						captured_epoch,
						ttl: self.options.value_ttl(),
					})
					.await
				{
					Ok(result) => (
						NativeCachePublicationAccounting::Completed(result.publication),
						result.observer,
					),
					Err(failure) => (
						NativeCachePublicationAccounting::Failed(failure.error),
						failure.observer,
					),
				},
				Err(failure) => (
					NativeCachePublicationAccounting::HandoffFailed(failure.source),
					failure.observer,
				),
			}
		};
		let release = NativeCacheReleaseAccounting::observe(observer, fields.close_wait).await;
		Ok(NativeCacheFetchResult::Computed(NativeCacheComputed {
			task,
			publication,
			release,
		}))
	}
}

async fn close_caller(
	client: NativeOwnedLeaseClient,
	caller: NativeLeaseCallerScope,
	wait: &NativeLeaseCloseWait,
) -> NativeCacheReleaseAccounting {
	drop(caller);
	NativeCacheReleaseAccounting::observe(client.close(), wait).await
}
