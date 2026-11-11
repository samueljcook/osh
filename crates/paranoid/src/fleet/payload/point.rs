//! Fixed private Cache/Topic observation and consuming point completions.
//!
//! These kernels provide no cache computation, publication allocation or worker authority.

pub(crate) mod cache;
pub(crate) mod cache_handle;
pub(crate) mod factory;
pub(crate) mod read;
pub(crate) mod topic;
pub(crate) mod topic_handle;

use std::fmt;

use crate::fleet::payload::scope::PayloadScopeError;
use crate::fleet::payload::state::FleetPayloadStateError;
use crate::records::RecordIdentity;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PayloadPointFamily {
	CacheEntry,
	TopicEvent,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PayloadPointAction {
	ObserveEnvelope,
	ObserveValue,
}
#[derive(Clone, Copy)]
pub(crate) struct PayloadPointContext {
	pub(crate) family: PayloadPointFamily,
	pub(crate) action: PayloadPointAction,
}
pub(crate) enum PayloadPointCause<C> {
	Envelope(PayloadScopeError),
	Payload(FleetPayloadStateError<C>),
}
pub(crate) struct PayloadPointError<C> {
	context: PayloadPointContext,
	source: PayloadPointCause<C>,
}
impl<C> PayloadPointError<C> {
	fn new(context: PayloadPointContext, source: PayloadPointCause<C>) -> Self {
		Self { context, source }
	}
}
impl<C> fmt::Debug for PayloadPointError<C> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("PayloadPointError")
			.field("family", &self.context.family)
			.field("action", &self.context.action)
			.finish_non_exhaustive()
	}
}
impl<C> fmt::Display for PayloadPointError<C> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("native family payload point operation failed")
	}
}
impl<C: std::error::Error + 'static> std::error::Error for PayloadPointError<C> {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match &self.source {
			PayloadPointCause::Envelope(source) => Some(source),
			PayloadPointCause::Payload(source) => Some(source),
		}
	}
}
