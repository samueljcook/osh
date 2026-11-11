//! Exact private acquisition modes and their actual reservation material.

use super::{AcquisitionCapability, HolderId, ThrottlerKey, outcome::ThrottlerOutcomeId};

#[derive(Clone)]
pub(super) struct ThrottlerAcquisitionPredicates {
	pub(super) key: ThrottlerKey,
	pub(super) authority: AcquisitionAuthority,
}

#[derive(Clone)]
pub(super) struct PrivateAcquisitionIdentity {
	pub(super) holder: HolderId,
	pub(super) capability: AcquisitionCapability,
}

#[derive(Clone)]
pub(super) enum AcquisitionAuthority {
	RateOnly,
	Concurrency {
		identity: PrivateAcquisitionIdentity,
		slot: u16,
	},
	OrdinaryCircuit {
		identity: PrivateAcquisitionIdentity,
		slot: Option<u16>,
		epoch: AcquisitionCapability,
		outcome_id: ThrottlerOutcomeId,
	},
	Probe {
		identity: PrivateAcquisitionIdentity,
		slot: Option<u16>,
		epoch: AcquisitionCapability,
	},
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum NativeThrottlerReservationKind {
	Unreserved,
	Slot,
	Probe,
	SlotAndProbe,
}

impl ThrottlerAcquisitionPredicates {
	fn identity(&self) -> Option<&PrivateAcquisitionIdentity> {
		match &self.authority {
			AcquisitionAuthority::RateOnly => None,
			AcquisitionAuthority::Concurrency { identity, .. }
			| AcquisitionAuthority::OrdinaryCircuit { identity, .. }
			| AcquisitionAuthority::Probe { identity, .. } => Some(identity),
		}
	}
	pub(super) fn holder(&self) -> Option<&HolderId> {
		self.identity().map(|identity| &identity.holder)
	}
	pub(super) fn capability(&self) -> Option<&AcquisitionCapability> {
		self.identity().map(|identity| &identity.capability)
	}
	pub(super) fn epoch(&self) -> Option<&AcquisitionCapability> {
		match &self.authority {
			AcquisitionAuthority::OrdinaryCircuit { epoch, .. }
			| AcquisitionAuthority::Probe { epoch, .. } => Some(epoch),
			_ => None,
		}
	}
	pub(super) fn slot(&self) -> Option<u16> {
		match &self.authority {
			AcquisitionAuthority::RateOnly => None,
			AcquisitionAuthority::Concurrency { slot, .. } => Some(*slot),
			AcquisitionAuthority::OrdinaryCircuit { slot, .. }
			| AcquisitionAuthority::Probe { slot, .. } => *slot,
		}
	}
	pub(super) fn is_probe(&self) -> bool {
		matches!(self.authority, AcquisitionAuthority::Probe { .. })
	}
	pub(super) fn outcome_id(&self) -> Option<&ThrottlerOutcomeId> {
		match &self.authority {
			AcquisitionAuthority::OrdinaryCircuit { outcome_id, .. } => Some(outcome_id),
			_ => None,
		}
	}
	pub(super) fn reservation_kind(&self) -> NativeThrottlerReservationKind {
		match (self.slot(), self.is_probe()) {
			(None, false) => NativeThrottlerReservationKind::Unreserved,
			(Some(_), false) => NativeThrottlerReservationKind::Slot,
			(None, true) => NativeThrottlerReservationKind::Probe,
			(Some(_), true) => NativeThrottlerReservationKind::SlotAndProbe,
		}
	}
}
