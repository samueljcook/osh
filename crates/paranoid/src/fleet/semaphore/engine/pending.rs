//! Persistent private Semaphore material and exact registered endings.

#[cfg(test)]
mod tests;

use crate::runtime::local_admission::membership::RuntimeResourceWorkState;
use std::{sync::Arc, time::Duration};
use tokio::time::Instant;

use super::{
	NativeSemaphoreError, NativeSemaphoreExecutionBindings, PgNativeSemaphore, PointRecordLock,
	RecordMetadata, SemaphoreDriverTiming, SemaphoreResourceWriteFields, StateStorageError,
	TransactionFailure,
};
use crate::db::transaction_ending::{
	TransactionEndingDisposition, TransactionEndingReceipt, TransactionEndingResult,
	TransactionEndingSnapshot,
};
use crate::db::{
	OwnedTransactionRegistration, WriteTx,
	finish_pool_owned_write_transaction_and_preserve_rollback_error,
};
use crate::fleet::primitive_scope::SemaphoreUndeliveredScopeReleaseFields;
use crate::fleet::semaphore::state::{
	NativeSemaphoreState, NativeSemaphoreStateFields, SemaphoreAcquireInput, SemaphoreReleaseInput,
};
use crate::fleet::semaphore::state::{SemaphoreUndeliveredTarget, TentativeSemaphoreGrant};
use crate::fleet::{HolderId, SemaphoreClaim, SemaphoreClaimSnapshot};
use crate::runtime::admission::owned_primitive::OwnedPrimitiveAdmissionError;
use crate::runtime::admission::owned_primitive::{
	OriginOrderingProgress, OwnedPrimitiveCleanupPhase, OwnedPrimitiveOriginEndings,
	OwnedPrimitiveOriginOrdering, OwnedPrimitivePendingOrigins, SemaphoreOriginEndingsFields,
};
use crate::runtime::local_admission::semaphore_work::{
	RuntimeSemaphoreCallerWorkPermit, RuntimeSemaphoreWorkIdentity, RuntimeSemaphoreWorkPermit,
};
use crate::runtime::local_admission::{
	RuntimeAdmissionError, RuntimeAdmissionStop, RuntimeAdmissionTracker,
};

pub(crate) struct NativeSemaphoreAcquireFields {
	pub(crate) holder: HolderId,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeSemaphoreCompletionObservation {
	DeniedAcquisition,
	Released(NativeSemaphoreReleaseObservation),
}
/// The snapshot and timing provide no private claim or caller permission.
pub(super) struct NativeSemaphoreHeldObservation {
	pub(super) claim: SemaphoreClaimSnapshot,
	pub(super) hold_until: Instant,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeSemaphoreAcquisitionObservation {
	Acquired,
	Unavailable,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeSemaphoreCleanupObservation {
	NotAcquired,
	Released(NativeSemaphoreReleaseObservation),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeSemaphoreReleaseObservation {
	Released,
	NoMatch,
}
enum OperationState {
	New,
	AcquisitionPending {
		hold_until: Instant,
		candidate: AcquisitionCandidate,
	},
	ConfirmingHeld {
		claim: SemaphoreClaim,
		hold_until: Instant,
		target: DeliveryTargetState,
	},
	Held {
		claim: SemaphoreClaim,
		hold_until: Instant,
		target: DeliveryTargetState,
	},
	ReleasePending(SemaphoreClaim),
	CleanupPending(DeliveryTargetState),
	Committed(CompletedSemaphoreOperation),
	DenialSettled(TransactionEndingReceipt),
	Complete,
}

enum AcquisitionCandidate {
	Unprepared,
	Prepared(Box<PreparedAcquisition>),
}

struct PreparedAcquisition {
	grant: TentativeSemaphoreGrant,
	target: SemaphoreUndeliveredTarget,
}

enum DeliveryTargetState {
	Retained(SemaphoreUndeliveredTarget),
	Unavailable,
}

impl DeliveryTargetState {
	fn take(&mut self) -> Option<SemaphoreUndeliveredTarget> {
		match std::mem::replace(self, Self::Unavailable) {
			Self::Retained(target) => Some(target),
			Self::Unavailable => None,
		}
	}
	fn target(&self) -> Option<&SemaphoreUndeliveredTarget> {
		match self {
			Self::Retained(target) => Some(target),
			Self::Unavailable => None,
		}
	}
}

enum ReceiptHistory {
	Unstarted,
	Started {
		acquisition: TransactionEndingReceipt,
		first_release: std::sync::OnceLock<TransactionEndingReceipt>,
		latest: TransactionEndingReceipt,
	},
}

impl ReceiptHistory {
	/// Records `receipt` as the latest registered transaction. A release attempt while the
	/// operation's state is `ReleasePending` also becomes the first claimed release when none is
	/// recorded yet.
	fn record(
		&mut self,
		purpose: AttemptPurpose,
		state: &OperationState,
		receipt: &TransactionEndingReceipt,
	) -> Result<(), NativeSemaphoreError> {
		let Self::Started {
			latest,
			first_release,
			..
		} = self
		else {
			return Err(NativeSemaphoreError::OperationState);
		};
		match purpose {
			AttemptPurpose::Release if matches!(state, OperationState::ReleasePending(_)) => {
				first_release.get_or_init(|| receipt.clone());
			}
			AttemptPurpose::Acquisition | AttemptPurpose::Release | AttemptPurpose::Proof => {}
		}
		*latest = receipt.clone();
		Ok(())
	}
}

/// The purpose of a registered transaction. Only a release attempt for a pending claimed release
/// can become the first claimed release; an acquisition and a proof of physical ordering never do.
enum AttemptPurpose {
	Acquisition,
	Release,
	Proof,
}

enum AttemptState {
	Unregistered,
	Registered {
		receipt: TransactionEndingReceipt,
		ordering: OriginOrderingProgress<OwnedPrimitiveOriginOrdering>,
	},
}

impl AttemptState {
	fn ordering_mut(
		&mut self,
	) -> Result<&mut OriginOrderingProgress<OwnedPrimitiveOriginOrdering>, NativeSemaphoreError> {
		match self {
			Self::Registered { ordering, .. } => Ok(ordering),
			Self::Unregistered => Err(NativeSemaphoreError::OperationState),
		}
	}
	fn ordering(&self) -> Option<&OwnedPrimitiveOriginOrdering> {
		match self {
			Self::Registered { ordering, .. } => ordering.proof(),
			Self::Unregistered => None,
		}
	}
}

enum OriginSettlement {
	Awaiting,
	Settled(SettledSemaphoreOrigins),
}
/// Constructed only after actual successful owned finish, before fallible local settlement.
enum CompletedSemaphoreOperation {
	Denied {
		receipt: TransactionEndingReceipt,
	},
	Released {
		receipt: TransactionEndingReceipt,
		result: NativeSemaphoreReleaseObservation,
		dispatch: ReleaseDispatch,
		origins: OwnedPrimitiveOriginEndings,
		ordering: OwnedPrimitiveOriginOrdering,
	},
}
impl CompletedSemaphoreOperation {
	fn receipt(&self) -> &TransactionEndingReceipt {
		match self {
			Self::Denied { receipt } | Self::Released { receipt, .. } => receipt,
		}
	}
}
struct SettledSemaphoreOrigins {
	receipt: TransactionEndingReceipt,
	origins: OwnedPrimitiveOriginEndings,
	ordering: SettledOriginOrdering,
}

enum SettledOriginOrdering {
	KnownEndings,
	Proven(OwnedPrimitiveOriginOrdering),
}
/// Diagnostics retain bounded exact causes and grant no slot, caller or SQL authority.
pub(crate) struct NativeSemaphoreDiagnosticError {
	primary_error: Option<Arc<NativeSemaphoreError>>,
	cleanup_error: Option<Arc<NativeSemaphoreError>>,
	acquisition_ending: Option<Arc<TransactionEndingResult>>,
	first_release_ending: Option<Arc<TransactionEndingResult>>,
	latest_ending: Option<Arc<TransactionEndingResult>>,
	unsettled_ending: Option<Arc<TransactionEndingResult>>,
}
impl NativeSemaphoreDiagnosticError {
	pub(crate) fn primary_error(&self) -> Option<&NativeSemaphoreError> {
		self.primary_error.as_deref()
	}
	pub(crate) fn cleanup_error(&self) -> Option<&NativeSemaphoreError> {
		self.cleanup_error.as_deref()
	}
	pub(crate) fn acquisition_ending(&self) -> Option<&TransactionEndingResult> {
		self.acquisition_ending.as_deref()
	}
	pub(crate) fn first_release_ending(&self) -> Option<&TransactionEndingResult> {
		self.first_release_ending.as_deref()
	}
	pub(crate) fn unsettled_ending(&self) -> Option<&TransactionEndingResult> {
		self.unsettled_ending.as_deref()
	}
	pub(crate) fn latest_ending(&self) -> Option<&TransactionEndingResult> {
		self.latest_ending.as_deref()
	}
}
impl std::fmt::Debug for NativeSemaphoreDiagnosticError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("NativeSemaphoreDiagnosticError { bounded_causes_retained: true }")
	}
}
impl std::fmt::Display for NativeSemaphoreDiagnosticError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("native Semaphore operation, cleanup and transaction-ending diagnostics")
	}
}
impl std::error::Error for NativeSemaphoreDiagnosticError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		if let Some(error) = self.primary_error().or(self.cleanup_error()) {
			return Some(error);
		}
		self.acquisition_ending()
			.and_then(TransactionEndingResult::error)
			.or_else(|| {
				self.first_release_ending()
					.and_then(TransactionEndingResult::error)
			})
			.or_else(|| {
				self.latest_ending()
					.and_then(TransactionEndingResult::error)
			})
			.or_else(|| {
				self.unsettled_ending()
					.and_then(TransactionEndingResult::error)
			})
			.map(|error| error as &(dyn std::error::Error + 'static))
	}
}

