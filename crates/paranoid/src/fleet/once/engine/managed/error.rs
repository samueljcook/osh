//! Failures before caller invocation retain actual placement and close observation.

use std::{fmt, sync::Arc};

use crate::fleet::RandomnessError;
use crate::fleet::lease::owned_driver::NativeLeaseDriverError;
use crate::fleet::once::engine::NativeOnceError;

pub(crate) enum NativeOnceManagedCause {
	Read(NativeOnceError),
	Acquire(Arc<NativeLeaseDriverError>),
	Caller(Arc<NativeLeaseDriverError>),
	AuthorityLost,
	Randomness(RandomnessError),
	LocalClockDomain,
}
/// An error means that this caller's task function was never invoked.
pub(crate) struct NativeOnceManagedError {
	source: Box<NativeOnceManagedCause>,
}
impl NativeOnceManagedError {
	pub(super) fn new(source: NativeOnceManagedCause) -> Self {
		Self {
			source: Box::new(source),
		}
	}
	pub(crate) fn typed_source(&self) -> &NativeOnceManagedCause {
		&self.source
	}
}
impl fmt::Debug for NativeOnceManagedError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("NativeOnceManagedError { typed_cause_retained: true }")
	}
}
impl fmt::Display for NativeOnceManagedError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("native Once task did not start; actual close remains observable")
	}
}
impl std::error::Error for NativeOnceManagedError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self.source.as_ref() {
			NativeOnceManagedCause::Read(source) => Some(source),
			NativeOnceManagedCause::Acquire(source) | NativeOnceManagedCause::Caller(source) => {
				Some(source.as_ref())
			}
			NativeOnceManagedCause::Randomness(source) => Some(source),
			NativeOnceManagedCause::AuthorityLost | NativeOnceManagedCause::LocalClockDomain => {
				None
			}
		}
	}
}
