//! Exact registered transaction history and per-attempt physical ordering progress.

use super::{
	Instant, NativeThrottlerError, OriginOrderingProgress, OwnedPrimitiveOriginOrdering,
	TransactionEndingReceipt,
};

pub(super) enum ReceiptHistory {
	Unstarted,
	Started {
		requested_at: Instant,
		acquisition: TransactionEndingReceipt,
		outcome: OutcomeDispatch,
		probe: ProbeHistory,
		latest: LatestReceipt,
	},
}

pub(super) enum OutcomeDispatch {
	Awaiting,
	Registered(TransactionEndingReceipt),
}

pub(super) enum ProbeHistory {
	Unextended,
	Extended(TransactionEndingReceipt),
}

pub(super) enum LatestReceipt {
	Recorded(TransactionEndingReceipt),
	AwaitingOutcomeDispatch,
}

pub(super) enum OriginPurpose {
	Acquisition(Instant),
	Outcome,
	Probe,
	Reconciliation,
}

pub(super) enum AttemptState {
	Unregistered,
	Registered {
		receipt: TransactionEndingReceipt,
		ordering: OriginOrderingProgress<OwnedPrimitiveOriginOrdering>,
	},
}

impl ReceiptHistory {
	pub(super) fn record(
		&mut self,
		purpose: OriginPurpose,
		receipt: TransactionEndingReceipt,
	) -> Result<(), NativeThrottlerError> {
		if let OriginPurpose::Acquisition(requested_at) = purpose {
			*self = Self::Started {
				requested_at,
				acquisition: receipt.clone(),
				outcome: OutcomeDispatch::Awaiting,
				probe: ProbeHistory::Unextended,
				latest: LatestReceipt::Recorded(receipt),
			};
			return Ok(());
		}
		let Self::Started {
			outcome,
			probe,
			latest,
			..
		} = self
		else {
			return Err(NativeThrottlerError::OperationState);
		};
		match purpose {
			OriginPurpose::Acquisition(_) => {
				unreachable!("acquisition initializes its complete receipt history")
			}
			OriginPurpose::Outcome => *outcome = OutcomeDispatch::Registered(receipt.clone()),
			OriginPurpose::Probe => *probe = ProbeHistory::Extended(receipt.clone()),
			OriginPurpose::Reconciliation => {}
		}
		*latest = LatestReceipt::Recorded(receipt);
		Ok(())
	}
	pub(super) fn prepare_outcome(&mut self) -> Result<(), NativeThrottlerError> {
		let Self::Started { latest, .. } = self else {
			return Err(NativeThrottlerError::OperationState);
		};
		*latest = LatestReceipt::AwaitingOutcomeDispatch;
		Ok(())
	}
	pub(super) fn acquisition(&self) -> Option<&TransactionEndingReceipt> {
		match self {
			Self::Unstarted => None,
			Self::Started { acquisition, .. } => Some(acquisition),
		}
	}
	pub(super) fn outcome(&self) -> Option<&TransactionEndingReceipt> {
		match self {
			Self::Started {
				outcome: OutcomeDispatch::Registered(receipt),
				..
			} => Some(receipt),
			_ => None,
		}
	}
	pub(super) fn probe(&self) -> Option<&TransactionEndingReceipt> {
		match self {
			Self::Started {
				probe: ProbeHistory::Extended(receipt),
				..
			} => Some(receipt),
			_ => None,
		}
	}
	pub(super) fn latest(&self) -> Option<&TransactionEndingReceipt> {
		match self {
			Self::Started {
				latest: LatestReceipt::Recorded(receipt),
				..
			} => Some(receipt),
			_ => None,
		}
	}
	pub(super) fn requested_at(&self) -> Option<Instant> {
		match self {
			Self::Unstarted => None,
			Self::Started { requested_at, .. } => Some(*requested_at),
		}
	}
}

impl AttemptState {
	pub(super) fn registered(receipt: TransactionEndingReceipt) -> Self {
		Self::Registered {
			receipt,
			ordering: OriginOrderingProgress::Awaiting,
		}
	}
	pub(super) fn receipt(&self) -> Option<&TransactionEndingReceipt> {
		match self {
			Self::Unregistered => None,
			Self::Registered { receipt, .. } => Some(receipt),
		}
	}
	pub(super) fn ordering(&self) -> Option<&OwnedPrimitiveOriginOrdering> {
		match self {
			Self::Unregistered => None,
			Self::Registered { ordering, .. } => ordering.proof(),
		}
	}
	pub(super) fn ordering_mut(
		&mut self,
	) -> Result<&mut OriginOrderingProgress<OwnedPrimitiveOriginOrdering>, NativeThrottlerError> {
		match self {
			Self::Unregistered => Err(NativeThrottlerError::OperationState),
			Self::Registered { ordering, .. } => Ok(ordering),
		}
	}
}
