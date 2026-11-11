//! Pre-result failures retain exact Cache identity and actual cleanup observation.

use super::acquire::NativeCacheAcquireCauseError;
use crate::fleet::cache::engine::publication_identity::NativeCachePublicationIdentityError;
use crate::fleet::lease::owned_driver::NativeLeaseDriverError;
use crate::fleet::payload::point::read::NativePayloadReadError;
use std::{fmt, sync::Arc};

pub(crate) enum NativeCacheComputeCause<C> {
	Read(NativePayloadReadError<C>),
	Identity(NativeCachePublicationIdentityError),
	Acquire(NativeCacheAcquireCauseError),
	Caller(Arc<NativeLeaseDriverError>),
	AuthorityLost,
	LocalClockDomain,
	ComputeTimeout,
}
pub(crate) struct NativeCacheComputeError<C> {
	source: Box<NativeCacheComputeCause<C>>,
}
impl<C> NativeCacheComputeError<C> {
	pub(super) fn new(source: NativeCacheComputeCause<C>) -> Self {
		Self {
			source: Box::new(source),
		}
	}
	/// Whether the fetch waited longer than its lease wait timeout for another holder's
	/// computation, or the computation ran longer than its compute timeout.
	pub(crate) fn timed_out(&self) -> bool {
		matches!(
			*self.source,
			NativeCacheComputeCause::Acquire(NativeCacheAcquireCauseError::Deadline)
				| NativeCacheComputeCause::ComputeTimeout
		)
	}
	/// Whether the fetch lost the entry's lease before its computation started.
	pub(crate) fn leadership_lost(&self) -> bool {
		matches!(*self.source, NativeCacheComputeCause::AuthorityLost)
	}
}
impl<C> fmt::Debug for NativeCacheComputeError<C> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("NativeCacheComputeError { typed_cause_retained: true }")
	}
}
impl<C> fmt::Display for NativeCacheComputeError<C> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("native Cache fetch or incomplete computation failed")
	}
}
impl<C: std::error::Error + 'static> std::error::Error for NativeCacheComputeError<C> {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self.source.as_ref() {
			NativeCacheComputeCause::Read(source) => Some(source),
			NativeCacheComputeCause::Identity(source) => Some(source),
			NativeCacheComputeCause::Acquire(source) => Some(source),
			NativeCacheComputeCause::Caller(source) => Some(source.as_ref()),
			NativeCacheComputeCause::AuthorityLost
			| NativeCacheComputeCause::LocalClockDomain
			| NativeCacheComputeCause::ComputeTimeout => None,
		}
	}
}
