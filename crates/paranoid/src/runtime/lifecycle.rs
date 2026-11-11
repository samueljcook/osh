//! Observations of a runtime's lifecycle and of its shutdown.

use std::{error::Error, fmt, sync::Arc};

use super::work::PgRuntimeWorkSnapshot;
use crate::runtime::local_admission::RuntimeAdmissionSnapshot;
use crate::runtime::local_admission::lifecycle::{AdmissionLifecycle, StopReason};
use crate::runtime::participation::{
	NativeParticipationFenceError, NativeParticipationRemovalResult,
	NativeParticipationRenewalCompletionError, NativeParticipationRenewalStatus,
	NativeParticipationRuntimeObserver, NativeParticipationRuntimeSnapshot,
	NativeParticipationShutdown, NativeParticipationShutdownResult,
	NativeParticipationShutdownStatus, NativeParticipationShutdownWaitError,
};

/// An observer of a runtime's lifecycle. Keeping an observer does not keep the runtime
/// running.
#[derive(Clone)]
pub struct PgRuntimeObserver {
	pub(super) observer: NativeParticipationRuntimeObserver,
}

impl PgRuntimeObserver {
	/// The runtime's current state.
	pub fn snapshot(&self) -> PgRuntimeSnapshot {
		PgRuntimeSnapshot(self.observer.snapshot())
	}

	/// Waits until the runtime's renewal of its participation ends, and returns how it ended.
	/// It returns at once when renewal has already ended. The wait performs no database work.
	pub async fn wait_until_renewal_ends(&self) -> PgRuntimeRenewalEnd {
		match self.observer.wait_until_renewal_ends().await {
			NativeParticipationRenewalStatus::Running { .. } => {
				unreachable!("the wait returns only after renewal has ended")
			}
			NativeParticipationRenewalStatus::Stopped => PgRuntimeRenewalEnd::Stopped,
			NativeParticipationRenewalStatus::Fenced(_) => PgRuntimeRenewalEnd::ParticipationLost,
		}
	}
}

/// A runtime's state in this process when the snapshot was taken. The counts describe this
/// process; they do not show that the database has finished the runtime's work.
#[derive(Clone)]
pub struct PgRuntimeSnapshot(pub(super) NativeParticipationRuntimeSnapshot);

impl PgRuntimeSnapshot {
	/// The runtime's work, transactions, and admission of operations in this process.
	pub fn local_work(&self) -> PgRuntimeLocalSnapshot<'_> {
		PgRuntimeLocalSnapshot(&self.0.admission)
	}

	/// The stage of the runtime's shutdown.
	pub fn shutdown(&self) -> PgRuntimeShutdownState {
		match &self.0.shutdown {
			NativeParticipationShutdownStatus::NotStarted => PgRuntimeShutdownState::NotStarted,
			NativeParticipationShutdownStatus::Running => PgRuntimeShutdownState::Running,
			NativeParticipationShutdownStatus::Finished(outcome) => {
				PgRuntimeShutdownState::Finished(PgRuntimeShutdownResult(Arc::clone(outcome)))
			}
		}
	}

	/// The state of the runtime's renewal of its participation in the installation.
	pub fn renewal_status(&self) -> PgRuntimeRenewalStatus {
		match &self.0.renewal {
			NativeParticipationRenewalStatus::Running { .. } => PgRuntimeRenewalStatus::Running,
			NativeParticipationRenewalStatus::Stopped => PgRuntimeRenewalStatus::Stopped,
			NativeParticipationRenewalStatus::Fenced(_) => {
				PgRuntimeRenewalStatus::ParticipationLost
			}
		}
	}

	/// The last renewal failure while renewal runs, or the failure that ended the runtime's
	/// participation.
	pub fn renewal_error(&self) -> Option<&(dyn Error + 'static)> {
		match &self.0.renewal {
			NativeParticipationRenewalStatus::Running { last_failure, .. } => last_failure
				.as_deref()
				.map(|error| error as &(dyn Error + 'static)),
			NativeParticipationRenewalStatus::Stopped => None,
			NativeParticipationRenewalStatus::Fenced(source) => Some(source.as_ref()),
		}
	}
}

/// The stage of a runtime's shutdown.
#[derive(Clone)]
pub enum PgRuntimeShutdownState {
	/// No shutdown has started.
	NotStarted,
	/// The shutdown runs: it waits for the runtime's work and transactions to end, or it
	/// removes the runtime from the installation's participants.
	Running,
	/// The shutdown finished, also when its cleanup failed. The result reports the cleanup.
	Finished(PgRuntimeShutdownResult),
}

/// How a runtime's renewal of its participation in the installation ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PgRuntimeRenewalEnd {
	/// Renewal stopped because the runtime shut down.
	Stopped,
	/// The runtime stopped accepting operations because its participation ended or could no
	/// longer be confirmed. [`PgRuntimeSnapshot::renewal_error`] returns the failure.
	ParticipationLost,
}

