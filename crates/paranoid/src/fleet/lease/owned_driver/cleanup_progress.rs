//! Cleanup failure budgets advance only after the corresponding origins are settled.

use super::{
	Arc, Duration, Instant, LeaseCleanupPhase, NativeLeaseDriverError, NativeLeaseWorkPhase,
};

pub(super) enum CleanupFailureProgress {
	Ready {
		failures: u32,
	},
	Pending {
		failures: u32,
		error: Arc<NativeLeaseDriverError>,
	},
}

impl CleanupFailureProgress {
	fn failures(&self) -> u32 {
		match self {
			Self::Ready { failures } | Self::Pending { failures, .. } => *failures,
		}
	}
	pub(super) fn settle(&mut self) -> Option<Arc<NativeLeaseDriverError>> {
		let failures = self.failures();
		match std::mem::replace(self, Self::Ready { failures }) {
			Self::Ready { .. } => None,
			Self::Pending { error, .. } => {
				*self = Self::Ready {
					failures: failures + 1,
				};
				Some(error)
			}
		}
	}
	pub(super) fn record(&mut self, error: Arc<NativeLeaseDriverError>) {
		let Self::Ready { failures } = self else {
			unreachable!("a cleanup failure is settled before its replacement is retained");
		};
		*self = Self::Pending {
			failures: *failures,
			error,
		};
	}
	pub(super) fn is_exhausted(&self, budget: u32) -> bool {
		self.failures() >= budget
	}
	pub(super) fn needs_final_proof(&self, budget: u32) -> bool {
		matches!(self, Self::Pending { failures, .. } if *failures + 1 >= budget)
	}
}

pub(super) enum CleanupDeadline {
	OriginOrdering(Instant),
	AuthenticationAndRelease(Instant),
}

impl CleanupDeadline {
	pub(super) fn new(
		phase: LeaseCleanupPhase,
		timing: &super::super::driver_timing::LeaseControlTiming,
	) -> Result<Self, NativeLeaseDriverError> {
		let timeout = match phase {
			LeaseCleanupPhase::OriginOrdering => timing.physical_proof_attempt_timeout,
			LeaseCleanupPhase::AuthenticationAndRelease(_) => timing.cleanup_attempt_timeout,
		};
		let until = Instant::now()
			.checked_add(timeout)
			.ok_or(NativeLeaseDriverError::LocalClockDomain)?;
		Ok(match phase {
			LeaseCleanupPhase::OriginOrdering => Self::OriginOrdering(until),
			LeaseCleanupPhase::AuthenticationAndRelease(_) => Self::AuthenticationAndRelease(until),
		})
	}
	pub(super) fn until(&self) -> Instant {
		match self {
			Self::OriginOrdering(until) | Self::AuthenticationAndRelease(until) => *until,
		}
	}
	pub(super) fn is_ordering(&self) -> bool {
		matches!(self, Self::OriginOrdering(_))
	}
	pub(super) fn phase(&self) -> NativeLeaseWorkPhase {
		match self {
			Self::OriginOrdering(_) => NativeLeaseWorkPhase::PhysicalProof,
			Self::AuthenticationAndRelease(_) => NativeLeaseWorkPhase::Cleanup,
		}
	}
	pub(super) fn observe(
		&mut self,
		observed: LeaseCleanupPhase,
		timeout: Duration,
	) -> Result<(), NativeLeaseDriverError> {
		if let Self::OriginOrdering(until) = self
			&& let LeaseCleanupPhase::AuthenticationAndRelease(started) = observed
		{
			if started >= *until {
				return Err(NativeLeaseDriverError::Timeout(
					NativeLeaseWorkPhase::PhysicalProof,
				));
			}
			*self = Self::AuthenticationAndRelease(
				started
					.checked_add(timeout)
					.ok_or(NativeLeaseDriverError::LocalClockDomain)?,
			);
		}
		Ok(())
	}
}