/// No claim, target or cleanup capability leaves this fixed operation owner.
pub(crate) struct NativeSemaphoreOperation {
	bindings: Arc<NativeSemaphoreExecutionBindings>,
	identity: RuntimeSemaphoreWorkIdentity,
	owner: RuntimeSemaphoreWorkPermit,
	state: OperationState,
	receipts: ReceiptHistory,
	attempt: AttemptState,
	pending_origins: OwnedPrimitivePendingOrigins,
	proof_settlement: OriginSettlement,
	primary_error: Option<Arc<NativeSemaphoreError>>,
	cleanup_error: Option<Arc<NativeSemaphoreError>>,
}
impl NativeSemaphoreOperation {
	pub(super) fn new(
		bindings: Arc<NativeSemaphoreExecutionBindings>,
		identity: RuntimeSemaphoreWorkIdentity,
		owner: RuntimeSemaphoreWorkPermit,
	) -> Self {
		Self {
			bindings,
			identity,
			owner,
			state: OperationState::New,
			receipts: ReceiptHistory::Unstarted,
			attempt: AttemptState::Unregistered,
			pending_origins: OwnedPrimitivePendingOrigins::new(),
			proof_settlement: OriginSettlement::Awaiting,
			primary_error: None,
			cleanup_error: None,
		}
	}
	fn acquisition(&self) -> Option<&TransactionEndingReceipt> {
		match &self.receipts {
			ReceiptHistory::Unstarted => None,
			ReceiptHistory::Started { acquisition, .. } => Some(acquisition),
		}
	}
	fn latest(&self) -> Option<&TransactionEndingReceipt> {
		match &self.receipts {
			ReceiptHistory::Unstarted => None,
			ReceiptHistory::Started { latest, .. } => Some(latest),
		}
	}
	fn first_release(&self) -> Option<&TransactionEndingReceipt> {
		match &self.receipts {
			ReceiptHistory::Unstarted => None,
			ReceiptHistory::Started { first_release, .. } => first_release.get(),
		}
	}
	fn register_attempt(
		&mut self,
		purpose: AttemptPurpose,
		receipt: TransactionEndingReceipt,
	) -> Result<(), NativeSemaphoreError> {
		self.receipts.record(purpose, &self.state, &receipt)?;
		self.attempt = AttemptState::Registered {
			receipt,
			ordering: OriginOrderingProgress::Awaiting,
		};
		Ok(())
	}
	fn pending_release(&self) -> Result<&SemaphoreClaim, NativeSemaphoreError> {
		match &self.state {
			OperationState::ReleasePending(claim) => Ok(claim),
			_ => Err(NativeSemaphoreError::OperationState),
		}
	}
	fn cleanup_target(&self) -> Option<&SemaphoreUndeliveredTarget> {
		match &self.state {
			OperationState::CleanupPending(target) => target.target(),
			_ => None,
		}
	}
	fn completed(&self) -> Option<&CompletedSemaphoreOperation> {
		match &self.state {
			OperationState::Committed(completed) => Some(completed),
			_ => None,
		}
	}
	pub(crate) fn diagnostic(&self) -> Arc<NativeSemaphoreDiagnosticError> {
		fn terminal(
			receipt: Option<&TransactionEndingReceipt>,
		) -> Option<Arc<TransactionEndingResult>> {
			match receipt?.snapshot() {
				TransactionEndingSnapshot::Terminal(ending) => Some(ending),
				_ => None,
			}
		}
		Arc::new(NativeSemaphoreDiagnosticError {
			primary_error: self.primary_error.clone(),
			cleanup_error: self.cleanup_error.clone(),
			acquisition_ending: terminal(self.acquisition()),
			first_release_ending: terminal(self.first_release()),
			latest_ending: terminal(self.latest()),
			unsettled_ending: self.pending_origins.ending(),
		})
	}
	/// This observation contains no usable claim or caller-task permission.
	pub(crate) async fn acquire_attempt(
		&mut self,
		fields: NativeSemaphoreAcquireFields,
	) -> Result<NativeSemaphoreAcquisitionObservation, Arc<NativeSemaphoreError>> {
		self.begin_attempt();
		let result = self.acquire_attempt_inner(fields).await;
		self.retain_result(result, false)
	}
	async fn acquire_attempt_inner(
		&mut self,
		fields: NativeSemaphoreAcquireFields,
	) -> Result<NativeSemaphoreAcquisitionObservation, NativeSemaphoreError> {
		if !matches!(self.state, OperationState::New) {
			return Err(NativeSemaphoreError::OperationState);
		}
		let bindings = Arc::clone(&self.bindings);
		let handle = &bindings.handle;
		let runtime = self.owner.admission_tracker();
		// The acquisition waits for its turn holding no connection, so waiting for the key under
		// contention bounds nothing; the bound below starts with the turn.
		let _turn = runtime.take_record_turn(&handle.identity).await;
		let start = Instant::now();
		require_confirmed_lifetime(&runtime)?;
		let hold = Duration::from_micros(
			u64::try_from(handle.settings.max_hold_micros())
				.expect("parsed positive hold duration"),
		);
		// The stored hold starts when the transaction observes the database's time, after this
		// local start, so the local end of the hold is never later than the stored end.
		let hold_until = checked_deadline(start, hold)?;
		let mut pack = handle
			.bindings
			.admission
			.register_semaphore_acquisition(&self.owner, &self.identity)?;
		let receipt = pack.receipt();
		self.receipts = ReceiptHistory::Started {
			acquisition: receipt.clone(),
			first_release: std::sync::OnceLock::new(),
			latest: receipt.clone(),
		};
		self.register_attempt(AttemptPurpose::Acquisition, receipt)?;
		self.state = OperationState::AcquisitionPending {
			hold_until,
			candidate: AcquisitionCandidate::Unprepared,
		};
		let acquired = execute_registered(
			&bindings,
			pack.take_registration(),
			"fleet.native_semaphore.acquire",
			OperationWait::ConfirmedLifetime(runtime.clone()),
			async |tx| {
				let contracts = pack
					.authenticate(tx, self.attempt.ordering_mut()?, None)
					.await?;
				let declaration = handle.bindings.code.declaration();
				let writer = contracts
					.bind_semaphore_writer(&declaration)
					.map_err(OwnedPrimitiveAdmissionError::from)?;
				let resource = writer.bind_resource(resource_fields(handle))?;
				let lock = handle
					.bindings
					.storage
					.lock(tx, &handle.identity, handle.bindings.conflict_attempts)
					.await?;
				let prepared = (|| {
					require_confirmed_lifetime(&runtime)?;
					let mut state = match lock.snapshot().record() {
						Some(record) => resource.decode_record(record)?,
						None => NativeSemaphoreState::new(NativeSemaphoreStateFields {
							key: handle.key.clone(),
							settings: handle.settings,
						}),
					};
					let Some(acquisition) = resource.acquire(
						&mut state,
						SemaphoreAcquireInput {
							holder: fields.holder,
							observed_at: lock.snapshot().observed_at(),
						},
					)?
					else {
						return Ok(None);
					};
					let (tentative, target) = acquisition.into_parts();
					let OperationState::AcquisitionPending { candidate, .. } = &mut self.state
					else {
						return Err(NativeSemaphoreError::OperationState);
					};
					*candidate = AcquisitionCandidate::Prepared(Box::new(PreparedAcquisition {
						grant: tentative,
						target,
					}));
					let record = resource.encode_record(
						&state,
						RecordMetadata {
							expires_at: None,
							updated_at: lock.snapshot().observed_at(),
						},
					)?;
					require_confirmed_lifetime(&runtime)?;
					Ok::<_, NativeSemaphoreError>(Some(record))
				})()
				.map_err(|error| identified(handle, error));
				let record = match prepared {
					Ok(record) => record,
					Err(error) => return reject_preparation(lock, error).await,
				};
				match record {
					Some(record) => {
						lock.write(record).await?;
						require_confirmed_lifetime(&runtime)?;
						Ok(true)
					}
					None => {
						lock.keep().await?;
						require_confirmed_lifetime(&runtime)?;
						Ok(false)
					}
				}
			},
		)
		.await?;
		if acquired {
			let OperationState::AcquisitionPending {
				hold_until,
				candidate,
			} = &mut self.state
			else {
				return Err(NativeSemaphoreError::OperationState);
			};
			let AcquisitionCandidate::Prepared(material) =
				std::mem::replace(candidate, AcquisitionCandidate::Unprepared)
			else {
				return Err(NativeSemaphoreError::OperationState);
			};
			let PreparedAcquisition { grant, target } = *material;
			let hold_until = *hold_until;
			self.state = OperationState::ConfirmingHeld {
				claim: grant.into_claim_after_known_commit(),
				hold_until,
				target: DeliveryTargetState::Retained(target),
			};
			self.wait_latest().await?;
			require_confirmed_lifetime(&runtime)?;
			require_before_deadline(hold_until)?;
			self.owner.confirm_held()?;
			let OperationState::ConfirmingHeld {
				claim,
				target,
				hold_until,
			} = std::mem::replace(&mut self.state, OperationState::Complete)
			else {
				unreachable!("successful confirmation retains its committed claim");
			};
			self.state = OperationState::Held {
				claim,
				target,
				hold_until,
			};
			Ok(NativeSemaphoreAcquisitionObservation::Acquired)
		} else {
			if !matches!(
				self.state,
				OperationState::AcquisitionPending {
					candidate: AcquisitionCandidate::Unprepared,
					..
				}
			) {
				return Err(NativeSemaphoreError::OperationState);
			}
			self.state = OperationState::Committed(CompletedSemaphoreOperation::Denied {
				receipt: self
					.latest()
					.expect("registered acquisition retains ending")
					.clone(),
			});
			self.settle_known_completion().await?;
			Ok(NativeSemaphoreAcquisitionObservation::Unavailable)
		}
	}

