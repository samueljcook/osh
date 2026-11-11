//! One Lease owner retains every private claim, renewal proposal, and terminal purpose.

use super::{
	Arc, Claim, Instant, LeaseCleanupTarget, LeaseCommittedCompletion, LeaseFinalPurposeOrigin,
	Token,
};

pub(super) enum LeaseOwnership {
	New,
	Acquiring(LeaseCleanupTarget),
	Held(HeldLease),
	Renewing {
		prior: HeldLease,
		target: LeaseCleanupTarget,
	},
	ConfirmingAcquisition {
		claim: Claim,
		requested_at: Instant,
		target: LeaseCleanupTarget,
	},
	ConfirmingRenewal {
		claim: Claim,
		requested_at: Instant,
		prior: HeldLease,
		target: LeaseCleanupTarget,
	},
	FinalPurpose {
		until: Instant,
		target: LeaseCleanupTarget,
		origin: LeaseFinalPurposeOrigin,
	},
	Committed(LeaseCommittedCompletion),
	Finished,
}

pub(super) struct HeldLease {
	claim: Claim,
	until: Instant,
	target: LeaseCleanupTarget,
}

impl LeaseOwnership {
	pub(super) fn until(&self) -> Option<Instant> {
		match self {
			Self::Held(held) | Self::Renewing { prior: held, .. } => Some(held.until),
			Self::ConfirmingRenewal { prior, .. } => Some(prior.until),
			Self::FinalPurpose { until, .. } => Some(*until),
			_ => None,
		}
	}
	pub(super) fn claim(&self) -> Option<&Claim> {
		match self {
			Self::Held(held) | Self::Renewing { prior: held, .. } => Some(&held.claim),
			_ => None,
		}
	}
	pub(super) fn target(&self) -> Option<&LeaseCleanupTarget> {
		match self {
			Self::Acquiring(target)
			| Self::Renewing { target, .. }
			| Self::ConfirmingAcquisition { target, .. }
			| Self::ConfirmingRenewal { target, .. }
			| Self::FinalPurpose { target, .. } => Some(target),
			Self::Held(held) => Some(&held.target),
			_ => None,
		}
	}
	pub(super) fn begin_renewal(&mut self, proposed: &Token) {
		let Self::Held(prior) = std::mem::replace(self, Self::Finished) else {
			unreachable!("renewal consumes the currently held private claim");
		};
		let target = LeaseCleanupTarget::rotation(&prior.claim, proposed);
		*self = Self::Renewing { prior, target };
	}
	pub(super) fn retain_candidate(&mut self, claim: Claim, requested_at: Instant) {
		*self = match std::mem::replace(self, Self::Finished) {
			Self::Acquiring(target) => Self::ConfirmingAcquisition {
				claim,
				requested_at,
				target,
			},
			Self::Renewing { prior, target } => Self::ConfirmingRenewal {
				claim,
				requested_at,
				prior,
				target,
			},
			_ => unreachable!("a committed candidate belongs to its acquisition or renewal"),
		};
	}
	pub(super) fn candidate(&self) -> (&Claim, Instant) {
		match self {
			Self::ConfirmingAcquisition {
				claim,
				requested_at,
				..
			}
			| Self::ConfirmingRenewal {
				claim,
				requested_at,
				..
			} => (claim, *requested_at),
			_ => unreachable!("local confirmation requires the committed candidate"),
		}
	}
	pub(super) fn confirm(&mut self, until: Instant) {
		let claim = match std::mem::replace(self, Self::Finished) {
			Self::ConfirmingAcquisition { claim, .. } | Self::ConfirmingRenewal { claim, .. } => {
				claim
			}
			_ => unreachable!("confirmation consumes its retained candidate"),
		};
		*self = Self::Held(HeldLease {
			target: LeaseCleanupTarget::confirmed(&claim),
			claim,
			until,
		});
	}
	pub(super) fn restore_unchanged_renewal(&mut self) {
		match std::mem::replace(self, Self::Finished) {
			Self::Held(held) | Self::Renewing { prior: held, .. } => *self = Self::Held(held),
			_ => unreachable!("an unchanged failed renewal retains its original claim"),
		}
	}
	pub(super) fn permits_final_purpose(&self) -> bool {
		matches!(self, Self::Held(_))
	}
	pub(super) fn transfer_to_final_purpose(&mut self, origin: LeaseFinalPurposeOrigin) -> Claim {
		let Self::Held(HeldLease {
			claim,
			until,
			target,
		}) = std::mem::replace(self, Self::Finished)
		else {
			unreachable!("one held claim transfers to one final purpose");
		};
		*self = Self::FinalPurpose {
			until,
			target,
			origin,
		};
		claim
	}
	pub(super) fn final_purpose(&self) -> Option<&LeaseFinalPurposeOrigin> {
		match self {
			Self::FinalPurpose { origin, .. } => Some(origin),
			_ => None,
		}
	}
	pub(super) fn completion(&self) -> Option<&LeaseCommittedCompletion> {
		match self {
			Self::Committed(completion) => Some(completion),
			_ => None,
		}
	}
}

pub(super) enum RenewalSchedule {
	AwaitingClaim,
	Scheduled { next: Instant, failures: u32 },
	Stopped,
}

impl RenewalSchedule {
	pub(super) fn next(&self) -> Option<Instant> {
		match self {
			Self::Scheduled { next, .. } => Some(*next),
			_ => None,
		}
	}
	pub(super) fn record_failure(&mut self) -> u32 {
		let Self::Scheduled { failures, .. } = self else {
			unreachable!("automatic renewal requires its actual schedule");
		};
		*failures += 1;
		*failures
	}
	pub(super) fn reschedule(&mut self, next: Instant) {
		let Self::Scheduled { next: retained, .. } = self else {
			unreachable!("renewal retry retains its original schedule and failure count");
		};
		*retained = next;
	}
}

pub(super) enum RenewalAttemptEnding {
	Unobserved,
	Observed(Arc<crate::db::transaction_ending::TransactionEndingResult>),
}

impl RenewalAttemptEnding {
	pub(super) fn commit_was_not_attempted(&self) -> bool {
		matches!(self, Self::Observed(ending) if !ending.commit_attempted())
	}
}
