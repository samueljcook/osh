//! One final Cache purpose in the existing Lease task with exact registered publication.

pub(crate) mod error;
pub(crate) mod execution;

use super::{
	Arc, Claim, DeliveryAuthority, DriverShared, FinalPurposeCleanupOriginFields, Instant,
	LeaseCmd, LeaseDeliveryDeadline, LeaseDriver, LeaseFinalPurposeOrigin, Mutex,
	NativeLeaseCloseObserver, NativeLeaseDriverError, NativeLeaseError, NativeLeaseWorkPhase,
	NativeOwnedLeaseClient, PurposeDispatch, TransactionEndingDisposition, oneshot,
};
use crate::db::OwnedTransactionRegistration;
use crate::fleet::cache::engine::publication_identity::NativeCachePublicationIdentity;
use crate::runtime::admission::lease::{
	CacheLeaseRegistrationFields, RegisteredCacheLeaseTransaction,
};
use crate::runtime::admission::owned_primitive::{
	CacheLeaseOriginEndingsFields, CacheLeaseOriginIdentity, OwnedPrimitiveOriginEndings,
};
use crate::runtime::local_admission::lease_work::RuntimeLeaseCallerWorkPermit;

/// One actual caller count. Abandonment requests cleanup before this count drops.
pub(crate) struct NativeLeaseCallerScope {
	caller: CallerOwnership,
	pub(super) shared: Arc<DriverShared>,
}

enum CallerOwnership {
	Retained(RuntimeLeaseCallerWorkPermit),
	Transferred,
}
impl NativeLeaseCallerScope {
	pub(super) fn new(caller: RuntimeLeaseCallerWorkPermit, shared: &Arc<DriverShared>) -> Self {
		Self {
			caller: CallerOwnership::Retained(caller),
			shared: Arc::clone(shared),
		}
	}
	pub(super) fn finish(mut self) {
		drop(self.take_caller());
	}
	pub(super) fn take_caller(&mut self) -> RuntimeLeaseCallerWorkPermit {
		match std::mem::replace(&mut self.caller, CallerOwnership::Transferred) {
			CallerOwnership::Retained(caller) => caller,
			CallerOwnership::Transferred => unreachable!("a live caller scope transfers once"),
		}
	}
	pub(super) fn restore_caller(&mut self, caller: RuntimeLeaseCallerWorkPermit) {
		assert!(
			matches!(self.caller, CallerOwnership::Transferred),
			"a rejected handoff returns its original caller permit"
		);
		self.caller = CallerOwnership::Retained(caller);
	}
}
impl Drop for NativeLeaseCallerScope {
	fn drop(&mut self) {
		if matches!(self.caller, CallerOwnership::Retained(_)) {
			self.shared.close();
		}
	}
}

pub(crate) struct NativeLeaseCachePublicationFields {
	pub(crate) identity: NativeCachePublicationIdentity,
	pub(crate) caller: NativeLeaseCallerScope,
	pub(crate) requested_at: Instant,
}

