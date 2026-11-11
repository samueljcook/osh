//! Application output stays separate from publication and actual Lease release accounting.

use crate::fleet::lease::owned_driver::{
	NativeLeaseDriverError,
	cache_publication::{NativeCachePreparedPublication, error::NativeCachePublicationError},
	close::{
		NativeLeaseCloseObserver, NativeLeaseCloseWait, NativeLeaseCloseWaitError,
		NativeLeaseCloseWaitResult,
	},
};
use crate::fleet::payload::point::cache::CacheValueRead;
use std::{fmt, sync::Arc};

pub(crate) enum NativeCacheFetchResult<T, E, C> {
	Cached(NativeCacheCached<T>),
	Computed(NativeCacheComputed<T, E, C>),
}
pub(crate) struct NativeCacheCached<T> {
	pub(crate) value: CacheValueRead<T>,
}
pub(crate) struct NativeCacheComputed<T, E, C> {
	pub(crate) task: Result<T, E>,
	pub(crate) publication: NativeCachePublicationAccounting<C>,
	pub(crate) release: NativeCacheReleaseAccounting,
}
pub(crate) enum NativeCachePublicationAccounting<C> {
	NotRequested(NativeCachePublicationNotRequested),
	Completed(NativeCachePreparedPublication),
	HandoffFailed(Arc<NativeLeaseDriverError>),
	Failed(NativeCachePublicationError<C>),
}
#[derive(Clone, Copy, Debug)]
pub(crate) enum NativeCachePublicationNotRequested {
	AppError,
	AuthorityLost,
}
pub(crate) struct NativeCacheReleaseAccounting {
	pub(crate) wait: Result<NativeLeaseCloseWaitResult, NativeLeaseCloseWaitError>,
}
impl NativeCacheReleaseAccounting {
	pub(super) async fn observe(
		observer: NativeLeaseCloseObserver,
		wait: &NativeLeaseCloseWait,
	) -> Self {
		let result = observer.wait(wait).await;
		Self { wait: result }
	}
}
impl<T, E, C> fmt::Debug for NativeCacheFetchResult<T, E, C> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Cached(_) => f.write_str("NativeCacheFetchResult::Cached { .. }"),
			Self::Computed(_) => f.write_str("NativeCacheFetchResult::Computed { .. }"),
		}
	}
}
impl<C> fmt::Debug for NativeCachePublicationAccounting<C> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("NativeCachePublicationAccounting { typed_cause_retained: true }")
	}
}
impl fmt::Debug for NativeCacheReleaseAccounting {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("NativeCacheReleaseAccounting { actual_observer_retained: true }")
	}
}