/// The state of a runtime's renewal of its participation in the installation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PgRuntimeRenewalStatus {
	/// Renewal runs. The last renewal can have failed; `renewal_error` returns that failure.
	Running,
	/// Renewal stopped because the runtime shut down.
	Stopped,
	/// The runtime stopped accepting operations because its participation ended or could no
	/// longer be confirmed, for example because renewal failed, or because the task of a piece
	/// of the runtime's work stopped before the work ended, which leaves that work `Abandoned`.
	/// `renewal_error` returns the failure.
	ParticipationLost,
}

/// The work, transactions, and admission of operations of a runtime in this process.
#[derive(Clone, Copy)]
pub struct PgRuntimeLocalSnapshot<'a>(&'a RuntimeAdmissionSnapshot);

impl<'a> PgRuntimeLocalSnapshot<'a> {
	/// Every piece of work that the runtime still tracks, including ended work with an
	/// unresolved outcome. The order has no meaning.
	pub fn work(&self) -> impl Iterator<Item = PgRuntimeWorkSnapshot<'a>> + 'a {
		self.0
			.lease_work
			.iter()
			.map(PgRuntimeWorkSnapshot::lease)
			.chain(
				self.0
					.upgrade_work
					.iter()
					.map(PgRuntimeWorkSnapshot::upgrade),
			)
			.chain(
				self.0
					.throttler_work
					.iter()
					.map(PgRuntimeWorkSnapshot::throttler),
			)
			.chain(
				self.0
					.semaphore_work
					.iter()
					.map(PgRuntimeWorkSnapshot::semaphore),
			)
	}

	/// Whether the runtime accepts operations, and why it stopped accepting them, once it has.
	pub fn admission(&self) -> PgRuntimeAdmissionState {
		let stop = |reason| match reason {
			StopReason::Shutdown => PgRuntimeAdmissionStop::Shutdown,
			StopReason::Fenced(_) => PgRuntimeAdmissionStop::ParticipationLost,
		};
		match self.0.lifecycle {
			AdmissionLifecycle::Open => PgRuntimeAdmissionState::Accepting,
			AdmissionLifecycle::Stopped(reason) => PgRuntimeAdmissionState::Stopped(stop(reason)),
			AdmissionLifecycle::Finished(reason) => PgRuntimeAdmissionState::Finished(stop(reason)),
		}
	}

	/// The number of the runtime's transactions that have not ended, including transactions
	/// that the runtime is still waiting to end.
	pub fn pending_transaction_count(&self) -> usize {
		self.0.pending_transaction_count
	}
}

/// Whether a runtime accepts operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PgRuntimeAdmissionState {
	/// The runtime accepts operations.
	Accepting,
	/// The runtime stopped accepting operations, and no shutdown has confirmed yet that every
	/// operation and piece of work of the runtime ended. A transaction count of zero does not
	/// show that by itself.
	Stopped(PgRuntimeAdmissionStop),
	/// The runtime stopped accepting operations, and its shutdown confirmed that every operation
	/// and piece of work of the runtime ended.
	Finished(PgRuntimeAdmissionStop),
}

/// Why a runtime stopped accepting operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PgRuntimeAdmissionStop {
	/// The runtime shut down.
	Shutdown,
	/// The runtime's participation in the installation ended or could no longer be
	/// confirmed, for example because renewal failed, or the task of a piece of the runtime's
	/// work stopped before the work ended, which leaves that work `Abandoned`.
	ParticipationLost,
}

