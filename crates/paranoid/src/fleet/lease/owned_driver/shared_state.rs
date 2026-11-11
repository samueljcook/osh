//! Shared Lease authority has one phase owner; snapshots are read-only projections.

use super::{
	Arc, HolderSnapshot, Instant, NativeLeaseDriverError, NativeLeaseDriverSnapshot,
	NativeLeaseDriverState, NativeLeaseLossReason, NativeLeaseReleaseAccounting, OnceLock,
};

pub(super) enum SharedLeasePhase {
	Acquiring,
	Held {
		holder: HolderSnapshot,
		confirmed_until: Instant,
	},
	Closing,
	Finished,
	Unresolved,
}

impl SharedLeasePhase {
	pub(super) fn state(&self) -> NativeLeaseDriverState {
		match self {
			Self::Acquiring => NativeLeaseDriverState::Acquiring,
			Self::Held { .. } => NativeLeaseDriverState::Held,
			Self::Closing => NativeLeaseDriverState::Closing,
			Self::Finished => NativeLeaseDriverState::Finished,
			Self::Unresolved => NativeLeaseDriverState::Unresolved,
		}
	}
	pub(super) fn confirmed_until(&self) -> Option<Instant> {
		match self {
			Self::Held {
				confirmed_until, ..
			} => Some(*confirmed_until),
			_ => None,
		}
	}
	pub(super) fn holder(&self) -> Option<&HolderSnapshot> {
		match self {
			Self::Held { holder, .. } => Some(holder),
			_ => None,
		}
	}
}

pub(super) struct SharedLeaseState {
	pub(super) phase: SharedLeasePhase,
	pub(super) loss: OnceLock<NativeLeaseLossReason>,
	pub(super) release: NativeLeaseReleaseAccounting,
	pub(super) renewal_failures: Vec<Arc<NativeLeaseDriverError>>,
	pub(super) failures: Vec<Arc<NativeLeaseDriverError>>,
}

impl SharedLeaseState {
	pub(super) fn new() -> Self {
		Self {
			phase: SharedLeasePhase::Acquiring,
			loss: OnceLock::new(),
			release: NativeLeaseReleaseAccounting::Pending,
			renewal_failures: Vec::new(),
			failures: Vec::new(),
		}
	}
	pub(super) fn snapshot(&self) -> NativeLeaseDriverSnapshot {
		NativeLeaseDriverSnapshot {
			state: self.phase.state(),
			release: self.release.clone(),
			loss: self.loss.get().cloned(),
			lease_confirmed_until: self.phase.confirmed_until(),
			usable_until: None,
			failures: self.failures.clone(),
		}
	}
}
