//! Driver states own delivery, caller work, reservation bounds, and terminal accounting.

use super::super::NativeThrottlerDiagnosticError;
use super::{
	Arc, CallerDelivery, Instant, NativeThrottlerError, NativeThrottlerProbeHeartbeatPolicy,
	RuntimeThrottlerCallerWorkPermit, ThrottlerTaskOutcome, ThrottlerUndeliveredTarget, oneshot,
};

pub(super) type CallerResponse = oneshot::Sender<Result<CallerDelivery, Arc<NativeThrottlerError>>>;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum DriverStage {
	Acquiring,
	Queued,
	DeliveredIdle,
	CallerRequested,
	CallerQueued,
	Running,
	Closing,
	Finished,
}

pub(super) enum ClientAvailability {
	Open,
	Closed,
}
impl ClientAvailability {
	pub(super) fn is_closed(&self) -> bool {
		matches!(self, Self::Closed)
	}
}

#[derive(Clone, Copy)]
pub(super) enum ReservationBounds {
	Unreserved,
	Slot(Instant),
	Probe(Instant),
	SlotAndProbe { slot: Instant, probe: Instant },
}
impl ReservationBounds {
	pub(super) fn until(&self) -> Option<Instant> {
		match self {
			Self::Unreserved => None,
			Self::Slot(until) | Self::Probe(until) => Some(*until),
			Self::SlotAndProbe { slot, probe } => Some((*slot).min(*probe)),
		}
	}
	pub(super) fn probe_until(&self) -> Option<Instant> {
		match self {
			Self::Probe(until) | Self::SlotAndProbe { probe: until, .. } => Some(*until),
			_ => None,
		}
	}
	pub(super) fn extend_probe(&mut self, until: Instant) {
		match self {
			Self::Probe(probe) | Self::SlotAndProbe { probe, .. } => *probe = until,
			_ => unreachable!("only an actual probe reservation can be extended"),
		}
	}
}

#[derive(Clone, Copy)]
pub(super) enum ClosingPurpose {
	Undelivered,
	Outcome(ThrottlerTaskOutcome),
}

pub(super) enum TerminalAccounting {
	Complete(Arc<NativeThrottlerDiagnosticError>),
	Unresolved(Arc<NativeThrottlerDiagnosticError>),
}

