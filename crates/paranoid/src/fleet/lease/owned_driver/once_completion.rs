//! One ordinary Once terminal purpose in the existing Lease task.

pub(crate) mod error;
pub(crate) mod execution;

use super::cache_publication::NativeLeasePurposeErrorWithObserver;
use super::{
	Arc, Claim, DeliveryAuthority, FinalPurposeCleanupOriginFields, Instant, LeaseCmd,
	LeaseCommittedCompletion, LeaseCompletionKind, LeaseDriver, LeaseFinalPurposeOrigin, Mutex,
	NativeLeaseCallerScope, NativeLeaseCloseObserver, NativeLeaseDriverError, NativeLeaseError,
	NativeLeaseWorkPhase, NativeOwnedLeaseClient, PurposeDispatch, TransactionEndingDisposition,
	oneshot,
};
use crate::db::transaction_ending::{
	TransactionEndingReceipt, TransactionEndingResult, TransactionEndingSnapshot,
};
use crate::fleet::once::engine::PgNativeOnce;
use crate::records::postgres::record_set::{PgRecordTarget, PgRecordTargetFields};
use crate::runtime::admission::lease::{
	OnceLeaseRegistrationFields, RegisteredOnceLeaseTransaction,
};
use crate::runtime::admission::owned_primitive::{
	OnceLeaseOriginIdentity, OnceLeaseOriginIdentityFields, OrdinaryOnceLeaseOriginEndingsFields,
	OwnedPrimitiveOriginEndings,
};