/// A handle to a runtime's shutdown, which runs as its own task.
///
/// Waiting, a wait that times out, and dropping the handle do not cancel the shutdown. The
/// Tokio runtime that started the shutdown must stay alive until the shutdown finishes;
/// keeping this handle does not keep a dropped Tokio runtime running.
#[derive(Clone)]
pub struct PgRuntimeShutdown {
	pub(super) shutdown: NativeParticipationShutdown,
}

impl PgRuntimeShutdown {
	/// The runtime's current state.
	pub fn snapshot(&self) -> PgRuntimeSnapshot {
		PgRuntimeSnapshot(self.shutdown.snapshot())
	}

	/// Waits at most `options.timeout` until the shutdown finishes. When the timeout elapses
	/// first, the shutdown keeps running. The outcome reports renewal and removal failures
	/// separately.
	pub async fn wait_until_finished(
		&self,
		options: crate::runtime::PgWaitOptions,
	) -> Result<PgRuntimeShutdownResult, PgRuntimeShutdownWaitError> {
		self.shutdown
			.wait_until_finished(options.timeout)
			.await
			.map(PgRuntimeShutdownResult)
			.map_err(PgRuntimeShutdownWaitError::new)
	}
}

impl fmt::Debug for PgRuntimeShutdown {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("PgRuntimeShutdown { .. }")
	}
}

/// The outcome of a runtime's shutdown. A finished shutdown can include failed cleanup.
#[derive(Clone)]
pub struct PgRuntimeShutdownResult(pub(super) Arc<NativeParticipationShutdownResult>);

impl PgRuntimeShutdownResult {
	/// Whether the result of the last renewal or of the runtime's removal from the
	/// installation's participants is unknown. Unresolved work appears in `local_work`.
	pub fn commit_outcome_is_unknown(&self) -> bool {
		shutdown_commit_outcome_is_unknown(&self.0)
	}

	/// The runtime's work, transactions, and admission of operations when shutdown finished.
	/// This does not show that the database released every lease or slot.
	pub fn local_work(&self) -> PgRuntimeLocalSnapshot<'_> {
		PgRuntimeLocalSnapshot(&self.0.local_drainage)
	}

	/// The failure that ended renewal, if renewal failed.
	pub fn renewal_error(&self) -> Option<&(dyn Error + 'static)> {
		self.0
			.renewal
			.as_ref()
			.err()
			.map(|error| error as &(dyn Error + 'static))
	}

	/// The result of removing the runtime from the installation's participants.
	pub fn removal(&self) -> PgRuntimeRemovalResult {
		removal_outcome(&self.0.removal)
	}

	/// Whether the result of the runtime's removal from the installation's participants is
	/// unknown. If the removal did not commit, the runtime's participation expires on its
	/// own.
	pub fn removal_outcome_is_unknown(&self) -> bool {
		removal_outcome_is_unknown(&self.0.removal)
	}

	/// The failure of the runtime's removal from the installation's participants, if any.
	pub fn removal_error(&self) -> Option<&(dyn Error + 'static)> {
		removal_error(&self.0.removal)
	}
}

/// The result of removing a runtime from the installation's participants during shutdown.
/// When the runtime was not removed, its participation expires on its own.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PgRuntimeRemovalResult {
	/// The removal committed.
	Removed {
		/// Whether the installation still listed the runtime when the removal ran.
		was_present: bool,
	},
	/// The removal failed; `removal_error` returns the failure.
	Failed,
	/// The removal exceeded its time limit; it may or may not have committed.
	TimedOut,
	/// The runtime could not end all of its operations, so it did not remove itself.
	LocalLifetimeRejected,
	/// Some of the runtime's work did not end cleanly, so the runtime did not remove itself.
	DrainageRejected,
	/// The shutdown task stopped before the removal ended.
	ContinuationAbandoned,
	/// A deadline exceeded the range of the local monotonic clock.
	LocalClockDomain,
}

/// A wait on a runtime's shutdown that ended without an outcome. The shutdown keeps
/// running, and the handle can wait again.
pub struct PgRuntimeShutdownWaitError {
	source: Box<NativeParticipationShutdownWaitError>,
}

impl PgRuntimeShutdownWaitError {
	pub(crate) fn new(source: NativeParticipationShutdownWaitError) -> Self {
		Self {
			source: Box::new(source),
		}
	}

