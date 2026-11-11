//! Read-only progress and explicit stop notification for a retained upgrade owner.

use std::{
	fmt,
	sync::{Arc, Mutex},
	time::Duration,
};
use tokio::sync::Notify;

use crate::db::transaction_ending::TransactionEndingReceipt;
use crate::upgrade::wait::{deadline_after_positive_wait, tokio_instant};
use crate::upgrade::work::UpgradeWork;

use super::super::selection::{NativeUpgradePrerequisiteError, PgUpgradeStep};
use super::error::NativeUpgradeDriverError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// The phase of an upgrade task in this process. The last seven phases are final: the task has
/// ended.
#[non_exhaustive]
pub enum PgUpgradePhase {
	/// The task is starting or creating the upgrade's record.
	Starting,
	/// The task is reading the upgrade's record.
	Observing,
	/// The task is running a step or changing the upgrade's record.
	Executing,
	/// The task is waiting for a transaction to end.
	WaitingForEnding,
	/// The task is waiting until a prerequisite holds, for example until an older deployment
	/// stops, or until a step that failed with a transient database error or exceeded its time
	/// limit can run again. The task then tries again.
	WaitingForCompatibility,
	/// The task is finding out whether an earlier transaction with an unknown result
	/// committed.
	Recovering,
	/// The task is finishing its current work before it ends.
	Stopping,
	/// The upgrade completed.
	Completed,
	/// The upgrade was cancelled.
	Cancelled,
	/// The task stopped on request. The upgrade can still be unfinished, and
	/// `PgRuntime::resume_upgrade` can continue it.
	Stopped,
	/// The task read the upgrade's record and ended, as `PgRuntime::inspect_upgrade` requests.
	/// The upgrade can still be unfinished.
	Inspected,
	/// The task ended because the next step cannot run until the conditions in the snapshot's
	/// prerequisites hold. If the task had recorded the upgrade, `PgRuntime::resume_upgrade`
	/// can continue it once they hold.
	PrerequisitesUnmet,
	/// The task ended because of the failure that the snapshot's `failure` describes, for
	/// example a step that failed, or a recorded upgrade that this runtime cannot read. If the
	/// task had recorded the upgrade, `PgRuntime::resume_upgrade` can try it again.
	Failed,
	/// The task ended while the result of one of its transactions was unknown, or after its
	/// final cleanup failed. The snapshot's failure describes the cause. If the task had
	/// recorded the upgrade, `PgRuntime::resume_upgrade` can continue it.
	Unresolved,
}

impl PgUpgradePhase {
	pub(super) fn is_terminal(self) -> bool {
		matches!(
			self,
			Self::Completed
				| Self::Cancelled
				| Self::Stopped
				| Self::Inspected
				| Self::PrerequisitesUnmet
				| Self::Failed
				| Self::Unresolved
		)
	}
}

#[derive(Clone)]
pub(crate) struct NativeUpgradeDriverSnapshot {
	pub(crate) phase: PgUpgradePhase,
	pub(crate) work: Option<Arc<UpgradeWork>>,
	pub(crate) step: Option<PgUpgradeStep>,
	pub(crate) prerequisites: Arc<Vec<NativeUpgradePrerequisiteError>>,
	pub(crate) ending: Option<TransactionEndingReceipt>,
	pub(crate) failure: Option<Arc<NativeUpgradeDriverError>>,
}

impl fmt::Debug for NativeUpgradeDriverSnapshot {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("NativeUpgradeDriverSnapshot")
			.field("phase", &self.phase)
			.field("has_work", &self.work.is_some())
			.field("step", &self.step)
			.field("prerequisite_count", &self.prerequisites.len())
			.field("has_ending", &self.ending.is_some())
			.field("has_failure", &self.failure.is_some())
			.finish_non_exhaustive()
	}
}

pub(super) struct DriverShared {
	state: Mutex<DriverSharedState>,
	pub(super) stopped: Notify,
	changed: Notify,
}

struct DriverSharedState {
	snapshot: NativeUpgradeDriverSnapshot,
	stop: StopRequest,
	cancel: CancelRequest,
}

enum StopRequest {
	Running,
	Requested,
}

enum CancelRequest {
	Idle,
	Requested,
}

