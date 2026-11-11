//! Exact release-only material derived from an actual tentative acquisition, never stored diagnostics.

use super::{
	AcquisitionCapability, HolderId, SemaphoreAcquisition, SemaphoreClaim, SemaphoreKey,
	StateTimestampMicros,
};

/// Decomposition separates the sole cleanup target from the still-private tentative grant.
/// An owning registered executor must establish known COMMIT before client delivery.
pub(crate) struct TentativeSemaphoreGrant {
	acquisition: SemaphoreAcquisition,
}
impl TentativeSemaphoreGrant {
	pub(crate) fn into_claim_after_known_commit(self) -> SemaphoreClaim {
		self.acquisition.claim
	}
}
impl std::fmt::Debug for TentativeSemaphoreGrant {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("TentativeSemaphoreGrant { .. }")
	}
}

/// No Clone, serialization, claim extraction, acquisition or extension operation is provided.
pub(crate) struct SemaphoreUndeliveredTarget {
	key: SemaphoreKey,
	slot_suffix: String,
	holder: HolderId,
	capability: AcquisitionCapability,
}
impl std::fmt::Debug for SemaphoreUndeliveredTarget {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("SemaphoreUndeliveredTarget { .. }")
	}
}
pub(super) fn decompose(
	acquisition: SemaphoreAcquisition,
) -> (TentativeSemaphoreGrant, SemaphoreUndeliveredTarget) {
	let target = SemaphoreUndeliveredTarget {
		key: acquisition.claim.semaphore_key.clone(),
		slot_suffix: acquisition.claim.slot_suffix.clone(),
		holder: acquisition.claim.holder_id.clone(),
		capability: acquisition.claim.acquisition_capability.clone(),
	};
	(TentativeSemaphoreGrant { acquisition }, target)
}

pub(crate) struct SemaphoreUndeliveredReleaseInput<'a> {
	pub(crate) target: &'a SemaphoreUndeliveredTarget,
	pub(crate) observed_at: StateTimestampMicros,
}

pub(super) enum SemaphoreReleaseAuthorization<'a> {
	Claimed(&'a SemaphoreClaim),
	Undelivered(&'a SemaphoreUndeliveredTarget),
}
pub(super) struct SemaphoreReleasePredicates<'a> {
	pub(super) key: &'a SemaphoreKey,
	pub(super) slot_suffix: &'a str,
	pub(super) holder: &'a HolderId,
	pub(super) capability: &'a AcquisitionCapability,
}
impl<'a> SemaphoreReleaseAuthorization<'a> {
	pub(super) fn predicates(self) -> SemaphoreReleasePredicates<'a> {
		match self {
			Self::Claimed(claim) => SemaphoreReleasePredicates {
				key: &claim.semaphore_key,
				slot_suffix: &claim.slot_suffix,
				holder: &claim.holder_id,
				capability: &claim.acquisition_capability,
			},
			Self::Undelivered(target) => SemaphoreReleasePredicates {
				key: &target.key,
				slot_suffix: &target.slot_suffix,
				holder: &target.holder,
				capability: &target.capability,
			},
		}
	}
}
