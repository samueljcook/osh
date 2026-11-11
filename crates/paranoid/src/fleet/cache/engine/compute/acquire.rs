//! Only a settled no-grant start permits another attempt within one absolute deadline. Each
//! retry delay is jittered and doubles up to the waiting policy's maximum. After each denial,
//! the fetch reads the entry again so that a value that another holder stored ends the wait.

use crate::fleet::lease::PgNativeLease;
use crate::fleet::lease::owned_driver::options::NativeLeaseWaitingPolicy;
use crate::fleet::lease::owned_driver::{
	NativeLeaseDriverError, NativeLeaseOwnedStartFields, NativeOwnedLeaseClient,
	close::NativeLeaseCloseObserver, denial::NativeLeaseOwnedStart,
};
use crate::fleet::{RandomnessError, generate_jittered_acquire_retry_delay};
use std::{fmt, sync::Arc};
use tokio::time::{Instant, sleep_until};

pub(super) struct CacheAcquireFields<'a, ReadFresh> {
	pub(super) lease: &'a PgNativeLease,
	pub(super) options: &'a crate::fleet::lease::owned_driver::options::NativeLeaseOwnerOptions,
	pub(super) deadline: Instant,
	pub(super) waiting: NativeLeaseWaitingPolicy,
	pub(super) read_fresh: ReadFresh,
}
pub(super) enum CacheAcquisition<V> {
	Acquired(NativeOwnedLeaseClient),
	/// Another holder stored a fresh value while this fetch waited.
	Fresh {
		value: V,
		observer: NativeLeaseCloseObserver,
	},
}
pub(super) enum CacheAcquireError<E> {
	Acquire(CacheAcquireFailure),
	Read {
		source: E,
		observer: NativeLeaseCloseObserver,
	},
}
#[derive(Debug)]
pub(crate) enum NativeCacheAcquireCauseError {
	Deadline,
	LocalClockDomain,
	Randomness(RandomnessError),
	Lease(Arc<NativeLeaseDriverError>),
}
pub(super) struct CacheAcquireFailure {
	pub(super) source: NativeCacheAcquireCauseError,
	pub(super) observer: Option<NativeLeaseCloseObserver>,
}
// The future of a fetch is `Send` whenever the read's future is, because `read_fresh` is a
// closure that returns a future and not an async closure: rustc cannot prove that the futures of
// an async closure that captures references are `Send` for every lifetime.
pub(super) async fn acquire<V, E, ReadFresh, ReadFuture>(
	mut fields: CacheAcquireFields<'_, ReadFresh>,
) -> Result<CacheAcquisition<V>, CacheAcquireError<E>>
where
	ReadFresh: FnMut() -> ReadFuture,
	ReadFuture: Future<Output = Result<Option<V>, E>>,
{
	let mut last_denial = None;
	let mut retry_interval = fields.waiting.acquire_retry_interval;
	loop {
		if Instant::now() >= fields.deadline {
			return Err(CacheAcquireError::Acquire(CacheAcquireFailure {
				source: NativeCacheAcquireCauseError::Deadline,
				observer: last_denial,
			}));
		}
		match fields
			.lease
			.start_owned(NativeLeaseOwnedStartFields {
				options: fields.options.clone(),
				delivery_deadline: Some(fields.deadline),
			})
			.await
		{
			Ok(NativeLeaseOwnedStart::Acquired(client)) => {
				return Ok(CacheAcquisition::Acquired(client));
			}
			Ok(NativeLeaseOwnedStart::Denied(denial)) => {
				let observer = denial.close_observer();
				match (fields.read_fresh)().await {
					Ok(Some(value)) => return Ok(CacheAcquisition::Fresh { value, observer }),
					Ok(None) => {}
					Err(source) => return Err(CacheAcquireError::Read { source, observer }),
				}
				last_denial = Some(observer);
			}
			Err(failure) => {
				return Err(CacheAcquireError::Acquire(CacheAcquireFailure {
					source: NativeCacheAcquireCauseError::Lease(failure.source),
					observer: failure.observer,
				}));
			}
		}
		let delay = generate_jittered_acquire_retry_delay(retry_interval).map_err(|error| {
			CacheAcquireError::Acquire(CacheAcquireFailure {
				source: NativeCacheAcquireCauseError::Randomness(error),
				observer: last_denial.clone(),
			})
		})?;
		let retry_at = Instant::now().checked_add(delay).ok_or_else(|| {
			CacheAcquireError::Acquire(CacheAcquireFailure {
				source: NativeCacheAcquireCauseError::LocalClockDomain,
				observer: last_denial.clone(),
			})
		})?;
		sleep_until(retry_at.min(fields.deadline)).await;
		retry_interval = retry_interval
			.saturating_mul(2)
			.min(fields.waiting.max_acquire_retry_interval);
	}
}
impl fmt::Display for NativeCacheAcquireCauseError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("Cache computation Lease acquisition did not deliver authority")
	}
}
impl std::error::Error for NativeCacheAcquireCauseError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Lease(source) => Some(source.as_ref()),
			Self::Randomness(source) => Some(source),
			Self::Deadline | Self::LocalClockDomain => None,
		}
	}
}
