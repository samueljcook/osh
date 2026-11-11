//! A settled no-grant result supplies no private authority or automatic retry.

use super::close::NativeLeaseCloseObserver;
use super::{Arc, NativeLeaseDriverError, NativeOwnedLeaseClient};

pub(crate) enum NativeLeaseOwnedStart {
	Acquired(NativeOwnedLeaseClient),
	Denied(SettledLeaseDenial),
}
pub(crate) struct SettledLeaseDenial {
	observer: NativeLeaseCloseObserver,
}
impl SettledLeaseDenial {
	pub(super) fn new(observer: NativeLeaseCloseObserver) -> Self {
		Self { observer }
	}
	pub(crate) fn close_observer(&self) -> NativeLeaseCloseObserver {
		self.observer.clone()
	}
}
/// An actual task observer survives a failed or undelivered acquisition.
pub(crate) struct NativeLeaseStartErrorWithObserver {
	pub(crate) source: Arc<NativeLeaseDriverError>,
	pub(crate) observer: Option<NativeLeaseCloseObserver>,
}
impl std::fmt::Debug for NativeLeaseStartErrorWithObserver {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter
			.debug_struct("NativeLeaseStartErrorWithObserver")
			.field("source", &self.source)
			.field("has_observer", &self.observer.is_some())
			.finish()
	}
}
impl std::fmt::Display for NativeLeaseStartErrorWithObserver {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str("native owned lease start failed")
	}
}
impl std::error::Error for NativeLeaseStartErrorWithObserver {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(self.source.as_ref())
	}
}
