//! Only actual settled no-grant acquisition permits a completion-aware retry.

use std::sync::Arc;
use tokio::time::{Instant, sleep_until};

use super::error::{NativeOnceManagedCause as Cause, NativeOnceManagedError};
use super::options::NativeOnceManagedOptions;
use super::result::NativeOnceReleaseAccounting;
use crate::db::WritePool;
use crate::fleet::generate_jittered_acquire_retry_delay;
use crate::fleet::lease::owned_driver::options::NativeLeaseAcquisitionChoice;
use crate::fleet::lease::owned_driver::{
	NativeLeaseOwnedStartFields, NativeOwnedLeaseClient, close::NativeLeaseCloseObserver,
	denial::NativeLeaseOwnedStart,
};
use crate::fleet::once::engine::PgNativeOnce;
use crate::fleet::once::state::NativeOnceCompletion;

pub(super) struct NativeOnceAcquireFields<'a> {
	pub(super) once: &'a Arc<PgNativeOnce>,
	pub(super) pool: &'a WritePool,
	pub(super) options: &'a NativeOnceManagedOptions,
}
pub(super) enum NativeOnceAcquisition {
	Acquired(NativeOwnedLeaseClient),
	AlreadyDone { completion: NativeOnceCompletion },
	AlreadyRunning,
}
pub(super) async fn acquire(
	fields: NativeOnceAcquireFields<'_>,
) -> Result<NativeOnceAcquisition, NativeOnceManagedError> {
	let fail = NativeOnceManagedError::new;
	if let Some(completion) = fields
		.once
		.fetch_completion(fields.pool)
		.await
		.map_err(|error| fail(Cause::Read(error)))?
	{
		return Ok(NativeOnceAcquisition::AlreadyDone { completion });
	}
	let mut retry_interval = match fields.options.acquisition {
		NativeLeaseAcquisitionChoice::Waiting(policy) => policy.acquire_retry_interval,
		NativeLeaseAcquisitionChoice::TryOnce => std::time::Duration::ZERO,
	};
	loop {
		let last_denial = match fields
			.once
			.lease()
			.start_owned(NativeLeaseOwnedStartFields {
				options: fields.options.lease.clone(),
				delivery_deadline: None,
			})
			.await
		{
			Ok(NativeLeaseOwnedStart::Acquired(client)) => {
				return Ok(NativeOnceAcquisition::Acquired(client));
			}
			Ok(NativeLeaseOwnedStart::Denied(denial)) => Some(denial.close_observer()),
			Err(failure) => {
				observe_optional(failure.observer, fields.options).await;
				return Err(fail(Cause::Acquire(failure.source)));
			}
		};
		let observed = fields.once.fetch_completion(fields.pool).await;
		match observed {
			Ok(Some(completion)) => {
				observe_optional(last_denial, fields.options).await;
				return Ok(NativeOnceAcquisition::AlreadyDone { completion });
			}
			Ok(None) => {}
			Err(source) => {
				observe_optional(last_denial, fields.options).await;
				return Err(fail(Cause::Read(source)));
			}
		}
		let NativeLeaseAcquisitionChoice::Waiting(policy) = fields.options.acquisition else {
			NativeOnceReleaseAccounting::observe(
				last_denial.expect("an actual settled denial precedes the try result"),
				&fields.options.close_wait,
			)
			.await;
			return Ok(NativeOnceAcquisition::AlreadyRunning);
		};
		let delay = match generate_jittered_acquire_retry_delay(retry_interval) {
			Ok(delay) => delay,
			Err(source) => {
				observe_optional(last_denial, fields.options).await;
				return Err(fail(Cause::Randomness(source)));
			}
		};
		let Some(retry_at) = Instant::now().checked_add(delay) else {
			observe_optional(last_denial, fields.options).await;
			return Err(fail(Cause::LocalClockDomain));
		};
		sleep_until(retry_at).await;
		retry_interval = retry_interval
			.saturating_mul(2)
			.min(policy.max_acquire_retry_interval);
	}
}
async fn observe_optional(
	observer: Option<NativeLeaseCloseObserver>,
	options: &NativeOnceManagedOptions,
) {
	if let Some(observer) = observer {
		NativeOnceReleaseAccounting::observe(observer, &options.close_wait).await;
	}
}
