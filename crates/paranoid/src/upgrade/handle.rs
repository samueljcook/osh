//! The controller of an upgrade task that a runtime runs.

use std::{error::Error, fmt};

use crate::upgrade::engine::driver::observer::{
	NativeUpgradeDriverObserver, NativeUpgradeDriverWaitError,
};

pub(crate) mod error;
mod observation;

pub use observation::{
	PgUpgradeEndingKind, PgUpgradeEndingSnapshot, PgUpgradePrerequisite, PgUpgradePrerequisiteKind,
	PgUpgradeSnapshot,
};

/// The controller of one upgrade task that a runtime runs.
///
/// Cloning, dropping, or waiting on a controller does not change the upgrade or the task.
/// The Tokio runtime that started the task must stay alive while the task runs.
#[derive(Clone)]
pub struct PgUpgradeController {
	observer: NativeUpgradeDriverObserver,
}

impl PgUpgradeController {
	pub(crate) fn new(observer: NativeUpgradeDriverObserver) -> Self {
		Self { observer }
	}

	/// The task's current state. This does no database work.
	pub fn snapshot(&self) -> PgUpgradeSnapshot {
		PgUpgradeSnapshot(self.observer.snapshot())
	}

	/// Asks the task to stop after its current transaction ends. If the task had recorded the
	/// upgrade when it stopped, `PgRuntime::resume_upgrade` can continue it.
	pub fn request_stop(&self) {
		self.observer.request_stop();
	}

	/// Asks the task to cancel the upgrade.
	///
	/// The task can complete, stop, or fail before it cancels the upgrade; the final phase
	/// from `wait_until_finished` reports which of these happened.
	pub fn request_cancel(&self) {
		self.observer.request_cancel();
	}

	/// Waits at most `options.timeout` until the task has read the upgrade's record from the
	/// installation. This does not wait for the upgrade to finish.
	pub async fn wait_until_accepted(
		&self,
		options: crate::runtime::PgWaitOptions,
	) -> Result<PgUpgradeSnapshot, PgUpgradeWaitError> {
		self.observer
			.wait_until_accepted(options.timeout)
			.await
			.map_err(|error| self.wait_error(error))?;
		Ok(self.snapshot())
	}

	/// Waits at most `options.timeout` until the task ends, and returns its final state.
	///
	/// The final phase is `Completed`, `Cancelled`, `Stopped`, `Inspected`,
	/// `PrerequisitesUnmet`, `Failed`, or `Unresolved`.
	pub async fn wait_until_finished(
		&self,
		options: crate::runtime::PgWaitOptions,
	) -> Result<PgUpgradeSnapshot, PgUpgradeWaitError> {
		self.observer
			.wait_until_finished(options.timeout)
			.await
			.map(PgUpgradeSnapshot)
			.map_err(|error| self.wait_error(error))
	}

	fn wait_error(&self, source: NativeUpgradeDriverWaitError) -> PgUpgradeWaitError {
		PgUpgradeWaitError {
			source: Box::new(source),
			controller: self.clone(),
		}
	}
}

impl fmt::Debug for PgUpgradeController {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("PgUpgradeController")
			.field("phase", &self.snapshot().phase())
			.finish()
	}
}

/// The class of a wait that ended without the requested result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgUpgradeWaitErrorKind {
	/// The wait is zero or exceeds the range of the local monotonic clock.
	InvalidWait,
	/// The wait elapsed first. The task keeps running, and the controller can wait again.
	Pending,
	/// The task ended before it read the upgrade's record.
	NotAccepted,
}

/// A wait on an upgrade controller that ended without the requested result.
pub struct PgUpgradeWaitError {
	source: Box<NativeUpgradeDriverWaitError>,
	controller: PgUpgradeController,
}

impl PgUpgradeWaitError {
	/// The class of the failure.
	pub fn kind(&self) -> PgUpgradeWaitErrorKind {
		match self.source.as_ref() {
			NativeUpgradeDriverWaitError::InvalidWaitTimeout => PgUpgradeWaitErrorKind::InvalidWait,
			NativeUpgradeDriverWaitError::Pending { .. } => PgUpgradeWaitErrorKind::Pending,
			NativeUpgradeDriverWaitError::NotAccepted { .. } => PgUpgradeWaitErrorKind::NotAccepted,
		}
	}

	/// The controller that was waited on.
	pub fn controller(&self) -> &PgUpgradeController {
		&self.controller
	}

	/// The task's state when the wait ended, unless the kind is `InvalidWait`.
	pub fn snapshot(&self) -> Option<PgUpgradeSnapshot> {
		match self.source.as_ref() {
			NativeUpgradeDriverWaitError::InvalidWaitTimeout => None,
			NativeUpgradeDriverWaitError::Pending { snapshot }
			| NativeUpgradeDriverWaitError::NotAccepted { snapshot } => {
				Some(PgUpgradeSnapshot(snapshot.as_ref().clone()))
			}
		}
	}
}

impl fmt::Debug for PgUpgradeWaitError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("PgUpgradeWaitError")
			.field("kind", &self.kind())
			.finish_non_exhaustive()
	}
}

impl fmt::Display for PgUpgradeWaitError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("upgrade controller observation did not reach the requested result")
	}
}

impl Error for PgUpgradeWaitError {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		Some(self.source.as_ref())
	}
}
