use super::{RuntimeAdmissionError, RuntimeAdmissionFenceReason, RuntimeAdmissionStop};

/// Whether a runtime admits operations. A stopped runtime finishes its local lifetime when its
/// shutdown has confirmed that every operation and piece of work ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::runtime) enum AdmissionLifecycle {
	Open,
	Stopped(StopReason),
	Finished(StopReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::runtime) enum StopReason {
	Shutdown,
	Fenced(RuntimeAdmissionFenceReason),
}

impl AdmissionLifecycle {
	pub(super) fn require_open(self) -> Result<(), RuntimeAdmissionError> {
		match self {
			Self::Open => Ok(()),
			Self::Stopped(reason) => Err(RuntimeAdmissionError::Stopped {
				stop: reason.kind(),
			}),
			Self::Finished(_) => Err(RuntimeAdmissionError::LifetimeFinished),
		}
	}

	pub(super) fn stop(self) -> Option<RuntimeAdmissionStop> {
		match self {
			Self::Open => None,
			Self::Stopped(reason) | Self::Finished(reason) => Some(reason.kind()),
		}
	}

	pub(super) fn fence_reason(self) -> Option<RuntimeAdmissionFenceReason> {
		match self {
			Self::Stopped(StopReason::Fenced(reason))
			| Self::Finished(StopReason::Fenced(reason)) => Some(reason),
			Self::Open
			| Self::Stopped(StopReason::Shutdown)
			| Self::Finished(StopReason::Shutdown) => None,
		}
	}

	pub(super) fn is_finished(self) -> bool {
		matches!(self, Self::Finished(_))
	}

	pub(super) fn stop_admission(&mut self, stop: RuntimeAdmissionStop) {
		if self.stop() == Some(RuntimeAdmissionStop::Fenced) {
			return;
		}
		let reason = match stop {
			RuntimeAdmissionStop::Shutdown => StopReason::Shutdown,
			RuntimeAdmissionStop::Fenced => {
				StopReason::Fenced(RuntimeAdmissionFenceReason::ExplicitFence)
			}
		};
		*self = match self {
			Self::Open | Self::Stopped(_) => Self::Stopped(reason),
			Self::Finished(_) => Self::Finished(reason),
		};
	}

	pub(super) fn fence(&mut self, reason: RuntimeAdmissionFenceReason) -> bool {
		if self.stop() == Some(RuntimeAdmissionStop::Fenced) {
			return false;
		}
		*self = match self {
			Self::Open | Self::Stopped(_) => Self::Stopped(StopReason::Fenced(reason)),
			Self::Finished(_) => Self::Finished(StopReason::Fenced(reason)),
		};
		true
	}

	pub(super) fn finish(&mut self) -> Result<(), RuntimeAdmissionError> {
		*self = match *self {
			Self::Open => return Err(RuntimeAdmissionError::LifetimeStillActive),
			Self::Stopped(reason) | Self::Finished(reason) => Self::Finished(reason),
		};
		Ok(())
	}
}

impl StopReason {
	fn kind(self) -> RuntimeAdmissionStop {
		match self {
			Self::Shutdown => RuntimeAdmissionStop::Shutdown,
			Self::Fenced(_) => RuntimeAdmissionStop::Fenced,
		}
	}
}