pub(crate) struct NativeLeaseOnceCompletionFields {
	pub(crate) once: Arc<PgNativeOnce>,
	pub(crate) caller: NativeLeaseCallerScope,
	pub(crate) requested_at: Instant,
}
/// Only the fixed completion consumer can use the moved private Claim.
pub(crate) struct NativeLeaseOnceCompletion {
	client: NativeOwnedLeaseClient,
	prepared: PreparedLeaseOnceCompletion,
}
impl NativeLeaseOnceCompletion {
	pub(crate) fn close_observer(&self) -> NativeLeaseCloseObserver {
		self.client.close_observer()
	}
}
pub(super) struct PreparedLeaseOnceCompletion {
	once: Arc<PgNativeOnce>,
	registered: RegisteredOnceLeaseTransaction,
	claim: Claim,
	pub(super) deadline: Instant,
	progress: Arc<OnceCompletionProgress>,
}
use super::once_pair::NativeOnceCompletionSnapshot;
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NativeOncePreparedCompletion {
	Recorded(NativeOnceCompletionSnapshot),
	AlreadyRecorded(NativeOnceCompletionSnapshot),
	Refused,
}
impl NativeOncePreparedCompletion {
	pub(in crate::fleet::lease::owned_driver) fn released(&self) -> bool {
		!matches!(self, Self::Refused)
	}
}
#[derive(Clone, Debug)]
pub(crate) enum NativeOnceCompletionObservation {
	NotDispatched,
	Pending,
	Committed(NativeOncePreparedCompletion),
	RolledBack,
	Uncertain,
}
pub(super) struct OnceCompletionProgress {
	state: Mutex<PurposeDispatch<NativeOncePreparedCompletion>>,
	receipt: TransactionEndingReceipt,
}
impl OnceCompletionProgress {
	fn registration_transferred(&self) {
		self.state
			.lock()
			.expect("Once progress contains no caller code")
			.registration_transferred();
	}
	fn prepare(&self, intended: NativeOncePreparedCompletion) {
		self.state
			.lock()
			.expect("Once progress contains no caller code")
			.prepare(intended);
	}
	pub(super) fn observation(&self) -> NativeOnceCompletionObservation {
		let state = self
			.state
			.lock()
			.expect("Once progress contains no caller code");
		if !state.is_dispatched() {
			return NativeOnceCompletionObservation::NotDispatched;
		}
		let intended = state.prepared().cloned();
		match self.receipt.snapshot() {
			TransactionEndingSnapshot::Terminal(ending) => match ending.disposition() {
				TransactionEndingDisposition::KnownCommit => match intended {
					Some(result) => NativeOnceCompletionObservation::Committed(result),
					None => NativeOnceCompletionObservation::Uncertain,
				},
				TransactionEndingDisposition::KnownRollback => {
					NativeOnceCompletionObservation::RolledBack
				}
				_ => NativeOnceCompletionObservation::Uncertain,
			},
			_ => NativeOnceCompletionObservation::Pending,
		}
	}
	fn dispatched(&self) -> bool {
		self.state
			.lock()
			.expect("Once progress contains no caller code")
			.is_dispatched()
	}
}
pub(super) struct OnceCompletionOrigin {
	identity: OnceLeaseOriginIdentity,
	receipt: TransactionEndingReceipt,
	progress: Arc<OnceCompletionProgress>,
}
impl OnceCompletionOrigin {
	pub(super) fn receipt(&self) -> &TransactionEndingReceipt {
		&self.receipt
	}
	pub(super) fn ending_failure(&self) -> Option<Arc<TransactionEndingResult>> {
		if !self.progress.dispatched() {
			return None;
		}
		match self.receipt.snapshot() {
			TransactionEndingSnapshot::Terminal(ending)
				if ending.error().is_some()
					|| !matches!(
						ending.disposition(),
						TransactionEndingDisposition::KnownCommit
							| TransactionEndingDisposition::KnownRollback
					) =>
			{
				Some(ending)
			}
			_ => None,
		}
	}
	pub(super) fn committed_completion(&self) -> Option<LeaseCommittedCompletion> {
		let NativeOnceCompletionObservation::Committed(result) = self.progress.observation() else {
			return None;
		};
		Some(LeaseCommittedCompletion {
			receipt: self.receipt.clone(),
			kind: LeaseCompletionKind::Release(result.released()),
		})
	}
	pub(super) fn cleanup_origins(
		&self,
		fields: FinalPurposeCleanupOriginFields<'_>,
	) -> Option<OwnedPrimitiveOriginEndings> {
		if !self.progress.dispatched() {
			return None;
		}
		Some(OwnedPrimitiveOriginEndings::ordinary_once_lease(
			OrdinaryOnceLeaseOriginEndingsFields {
				identity: &self.identity,
				lease_operation: fields.lease_operation,
				completion: &self.receipt,
				preceding_operation: fields.preceding_operation,
				pending_origins: fields.pending_origins,
			},
		))
	}
}
impl NativeOwnedLeaseClient {
	pub(crate) async fn into_once_completion(
		self,
		fields: NativeLeaseOnceCompletionFields,
	) -> Result<NativeLeaseOnceCompletion, NativeLeasePurposeErrorWithObserver> {
		let observer = self.close_observer();
		if !Arc::ptr_eq(&self.shared, &fields.caller.shared) {
			return Err(NativeLeasePurposeErrorWithObserver {
				source: Arc::new(NativeLeaseDriverError::AuthorityLost),
				observer,
			});
		}
		let (response, receive) = oneshot::channel();
		self.cmds
			.send(LeaseCmd::OnceCompletion {
				fields: Box::new(fields),
				response,
			})
			.await
			.map_err(|_| NativeLeasePurposeErrorWithObserver {
				source: Arc::new(NativeLeaseDriverError::DeliveryClosed),
				observer: observer.clone(),
			})?;
		let prepared = self
			.receive(DeliveryAuthority::HeldRenewal, receive)
			.await
			.map_err(|source| NativeLeasePurposeErrorWithObserver { source, observer })?;
		Ok(NativeLeaseOnceCompletion {
			client: self,
			prepared,
		})
	}
}
impl LeaseDriver {
	pub(super) fn prepare_once_completion(
		&mut self,
		mut fields: NativeLeaseOnceCompletionFields,
	) -> Result<PreparedLeaseOnceCompletion, NativeLeaseDriverError> {
		let deadline = self.operation_deadline(fields.requested_at)?;
		Self::current_operation_deadline(
			&self.owner,
			&self.shared,
			Some(deadline),
			NativeLeaseWorkPhase::OnceCompletion,
		)?;
		if !Arc::ptr_eq(&fields.once.lease().bindings, &self.lease.bindings)
			|| fields.once.lease().identity() != self.lease.identity()
			|| fields.once.lease().key() != self.lease.key()
			|| !Arc::ptr_eq(fields.once.admission(), &self.lease.bindings.admission)
			|| !Arc::ptr_eq(&fields.caller.shared, &self.shared)
			|| !self.ownership.permits_final_purpose()
		{
			return Err(NativeLeaseDriverError::AuthorityLost);
		}
		let completion = PgRecordTarget::new(PgRecordTargetFields {
			storage: fields.once.completion_storage(),
			record: fields.once.completion_identity().clone(),
		});
		let lease = PgRecordTarget::new(PgRecordTargetFields {
			storage: &self.lease.bindings.storage,
			record: self.lease.identity().clone(),
		});
		if completion.identity() == lease.identity() {
			return Err(NativeLeaseDriverError::Operation(
				NativeLeaseError::Registered(
					crate::runtime::admission::lease::LeaseRegistrationError::BindingMismatch,
				),
			));
		}
		let identity = OnceLeaseOriginIdentity::new(OnceLeaseOriginIdentityFields {
			key: fields.once.key().clone(),
			completion: completion.identity().clone(),
			lease: lease.identity().clone(),
		});
		let caller = fields.caller.take_caller();
		let registered = match self.lease.bindings.admission.prepare_once_lease_completion(
			OnceLeaseRegistrationFields {
				owner: &self.owner,
				once: &fields.once,
				caller,
			},
		) {
			Ok(registered) => registered,
			Err(rejected) => {
				fields.caller.restore_caller(rejected.caller);
				return Err(NativeLeaseDriverError::Operation(
					NativeLeaseError::Registered(rejected.source),
				));
			}
		};
		let progress = Arc::new(OnceCompletionProgress {
			state: Mutex::new(PurposeDispatch::Registered),
			receipt: registered.receipt(),
		});
		let claim =
			self.ownership
				.transfer_to_final_purpose(LeaseFinalPurposeOrigin::OnceCompletion(
					OnceCompletionOrigin {
						identity,
						receipt: registered.receipt(),
						progress: Arc::clone(&progress),
					},
				));
		Ok(PreparedLeaseOnceCompletion {
			once: fields.once,
			registered,
			claim,
			deadline,
			progress,
		})
	}
}