	/// Purpose changes before any fallible wait, registration or deadline calculation.
	pub(crate) async fn consume_release(
		&mut self,
	) -> Result<NativeSemaphoreReleaseObservation, Arc<NativeSemaphoreError>> {
		self.begin_attempt();
		let result = self.consume_release_inner().await;
		self.retain_result(result, false)
	}
	async fn consume_release_inner(
		&mut self,
	) -> Result<NativeSemaphoreReleaseObservation, NativeSemaphoreError> {
		if !matches!(self.state, OperationState::Held { .. }) {
			return Err(NativeSemaphoreError::OperationState);
		}
		let OperationState::Held { claim, .. } =
			std::mem::replace(&mut self.state, OperationState::Complete)
		else {
			unreachable!("release consumes the held claim");
		};
		self.state = OperationState::ReleasePending(claim);
		self.proof_settlement = OriginSettlement::Awaiting;
		self.release_attempt(ReleaseDispatch::InitialClaimed).await
	}

	/// Reconciliation retains the original claimed purpose and never replays acquisition.
	pub(crate) async fn reconcile_release(
		&mut self,
	) -> Result<NativeSemaphoreReleaseObservation, Arc<NativeSemaphoreError>> {
		self.begin_attempt();
		let result = async {
			if !self.has_pending_release() {
				return Err(NativeSemaphoreError::OperationState);
			}
			if self.has_completion() {
				return match self.settle_known_completion().await? {
					NativeSemaphoreCompletionObservation::Released(result) => Ok(result),
					NativeSemaphoreCompletionObservation::DeniedAcquisition => {
						Err(NativeSemaphoreError::OperationState)
					}
				};
			}
			self.owner.begin_closing()?;
			self.release_attempt(ReleaseDispatch::ReconcileClaimed)
				.await
		}
		.await;
		self.retain_result(result, true)
	}
	pub(crate) async fn cleanup_undelivered(
		&mut self,
	) -> Result<NativeSemaphoreCleanupObservation, Arc<NativeSemaphoreError>> {
		self.begin_attempt();
		let result = self.cleanup_inner().await;
		self.retain_result(result, true)
	}
	async fn cleanup_inner(
		&mut self,
	) -> Result<NativeSemaphoreCleanupObservation, NativeSemaphoreError> {
		if matches!(
			self.state,
			OperationState::ReleasePending(_) | OperationState::Complete
		) {
			return Err(NativeSemaphoreError::OperationState);
		}
		if self.has_completion() {
			return match self.settle_known_completion().await? {
				NativeSemaphoreCompletionObservation::DeniedAcquisition => {
					self.finish_denial()?;
					Ok(NativeSemaphoreCleanupObservation::NotAcquired)
				}
				NativeSemaphoreCompletionObservation::Released(result) => {
					Ok(NativeSemaphoreCleanupObservation::Released(result))
				}
			};
		}
		self.owner.begin_closing()?;
		if self.acquisition().is_none() {
			self.owner.complete()?;
			self.state = OperationState::Complete;
			return Ok(NativeSemaphoreCleanupObservation::NotAcquired);
		}
		self.wait_origins().await?;
		let ending = self
			.acquisition()
			.expect("checked origin")
			.wait_until_terminal()
			.await;
		if ending.disposition() == TransactionEndingDisposition::KnownRollback {
			self.owner.complete()?;
			self.state = OperationState::Complete;
			return Ok(NativeSemaphoreCleanupObservation::NotAcquired);
		}
		let target = match std::mem::replace(&mut self.state, OperationState::Complete) {
			OperationState::AcquisitionPending {
				candidate: AcquisitionCandidate::Prepared(material),
				..
			} => DeliveryTargetState::Retained(material.target),
			OperationState::AcquisitionPending {
				candidate: AcquisitionCandidate::Unprepared,
				..
			} => DeliveryTargetState::Unavailable,
			OperationState::ConfirmingHeld { target, .. }
			| OperationState::Held { target, .. }
			| OperationState::CleanupPending(target) => target,
			other => {
				self.state = other;
				return Err(NativeSemaphoreError::OperationState);
			}
		};
		self.state = OperationState::CleanupPending(target);
		self.release_attempt(ReleaseDispatch::Undelivered)
			.await
			.map(NativeSemaphoreCleanupObservation::Released)
	}
	async fn release_attempt(
		&mut self,
		dispatch: ReleaseDispatch,
	) -> Result<NativeSemaphoreReleaseObservation, NativeSemaphoreError> {
		let bindings = Arc::clone(&self.bindings);
		let handle = &bindings.handle;
		self.wait_origins().await?;
		let prior_origins = self.origins()?;
		self.pending_origins.retain(&prior_origins)?;
		let origins = self.origins()?;
		let proof_deadline = deadline(bindings.timing.physical_proof_attempt_timeout)?;
		let (phase, receiver) =
			tokio::sync::watch::channel(OwnedPrimitiveCleanupPhase::OriginOrdering);
		let mut pack = match dispatch {
			ReleaseDispatch::InitialClaimed => handle
				.bindings
				.admission
				.register_semaphore_release(&self.owner, &self.identity, origins.clone())?,
			ReleaseDispatch::ReconcileClaimed | ReleaseDispatch::Undelivered => handle
				.bindings
				.admission
				.register_semaphore_cleanup(&self.owner, &self.identity, origins.clone())?,
		};
		let receipt = pack.receipt();
		self.register_attempt(AttemptPurpose::Release, receipt.clone())?;
		self.proof_settlement = OriginSettlement::Awaiting;
		let execution = execute_registered(
			&bindings,
			pack.take_registration(),
			"fleet.native_semaphore.release",
			OperationWait::Cleanup {
				proof_deadline,
				receiver,
				cleanup_timeout: bindings.timing.cleanup_attempt_timeout,
			},
			async |tx| {
				let contracts = pack
					.authenticate(
						tx,
						self.attempt.ordering_mut()?,
						Some((&phase, proof_deadline)),
					)
					.await?;
				require_cleanup_deadline(
					&phase,
					proof_deadline,
					bindings.timing.cleanup_attempt_timeout,
				)?;
				let declaration = handle.bindings.code.declaration();
				let writer = contracts
					.bind_semaphore_writer(&declaration)
					.map_err(OwnedPrimitiveAdmissionError::from)?;
				let resource = writer.bind_resource(resource_fields(handle))?;
				if matches!(dispatch, ReleaseDispatch::Undelivered)
					&& self.cleanup_target().is_none()
				{
					return Ok(NativeSemaphoreReleaseObservation::NoMatch);
				}
				let lock = handle
					.bindings
					.storage
					.lock(tx, &handle.identity, handle.bindings.conflict_attempts)
					.await?;
				let prepared = (|| {
					require_cleanup_deadline(
						&phase,
						proof_deadline,
						bindings.timing.cleanup_attempt_timeout,
					)?;
					let Some(record) = lock.snapshot().record() else {
						return Ok(None);
					};
					let mut state = resource.decode_record(record)?;
					let matched = match dispatch {
						ReleaseDispatch::InitialClaimed | ReleaseDispatch::ReconcileClaimed => {
							resource.release(
								&mut state,
								SemaphoreReleaseInput {
									claim: self.pending_release()?,
									observed_at: lock.snapshot().observed_at(),
								},
							)?
						}
						ReleaseDispatch::Undelivered => writer.prepare_undelivered_release(
							SemaphoreUndeliveredScopeReleaseFields {
								resource: resource_fields(handle),
								state: &mut state,
								target: self
									.cleanup_target()
									.ok_or(NativeSemaphoreError::OperationState)?,
								observed_at: lock.snapshot().observed_at(),
							},
						)?,
					};
					if !matched {
						return Ok(None);
					}
					let record = resource.encode_record(
						&state,
						RecordMetadata {
							expires_at: None,
							updated_at: lock.snapshot().observed_at(),
						},
					)?;
					require_cleanup_deadline(
						&phase,
						proof_deadline,
						bindings.timing.cleanup_attempt_timeout,
					)?;
					Ok::<_, NativeSemaphoreError>(Some(record))
				})()
				.map_err(|error| identified(handle, error));
				let record = match prepared {
					Ok(record) => record,
					Err(error) => return reject_preparation(lock, error).await,
				};
				let result = match record {
					Some(record) => {
						lock.write(record).await?;
						NativeSemaphoreReleaseObservation::Released
					}
					None => {
						lock.keep().await?;
						NativeSemaphoreReleaseObservation::NoMatch
					}
				};
				require_cleanup_deadline(
					&phase,
					proof_deadline,
					bindings.timing.cleanup_attempt_timeout,
				)?;
				Ok(result)
			},
		)
		.await;
		if let Some(ordering) = self.attempt.ordering() {
			self.pending_origins.clear_observed(ordering);
		}
		let result = execution?;
		let ordering = self
			.attempt
			.ordering_mut()?
			.take_proof()
			.expect("successful ordered authentication retains exact origin ordering");
		self.state = OperationState::Committed(CompletedSemaphoreOperation::Released {
			receipt,
			result,
			dispatch,
			origins,
			ordering,
		});
		match self.settle_known_completion().await? {
			NativeSemaphoreCompletionObservation::Released(result) => Ok(result),
			NativeSemaphoreCompletionObservation::DeniedAcquisition => {
				Err(NativeSemaphoreError::OperationState)
			}
		}
	}
	fn origins(&self) -> Result<OwnedPrimitiveOriginEndings, NativeSemaphoreError> {
		Ok(OwnedPrimitiveOriginEndings::semaphore(
			SemaphoreOriginEndingsFields {
				acquisition: self
					.acquisition()
					.ok_or(NativeSemaphoreError::OperationState)?,
				first_claimed_release: self.first_release(),
				pending_origins: &self.pending_origins,
				preceding_operation: self.latest().ok_or(NativeSemaphoreError::OperationState)?,
			},
		))
	}
	async fn wait_latest(&self) -> Result<(), NativeSemaphoreError> {
		let until = deadline(self.bindings.timing.ending_wait_timeout)?;
		tokio::time::timeout_at(until, async {
			self.latest()
				.ok_or(NativeSemaphoreError::OperationState)?
				.wait_until_terminal()
				.await;
			self.owner.wait_until_continuations_end().await?;
			Ok::<_, NativeSemaphoreError>(())
		})
		.await
		.map_err(|_| NativeSemaphoreError::EndingPending)??;
		self.require_local_settlement()
	}
	async fn wait_origins(&self) -> Result<(), NativeSemaphoreError> {
		let origins = self.origins()?;
		let until = deadline(self.bindings.timing.ending_wait_timeout)?;
		tokio::time::timeout_at(until, async {
			origins.wait_until_terminal().await;
			self.owner.wait_until_continuations_end().await?;
			Ok::<_, NativeSemaphoreError>(())
		})
		.await
		.map_err(|_| NativeSemaphoreError::EndingPending)??;
		self.require_local_settlement()
	}
	fn require_local_settlement(&self) -> Result<(), NativeSemaphoreError> {
		let entry = self.owner.snapshot()?;
		if entry.continuation.is_retained() || entry.pending_caller_work_count != 0 {
			return Err(NativeSemaphoreError::EndingPending);
		}
		Ok(())
	}
	/// Repeats only local receipt/count settlement after a recorded successful finish.
	pub(crate) async fn settle_known_completion(
		&mut self,
	) -> Result<NativeSemaphoreCompletionObservation, NativeSemaphoreError> {
		if matches!(self.state, OperationState::DenialSettled(_)) {
			self.require_settled_denial()?;
			return Ok(NativeSemaphoreCompletionObservation::DeniedAcquisition);
		}
		let completed = self
			.completed()
			.ok_or(NativeSemaphoreError::OperationState)?;
		let current = self.latest().ok_or(NativeSemaphoreError::OperationState)?;
		if !completed.receipt().shares_ending(current) {
			return Err(NativeSemaphoreError::EndingPending);
		}
		self.wait_latest().await?;
		let ending = current.wait_until_terminal().await;
		if ending.disposition() != TransactionEndingDisposition::KnownCommit {
			return Err(NativeSemaphoreError::EndingPending);
		}
		let result = match completed {
			CompletedSemaphoreOperation::Denied { receipt } => {
				if self.first_release().is_some() {
					return Err(NativeSemaphoreError::OperationState);
				}
				self.state = OperationState::DenialSettled(receipt.clone());
				return Ok(NativeSemaphoreCompletionObservation::DeniedAcquisition);
			}
			CompletedSemaphoreOperation::Released {
				result,
				origins,
				ordering,
				..
			} => {
				if !ordering.matches(origins) {
					return Err(NativeSemaphoreError::EndingPending);
				}
				*result
			}
		};
		self.owner.complete()?;
		self.state = OperationState::Complete;
		Ok(NativeSemaphoreCompletionObservation::Released(result))
	}

