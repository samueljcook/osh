//! Acquisition of an owned lease in one attempt or by waiting until the lease is acquired.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::{Instant, sleep_until};

use super::close::NativeLeaseCloseObserver;
use super::denial::NativeLeaseOwnedStart;
use super::options::{NativeLeaseAcquisitionChoice, NativeLeaseOwnerOptions};
use super::{NativeLeaseDriverError, NativeLeaseOwnedStartFields, NativeOwnedLeaseClient};
use crate::fleet::RandomnessError;
use crate::fleet::generate_jittered_acquire_retry_delay;
use crate::fleet::lease::PgNativeLease;

pub(crate) struct NativeLeaseAcquireFields<'a> {
	pub(crate) options: &'a NativeLeaseOwnerOptions,
	pub(crate) acquisition: NativeLeaseAcquisitionChoice,
}

pub(crate) enum NativeLeaseAcquisition {
	Acquired(NativeOwnedLeaseClient),
	/// Another holder held the lease, and the choice was one attempt. The observer belongs to
	/// the denied attempt's driver.
	Denied(NativeLeaseCloseObserver),
}

pub(crate) enum NativeLeaseAcquireError {
	Start {
		source: Arc<NativeLeaseDriverError>,
		observer: Option<NativeLeaseCloseObserver>,
	},
	Randomness(RandomnessError),
	LocalClockDomain,
}

impl PgNativeLease {
	/// Acquires the lease for owned work. A waiting choice retries after each settled denial
	/// with a jittered delay that doubles up to the policy's maximum.
	pub(crate) async fn acquire_owned(
		&self,
		fields: NativeLeaseAcquireFields<'_>,
	) -> Result<NativeLeaseAcquisition, NativeLeaseAcquireError> {
		let mut retry_interval = match fields.acquisition {
			NativeLeaseAcquisitionChoice::TryOnce => Duration::ZERO,
			NativeLeaseAcquisitionChoice::Waiting(policy) => policy.acquire_retry_interval,
		};
		loop {
			let denial = match self
				.start_owned(NativeLeaseOwnedStartFields {
					options: fields.options.clone(),
					delivery_deadline: None,
				})
				.await
			{
				Ok(NativeLeaseOwnedStart::Acquired(client)) => {
					return Ok(NativeLeaseAcquisition::Acquired(client));
				}
				Ok(NativeLeaseOwnedStart::Denied(denial)) => denial.close_observer(),
				Err(failure) => {
					return Err(NativeLeaseAcquireError::Start {
						source: failure.source,
						observer: failure.observer,
					});
				}
			};
			let NativeLeaseAcquisitionChoice::Waiting(policy) = fields.acquisition else {
				return Ok(NativeLeaseAcquisition::Denied(denial));
			};
			// The denied attempt's driver settles on its own; a waiting acquisition does not
			// report it.
			drop(denial);
			let delay = generate_jittered_acquire_retry_delay(retry_interval)
				.map_err(NativeLeaseAcquireError::Randomness)?;
			let retry_at = Instant::now()
				.checked_add(delay)
				.ok_or(NativeLeaseAcquireError::LocalClockDomain)?;
			sleep_until(retry_at).await;
			retry_interval = retry_interval
				.saturating_mul(2)
				.min(policy.max_acquire_retry_interval);
		}
	}
}
