use super::owned_primitive::{
	CacheLeaseOriginIdentity, OriginOrderingProgress, OwnedPrimitiveAdmissionError,
	OwnedPrimitiveOrderedOrigins, OwnedPrimitiveOrderingFields, OwnedPrimitiveOriginEndings,
	OwnedPrimitiveOriginOrdering,
};
use super::{
	Arc, NativeAdmissionError, NativeAdmittedContracts, NativeSourceGatePhase, PgNativeAdmission,
	WriteTx,
};
use crate::db::OwnedTransactionRegistration;
use crate::db::transaction_ending::{TransactionEndingDisposition, TransactionEndingReceipt};
use crate::fleet::cache::engine::publication_identity::NativeCachePublicationIdentity;
use crate::fleet::lease::Token;
use crate::fleet::lease::cleanup_target::LeaseCleanupTarget;
use crate::fleet::lease::{
	Claim, ClaimDuration, LeaseAcquisition, NativeLeaseError, PgNativeLease,
	TentativeLeaseAuthority,
};
use crate::installation::control_storage::ExclusiveControlOrderingProof;
use crate::runtime::local_admission::lease_work::{
	RuntimeLeaseCallerWorkPermit, RuntimeLeaseFinalPurposeAdmissionError,
};
use crate::runtime::local_admission::lease_work::{
	RuntimeLeaseContinuation, RuntimeLeaseWorkPermit,
};

/// One fixed lease purpose admits one continuation before registered BEGIN.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LeaseTransactionPurpose {
	Acquire,
	RenewHeld,
	Cleanup,
}