	fn require_settled_denial(&self) -> Result<(), NativeSemaphoreError> {
		let OperationState::DenialSettled(receipt) = &self.state else {
			return Err(NativeSemaphoreError::OperationState);
		};
		if self.first_release().is_some() {
			return Err(NativeSemaphoreError::OperationState);
		}
		let acquisition = self
			.acquisition()
			.ok_or(NativeSemaphoreError::OperationState)?;
		let latest = self.latest().ok_or(NativeSemaphoreError::OperationState)?;
		if !receipt.shares_ending(acquisition)
			|| !receipt.shares_ending(latest)
			|| !matches!(receipt.snapshot(), TransactionEndingSnapshot::Terminal(ending) if ending.disposition() == TransactionEndingDisposition::KnownCommit)
		{
			return Err(NativeSemaphoreError::EndingPending);
		}
		self.require_local_settlement()
	}
	/// Completes only the sealed settled no-grant attempt, including after admission stops.
	pub(super) fn finish_denial(&mut self) -> Result<(), NativeSemaphoreError> {
		self.require_settled_denial()?;
		self.owner.complete()?;
		self.state = OperationState::Complete;
		Ok(())
	}
	/// A known denial supplies no slot; only the same still-Running Acquiring owner can continue.
	pub(super) fn continue_waiting_after_denial(&mut self) -> Result<(), NativeSemaphoreError> {
		self.require_settled_denial()?;
		let runtime = self.owner.admission_tracker().work_view(self.owner.id());
		if runtime.lifetime_finished() {
			return Err(
				crate::runtime::local_admission::RuntimeAdmissionError::LifetimeFinished.into(),
			);
		}
		if let Some(stop) = runtime.stop() {
			return Err(
				crate::runtime::local_admission::RuntimeAdmissionError::Stopped { stop }.into(),
			);
		}
		let entry = runtime
			.work
			.as_ref()
			.ok_or(crate::runtime::local_admission::RuntimeAdmissionError::SemaphoreWorkAbsent)?;
		if entry.identity != self.identity
			|| entry.state != RuntimeResourceWorkState::Acquiring
			|| entry.continuation.is_retained()
			|| entry.pending_caller_work_count != 0
		{
			return Err(
				crate::runtime::local_admission::RuntimeAdmissionError::SemaphoreWorkStateMismatch
					.into(),
			);
		}
		self.receipts = ReceiptHistory::Unstarted;
		self.attempt = AttemptState::Unregistered;
		self.proof_settlement = OriginSettlement::Awaiting;
		self.state = OperationState::New;
		Ok(())
	}
	pub(super) fn try_start_caller_work(
		&self,
	) -> Result<RuntimeSemaphoreCallerWorkPermit, NativeSemaphoreError> {
		if !matches!(self.state, OperationState::Held { .. }) {
			return Err(NativeSemaphoreError::OperationState);
		}
		Ok(self.owner.try_start_caller_work()?)
	}
	pub(super) fn held_observation(
		&self,
	) -> Result<NativeSemaphoreHeldObservation, NativeSemaphoreError> {
		let OperationState::Held {
			claim, hold_until, ..
		} = &self.state
		else {
			return Err(NativeSemaphoreError::OperationState);
		};
		Ok(NativeSemaphoreHeldObservation {
			claim: claim.snapshot(),
			hold_until: *hold_until,
		})
	}
	pub(super) fn take_delivery_target(&mut self) -> Option<SemaphoreUndeliveredTarget> {
		match &mut self.state {
			OperationState::Held { target, .. } => target.take(),
			_ => None,
		}
	}
	pub(super) fn restore_delivery_target(&mut self, restored: Option<SemaphoreUndeliveredTarget>) {
		let OperationState::Held { target, .. } = &mut self.state else {
			unreachable!("delivery returns the target before held work ends");
		};
		*target = match restored {
			Some(target) => DeliveryTargetState::Retained(target),
			None => DeliveryTargetState::Unavailable,
		};
	}