	/// The class of the failure.
	pub fn kind(&self) -> PgRuntimeShutdownWaitErrorKind {
		match self.source.as_ref() {
			NativeParticipationShutdownWaitError::InvalidWait => {
				PgRuntimeShutdownWaitErrorKind::InvalidWait
			}
			NativeParticipationShutdownWaitError::Pending { .. } => {
				PgRuntimeShutdownWaitErrorKind::Pending
			}
		}
	}

	/// The runtime's state when the wait elapsed, when the kind is `Pending`.
	pub fn pending_snapshot(&self) -> Option<PgRuntimeSnapshot> {
		match self.source.as_ref() {
			NativeParticipationShutdownWaitError::Pending { snapshot } => {
				Some(PgRuntimeSnapshot(snapshot.as_ref().clone()))
			}
			_ => None,
		}
	}
}

/// The class of a shutdown wait that ended without an outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgRuntimeShutdownWaitErrorKind {
	/// The wait's timeout is zero or exceeds the range of the local monotonic clock.
	InvalidWait,
	/// The wait elapsed before the shutdown finished.
	Pending,
}

impl fmt::Debug for PgRuntimeShutdownWaitError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("PgRuntimeShutdownWaitError")
			.field("kind", &self.kind())
			.finish()
	}
}
impl fmt::Display for PgRuntimeShutdownWaitError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("PostgreSQL runtime shutdown wait failed")
	}
}
impl Error for PgRuntimeShutdownWaitError {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		Some(self.source.as_ref())
	}
}

pub(crate) fn removal_outcome(
	removal: &NativeParticipationRemovalResult,
) -> PgRuntimeRemovalResult {
	match removal {
		NativeParticipationRemovalResult::Removed { was_present } => {
			PgRuntimeRemovalResult::Removed {
				was_present: *was_present,
			}
		}
		NativeParticipationRemovalResult::Failed(_) => PgRuntimeRemovalResult::Failed,
		NativeParticipationRemovalResult::TimedOut => PgRuntimeRemovalResult::TimedOut,
		NativeParticipationRemovalResult::LocalLifetimeRejected(_) => {
			PgRuntimeRemovalResult::LocalLifetimeRejected
		}
		NativeParticipationRemovalResult::DrainageRejected(_) => {
			PgRuntimeRemovalResult::DrainageRejected
		}
		NativeParticipationRemovalResult::ContinuationAbandoned => {
			PgRuntimeRemovalResult::ContinuationAbandoned
		}
		NativeParticipationRemovalResult::LocalClockDomain => {
			PgRuntimeRemovalResult::LocalClockDomain
		}
	}
}
pub(crate) fn removal_error(
	removal: &NativeParticipationRemovalResult,
) -> Option<&(dyn Error + 'static)> {
	match removal {
		NativeParticipationRemovalResult::Failed(source) => Some(source.as_ref()),
		NativeParticipationRemovalResult::LocalLifetimeRejected(source)
		| NativeParticipationRemovalResult::DrainageRejected(source) => Some(source),
		_ => None,
	}
}
pub(crate) fn removal_outcome_is_unknown(removal: &NativeParticipationRemovalResult) -> bool {
	match removal {
		NativeParticipationRemovalResult::Failed(source) => {
			crate::runtime::participation::commit_outcome_is_unknown(source)
		}
		NativeParticipationRemovalResult::TimedOut
		| NativeParticipationRemovalResult::ContinuationAbandoned => true,
		_ => false,
	}
}

pub(crate) fn shutdown_commit_outcome_is_unknown(
	outcome: &NativeParticipationShutdownResult,
) -> bool {
	let renewal_uncertain = match &outcome.renewal {
		Err(NativeParticipationRenewalCompletionError::Fenced(fence)) => match fence.as_ref() {
			NativeParticipationFenceError::RenewalTimedOut
			| NativeParticipationFenceError::RenewalCommitUnknown(_)
			| NativeParticipationFenceError::RenewalTaskAbandoned => true,
			NativeParticipationFenceError::AuthorityLost(source) => {
				crate::runtime::participation::commit_outcome_is_unknown(source)
			}
			_ => false,
		},
		Err(
			NativeParticipationRenewalCompletionError::TaskAbandoned
			| NativeParticipationRenewalCompletionError::ShutdownContinuationAbandoned,
		) => true,
		Ok(()) => false,
	};
	renewal_uncertain || removal_outcome_is_unknown(&outcome.removal)
}
