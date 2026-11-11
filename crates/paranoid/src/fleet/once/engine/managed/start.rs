//! Ordinary and manual starts retain one actual caller scope through their checked delivery.

use std::sync::Arc;

use super::acquire::{NativeOnceAcquireFields, NativeOnceAcquisition, acquire};
use super::error::{NativeOnceManagedCause as Cause, NativeOnceManagedError};
use super::options::NativeOnceManagedOptions;
use super::result::NativeOnceReleaseAccounting;
use super::{PgNativeOnce, close_caller};
use crate::db::WritePool;
use crate::fleet::lease::owned_driver::{
	NativeOwnedLeaseClient,
	cache_publication::NativeLeaseCallerScope,
	close::NativeLeaseCloseObserver,
	loss::{NativeLeaseAuthorityObservation, NativeLeaseCallerCtx},
};
use crate::fleet::once::state::NativeOnceCompletion;

pub(in crate::fleet::once::engine) struct NativeOnceStartFields<'a> {
	pub(in crate::fleet::once::engine) once: &'a Arc<PgNativeOnce>,
	pub(in crate::fleet::once::engine) pool: &'a WritePool,
	pub(in crate::fleet::once::engine) options: &'a NativeOnceManagedOptions,
}
pub(in crate::fleet::once::engine) enum NativeOnceStarted {
	Ready {
		client: NativeOwnedLeaseClient,
		caller: NativeLeaseCallerScope,
		ctx: NativeLeaseCallerCtx,
	},
	AlreadyDone {
		completion: NativeOnceCompletion,
	},
	AlreadyRunning,
}
pub(in crate::fleet::once::engine) async fn start(
	fields: NativeOnceStartFields<'_>,
) -> Result<NativeOnceStarted, NativeOnceManagedError> {
	let fail = NativeOnceManagedError::new;
	let mut client = match acquire(NativeOnceAcquireFields {
		once: fields.once,
		pool: fields.pool,
		options: fields.options,
	})
	.await?
	{
		NativeOnceAcquisition::Acquired(client) => client,
		NativeOnceAcquisition::AlreadyDone { completion } => {
			return Ok(NativeOnceStarted::AlreadyDone { completion });
		}
		NativeOnceAcquisition::AlreadyRunning => {
			return Ok(NativeOnceStarted::AlreadyRunning);
		}
	};
	let caller = match client.begin_caller_work().await {
		Ok(caller) => caller,
		Err(source) => {
			observe_close(client.close(), fields.options).await;
			return Err(fail(Cause::Caller(source)));
		}
	};
	match fields.once.fetch_completion(fields.pool).await {
		// The completion is recorded whether or not the release is confirmed, and a later run
		// reads the completion before it tries the lease, so an unconfirmed release delays no
		// later run.
		Ok(Some(completion)) => {
			close_caller(client, caller, fields.options).await;
			return Ok(NativeOnceStarted::AlreadyDone { completion });
		}
		Ok(None) => {}
		Err(source) => {
			close_caller(client, caller, fields.options).await;
			return Err(fail(Cause::Read(source)));
		}
	}
	let loss = client.loss_observer();
	let holder = match loss.observe() {
		NativeLeaseAuthorityObservation::ConfirmedLocally { holder, .. } => holder,
		NativeLeaseAuthorityObservation::Lost { .. } => {
			close_caller(client, caller, fields.options).await;
			return Err(fail(Cause::AuthorityLost));
		}
	};
	Ok(NativeOnceStarted::Ready {
		client,
		caller,
		ctx: NativeLeaseCallerCtx { holder, loss },
	})
}
pub(in crate::fleet::once::engine) async fn observe_close(
	observer: NativeLeaseCloseObserver,
	options: &NativeOnceManagedOptions,
) -> NativeOnceReleaseAccounting {
	NativeOnceReleaseAccounting::observe(observer, &options.close_wait).await
}