pub(crate) struct RegisteredLeaseTransaction<'a> {
	admission: &'a PgNativeAdmission,
	identity: crate::records::RecordIdentity,
	purpose: LeaseTransactionPurpose,
	receipt: TransactionEndingReceipt,
	/// The registration until the dispatch of its transaction takes it.
	registration: Option<OwnedTransactionRegistration>,
}
impl RegisteredLeaseTransaction<'_> {
	pub(crate) fn receipt(&self) -> &TransactionEndingReceipt {
		&self.receipt
	}
	pub(crate) fn take_registration(
		&mut self,
	) -> Result<OwnedTransactionRegistration, LeaseRegistrationError> {
		self.registration
			.take()
			.ok_or(LeaseRegistrationError::RegistrationConsumed)
	}
	fn require(
		&self,
		admission: &PgNativeAdmission,
		tx: &mut WriteTx<'_>,
		lease: &PgNativeLease,
		purpose: LeaseTransactionPurpose,
	) -> Result<(), LeaseRegistrationError> {
		if !std::ptr::eq(self.admission, admission)
			|| self.identity != *lease.identity()
			|| self.purpose != purpose
		{
			return Err(LeaseRegistrationError::BindingMismatch);
		}
		if !tx.ending_receipt().shares_ending(&self.receipt) {
			return Err(LeaseRegistrationError::EndingMismatch);
		}
		Ok(())
	}
}
#[derive(Debug, thiserror::Error)]
pub(crate) enum LeaseRegistrationError {
	#[error("Lease final-purpose local continuation failed")]
	FinalPurpose(#[source] RuntimeLeaseFinalPurposeAdmissionError),
	#[error("Lease final-purpose caller retention failed")]
	CallerRetention(#[source] crate::db::LeaseCallerRetentionError),
	#[error("Lease originating operation has not ended locally")]
	OriginPending,
	#[error("Lease physical ordering exceeded its attempt budget")]
	OrderingWaitElapsed,
	#[error("lease registered admission failed")]
	Admission(#[source] Box<NativeAdmissionError>),
	#[error("lease registered purpose or binding does not match")]
	BindingMismatch,
	#[error("lease transaction does not own its registered ending")]
	EndingMismatch,
	#[error("lease transaction registration was already consumed")]
	RegistrationConsumed,
}
impl LeaseRegistrationError {
	pub(crate) fn involves_database_failure(&self) -> bool {
		matches!(self, Self::Admission(error) if error.involves_database_failure())
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		matches!(self, Self::Admission(error) if error.commit_outcome_is_unknown())
	}
}

/// The task retains its exact receipt before this owned registration leaves the task.
pub(crate) struct RegisteredCacheLeaseTransaction {
	admission: Arc<PgNativeAdmission>,
	identity: CacheLeaseOriginIdentity,
	receipt: TransactionEndingReceipt,
	/// The registration until the dispatch of its transaction takes it.
	registration: Option<OwnedTransactionRegistration>,
}
impl RegisteredCacheLeaseTransaction {
	/// Authenticates the exact registered Cache purpose on its own current pinned snapshot.
	pub(crate) async fn authenticate_cache_publication(
		&self,
		tx: &mut WriteTx<'_>,
		identity: &NativeCachePublicationIdentity,
	) -> Result<NativeAdmittedContracts, LeaseRegistrationError> {
		self.require_ending(tx, identity)?;
		// A cache's computation lease is never shared with v10, so publication runs on every
		// storage route.
		self.admission.observe_lease_final_purpose(tx).await
	}
	pub(crate) fn receipt(&self) -> TransactionEndingReceipt {
		self.receipt.clone()
	}
	pub(crate) fn take_registration(
		&mut self,
	) -> Result<OwnedTransactionRegistration, LeaseRegistrationError> {
		self.registration
			.take()
			.ok_or(LeaseRegistrationError::RegistrationConsumed)
	}
	pub(crate) fn require_ending(
		&self,
		tx: &mut WriteTx<'_>,
		identity: &NativeCachePublicationIdentity,
	) -> Result<(), LeaseRegistrationError> {
		if !Arc::ptr_eq(&self.admission, identity.admission())
			|| &self.identity != identity.origin_identity()
		{
			return Err(LeaseRegistrationError::BindingMismatch);
		}
		if !tx.ending_receipt().shares_ending(&self.receipt) {
			return Err(LeaseRegistrationError::EndingMismatch);
		}
		Ok(())
	}
}
pub(crate) struct CacheLeaseRegistrationFields<'a> {
	pub(crate) owner: &'a RuntimeLeaseWorkPermit,
	pub(crate) lease: &'a PgNativeLease,
	pub(crate) identity: &'a NativeCachePublicationIdentity,
	pub(crate) caller: RuntimeLeaseCallerWorkPermit,
}
pub(crate) struct LeaseCallerRegistrationRejection {
	pub(crate) caller: RuntimeLeaseCallerWorkPermit,
	pub(crate) source: LeaseRegistrationError,
}

pub(crate) struct RegisteredOnceLeaseTransaction {
	admission: Arc<PgNativeAdmission>,
	once: Arc<crate::fleet::once::engine::PgNativeOnce>,
	receipt: TransactionEndingReceipt,
	/// The registration until the dispatch of its transaction takes it.
	registration: Option<OwnedTransactionRegistration>,
}
impl RegisteredOnceLeaseTransaction {
	pub(crate) fn receipt(&self) -> TransactionEndingReceipt {
		self.receipt.clone()
	}
	pub(crate) fn take_registration(
		&mut self,
	) -> Result<OwnedTransactionRegistration, LeaseRegistrationError> {
		self.registration
			.take()
			.ok_or(LeaseRegistrationError::RegistrationConsumed)
	}
	pub(crate) async fn authenticate_once_completion(
		&self,
		tx: &mut WriteTx<'_>,
		once: &Arc<crate::fleet::once::engine::PgNativeOnce>,
	) -> Result<NativeAdmittedContracts, LeaseRegistrationError> {
		if !Arc::ptr_eq(&self.once, once) || !Arc::ptr_eq(&self.admission, once.admission()) {
			return Err(LeaseRegistrationError::BindingMismatch);
		}
		if !tx.ending_receipt().shares_ending(&self.receipt) {
			return Err(LeaseRegistrationError::EndingMismatch);
		}
		self.admission
			.authenticate_native_lease_final_purpose(tx)
			.await
	}
}
pub(crate) struct OnceLeaseRegistrationFields<'a> {
	pub(crate) owner: &'a RuntimeLeaseWorkPermit,
	pub(crate) once: &'a Arc<crate::fleet::once::engine::PgNativeOnce>,
	pub(crate) caller: RuntimeLeaseCallerWorkPermit,
}

pub(crate) struct LeaseFinalPurposeCleanupInvocation<'a> {
	pub(crate) origins: &'a OwnedPrimitiveOriginEndings,
	pub(crate) ordering: &'a mut OriginOrderingProgress<OwnedPrimitiveOriginOrdering>,
}

#[derive(Clone, Copy)]
pub(crate) enum LeaseCleanupResult {
	Release(bool),
	PhysicalProofOnly,
}

/// Exact originating ending evidence; it grants no participant or lease mutation permission.
#[derive(Debug)]
pub(crate) struct LeaseOriginEndingProof {
	_receipt: TransactionEndingReceipt,
	_ordering: LeaseOriginOrdering,
}

