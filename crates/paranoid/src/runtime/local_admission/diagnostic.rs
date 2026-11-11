//! Trusted retained terminal causes with redacted default diagnostics.

use std::{fmt, sync::Arc};

#[derive(Debug, thiserror::Error)]
#[error("runtime Throttler ownership was dropped before explicit local completion")]
pub(super) struct RuntimeThrottlerOwnerAbandonedError;

#[derive(Debug, thiserror::Error)]
#[error("runtime Semaphore ownership was dropped before explicit local completion")]
pub(super) struct RuntimeSemaphoreOwnerAbandonedError;

#[derive(Clone)]
pub(crate) struct RuntimeWorkTerminalError(pub(super) Arc<dyn std::error::Error + Send + Sync>);
impl RuntimeWorkTerminalError {
	pub(crate) fn error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
		self.0.as_ref()
	}
}
impl PartialEq for RuntimeWorkTerminalError {
	fn eq(&self, other: &Self) -> bool {
		Arc::ptr_eq(&self.0, &other.0)
	}
}
impl Eq for RuntimeWorkTerminalError {}
impl fmt::Debug for RuntimeWorkTerminalError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("RuntimeWorkTerminalError { typed_cause_retained: true }")
	}
}
impl fmt::Display for RuntimeWorkTerminalError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("runtime owned work has a retained terminal cause")
	}
}
impl std::error::Error for RuntimeWorkTerminalError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(self.0.as_ref())
	}
}