	/// Orders an uncertain final attempt without claiming semantic release or drainage.
	pub(crate) async fn prove_for_unresolved(&mut self) -> Result<(), Arc<NativeSemaphoreError>> {
		self.begin_attempt();
		let result = self.prove_inner().await;
		self.retain_result(result, true)
	}
	async fn prove_inner(&mut self) -> Result<(), NativeSemaphoreError> {
		if !matches!(
			self.state,
			OperationState::AcquisitionPending { .. }
				| OperationState::ConfirmingHeld { .. }
				| OperationState::ReleasePending(_)
				| OperationState::CleanupPending(_)
		) {
			return Err(NativeSemaphoreError::OperationState);
		}
		self.owner.begin_closing()?;
		self.wait_origins().await?;
		let prior_origins = self.origins()?;
		self.pending_origins.retain(&prior_origins)?;
		let origins = self.origins()?;
		if origins.all_known()? {
			self.proof_settlement = OriginSettlement::Settled(SettledSemaphoreOrigins {
				receipt: self
					.latest()
					.expect("actual origins retain latest receipt")
					.clone(),
				origins,
				ordering: SettledOriginOrdering::KnownEndings,
			});
			return Ok(());
		}
		let bindings = Arc::clone(&self.bindings);
		let mut pack = bindings
			.handle
			.bindings
			.admission
			.register_semaphore_cleanup(&self.owner, &self.identity, origins.clone())?;
		let receipt = pack.receipt();
		self.register_attempt(AttemptPurpose::Proof, receipt.clone())?;
		self.proof_settlement = OriginSettlement::Awaiting;
		let until = deadline(bindings.timing.physical_proof_attempt_timeout)?;
		let operation = execute_registered(
			&bindings,
			pack.take_registration(),
			"fleet.native_semaphore.prove",
			OperationWait::Protected(until),
			async |tx| {
				pack.prove_origins(tx, self.attempt.ordering_mut()?).await?;
				require_before_deadline(until)?;
				Ok(())
			},
		)
		.await;
		if let Some(ordering) = self.attempt.ordering() {
			self.pending_origins.clear_observed(ordering);
		}
		let settlement = self.wait_latest().await;
		if settlement.is_ok()
			&& self
				.attempt
				.ordering()
				.is_some_and(|ordering| ordering.matches(&origins))
		{
			let ending = receipt.wait_until_terminal().await;
			if matches!(
				ending.disposition(),
				TransactionEndingDisposition::KnownCommit
					| TransactionEndingDisposition::KnownRollback
			) {
				self.proof_settlement = OriginSettlement::Settled(SettledSemaphoreOrigins {
					receipt,
					origins,
					ordering: SettledOriginOrdering::Proven(
						self.attempt
							.ordering_mut()?
							.take_proof()
							.expect("settled physical ordering retains its actual proof"),
					),
				});
			}
		}
		match (operation, settlement) {
			(Err(operation_error), Err(settlement_error)) => {
				Err(NativeSemaphoreError::PhysicalSettlementFailed {
					operation_error: Box::new(operation_error),
					settlement_error: Box::new(settlement_error),
				})
			}
			(Err(error), _) | (_, Err(error)) => Err(error),
			(Ok(()), Ok(())) if matches!(self.proof_settlement, OriginSettlement::Settled(_)) => {
				Ok(())
			}
			_ => Err(NativeSemaphoreError::EndingPending),
		}
	}
	pub(crate) fn finish_unresolved(
		&mut self,
	) -> Result<Arc<NativeSemaphoreDiagnosticError>, NativeSemaphoreError> {
		let OriginSettlement::Settled(settled) = &self.proof_settlement else {
			return Err(NativeSemaphoreError::EndingPending);
		};
		let current = self.latest().ok_or(NativeSemaphoreError::OperationState)?;
		let ordered = match &settled.ordering {
			SettledOriginOrdering::KnownEndings => settled.origins.all_known()?,
			SettledOriginOrdering::Proven(proof) => proof.matches(&settled.origins),
		};
		if !settled.receipt.shares_ending(current) || !ordered {
			return Err(NativeSemaphoreError::EndingPending);
		}
		self.require_local_settlement()?;
		let diagnostic = self.diagnostic();
		self.owner.complete_unresolved(
			Arc::clone(&diagnostic) as Arc<dyn std::error::Error + Send + Sync>
		)?;
		self.state = OperationState::Complete;
		Ok(diagnostic)
	}
	pub(super) fn driver_timing(&self) -> SemaphoreDriverTiming {
		self.bindings.driver_timing
	}
	pub(super) fn has_completion(&self) -> bool {
		matches!(
			self.state,
			OperationState::Committed(_) | OperationState::DenialSettled(_)
		)
	}
	pub(super) fn has_pending_release(&self) -> bool {
		matches!(
			self.state,
			OperationState::ReleasePending(_)
				| OperationState::Committed(CompletedSemaphoreOperation::Released {
					dispatch: ReleaseDispatch::InitialClaimed | ReleaseDispatch::ReconcileClaimed,
					..
				})
		)
	}
	fn begin_attempt(&mut self) {
		self.attempt = AttemptState::Unregistered;
		self.proof_settlement = OriginSettlement::Awaiting;
	}
	/// Counts only this actual registered, ordered, terminal and locally settled failed attempt.
	pub(super) fn attempt_was_ordered(&self) -> bool {
		let AttemptState::Registered {
			receipt: attempt,
			ordering,
		} = &self.attempt
		else {
			return false;
		};
		self.latest()
			.is_some_and(|latest| latest.shares_ending(attempt))
			&& ordering.is_proven()
			&& matches!(attempt.snapshot(), TransactionEndingSnapshot::Terminal(_))
			&& self.owner.snapshot().is_ok_and(|entry| {
				!entry.continuation.is_retained() && entry.pending_caller_work_count == 0
			})
	}
	pub(super) fn retain_driver_cause(&mut self, error: Arc<NativeSemaphoreError>) {
		if self
			.primary_error
			.as_ref()
			.is_some_and(|retained| Arc::ptr_eq(retained, &error))
		{
			return;
		}
		if self.primary_error.is_none() {
			self.primary_error = Some(error);
		} else {
			self.cleanup_error = Some(error);
		}
		if !matches!(self.state, OperationState::Complete) {
			let _ = self.owner.retain_diagnostic(self.diagnostic());
		}
	}

