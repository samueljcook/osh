//! Observations of the work that a runtime runs for its handles and upgrades.

use crate::runtime::local_admission::membership::RuntimeResourceWorkState;
use std::{error::Error, fmt};

use tokio::task::JoinError;

use crate::fleet::{SemaphoreKey, ThrottlerKey};
use crate::runtime::local_admission::diagnostic::RuntimeWorkTerminalError;
use crate::runtime::local_admission::lease_work::RuntimeLeaseWorkSnapshot;
use crate::runtime::local_admission::semaphore_work::RuntimeSemaphoreWorkSnapshot;
use crate::runtime::local_admission::task::RuntimeTaskJoinError;
use crate::runtime::local_admission::throttler_work::RuntimeThrottlerWorkSnapshot;
use crate::runtime::local_admission::upgrade_work::{
	RuntimeUpgradeWorkSnapshot, RuntimeUpgradeWorkState,
};
use crate::scope::ScopeIdentity;

#[derive(Clone, Copy)]
enum WorkView<'a> {
	Lease(&'a RuntimeLeaseWorkSnapshot),
	Upgrade(&'a RuntimeUpgradeWorkSnapshot),
	Throttler(&'a RuntimeThrottlerWorkSnapshot),
	Semaphore(&'a RuntimeSemaphoreWorkSnapshot),
}

/// One piece of work that the runtime runs in this process: a lease, an upgrade task, a
/// Throttler run, or a Semaphore run.
#[derive(Clone, Copy)]
pub struct PgRuntimeWorkSnapshot<'a>(WorkView<'a>);

impl<'a> PgRuntimeWorkSnapshot<'a> {
	pub(crate) fn lease(snapshot: &'a RuntimeLeaseWorkSnapshot) -> Self {
		Self(WorkView::Lease(snapshot))
	}
	pub(crate) fn upgrade(snapshot: &'a RuntimeUpgradeWorkSnapshot) -> Self {
		Self(WorkView::Upgrade(snapshot))
	}
	pub(crate) fn throttler(snapshot: &'a RuntimeThrottlerWorkSnapshot) -> Self {
		Self(WorkView::Throttler(snapshot))
	}
	pub(crate) fn semaphore(snapshot: &'a RuntimeSemaphoreWorkSnapshot) -> Self {
		Self(WorkView::Semaphore(snapshot))
	}

	/// The kind of work.
	pub fn kind(&self) -> PgRuntimeWorkKind {
		match self.0 {
			WorkView::Lease(_) => PgRuntimeWorkKind::Lease,
			WorkView::Upgrade(_) => PgRuntimeWorkKind::Upgrade,
			WorkView::Throttler(_) => PgRuntimeWorkKind::Throttler,
			WorkView::Semaphore(_) => PgRuntimeWorkKind::Semaphore,
		}
	}

	/// The state of the work when the snapshot was taken.
	pub fn state(&self) -> PgRuntimeWorkState {
		use PgRuntimeWorkState as State;
		match self.0 {
			WorkView::Lease(snapshot) => match snapshot.state {
				RuntimeResourceWorkState::Acquiring => State::Acquiring,
				RuntimeResourceWorkState::Held => State::Held,
				RuntimeResourceWorkState::Closing => State::Closing,
				RuntimeResourceWorkState::Finished => State::Finished,
				RuntimeResourceWorkState::Abandoned => State::Abandoned,
				RuntimeResourceWorkState::Unresolved => State::Unresolved,
			},
			WorkView::Upgrade(snapshot) => match snapshot.state {
				RuntimeUpgradeWorkState::Running => State::Running,
				RuntimeUpgradeWorkState::Stopping => State::Stopping,
				RuntimeUpgradeWorkState::Finished => State::Finished,
				RuntimeUpgradeWorkState::Abandoned => State::Abandoned,
				RuntimeUpgradeWorkState::Unresolved => State::Unresolved,
			},
			WorkView::Throttler(snapshot) => match snapshot.state {
				RuntimeResourceWorkState::Acquiring => State::Acquiring,
				RuntimeResourceWorkState::Held => State::Held,
				RuntimeResourceWorkState::Closing => State::Closing,
				RuntimeResourceWorkState::Finished => State::Finished,
				RuntimeResourceWorkState::Abandoned => State::Abandoned,
				RuntimeResourceWorkState::Unresolved => State::Unresolved,
			},
			WorkView::Semaphore(snapshot) => match snapshot.state {
				RuntimeResourceWorkState::Acquiring => State::Acquiring,
				RuntimeResourceWorkState::Held => State::Held,
				RuntimeResourceWorkState::Closing => State::Closing,
				RuntimeResourceWorkState::Finished => State::Finished,
				RuntimeResourceWorkState::Abandoned => State::Abandoned,
				RuntimeResourceWorkState::Unresolved => State::Unresolved,
			},
		}
	}

	/// Whether the work is still pending: its state is neither `Finished` nor `Unresolved`, its
	/// task is starting, running, or being joined, or a caller still uses it. `Abandoned` work
	/// is pending. `Unresolved` work is not pending after its task and callers have ended, although
	/// its effect in the database is unknown.
	pub fn is_pending(&self) -> bool {
		match self.0 {
			WorkView::Lease(snapshot) => snapshot.is_pending(),
			WorkView::Upgrade(snapshot) => snapshot.is_pending(),
			WorkView::Throttler(snapshot) => snapshot.is_pending(),
			WorkView::Semaphore(snapshot) => snapshot.is_pending(),
		}
	}

	/// The number of caller tasks that still use the work, or `None` for an upgrade, which has
	/// no callers.
	pub fn pending_caller_work_count(&self) -> Option<usize> {
		match self.0 {
			WorkView::Lease(snapshot) => Some(snapshot.pending_caller_work_count),
			WorkView::Upgrade(_) => None,
			WorkView::Throttler(snapshot) => Some(snapshot.pending_caller_work_count),
			WorkView::Semaphore(snapshot) => Some(snapshot.pending_caller_work_count),
		}
	}

	/// The scope of a Semaphore or Throttler.
	pub fn scope(&self) -> Option<&'a ScopeIdentity> {
		match self.0 {
			WorkView::Throttler(snapshot) => Some(snapshot.identity.scope()),
			WorkView::Semaphore(snapshot) => Some(snapshot.identity.scope()),
			WorkView::Lease(_) | WorkView::Upgrade(_) => None,
		}
	}

	/// The Semaphore's key, for Semaphore work.
	pub fn semaphore_key(&self) -> Option<&'a SemaphoreKey> {
		match self.0 {
			WorkView::Semaphore(snapshot) => Some(snapshot.identity.resource()),
			_ => None,
		}
	}

	/// The Throttler's key, for Throttler work.
	pub fn throttler_key(&self) -> Option<&'a ThrottlerKey> {
		match self.0 {
			WorkView::Throttler(snapshot) => Some(snapshot.identity.resource()),
			_ => None,
		}
	}

	/// Why the work ended abnormally, if it did, including work with an unresolved outcome.
	pub fn terminal_diagnostic(&self) -> Option<PgRuntimeWorkDiagnostic<'a>> {
		let diagnostic = match self.0 {
			WorkView::Lease(snapshot) => snapshot.terminal_diagnostic.as_ref(),
			WorkView::Upgrade(snapshot) => snapshot.terminal_diagnostic.as_ref(),
			WorkView::Throttler(snapshot) => snapshot.terminal_diagnostic.as_ref(),
			WorkView::Semaphore(snapshot) => snapshot.terminal_diagnostic.as_ref(),
		};
		diagnostic.map(PgRuntimeWorkDiagnostic)
	}
}