impl DriverShared {
	pub(super) fn new(snapshot: NativeUpgradeDriverSnapshot) -> Arc<Self> {
		Arc::new(Self {
			state: Mutex::new(DriverSharedState {
				snapshot,
				stop: StopRequest::Running,
				cancel: CancelRequest::Idle,
			}),
			stopped: Notify::new(),
			changed: Notify::new(),
		})
	}
	pub(super) fn update(&self, update_fn: impl FnOnce(&mut NativeUpgradeDriverSnapshot)) {
		update_fn(
			&mut self
				.state
				.lock()
				.expect("upgrade diagnostics contain no caller code")
				.snapshot,
		);
		self.changed.notify_waiters();
	}
	pub(super) fn stop_requested(&self) -> bool {
		matches!(
			self.state
				.lock()
				.expect("upgrade requests contain no caller code")
				.stop,
			StopRequest::Requested
		)
	}
	pub(super) fn cancel_requested(&self) -> bool {
		matches!(
			self.state
				.lock()
				.expect("upgrade requests contain no caller code")
				.cancel,
			CancelRequest::Requested
		)
	}
	pub(super) fn take_cancel_request(&self) -> bool {
		let mut state = self
			.state
			.lock()
			.expect("upgrade requests contain no caller code");
		matches!(
			std::mem::replace(&mut state.cancel, CancelRequest::Idle),
			CancelRequest::Requested
		)
	}
	pub(super) fn snapshot(&self) -> NativeUpgradeDriverSnapshot {
		self.state
			.lock()
			.expect("upgrade diagnostics contain no caller code")
			.snapshot
			.clone()
	}
}

/// Dropping this observer changes no membership or task ownership.
#[derive(Clone)]
pub(crate) struct NativeUpgradeDriverObserver {
	pub(super) shared: Arc<DriverShared>,
}

impl NativeUpgradeDriverObserver {
	pub(crate) fn snapshot(&self) -> NativeUpgradeDriverSnapshot {
		self.shared.snapshot()
	}
	pub(crate) fn request_stop(&self) {
		self.shared
			.state
			.lock()
			.expect("upgrade requests contain no caller code")
			.stop = StopRequest::Requested;
		self.shared.stopped.notify_waiters();
	}
	/// Requests exact durable cancellation through this same owner's next settled dispatch.
	/// The actual terminal observation determines whether cancellation or another outcome won.
	pub(in crate::upgrade) fn request_cancel(&self) {
		self.shared
			.state
			.lock()
			.expect("upgrade requests contain no caller code")
			.cancel = CancelRequest::Requested;
		self.shared.stopped.notify_waiters();
	}
	/// Returns an actual accepted Work observation without issuing another database operation.
	/// Cancelling this wait leaves the registered upgrade task and its owner intact.
	pub(in crate::upgrade) async fn wait_until_accepted(
		&self,
		wait_timeout: Duration,
	) -> Result<Arc<UpgradeWork>, NativeUpgradeDriverWaitError> {
		let deadline = deadline_after_positive_wait(
			wait_timeout,
			NativeUpgradeDriverWaitError::InvalidWaitTimeout,
		)?;
		tokio::time::timeout_at(tokio_instant(deadline), async {
			loop {
				let changed = self.shared.changed.notified();
				tokio::pin!(changed);
				changed.as_mut().enable();
				let snapshot = self.snapshot();
				if let Some(work) = &snapshot.work {
					return Ok(Arc::clone(work));
				}
				if snapshot.phase.is_terminal() {
					return Err(NativeUpgradeDriverWaitError::NotAccepted {
						snapshot: Box::new(snapshot),
					});
				}
				changed.await;
			}
		})
		.await
		.map_err(|_| NativeUpgradeDriverWaitError::Pending {
			snapshot: Box::new(self.snapshot()),
		})?
	}
	/// Observes local driver completion, without claiming physical server drainage.
	pub(crate) async fn wait_until_finished(
		&self,
		wait_timeout: Duration,
	) -> Result<NativeUpgradeDriverSnapshot, NativeUpgradeDriverWaitError> {
		let deadline = deadline_after_positive_wait(
			wait_timeout,
			NativeUpgradeDriverWaitError::InvalidWaitTimeout,
		)?;
		tokio::time::timeout_at(tokio_instant(deadline), async {
			loop {
				let changed = self.shared.changed.notified();
				tokio::pin!(changed);
				changed.as_mut().enable();
				let snapshot = self.snapshot();
				if snapshot.phase.is_terminal() {
					return snapshot;
				}
				changed.await;
			}
		})
		.await
		.map_err(|_| NativeUpgradeDriverWaitError::Pending {
			snapshot: Box::new(self.snapshot()),
		})
	}
}

impl fmt::Debug for NativeUpgradeDriverObserver {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		self.snapshot().fmt(f)
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeUpgradeDriverWaitError {
	#[error("upgrade observer wait must be positive and fit the local clock")]
	InvalidWaitTimeout,
	#[error("upgrade local continuation remains pending at the observer deadline")]
	Pending {
		snapshot: Box<NativeUpgradeDriverSnapshot>,
	},
	#[error("upgrade local continuation ended before accepting the requested Work")]
	NotAccepted {
		snapshot: Box<NativeUpgradeDriverSnapshot>,
	},
}
