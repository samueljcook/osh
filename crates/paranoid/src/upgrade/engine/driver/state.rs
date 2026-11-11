use std::sync::Arc;

use crate::db::transaction_ending::TransactionEndingReceipt;
use crate::runtime::admission::upgrade::NativeUpgradeAdmissionError;
use crate::runtime::targets::ResolvedNativeRuntimeTarget;
use crate::upgrade::requests::work::creation_execution::NativeUpgradeCreateState;
use crate::upgrade::requests::work::observation::NativeUpgradeControllerObservation;
use crate::upgrade::requests::work::recovery_observation::NativeUpgradeRecoveryState;
use crate::upgrade::work::CompleteUpgradeTarget;

use super::error::NativeUpgradeDriverError;
use super::observer::PgUpgradePhase;

#[cfg(test)]
mod tests;

pub(super) enum DriverCreation {
	NotCreating,
	Direct(NativeUpgradeCreateState),
	Recovering(NativeUpgradeCreateState),
}

impl DriverCreation {
	pub(super) fn state(&self) -> Option<&NativeUpgradeCreateState> {
		match self {
			Self::NotCreating => None,
			Self::Direct(state) | Self::Recovering(state) => Some(state),
		}
	}
	pub(super) fn state_mut(&mut self) -> Option<&mut NativeUpgradeCreateState> {
		match self {
			Self::NotCreating => None,
			Self::Direct(state) | Self::Recovering(state) => Some(state),
		}
	}
	pub(super) fn is_recovering(&self) -> bool {
		matches!(self, Self::Recovering(_))
	}
	pub(super) fn require_recovery(&mut self) {
		if matches!(self, Self::NotCreating) {
			unreachable!("creation recovery retains its actual creation state");
		}
		if let Self::Direct(_) = self {
			let Self::Direct(state) = std::mem::replace(self, Self::NotCreating) else {
				unreachable!("direct creation was checked")
			};
			*self = Self::Recovering(state);
		}
	}
	pub(super) fn permit_direct(&mut self) {
		if matches!(self, Self::NotCreating) {
			unreachable!("creation recovery retains its actual creation state");
		}
		if let Self::Recovering(_) = self {
			let Self::Recovering(state) = std::mem::replace(self, Self::NotCreating) else {
				unreachable!("recovering creation was checked")
			};
			*self = Self::Direct(state);
		}
	}
}

/// The recovery of a mutation with an unknown result, and the failure that the mutation's
/// operation reported. A failed recovery read does not replace that failure.
pub(super) struct DriverRecovery {
	pub(super) state: NativeUpgradeRecoveryState,
	pub(super) origin_failure: Option<Arc<NativeUpgradeDriverError>>,
}

pub(super) enum DriverTarget {
	Unbound,
	Retained(Arc<CompleteUpgradeTarget>),
	Resolved(ResolvedNativeRuntimeTarget),
}

impl DriverTarget {
	pub(super) fn target(&self) -> Option<&Arc<CompleteUpgradeTarget>> {
		match self {
			Self::Unbound => None,
			Self::Retained(target) => Some(target),
			Self::Resolved(resolved) => Some(resolved.target()),
		}
	}
	pub(super) fn resolved(&self) -> Option<&ResolvedNativeRuntimeTarget> {
		match self {
			Self::Resolved(resolved) => Some(resolved),
			Self::Unbound | Self::Retained(_) => None,
		}
	}
}

pub(super) enum ObservationAttempt {
	Initial,
	Repeated,
}

pub(super) enum ObservationPurpose {
	Continue,
	ReportFailure(Arc<NativeUpgradeDriverError>),
}

pub(super) enum DriverObservation {
	Awaiting {
		attempt: ObservationAttempt,
		purpose: ObservationPurpose,
	},
	Observed {
		observation: Box<NativeUpgradeControllerObservation>,
		purpose: ObservationPurpose,
	},
}

impl DriverObservation {
	pub(super) fn new() -> Self {
		Self::Awaiting {
			attempt: ObservationAttempt::Initial,
			purpose: ObservationPurpose::Continue,
		}
	}
	pub(super) fn observed(&self) -> Option<&NativeUpgradeControllerObservation> {
		match self {
			Self::Observed { observation, .. } => Some(observation),
			Self::Awaiting { .. } => None,
		}
	}
	pub(super) fn invalidate(&mut self) {
		if matches!(self, Self::Observed { .. }) {
			let Self::Observed { purpose, .. } = std::mem::replace(self, Self::new()) else {
				unreachable!("observed context was checked")
			};
			*self = Self::Awaiting {
				attempt: ObservationAttempt::Initial,
				purpose,
			};
		}
	}
	pub(super) fn accept(&mut self, observation: NativeUpgradeControllerObservation) {
		let purpose = match std::mem::replace(self, Self::new()) {
			Self::Awaiting { purpose, .. } | Self::Observed { purpose, .. } => purpose,
		};
		*self = Self::Observed {
			observation: Box::new(observation),
			purpose,
		};
	}
	pub(super) fn request_reobservation(&mut self) -> bool {
		if matches!(
			self,
			Self::Awaiting {
				attempt: ObservationAttempt::Repeated,
				..
			}
		) {
			return false;
		}
		self.invalidate();
		let Self::Awaiting { attempt, .. } = self else {
			unreachable!("reobservation discards an accepted context")
		};
		*attempt = ObservationAttempt::Repeated;
		true
	}
	pub(super) fn defer_failure(&mut self, error: Arc<NativeUpgradeDriverError>) {
		self.invalidate();
		let Self::Awaiting { purpose, .. } = self else {
			unreachable!("deferred failure requires a fresh observation")
		};
		*purpose = ObservationPurpose::ReportFailure(error);
	}
	pub(super) fn take_deferred_failure(&mut self) -> Option<Arc<NativeUpgradeDriverError>> {
		let purpose = match self {
			Self::Awaiting { purpose, .. } | Self::Observed { purpose, .. } => purpose,
		};
		match std::mem::replace(purpose, ObservationPurpose::Continue) {
			ObservationPurpose::Continue => None,
			ObservationPurpose::ReportFailure(error) => Some(error),
		}
	}
}