	fn retain_result<T>(
		&mut self,
		result: Result<T, NativeSemaphoreError>,
		cleanup: bool,
	) -> Result<T, Arc<NativeSemaphoreError>> {
		result.map_err(|error| {
			let error = Arc::new(error);
			if cleanup {
				self.cleanup_error = Some(Arc::clone(&error));
			} else if self.primary_error.is_none() {
				self.primary_error = Some(Arc::clone(&error));
			}
			if !matches!(self.state, OperationState::Complete)
				&& let Err(retention_error) = self.owner.retain_diagnostic(self.diagnostic())
			{
				return Arc::new(NativeSemaphoreError::DiagnosticRetention {
					operation_error: error,
					retention_error,
				});
			}
			error
		})
	}
}
#[derive(Clone, Copy)]
enum ReleaseDispatch {
	InitialClaimed,
	ReconcileClaimed,
	Undelivered,
}
fn resource_fields(handle: &PgNativeSemaphore) -> SemaphoreResourceWriteFields<'_> {
	SemaphoreResourceWriteFields {
		identity: &handle.identity,
		key: &handle.key,
		settings: &handle.settings,
	}
}
fn identified(handle: &PgNativeSemaphore, source: NativeSemaphoreError) -> NativeSemaphoreError {
	NativeSemaphoreError::Record {
		identity: handle.identity.clone(),
		source: Box::new(source),
	}
}
async fn reject_preparation<T>(
	lock: impl PointRecordLock<Error = StateStorageError>,
	error: NativeSemaphoreError,
) -> Result<T, NativeSemaphoreError> {
	match lock.keep().await {
		Ok(()) => Err(error),
		Err(cleanup) => Err(NativeSemaphoreError::PreparationCleanup {
			preparation_error: Box::new(error),
			cleanup_error: Box::new(cleanup),
		}),
	}
}
fn checked_deadline(start: Instant, wait: Duration) -> Result<Instant, NativeSemaphoreError> {
	start
		.checked_add(wait)
		.ok_or(NativeSemaphoreError::InvalidTiming)
}
fn deadline(wait: Duration) -> Result<Instant, NativeSemaphoreError> {
	checked_deadline(Instant::now(), wait)
}
fn require_before_deadline(until: Instant) -> Result<(), NativeSemaphoreError> {
	if Instant::now() >= until {
		Err(NativeSemaphoreError::ProtectedTimeout)
	} else {
		Ok(())
	}
}
/// The end of the runtime's confirmed lifetime, which renewal extends. An acquisition that the
/// caller waits for must commit before it, and a fenced runtime or a finished lifetime ends the
/// acquisition at once.
fn confirmed_lifetime_deadline(
	runtime: &RuntimeAdmissionTracker,
) -> Result<Instant, NativeSemaphoreError> {
	let status = runtime.status();
	if status.lifetime_finished() {
		return Err(RuntimeAdmissionError::LifetimeFinished.into());
	}
	if status.stop() == Some(RuntimeAdmissionStop::Fenced) {
		return Err(RuntimeAdmissionError::Stopped {
			stop: RuntimeAdmissionStop::Fenced,
		}
		.into());
	}
	Ok(status.confirmed_until)
}
fn require_confirmed_lifetime(
	runtime: &RuntimeAdmissionTracker,
) -> Result<(), NativeSemaphoreError> {
	confirmed_lifetime_deadline(runtime).map(|_| ())
}
enum OperationWait {
	/// The work ends with the runtime's confirmed lifetime, which renewal extends.
	ConfirmedLifetime(RuntimeAdmissionTracker),
	Protected(Instant),
	Cleanup {
		proof_deadline: Instant,
		receiver: tokio::sync::watch::Receiver<OwnedPrimitiveCleanupPhase>,
		cleanup_timeout: Duration,
	},
}
impl OperationWait {
	fn deadline(&self) -> Result<Instant, NativeSemaphoreError> {
		match self {
			Self::ConfirmedLifetime(runtime) => confirmed_lifetime_deadline(runtime),
			Self::Protected(until) => Ok(*until),
			Self::Cleanup {
				proof_deadline,
				receiver,
				cleanup_timeout,
			} => cleanup_deadline(*receiver.borrow(), *proof_deadline, *cleanup_timeout),
		}
	}
	async fn changed(&mut self) {
		match self {
			Self::ConfirmedLifetime(runtime) => {
				let current = runtime.status();
				runtime.wait_for_status_change(current).await;
			}
			Self::Protected(_) => std::future::pending::<()>().await,
			Self::Cleanup { receiver, .. } => {
				if receiver.changed().await.is_err() {
					std::future::pending::<()>().await;
				}
			}
		}
	}
}
fn cleanup_deadline(
	phase: OwnedPrimitiveCleanupPhase,
	proof_deadline: Instant,
	cleanup_timeout: Duration,
) -> Result<Instant, NativeSemaphoreError> {
	match phase {
		OwnedPrimitiveCleanupPhase::OriginOrdering => Ok(proof_deadline),
		OwnedPrimitiveCleanupPhase::AuthenticationAndMutation(start) => {
			checked_deadline(start, cleanup_timeout)
		}
	}
}
fn require_cleanup_deadline(
	phase: &tokio::sync::watch::Sender<OwnedPrimitiveCleanupPhase>,
	proof_deadline: Instant,
	cleanup_timeout: Duration,
) -> Result<(), NativeSemaphoreError> {
	require_before_deadline(cleanup_deadline(
		*phase.borrow(),
		proof_deadline,
		cleanup_timeout,
	)?)
}
/// Registered acquisition/BEGIN and protected waits are bounded separately from owned ending.
async fn execute_registered<R>(
	bindings: &NativeSemaphoreExecutionBindings,
	registration: OwnedTransactionRegistration,
	operation_label: &'static str,
	mut wait: OperationWait,
	work: impl AsyncFnOnce(&mut WriteTx<'_>) -> Result<R, NativeSemaphoreError>,
) -> Result<R, NativeSemaphoreError> {
	let mut tx = {
		let begin = bindings
			.execution_pool
			.begin_registered_transaction(registration, operation_label);
		tokio::pin!(begin);
		loop {
			let until = wait.deadline()?;
			tokio::select! { biased;
				result = &mut begin => break result.map_err(NativeSemaphoreError::from_database_error)?,
				_ = wait.changed() => {},
				_ = tokio::time::sleep_until(until) => { if Instant::now() >= wait.deadline()? { return Err(NativeSemaphoreError::ProtectedTimeout); } }
			}
		}
	};
	let marker = tx.statement_interruption_marker();
	let result = {
		let work = work(&mut tx);
		tokio::pin!(work);
		loop {
			let until = match wait.deadline() {
				Ok(until) => until,
				Err(error) => {
					marker.mark_interrupted();
					break Err(error);
				}
			};
			tokio::select! { biased;
				_ = tokio::time::sleep_until(until) => {
					match wait.deadline() { Ok(current) if Instant::now() < current => continue,
						Err(error) => { marker.mark_interrupted(); break Err(error); }, Ok(_) => {} }
					marker.mark_interrupted(); break Err(NativeSemaphoreError::ProtectedTimeout);
				},
				result = &mut work => break result,
				_ = wait.changed() => {},
			}
		}
	};
	finish_pool_owned_write_transaction_and_preserve_rollback_error(
		operation_label,
		tx,
		result,
		NativeSemaphoreError::from_database_error,
		|_, operation_error, rollback_error| NativeSemaphoreError::RollbackFailed {
			operation_error: Box::new(operation_error),
			rollback_error,
		},
	)
	.await
}