#[derive(Debug)]
enum LeaseOriginOrdering {
	ConfirmedEnding,
	ExclusiveControl {
		_proof: ExclusiveControlOrderingProof,
	},
}

/// This sealed context is consumed only by exact fixed lease cleanup on its pinned owned Tx.
struct PreparedLeaseCleanup {
	identity: crate::records::RecordIdentity,
	source_phase: Option<NativeSourceGatePhase>,
	record: Result<
		crate::records::PointRecordRead,
		crate::installation::control_storage::ControlPinError,
	>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum LeaseCleanupPhase {
	OriginOrdering,
	AuthenticationAndRelease(tokio::time::Instant),
}

#[derive(Clone, Copy)]
pub(crate) enum LeaseCleanupExecution {
	Release,
	PhysicalProofOnly,
}

pub(crate) struct LeaseCleanupInvocation<'a> {
	pub(crate) owner: &'a RuntimeLeaseWorkPermit,
	pub(crate) lease: &'a PgNativeLease,
	pub(crate) target: &'a LeaseCleanupTarget,
	pub(crate) origin: &'a TransactionEndingReceipt,
	pub(crate) proof: &'a mut OriginOrderingProgress<LeaseOriginEndingProof>,
	pub(crate) phase: &'a tokio::sync::watch::Sender<LeaseCleanupPhase>,
	pub(crate) execution: LeaseCleanupExecution,
	pub(crate) final_purpose: Option<LeaseFinalPurposeCleanupInvocation<'a>>,
}

impl PgNativeAdmission {
	async fn observe_lease_final_purpose(
		&self,
		tx: &mut WriteTx<'_>,
	) -> Result<NativeAdmittedContracts, LeaseRegistrationError> {
		let observation = self
			.observe(tx)
			.await
			.map_err(|error| LeaseRegistrationError::Admission(Box::new(error)))?;
		Ok(NativeAdmittedContracts { observation })
	}
	async fn authenticate_native_lease_final_purpose(
		&self,
		tx: &mut WriteTx<'_>,
	) -> Result<NativeAdmittedContracts, LeaseRegistrationError> {
		let contracts = self.observe_lease_final_purpose(tx).await?;
		contracts
			.require_native_storage()
			.map_err(|error| LeaseRegistrationError::Admission(Box::new(error)))?;
		Ok(contracts)
	}
	pub(crate) fn prepare_once_lease_completion(
		self: &Arc<Self>,
		fields: OnceLeaseRegistrationFields<'_>,
	) -> Result<RegisteredOnceLeaseTransaction, LeaseCallerRegistrationRejection> {
		let OnceLeaseRegistrationFields {
			owner,
			once,
			caller,
		} = fields;
		if let Err(source) = self.require_lease_binding(once.lease()) {
			return Err(LeaseCallerRegistrationRejection {
				caller,
				source: LeaseRegistrationError::Admission(Box::new(source)),
			});
		}
		if !Arc::ptr_eq(self, once.admission()) {
			return Err(LeaseCallerRegistrationRejection {
				caller,
				source: LeaseRegistrationError::BindingMismatch,
			});
		}
		let permit = match self.config.local_admission.try_admit_lease_once_completion(
			owner,
			once.lease().identity(),
			&caller,
		) {
			Ok(permit) => permit,
			Err(source) => {
				return Err(LeaseCallerRegistrationRejection {
					caller,
					source: LeaseRegistrationError::FinalPurpose(source),
				});
			}
		};
		let mut registration = OwnedTransactionRegistration::new(permit);
		if let Err(rejected) = registration.retain_lease_caller(caller) {
			return Err(LeaseCallerRegistrationRejection {
				caller: rejected.caller,
				source: LeaseRegistrationError::CallerRetention(rejected.source),
			});
		}
		Ok(RegisteredOnceLeaseTransaction {
			admission: Arc::clone(self),
			once: Arc::clone(once),
			receipt: registration.receipt(),
			registration: Some(registration),
		})
	}
	pub(crate) fn prepare_cache_lease_publication(
		self: &Arc<Self>,
		fields: CacheLeaseRegistrationFields<'_>,
	) -> Result<RegisteredCacheLeaseTransaction, LeaseCallerRegistrationRejection> {
		let CacheLeaseRegistrationFields {
			owner,
			lease,
			identity,
			caller,
		} = fields;
		if let Err(error) = self.require_lease_binding(lease) {
			return Err(LeaseCallerRegistrationRejection {
				caller,
				source: LeaseRegistrationError::Admission(Box::new(error)),
			});
		}
		if !Arc::ptr_eq(self, identity.admission()) || identity.lease_identity() != lease.identity()
		{
			return Err(LeaseCallerRegistrationRejection {
				caller,
				source: LeaseRegistrationError::BindingMismatch,
			});
		}
		let permit = match self
			.config
			.local_admission
			.try_admit_lease_cache_publication(owner, lease.identity(), &caller)
		{
			Ok(permit) => permit,
			Err(error) => {
				return Err(LeaseCallerRegistrationRejection {
					caller,
					source: LeaseRegistrationError::FinalPurpose(error),
				});
			}
		};
		let mut registration = OwnedTransactionRegistration::new(permit);
		if let Err(rejected) = registration.retain_lease_caller(caller) {
			return Err(LeaseCallerRegistrationRejection {
				caller: rejected.caller,
				source: LeaseRegistrationError::CallerRetention(rejected.source),
			});
		}
		Ok(RegisteredCacheLeaseTransaction {
			admission: Arc::clone(self),
			identity: identity.origin_identity().clone(),
			receipt: registration.receipt(),
			registration: Some(registration),
		})
	}