impl fmt::Debug for PgRuntimeWorkSnapshot<'_> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("PgRuntimeWorkSnapshot { .. }")
	}
}

/// The kind of a piece of runtime work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgRuntimeWorkKind {
	/// A lease that the runtime holds.
	Lease,
	/// An upgrade task.
	Upgrade,
	/// A Throttler run.
	Throttler,
	/// A Semaphore run.
	Semaphore,
}

/// The state of a piece of runtime work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PgRuntimeWorkState {
	/// A lease, Semaphore slot, or Throttler slot is being acquired.
	Acquiring,
	/// A lease, Semaphore slot, or Throttler slot is held.
	Held,
	/// A lease, Semaphore slot, or Throttler slot is being released, or its outcome is being
	/// recorded.
	Closing,
	/// An upgrade task is running.
	Running,
	/// An upgrade task is stopping.
	Stopping,
	/// The work ended. Its task can still be finishing.
	Finished,
	/// The work's task stopped before the work ended, so the runtime cannot confirm that it
	/// finished its database work.
	Abandoned,
	/// The work ended, and its effect in the database is unknown, for example because a
	/// `COMMIT` result was lost.
	Unresolved,
}

/// Why a piece of runtime work ended abnormally.
#[derive(Clone, Copy)]
pub struct PgRuntimeWorkDiagnostic<'a>(&'a RuntimeWorkTerminalError);

impl<'a> PgRuntimeWorkDiagnostic<'a> {
	/// The cause as an error, with its complete source chain.
	pub fn error(&self) -> &'a (dyn Error + Send + Sync + 'static) {
		self.0.error()
	}

	/// The Tokio join error, when the work's task panicked or was cancelled.
	pub fn task_join_error(&self) -> Option<&'a JoinError> {
		self.error()
			.downcast_ref::<RuntimeTaskJoinError>()
			.map(RuntimeTaskJoinError::join_error)
	}

	/// The failure that came before the task's panic or cancellation, if any.
	pub fn prior(&self) -> Option<Self> {
		self.error()
			.downcast_ref::<RuntimeTaskJoinError>()
			.and_then(RuntimeTaskJoinError::prior_error)
			.map(Self)
	}
}

impl fmt::Debug for PgRuntimeWorkDiagnostic<'_> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("PgRuntimeWorkDiagnostic")?;
		let mut source: Option<&(dyn Error + 'static)> = Some(self.error());
		while let Some(error) = source {
			match error.downcast_ref::<JoinError>() {
				// A panic message can contain application data.
				Some(join) if join.is_panic() => formatter.write_str(": the task panicked")?,
				Some(_) => formatter.write_str(": the task was cancelled")?,
				None => write!(formatter, ": {error}")?,
			}
			source = error.source();
		}
		Ok(())
	}
}
