//! Read-only release accounting from the same Lease task and runtime membership.

use super::{
	Arc, DriverShared, Instant, NativeLeaseDriverSnapshot, NativeLeaseDriverState,
	NativeOwnedLeaseClient, sleep_until,
};
use crate::runtime::local_admission::lease_work::RuntimeLeaseWorkSnapshot;
use crate::runtime::local_admission::membership::RuntimeResourceWorkState;
use std::time::Duration;

#[derive(Clone, Debug)]
pub(crate) enum NativeLeaseReleaseAccounting {
	Pending,
	Released,
	NoMatch,
	NotAcquired,
	Unresolved,
}

#[derive(Clone, Debug)]
pub(crate) struct NativeLeaseCloseSnapshot {
	pub(crate) driver: NativeLeaseDriverSnapshot,
	/// Task join and local counts are separate from the database release result.
	pub(crate) work: Option<RuntimeLeaseWorkSnapshot>,
}

/// A clone observes the existing owner and grants no caller scope, Claim or SQL permission.
#[derive(Clone)]
pub(crate) struct NativeLeaseCloseObserver {
	pub(super) shared: Arc<DriverShared>,
}
impl NativeLeaseCloseObserver {
	pub(super) fn new(shared: &Arc<DriverShared>) -> Self {
		Self {
			shared: Arc::clone(shared),
		}
	}
	pub(crate) fn snapshot(&self) -> NativeLeaseCloseSnapshot {
		let driver = self.shared.snapshot();
		let runtime = self.shared.runtime.work_view(self.shared.work_id);
		NativeLeaseCloseSnapshot {
			driver,
			work: runtime.work,
		}
	}
	pub(crate) async fn wait(
		&self,
		wait: &NativeLeaseCloseWait,
	) -> Result<NativeLeaseCloseWaitResult, NativeLeaseCloseWaitError> {
		let deadline = Instant::now()
			.checked_add(wait.duration)
			.ok_or(NativeLeaseCloseWaitError::LocalClockDomain)?;
		let mut changes = self.shared.changed.subscribe();
		loop {
			let runtime = self.shared.runtime.work_view(self.shared.work_id);
			let snapshot = self.snapshot();
			if matches!(
				snapshot.driver.state,
				NativeLeaseDriverState::Finished | NativeLeaseDriverState::Unresolved
			) || snapshot
				.work
				.as_ref()
				.is_some_and(|entry| entry.state == RuntimeResourceWorkState::Abandoned)
			{
				return Ok(NativeLeaseCloseWaitResult::Settled(Box::new(snapshot)));
			}
			if Instant::now() >= deadline {
				return Ok(NativeLeaseCloseWaitResult::Pending);
			}
			tokio::select! {
			 _ = changes.changed() => {},
			 _ = self.shared.runtime.wait_for_work_change(self.shared.work_id, &runtime) => {},
			 _ = sleep_until(deadline) => {},
			}
		}
	}
}

#[derive(Clone)]
pub(crate) struct NativeLeaseCloseWait {
	duration: Duration,
}
#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeLeaseCloseWaitError {
	#[error("lease close observation wait exceeds the local monotonic clock domain")]
	LocalClockDomain,
}
impl NativeLeaseCloseWait {
	/// Waits as long as one complete release attempt can take. A release that needs more
	/// attempts continues after the wait ends.
	pub(crate) fn one_release_attempt(
		timing: &crate::fleet::lease::driver_timing::LeaseControlTiming,
	) -> Self {
		Self {
			duration: timing.release_attempt_timeout,
		}
	}
}
pub(crate) enum NativeLeaseCloseWaitResult {
	Settled(Box<NativeLeaseCloseSnapshot>),
	Pending,
}
impl NativeOwnedLeaseClient {
	pub(crate) fn close(self) -> NativeLeaseCloseObserver {
		let observer = NativeLeaseCloseObserver::new(&self.shared);
		self.shared.close();
		observer
	}
	pub(crate) fn close_observer(&self) -> NativeLeaseCloseObserver {
		NativeLeaseCloseObserver::new(&self.shared)
	}
}