	/// Registers one exact lease owner before randomness or protected work.
	pub(crate) fn register_lease_work(
		&self,
		lease: &PgNativeLease,
	) -> Result<RuntimeLeaseWorkPermit, NativeAdmissionError> {
		self.require_lease_binding(lease)?;
		self.config
			.local_admission
			.try_register_lease_work(lease.identity().clone())
			.map_err(NativeAdmissionError::Local)
	}

	pub(crate) fn prepare_owned_lease_acquisition<'a>(
		&'a self,
		owner: &RuntimeLeaseWorkPermit,
		lease: &PgNativeLease,
	) -> Result<RegisteredLeaseTransaction<'a>, LeaseRegistrationError> {
		self.prepare_owned_lease_transaction(owner, lease, LeaseTransactionPurpose::Acquire)
	}
	pub(crate) fn prepare_owned_lease_renewal<'a>(
		&'a self,
		owner: &RuntimeLeaseWorkPermit,
		lease: &PgNativeLease,
	) -> Result<RegisteredLeaseTransaction<'a>, LeaseRegistrationError> {
		self.prepare_owned_lease_transaction(owner, lease, LeaseTransactionPurpose::RenewHeld)
	}
	pub(crate) fn prepare_owned_lease_cleanup_transaction<'a>(
		&'a self,
		owner: &RuntimeLeaseWorkPermit,
		lease: &PgNativeLease,
	) -> Result<RegisteredLeaseTransaction<'a>, LeaseRegistrationError> {
		self.prepare_owned_lease_transaction(owner, lease, LeaseTransactionPurpose::Cleanup)
	}
	fn prepare_owned_lease_transaction<'a>(
		&'a self,
		owner: &RuntimeLeaseWorkPermit,
		lease: &PgNativeLease,
		purpose: LeaseTransactionPurpose,
	) -> Result<RegisteredLeaseTransaction<'a>, LeaseRegistrationError> {
		self.require_lease_binding(lease)
			.map_err(|error| LeaseRegistrationError::Admission(Box::new(error)))?;
		let permit = match purpose {
			LeaseTransactionPurpose::Acquire => self
				.config
				.local_admission
				.try_admit_lease_acquisition(owner, lease.identity()),
			LeaseTransactionPurpose::RenewHeld => {
				self.config.local_admission.try_admit_lease_continuation(
					owner,
					lease.identity(),
					RuntimeLeaseContinuation::RenewHeld,
				)
			}
			LeaseTransactionPurpose::Cleanup => {
				self.config.local_admission.try_admit_lease_continuation(
					owner,
					lease.identity(),
					RuntimeLeaseContinuation::ReleaseCleanup,
				)
			}
		}
		.map_err(|error| {
			LeaseRegistrationError::Admission(Box::new(NativeAdmissionError::Local(error)))
		})?;
		let registration = OwnedTransactionRegistration::new(permit);
		Ok(RegisteredLeaseTransaction {
			admission: self,
			identity: lease.identity().clone(),
			purpose,
			receipt: registration.receipt(),
			registration: Some(registration),
		})
	}

	/// Fixed library acquisition on one owned control transaction; no application callback is
	/// accepted.
	pub(crate) async fn acquire_owned_lease(
		&self,
		tx: &mut WriteTx<'_>,
		registered: &RegisteredLeaseTransaction<'_>,
		lease: &PgNativeLease,
		acquisition: LeaseAcquisition,
	) -> Result<Option<TentativeLeaseAuthority>, NativeLeaseError> {
		self.require_lease_binding(lease)
			.map_err(NativeLeaseError::Admission)?;
		registered
			.require(self, tx, lease, LeaseTransactionPurpose::Acquire)
			.map_err(NativeLeaseError::Registered)?;
		let mut completion = tx.track_state_mutation();
		let observation = self
			.observe(tx)
			.await
			.map_err(NativeLeaseError::Admission)?;
		let result = lease
			.acquire_admitted(NativeAdmittedContracts { observation }, tx, acquisition)
			.await;
		if result.is_ok() {
			completion.complete();
		}
		result
	}

	/// Renews only existing locally authorized held work under actual retained database contracts.
	pub(crate) async fn renew_owned_lease(
		&self,
		tx: &mut WriteTx<'_>,
		registered: &RegisteredLeaseTransaction<'_>,
		lease: &PgNativeLease,
		authority: &Claim,
		next_token: Token,
		duration: ClaimDuration,
	) -> Result<Option<TentativeLeaseAuthority>, NativeLeaseError> {
		self.require_lease_binding(lease)
			.map_err(NativeLeaseError::Admission)?;
		registered
			.require(self, tx, lease, LeaseTransactionPurpose::RenewHeld)
			.map_err(NativeLeaseError::Registered)?;
		let mut completion = tx.track_state_mutation();
		let observation = self
			.observe(tx)
			.await
			.map_err(NativeLeaseError::Admission)?;
		let result = lease
			.renew_admitted(
				NativeAdmittedContracts { observation },
				tx,
				authority,
				next_token,
				duration,
			)
			.await;
		if result.is_ok() {
			completion.complete();
		}
		result
	}

	/// One fixed owned operation retains its exact pinned snapshot through authentication and
	/// release.
	pub(crate) async fn execute_owned_lease_cleanup(
		&self,
		tx: &mut WriteTx<'_>,
		registered: &RegisteredLeaseTransaction<'_>,
		invocation: LeaseCleanupInvocation<'_>,
	) -> Result<LeaseCleanupResult, NativeLeaseError> {
		let LeaseCleanupInvocation {
			owner: _,
			lease,
			target,
			origin,
			proof,
			phase,
			execution,
			final_purpose,
		} = invocation;
		let prepared = self
			.prepare_owned_lease_cleanup(tx, registered, lease, origin, proof, final_purpose)
			.await?;
		if *phase.borrow() == LeaseCleanupPhase::OriginOrdering {
			phase.send_replace(LeaseCleanupPhase::AuthenticationAndRelease(
				tokio::time::Instant::now(),
			));
		}
		match execution {
			LeaseCleanupExecution::PhysicalProofOnly => prepared
				.record
				.map_err(NativeAdmissionError::ControlPin)
				.map_err(NativeLeaseError::Admission)
				.map(|_| LeaseCleanupResult::PhysicalProofOnly),
			LeaseCleanupExecution::Release => self
				.release_prepared_owned_lease_cleanup(tx, lease, target, prepared)
				.await
				.map(LeaseCleanupResult::Release),
		}
	}

	/// Prepares one exact cleanup under source-before-control pins; no application callback is
	/// accepted.
	async fn prepare_owned_lease_cleanup(
		&self,
		tx: &mut WriteTx<'_>,
		registered: &RegisteredLeaseTransaction<'_>,
		lease: &PgNativeLease,
		origin: &TransactionEndingReceipt,
		proof: &mut OriginOrderingProgress<LeaseOriginEndingProof>,
		final_purpose: Option<LeaseFinalPurposeCleanupInvocation<'_>>,
	) -> Result<PreparedLeaseCleanup, NativeLeaseError> {
		self.require_lease_binding(lease)
			.map_err(NativeLeaseError::Admission)?;
		let ending = origin.wait_until_terminal().await;
		registered
			.require(self, tx, lease, LeaseTransactionPurpose::Cleanup)
			.map_err(NativeLeaseError::Registered)?;
		if let Some(final_purpose) = final_purpose {
			if final_purpose
				.origins
				.lease_final_identity()
				.is_none_or(|identity| identity.record() != lease.identity())
			{
				return Err(NativeLeaseError::Registered(
					LeaseRegistrationError::BindingMismatch,
				));
			}
			let ordered = OwnedPrimitiveOrderedOrigins::new(final_purpose.origins.clone())
				.map_err(map_lease_ordering_error)?;
			let (source_phase, record) = self
				.observe_owned_primitive_origins(
					tx,
					OwnedPrimitiveOrderingFields {
						origins: &ordered,
						ordering: final_purpose.ordering,
					},
				)
				.await
				.map_err(map_lease_ordering_error)?;
			return Ok(PreparedLeaseCleanup {
				identity: lease.identity().clone(),
				source_phase,
				record: Ok(record),
			});
		}
		let source_phase = self
			.pin_source(tx)
			.await
			.map_err(NativeAdmissionError::from)
			.map_err(NativeLeaseError::Admission)?;
		let record = match ending.disposition() {
			TransactionEndingDisposition::KnownCommit
			| TransactionEndingDisposition::KnownRollback => {
				*proof = OriginOrderingProgress::Proven(LeaseOriginEndingProof {
					_receipt: origin.clone(),
					_ordering: LeaseOriginOrdering::ConfirmedEnding,
				});
				self.config
					.control_storage
					.read_pinned_shared(tx)
					.await
					.map_err(NativeAdmissionError::from)
					.map_err(NativeLeaseError::Admission)?
			}
			TransactionEndingDisposition::Unknown
			| TransactionEndingDisposition::UnconfirmedDisposal
			| TransactionEndingDisposition::TaskAbandoned => {
				let observation = self
					.config
					.control_storage
					.read_pinned_exclusive_physical(tx)
					.await
					.map_err(NativeAdmissionError::from)
					.map_err(NativeLeaseError::Admission)?;
				let (ordering_proof, record) = observation.into_parts();
				*proof = OriginOrderingProgress::Proven(LeaseOriginEndingProof {
					_receipt: origin.clone(),
					_ordering: LeaseOriginOrdering::ExclusiveControl {
						_proof: ordering_proof,
					},
				});
				return Ok(PreparedLeaseCleanup {
					identity: lease.identity().clone(),
					source_phase,
					record,
				});
			}
		};
		Ok(PreparedLeaseCleanup {
			identity: lease.identity().clone(),
			source_phase,
			record: Ok(record),
		})
	}

	/// Consumes the exact prepared snapshot in the same owned transaction and authenticates before
	/// release.
	async fn release_prepared_owned_lease_cleanup(
		&self,
		tx: &mut WriteTx<'_>,
		lease: &PgNativeLease,
		target: &LeaseCleanupTarget,
		prepared: PreparedLeaseCleanup,
	) -> Result<bool, NativeLeaseError> {
		self.require_lease_binding(lease)
			.map_err(NativeLeaseError::Admission)?;
		if prepared.identity != *lease.identity() {
			return Err(NativeLeaseError::Admission(
				NativeAdmissionError::LeaseOperationBindingMismatch,
			));
		}
		let mut completion = tx.track_state_mutation();
		let snapshot = prepared
			.record
			.map_err(NativeAdmissionError::ControlPin)
			.map_err(NativeLeaseError::Admission)?;
		let observation = self
			.authenticate_observation(tx, prepared.source_phase, snapshot)
			.await
			.map_err(NativeLeaseError::Admission)?;
		let result = lease
			.release_cleanup_admitted(NativeAdmittedContracts { observation }, tx, target)
			.await;
		if result.is_ok() {
			completion.complete();
		}
		result
	}

	fn require_lease_binding(&self, lease: &PgNativeLease) -> Result<(), NativeAdmissionError> {
		if !lease.shares_admission(self) {
			return Err(NativeAdmissionError::LeaseOperationBindingMismatch);
		}
		Ok(())
	}
}

pub(crate) fn map_lease_ordering_error(error: OwnedPrimitiveAdmissionError) -> NativeLeaseError {
	match error {
		OwnedPrimitiveAdmissionError::Admission(error) => NativeLeaseError::Admission(*error),
		OwnedPrimitiveAdmissionError::EndingMismatch => {
			NativeLeaseError::Registered(LeaseRegistrationError::EndingMismatch)
		}
		OwnedPrimitiveAdmissionError::OriginPending => {
			NativeLeaseError::Registered(LeaseRegistrationError::OriginPending)
		}
		OwnedPrimitiveAdmissionError::OrderingWaitElapsed => {
			NativeLeaseError::Registered(LeaseRegistrationError::OrderingWaitElapsed)
		}
	}
}