/// The original unique client remains owned here; it is never reconstructed or exposed.
pub(crate) struct NativeLeaseCachePublication {
	client: NativeOwnedLeaseClient,
	prepared: PreparedLeaseCachePublication,
}
impl NativeLeaseCachePublication {
	pub(crate) fn close_observer(&self) -> NativeLeaseCloseObserver {
		self.client.close_observer()
	}
	/// Only the fixed Cache consumer transfers this registration before its owned BEGIN.
	pub(in crate::fleet) fn take_registration(
		&mut self,
	) -> Result<OwnedTransactionRegistration, Arc<NativeLeaseDriverError>> {
		self.client
			.shared
			.require_delivery(
				DeliveryAuthority::HeldRenewal,
				Some(LeaseDeliveryDeadline::OriginalOperationDeadline(
					self.prepared.deadline,
				)),
			)
			.map_err(Arc::new)?;
		let registration = self
			.prepared
			.registered
			.take_registration()
			.map_err(|error| {
				Arc::new(NativeLeaseDriverError::Operation(
					NativeLeaseError::Registered(error),
				))
			})?;
		self.prepared.progress.registration_transferred();
		Ok(registration)
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeCachePreparedPublication {
	ValueWrite(crate::records::RecordMetadata),
	Refused,
}
pub(super) struct CachePublicationProgress {
	state: Mutex<PurposeDispatch<NativeCachePreparedPublication>>,
}
impl CachePublicationProgress {
	fn prepare(&self, intended: NativeCachePreparedPublication) {
		self.state
			.lock()
			.expect("Cache prepared result contains no caller code")
			.prepare(intended);
	}
	fn registration_transferred(&self) {
		self.state
			.lock()
			.expect("Cache publication dispatch contains no caller code")
			.registration_transferred();
	}
	fn dispatched(&self) -> bool {
		self.state
			.lock()
			.expect("Cache publication dispatch contains no caller code")
			.is_dispatched()
	}
}
pub(super) struct PreparedLeaseCachePublication {
	identity: NativeCachePublicationIdentity,
	registered: RegisteredCacheLeaseTransaction,
	claim: Claim,
	pub(super) deadline: Instant,
	progress: Arc<CachePublicationProgress>,
}
pub(super) struct CachePublicationOrigin {
	identity: CacheLeaseOriginIdentity,
	receipt: crate::db::transaction_ending::TransactionEndingReceipt,
	progress: Arc<CachePublicationProgress>,
}
impl CachePublicationOrigin {
	pub(super) fn receipt(&self) -> &crate::db::transaction_ending::TransactionEndingReceipt {
		&self.receipt
	}
	pub(super) fn ending_failure(
		&self,
	) -> Option<Arc<crate::db::transaction_ending::TransactionEndingResult>> {
		if !self.progress.dispatched() {
			return None;
		}
		match self.receipt.snapshot() {
			crate::db::transaction_ending::TransactionEndingSnapshot::Terminal(ending)
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

	pub(super) fn cleanup_origins(
		&self,
		fields: FinalPurposeCleanupOriginFields<'_>,
	) -> Option<OwnedPrimitiveOriginEndings> {
		if !self.progress.dispatched() {
			return None;
		}
		Some(OwnedPrimitiveOriginEndings::cache_lease(
			CacheLeaseOriginEndingsFields {
				identity: &self.identity,
				lease_operation: fields.lease_operation,
				publication: &self.receipt,
				preceding_operation: fields.preceding_operation,
				pending_origins: fields.pending_origins,
			},
		))
	}
}
pub(crate) struct NativeLeasePurposeErrorWithObserver {
	pub(crate) source: Arc<NativeLeaseDriverError>,
	pub(crate) observer: NativeLeaseCloseObserver,
}
impl std::fmt::Debug for NativeLeasePurposeErrorWithObserver {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("NativeLeasePurposeErrorWithObserver")
			.field("source", &self.source)
			.finish_non_exhaustive()
	}
}
impl std::fmt::Display for NativeLeasePurposeErrorWithObserver {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("native Lease final-purpose handoff failed")
	}
}
impl std::error::Error for NativeLeasePurposeErrorWithObserver {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(self.source.as_ref())
	}
}

impl NativeOwnedLeaseClient {
	pub(crate) async fn into_cache_publication(
		self,
		fields: NativeLeaseCachePublicationFields,
	) -> Result<NativeLeaseCachePublication, NativeLeasePurposeErrorWithObserver> {
		let observer = self.close_observer();
		if !Arc::ptr_eq(&self.shared, &fields.caller.shared) {
			return Err(NativeLeasePurposeErrorWithObserver {
				source: Arc::new(NativeLeaseDriverError::AuthorityLost),
				observer,
			});
		}
		let (response, receive) = oneshot::channel();
		self.cmds
			.send(LeaseCmd::CachePublication {
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
		Ok(NativeLeaseCachePublication {
			client: self,
			prepared,
		})
	}
}

impl LeaseDriver {
	pub(super) fn prepare_cache_publication(
		&mut self,
		mut fields: NativeLeaseCachePublicationFields,
	) -> Result<PreparedLeaseCachePublication, NativeLeaseDriverError> {
		let deadline = self.operation_deadline(fields.requested_at)?;
		Self::current_operation_deadline(
			&self.owner,
			&self.shared,
			Some(deadline),
			NativeLeaseWorkPhase::CachePublication,
		)?;
		if !Arc::ptr_eq(fields.identity.lease_bindings(), &self.lease.bindings)
			|| !fields.identity.matches_lease(&self.lease)
			|| !Arc::ptr_eq(&fields.caller.shared, &self.shared)
			|| !self.ownership.permits_final_purpose()
		{
			return Err(NativeLeaseDriverError::AuthorityLost);
		}
		let caller = fields.caller.take_caller();
		let registered = match self
			.lease
			.bindings
			.admission
			.prepare_cache_lease_publication(CacheLeaseRegistrationFields {
				owner: &self.owner,
				lease: &self.lease,
				identity: &fields.identity,
				caller,
			}) {
			Ok(registered) => registered,
			Err(rejected) => {
				fields.caller.restore_caller(rejected.caller);
				return Err(NativeLeaseDriverError::Operation(
					NativeLeaseError::Registered(rejected.source),
				));
			}
		};
		let progress = Arc::new(CachePublicationProgress {
			state: Mutex::new(PurposeDispatch::Registered),
		});
		let claim =
			self.ownership
				.transfer_to_final_purpose(LeaseFinalPurposeOrigin::CachePublication(
					CachePublicationOrigin {
						identity: fields.identity.origin_identity().clone(),
						receipt: registered.receipt(),
						progress: Arc::clone(&progress),
					},
				));
		Ok(PreparedLeaseCachePublication {
			identity: fields.identity,
			registered,
			claim,
			deadline,
			progress,
		})
	}
}
