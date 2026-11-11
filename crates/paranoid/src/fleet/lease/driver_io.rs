use super::engine::{NativeLeaseError, PgNativeLease};
use super::state::{LeaseAcquisition, TentativeLeaseAuthority};
use super::{Claim, ClaimDuration, Token};
use crate::db::transaction::run_registered_transaction_write;
use crate::db::transaction_ending::{
	TransactionEndingDisposition, TransactionEndingReceipt, TransactionEndingSnapshot,
};
use crate::runtime::admission::lease::LeaseCleanupResult;
use crate::runtime::local_admission::lease_work::RuntimeLeaseWorkPermit;

/// The actual receipt and prepared result survive cancellation of the operation future.
#[derive(Default)]
pub(super) struct LeaseOperationOrigin {
	state: OperationOriginState,
}

/// The receipt of a registered operation, taken from its origin with the ending that the
/// origin observed after the operation was interrupted.
pub(super) struct TakenLeaseOperation {
	pub(super) receipt: TransactionEndingReceipt,
	/// The ending of the interrupted operation when it was a failure or had an unknown outcome.
	pub(super) interrupted_ending:
		Option<std::sync::Arc<crate::db::transaction_ending::TransactionEndingResult>>,
}

#[derive(Default)]
enum OperationOriginState {
	#[default]
	Unregistered,
	Registered {
		receipt: TransactionEndingReceipt,
		completion: CompletionPreparation,
		ending_capture: EndingCapture,
	},
}

enum CompletionPreparation {
	Awaiting,
	Prepared(LeaseCompletionKind),
}

enum EndingCapture {
	NotRequested,
	Awaiting,
	/// The ending of the interrupted operation, retained when it was a failure or had an
	/// unknown outcome.
	Observed(Option<std::sync::Arc<crate::db::transaction_ending::TransactionEndingResult>>),
}
pub(super) struct LeaseCommittedCompletion {
	pub(super) receipt: TransactionEndingReceipt,
	pub(super) kind: LeaseCompletionKind,
}
#[derive(Clone, Copy)]
pub(super) enum LeaseCompletionKind {
	AcquisitionDenied,
	Release(bool),
}
impl LeaseOperationOrigin {
	fn register(&mut self, receipt: TransactionEndingReceipt) {
		assert!(
			matches!(self.state, OperationOriginState::Unregistered),
			"an operation origin registers once"
		);
		self.state = OperationOriginState::Registered {
			receipt,
			completion: CompletionPreparation::Awaiting,
			ending_capture: EndingCapture::NotRequested,
		};
	}
	fn prepare_completion(&mut self, kind: LeaseCompletionKind) {
		let OperationOriginState::Registered { completion, .. } = &mut self.state else {
			unreachable!("completion preparation requires its actual registered transaction");
		};
		assert!(
			matches!(completion, CompletionPreparation::Awaiting),
			"one registered operation prepares one completion"
		);
		*completion = CompletionPreparation::Prepared(kind);
	}
	pub(super) fn receipt(&self) -> Option<&TransactionEndingReceipt> {
		match &self.state {
			OperationOriginState::Unregistered => None,
			OperationOriginState::Registered { receipt, .. } => Some(receipt),
		}
	}
	pub(super) fn take_receipt(&mut self) -> Option<TakenLeaseOperation> {
		match std::mem::take(&mut self.state) {
			OperationOriginState::Unregistered => None,
			OperationOriginState::Registered {
				receipt,
				ending_capture,
				..
			} => Some(TakenLeaseOperation {
				receipt,
				interrupted_ending: match ending_capture {
					EndingCapture::Observed(ending) => ending,
					EndingCapture::NotRequested | EndingCapture::Awaiting => None,
				},
			}),
		}
	}
	pub(super) fn capture_interrupted_ending(&mut self, interrupted: bool) {
		if let OperationOriginState::Registered { ending_capture, .. } = &mut self.state {
			*ending_capture = if interrupted {
				EndingCapture::Awaiting
			} else {
				EndingCapture::NotRequested
			};
		}
	}
	pub(super) fn observe_ending(
		&mut self,
		ending: std::sync::Arc<crate::db::transaction_ending::TransactionEndingResult>,
	) {
		if let OperationOriginState::Registered {
			ending_capture: capture @ EndingCapture::Awaiting,
			..
		} = &mut self.state
		{
			*capture = EndingCapture::Observed(
				(ending.error().is_some()
					|| !matches!(
						ending.disposition(),
						TransactionEndingDisposition::KnownCommit
							| TransactionEndingDisposition::KnownRollback
					))
				.then_some(ending),
			);
		}
	}
	pub(super) fn committed_completion(&self) -> Option<LeaseCommittedCompletion> {
		let OperationOriginState::Registered {
			receipt,
			completion: CompletionPreparation::Prepared(kind),
			..
		} = &self.state
		else {
			return None;
		};
		if !matches!(receipt.snapshot(), TransactionEndingSnapshot::Terminal(ending) if ending.disposition() == TransactionEndingDisposition::KnownCommit)
		{
			return None;
		}
		Some(LeaseCommittedCompletion {
			receipt: receipt.clone(),
			kind: *kind,
		})
	}
}

