use crate::fleet::{AcquisitionCapability, HolderId, SemaphoreKey};

/// Live Fleet semaphore claim.
#[derive(Debug, Eq, PartialEq)]
#[must_use = "only the claim can release its slot; without it, the slot stays held until its hold window ends"]
pub struct SemaphoreClaim {
	pub(super) semaphore_key: SemaphoreKey,
	pub(super) slot_suffix: String,
	pub(super) holder_id: HolderId,
	pub(super) acquisition_capability: AcquisitionCapability,
}

/// A cloneable copy of a live semaphore claim's description. It cannot release the claim.
///
/// A guarded task receives this copy instead of the claim so that the task cannot release the
/// slot while it is still running.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemaphoreClaimSnapshot {
	pub(super) semaphore_key: SemaphoreKey,
	pub(super) slot_suffix: String,
	pub(super) holder_id: HolderId,
}

/// Current Fleet semaphore status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemaphoreStatus {
	pub(super) current_count: u64,
	pub(super) capacity: u16,
}

impl SemaphoreClaim {
	pub(super) fn snapshot(&self) -> SemaphoreClaimSnapshot {
		SemaphoreClaimSnapshot {
			semaphore_key: self.semaphore_key.clone(),
			slot_suffix: self.slot_suffix.clone(),
			holder_id: self.holder_id.clone(),
		}
	}
}

impl SemaphoreClaimSnapshot {
	/// Returns the semaphore key.
	pub fn semaphore_key(&self) -> &SemaphoreKey {
		&self.semaphore_key
	}

	/// Returns the observed slot suffix.
	pub fn slot_suffix(&self) -> &str {
		&self.slot_suffix
	}

	/// Returns the observable holder identifier.
	pub fn holder_id(&self) -> &HolderId {
		&self.holder_id
	}
}

impl SemaphoreStatus {
	/// Returns the current live slot count.
	pub fn current_count(&self) -> u64 {
		self.current_count
	}

	/// Returns the maximum live slot count.
	pub fn capacity(&self) -> u16 {
		self.capacity
	}
}