pub(super) enum DriverPhase {
	Acquiring,
	Queued {
		bounds: ReservationBounds,
		target: ThrottlerUndeliveredTarget,
	},
	DeliveredIdle(ReservationBounds),
	CallerRequested {
		bounds: ReservationBounds,
		response: CallerResponse,
	},
	CallerQueued {
		bounds: ReservationBounds,
		permit: RuntimeThrottlerCallerWorkPermit,
	},
	Running(ReservationBounds),
	Closing {
		bounds: Option<ReservationBounds>,
		purpose: ClosingPurpose,
	},
	Finished {
		bounds: Option<ReservationBounds>,
		accounting: TerminalAccounting,
	},
}
impl DriverPhase {
	pub(super) fn stage(&self) -> DriverStage {
		match self {
			Self::Acquiring => DriverStage::Acquiring,
			Self::Queued { .. } => DriverStage::Queued,
			Self::DeliveredIdle(_) => DriverStage::DeliveredIdle,
			Self::CallerRequested { .. } => DriverStage::CallerRequested,
			Self::CallerQueued { .. } => DriverStage::CallerQueued,
			Self::Running(_) => DriverStage::Running,
			Self::Closing { .. } => DriverStage::Closing,
			Self::Finished { .. } => DriverStage::Finished,
		}
	}
	pub(super) fn bounds(&self) -> Option<&ReservationBounds> {
		match self {
			Self::Acquiring => None,
			Self::Queued { bounds, .. }
			| Self::DeliveredIdle(bounds)
			| Self::CallerRequested { bounds, .. }
			| Self::CallerQueued { bounds, .. }
			| Self::Running(bounds) => Some(bounds),
			Self::Closing { bounds, .. } | Self::Finished { bounds, .. } => bounds.as_ref(),
		}
	}
	pub(super) fn bounds_mut(&mut self) -> Option<&mut ReservationBounds> {
		match self {
			Self::Acquiring => None,
			Self::Queued { bounds, .. }
			| Self::DeliveredIdle(bounds)
			| Self::CallerRequested { bounds, .. }
			| Self::CallerQueued { bounds, .. }
			| Self::Running(bounds) => Some(bounds),
			Self::Closing { bounds, .. } | Self::Finished { bounds, .. } => bounds.as_mut(),
		}
	}
	pub(super) fn into_closing(
		self,
	) -> (
		Option<ReservationBounds>,
		ClosingPurpose,
		Option<ThrottlerUndeliveredTarget>,
	) {
		match self {
			Self::Acquiring => (None, ClosingPurpose::Undelivered, None),
			Self::Queued { bounds, target } => {
				(Some(bounds), ClosingPurpose::Undelivered, Some(target))
			}
			Self::DeliveredIdle(bounds)
			| Self::CallerRequested { bounds, .. }
			| Self::CallerQueued { bounds, .. } => (
				Some(bounds),
				ClosingPurpose::Outcome(ThrottlerTaskOutcome::NotExecuted),
				None,
			),
			Self::Closing { bounds, purpose } => (bounds, purpose, None),
			Self::Running(_) => unreachable!(
				"running callers publish their outcomes before releasing their permits"
			),
			Self::Finished { .. } => {
				unreachable!("terminal accounting cannot return to caller drainage")
			}
		}
	}
	pub(super) fn into_bounds(self) -> Option<ReservationBounds> {
		match self {
			Self::Acquiring => None,
			Self::Queued { bounds, .. }
			| Self::DeliveredIdle(bounds)
			| Self::CallerRequested { bounds, .. }
			| Self::CallerQueued { bounds, .. }
			| Self::Running(bounds) => Some(bounds),
			Self::Closing { bounds, .. } | Self::Finished { bounds, .. } => bounds,
		}
	}
}

pub(super) enum ProbeHeartbeat {
	AwaitingReservation(Option<NativeThrottlerProbeHeartbeatPolicy>),
	Inactive,
	Scheduled {
		policy: NativeThrottlerProbeHeartbeatPolicy,
		next: Instant,
		failures: usize,
	},
	Lost,
}
impl ProbeHeartbeat {
	pub(super) fn schedule(
		&mut self,
		requested_at: Instant,
		bounds: ReservationBounds,
	) -> Result<(), NativeThrottlerError> {
		let Self::AwaitingReservation(policy) = self else {
			return Err(NativeThrottlerError::OperationState);
		};
		let next = match (bounds.probe_until(), *policy) {
			(Some(_), Some(policy)) => Self::Scheduled {
				policy,
				next: requested_at
					.checked_add(policy.interval)
					.ok_or(NativeThrottlerError::InvalidTiming)?,
				failures: 0,
			},
			_ => Self::Inactive,
		};
		*self = next;
		Ok(())
	}
	pub(super) fn next(&self) -> Option<Instant> {
		match self {
			Self::Scheduled { next, .. } => Some(*next),
			_ => None,
		}
	}
	pub(super) fn scheduled(&self) -> Option<(NativeThrottlerProbeHeartbeatPolicy, usize)> {
		match self {
			Self::Scheduled {
				policy, failures, ..
			} => Some((*policy, *failures)),
			_ => None,
		}
	}
	pub(super) fn reschedule(
		&mut self,
		fields: RescheduleProbe,
	) -> Result<(), NativeThrottlerError> {
		let Self::Scheduled { policy, .. } = self else {
			return Err(NativeThrottlerError::OperationState);
		};
		let next = fields
			.requested_at
			.checked_add(policy.interval)
			.ok_or(NativeThrottlerError::InvalidTiming)?;
		*self = Self::Scheduled {
			policy: *policy,
			next,
			failures: fields.failures,
		};
		Ok(())
	}
}

pub(super) struct RescheduleProbe {
	pub(super) requested_at: Instant,
	pub(super) failures: usize,
}