pub(super) async fn acquire(
	lease: &PgNativeLease,
	owner: &RuntimeLeaseWorkPermit,
	proposal: LeaseAcquisition,
	origin: &mut LeaseOperationOrigin,
) -> Result<Option<TentativeLeaseAuthority>, NativeLeaseError> {
	let mut registered = lease
		.bindings
		.admission
		.prepare_owned_lease_acquisition(owner, lease)
		.map_err(NativeLeaseError::Registered)?;
	let receipt = registered.receipt().clone();
	origin.register(receipt);
	let registration = registered
		.take_registration()
		.map_err(NativeLeaseError::Registered)?;
	run_registered_transaction_write(
		&lease.bindings.control_pool,
		"fleet.native_lease.owned.acquire",
		registration,
		async |tx| {
			let result = lease
				.bindings
				.admission
				.acquire_owned_lease(tx, &registered, lease, proposal)
				.await?;
			if result.is_none() {
				origin.prepare_completion(LeaseCompletionKind::AcquisitionDenied);
			}
			Ok(result)
		},
	)
	.await
}

pub(super) async fn renew(
	lease: &PgNativeLease,
	owner: &RuntimeLeaseWorkPermit,
	claim: &Claim,
	token: Token,
	duration: ClaimDuration,
	origin: &mut LeaseOperationOrigin,
) -> Result<Option<TentativeLeaseAuthority>, NativeLeaseError> {
	let mut registered = lease
		.bindings
		.admission
		.prepare_owned_lease_renewal(owner, lease)
		.map_err(NativeLeaseError::Registered)?;
	origin.register(registered.receipt().clone());
	let registration = registered
		.take_registration()
		.map_err(NativeLeaseError::Registered)?;
	run_registered_transaction_write(
		&lease.bindings.control_pool,
		"fleet.native_lease.owned.renew",
		registration,
		async |tx| {
			lease
				.bindings
				.admission
				.renew_owned_lease(tx, &registered, lease, claim, token, duration)
				.await
		},
	)
	.await
}

pub(super) async fn cleanup(
	invocation: crate::runtime::admission::lease::LeaseCleanupInvocation<'_>,
	attempt: &mut LeaseOperationOrigin,
) -> Result<LeaseCleanupResult, NativeLeaseError> {
	let lease = invocation.lease;
	let mut registered = lease
		.bindings
		.admission
		.prepare_owned_lease_cleanup_transaction(invocation.owner, lease)
		.map_err(NativeLeaseError::Registered)?;
	let receipt = registered.receipt().clone();
	attempt.register(receipt);
	let registration = registered
		.take_registration()
		.map_err(NativeLeaseError::Registered)?;
	run_registered_transaction_write(
		&lease.bindings.control_pool,
		"fleet.native_lease.owned.cleanup",
		registration,
		async move |tx| {
			let result = lease
				.bindings
				.admission
				.execute_owned_lease_cleanup(tx, &registered, invocation)
				.await?;
			if let LeaseCleanupResult::Release(released) = result {
				attempt.prepare_completion(LeaseCompletionKind::Release(released));
			}
			Ok(result)
		},
	)
	.await
}
