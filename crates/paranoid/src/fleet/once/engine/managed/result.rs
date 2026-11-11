//! Application output, marker intent and actual Lease release remain separate.

use std::{fmt, sync::Arc};

use crate::fleet::lease::owned_driver::{
	NativeLeaseDriverError,
	close::{
		NativeLeaseCloseObserver, NativeLeaseCloseWait, NativeLeaseCloseWaitError,
		NativeLeaseCloseWaitResult,
	},
	once_completion::{NativeOncePreparedCompletion, error::NativeOnceCompletionError},
};
use crate::fleet::once::state::NativeOnceCompletion;

pub(crate) enum NativeOnceManagedResult<T, E> {
	AlreadyDone { completion: NativeOnceCompletion },
	AlreadyRunning,
	Ran(NativeOnceManagedRan<T, E>),
}
pub(crate) struct NativeOnceManagedRan<T, E> {
	pub(crate) task: Result<T, E>,
	pub(crate) completion: NativeOnceManagedCompletion,
	pub(crate) release: NativeOnceReleaseAccounting,
}
pub(crate) enum NativeOnceManagedCompletion {
	NotRequested(NativeOnceCompletionNotRequested),
	Committed(NativeOncePreparedCompletion),
	HandoffFailed(Arc<NativeLeaseDriverError>),
	Failed(NativeOnceCompletionError),
}
#[derive(Clone, Copy, Debug)]
pub(crate) enum NativeOnceCompletionNotRequested {
	AppError,
	AuthorityLost,
}
pub(crate) struct NativeOnceReleaseAccounting {
	pub(crate) wait: Result<NativeLeaseCloseWaitResult, NativeLeaseCloseWaitError>,
}
impl NativeOnceReleaseAccounting {
	pub(super) async fn observe(
		observer: NativeLeaseCloseObserver,
		wait: &NativeLeaseCloseWait,
	) -> Self {
		let result = observer.wait(wait).await;
		Self { wait: result }
	}
}
impl<T, E> fmt::Debug for NativeOnceManagedResult<T, E> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let name = match self {
			Self::AlreadyDone { .. } => "AlreadyDone",
			Self::AlreadyRunning { .. } => "AlreadyRunning",
			Self::Ran(_) => "Ran",
		};
		f.debug_struct("NativeOnceManagedResult")
			.field("kind", &name)
			.finish_non_exhaustive()
	}
}
impl fmt::Debug for NativeOnceManagedCompletion {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("NativeOnceManagedCompletion { typed_cause_retained: true }")
	}
}
impl fmt::Debug for NativeOnceReleaseAccounting {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("NativeOnceReleaseAccounting { actual_observer_retained: true }")
	}
}