#[derive(Clone, Copy)]
pub(super) enum DispatchKind {
	Read,
	Mutation,
}

pub(super) enum DriverTransaction {
	Idle,
	Dispatching {
		kind: DispatchKind,
		ending: Option<TransactionEndingReceipt>,
	},
	AwaitingEnding {
		kind: DispatchKind,
		ending: TransactionEndingReceipt,
	},
	Settled(TransactionEndingReceipt),
}

impl DriverTransaction {
	pub(super) fn begin_dispatch(
		&mut self,
		kind: DispatchKind,
	) -> Result<(), NativeUpgradeDriverError> {
		match self {
			Self::Idle | Self::Settled(_) => {
				*self = Self::Dispatching { kind, ending: None };
				Ok(())
			}
			Self::Dispatching { .. } | Self::AwaitingEnding { .. } => {
				Err(NativeUpgradeAdmissionError::PreviousEndingPending.into())
			}
		}
	}
	pub(super) fn slot_mut(&mut self) -> &mut Option<TransactionEndingReceipt> {
		match self {
			Self::Dispatching { ending, .. } => ending,
			Self::Idle | Self::AwaitingEnding { .. } | Self::Settled(_) => {
				unreachable!("dispatch owns its receipt publication slot")
			}
		}
	}
	pub(super) fn finish_dispatch(&mut self) -> bool {
		if !matches!(self, Self::Dispatching { .. }) {
			return false;
		}
		let Self::Dispatching { kind, ending } = std::mem::replace(self, Self::Idle) else {
			unreachable!("dispatch was checked")
		};
		match ending {
			Some(ending) => {
				*self = Self::AwaitingEnding { kind, ending };
				true
			}
			None => false,
		}
	}
	pub(super) fn receipt(&self) -> Option<&TransactionEndingReceipt> {
		match self {
			Self::Idle => None,
			Self::Dispatching { ending, .. } => ending.as_ref(),
			Self::AwaitingEnding { ending, .. } | Self::Settled(ending) => Some(ending),
		}
	}
	pub(super) fn is_mutation_pending(&self) -> bool {
		matches!(
			self,
			Self::AwaitingEnding {
				kind: DispatchKind::Mutation,
				..
			}
		)
	}
	pub(super) fn record_settled(&mut self) {
		if matches!(self, Self::AwaitingEnding { .. }) {
			let Self::AwaitingEnding { ending, .. } = std::mem::replace(self, Self::Idle) else {
				unreachable!("pending ending was checked")
			};
			*self = Self::Settled(ending);
		}
	}
}

pub(super) enum DriverExit {
	Completed,
	Cancelled,
	Stopped,
	Inspected,
}

pub(super) enum DriverCompletion {
	Succeeded(DriverExit),
	PrerequisitesUnmet(Arc<NativeUpgradeDriverError>),
	Failed(Arc<NativeUpgradeDriverError>),
	Unresolved(Arc<NativeUpgradeDriverError>),
}

impl DriverCompletion {
	pub(super) fn failed(error: Arc<NativeUpgradeDriverError>) -> Self {
		match error.as_ref() {
			NativeUpgradeDriverError::Prerequisites { .. } => Self::PrerequisitesUnmet(error),
			_ => Self::Failed(error),
		}
	}
	pub(super) fn phase(&self) -> PgUpgradePhase {
		match self {
			Self::Succeeded(DriverExit::Completed) => PgUpgradePhase::Completed,
			Self::Succeeded(DriverExit::Cancelled) => PgUpgradePhase::Cancelled,
			Self::Succeeded(DriverExit::Stopped) => PgUpgradePhase::Stopped,
			Self::Succeeded(DriverExit::Inspected) => PgUpgradePhase::Inspected,
			Self::PrerequisitesUnmet(_) => PgUpgradePhase::PrerequisitesUnmet,
			Self::Failed(_) => PgUpgradePhase::Failed,
			Self::Unresolved(_) => PgUpgradePhase::Unresolved,
		}
	}
	pub(super) fn failure(&self) -> Option<&Arc<NativeUpgradeDriverError>> {
		match self {
			Self::Succeeded(_) => None,
			Self::PrerequisitesUnmet(error) | Self::Failed(error) | Self::Unresolved(error) => {
				Some(error)
			}
		}
	}
	pub(super) fn unresolved_recovery(
		self,
		recovery: &NativeUpgradeRecoveryState,
		recorded_failure: Option<&Arc<NativeUpgradeDriverError>>,
	) -> Self {
		Self::Unresolved(Arc::new(NativeUpgradeDriverError::UncertainMutation {
			origin: recovery.origin().clone(),
			source: self.failure().or(recorded_failure).cloned(),
		}))
	}
	pub(super) fn following_unresolved(
		self,
		following_error: Arc<NativeUpgradeDriverError>,
	) -> Self {
		let error = match self.failure() {
			Some(primary_error) => Arc::new(NativeUpgradeDriverError::FollowingFailure {
				primary_error: Arc::clone(primary_error),
				following_error,
			}),
			None => following_error,
		};
		Self::Unresolved(error)
	}
}
