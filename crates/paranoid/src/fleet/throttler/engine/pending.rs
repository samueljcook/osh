//! Persistent private acquisition/outcome state. Drop retains registry abandonment.

use super::{
	Arc, Duration, HolderId, Instant, NativeThrottlerAcquisitionObservation,
	NativeThrottlerBindings, NativeThrottlerCompletionObservation, NativeThrottlerDenial,
	NativeThrottlerDiagnosticError, NativeThrottlerError, OwnedPrimitiveAdmissionError,
	OwnedPrimitiveCleanupPhase, OwnedPrimitiveOriginOrdering, PgNativeThrottler,
	PgRecordGroupAttempt, PrimitiveScopeBindingError, ThrottlerAcquireGroupFields,
	ThrottlerAcquireScopeGroupFields, ThrottlerAcquisitionResult,
	ThrottlerPendingOutcomeGroupFields, ThrottlerPendingOutcomeScopeGroupFields,
	ThrottlerResourceWriteFields, ThrottlerSettings, ThrottlerTaskOutcome,
	ThrottlerUndeliveredGroupFields, ThrottlerUndeliveredScopeGroupFields, TransactionFailure,
	deadline, require_before_deadline,
};
use crate::db::transaction_ending::{TransactionEndingDisposition, TransactionEndingReceipt};
use crate::db::{
	OwnedTransactionRegistration, WriteTx,
	finish_pool_owned_write_transaction_and_preserve_rollback_error,
};
use crate::fleet::throttler::state::NativeThrottlerPermit;
use crate::fleet::throttler::state::outcome::{
	ThrottlerPendingOutcome, ThrottlerUndeliveredTarget,
};
use crate::runtime::admission::owned_primitive::{
	OriginOrderingProgress, OwnedPrimitiveOriginEndings, OwnedPrimitivePendingOrigins,
	ThrottlerOriginEndingsFields, ThrottlerProbeOriginEndingsFields,
};
use crate::runtime::local_admission::throttler_work::RuntimeThrottlerWorkPermit;
use crate::runtime::local_admission::{
	RuntimeAdmissionError, RuntimeAdmissionStop, RuntimeAdmissionTracker,
};

mod history;
mod probe;
use history::{AttemptState, OriginPurpose, ReceiptHistory};

enum OperationState {
	New,
	AcquisitionPending(DeliveryTargetState),
	ConfirmingHeld {
		permit: NativeThrottlerPermit,
		target: DeliveryTargetState,
	},
	Held {
		permit: NativeThrottlerPermit,
		target: DeliveryTargetState,
	},
	OutcomePending(ThrottlerPendingOutcome),
	CleanupPending(DeliveryTargetState),
	Committed(CompletedThrottlerOperation),
	Complete,
}

enum DeliveryTargetState {
	Unavailable,
	Retained(ThrottlerUndeliveredTarget),
}

impl DeliveryTargetState {
	fn from_target(target: Option<ThrottlerUndeliveredTarget>) -> Self {
		match target {
			None => Self::Unavailable,
			Some(target) => Self::Retained(target),
		}
	}
	fn take(&mut self) -> Option<ThrottlerUndeliveredTarget> {
		match std::mem::replace(self, Self::Unavailable) {
			Self::Unavailable => None,
			Self::Retained(target) => Some(target),
		}
	}
	fn target(&self) -> Option<&ThrottlerUndeliveredTarget> {
		match self {
			Self::Unavailable => None,
			Self::Retained(target) => Some(target),
		}
	}
}

/// Constructed only from an actual successful shared owned finish; no caller proof is accepted.
enum CompletedThrottlerOperation {
	DeniedAcquisition {
		receipt: TransactionEndingReceipt,
	},
	ConsumedOutcome {
		receipt: TransactionEndingReceipt,
	},
	UndeliveredCleanup {
		receipt: TransactionEndingReceipt,
		origins: OwnedPrimitiveOriginEndings,
		ordering: OwnedPrimitiveOriginOrdering,
	},
}
impl CompletedThrottlerOperation {
	fn receipt(&self) -> &TransactionEndingReceipt {
		match self {
			Self::DeniedAcquisition { receipt, .. }
			| Self::ConsumedOutcome { receipt, .. }
			| Self::UndeliveredCleanup { receipt, .. } => receipt,
		}
	}
}

/// Constructed only after exact known ending/count settlement or a full physical observation.
struct SettledThrottlerOrigins {
	receipt: TransactionEndingReceipt,
	origins: OwnedPrimitiveOriginEndings,
	ordering: SettledOriginOrdering,
}

enum SettledOriginOrdering {
	KnownEndings,
	Proven(OwnedPrimitiveOriginOrdering),
}

enum OriginSettlement {
	Awaiting,
	Settled(SettledThrottlerOrigins),
}
/// No private permit or cleanup material is exposed to a consumer or diagnostic observer.
pub(crate) struct NativeThrottlerOperation {
	handle: PgNativeThrottler,
	owner: RuntimeThrottlerWorkPermit,
	state: OperationState,
	history: ReceiptHistory,
	attempt: AttemptState,
	heartbeat_error: Option<Arc<NativeThrottlerError>>,
	pending_origins: OwnedPrimitivePendingOrigins,
	primary_error: Option<Arc<NativeThrottlerError>>,
	cleanup_error: Option<Arc<NativeThrottlerError>>,
	proof_settlement: OriginSettlement,
}
impl NativeThrottlerOperation {
	pub(super) fn new(handle: PgNativeThrottler, owner: RuntimeThrottlerWorkPermit) -> Self {
		Self {
			handle,
			owner,
			state: OperationState::New,
			history: ReceiptHistory::Unstarted,
			attempt: AttemptState::Unregistered,
			heartbeat_error: None,
			pending_origins: OwnedPrimitivePendingOrigins::new(),
			primary_error: None,
			cleanup_error: None,
			proof_settlement: OriginSettlement::Awaiting,
		}
	}
	fn register_attempt(
		&mut self,
		purpose: OriginPurpose,
		receipt: TransactionEndingReceipt,
	) -> Result<(), NativeThrottlerError> {
		self.history.record(purpose, receipt.clone())?;
		self.attempt = AttemptState::registered(receipt);
		Ok(())
	}
	fn origin(&self) -> Option<&TransactionEndingReceipt> {
		self.history.latest()
	}
	fn acquisition_origin(&self) -> Option<&TransactionEndingReceipt> {
		self.history.acquisition()
	}
	fn outcome_origin(&self) -> Option<&TransactionEndingReceipt> {
		self.history.outcome()
	}
	fn heartbeat_origin(&self) -> Option<&TransactionEndingReceipt> {
		self.history.probe()
	}
	fn held_permit(&self) -> Option<&NativeThrottlerPermit> {
		match &self.state {
			OperationState::Held { permit, .. } => Some(permit),
			_ => None,
		}
	}
	fn completed(&self) -> Option<&CompletedThrottlerOperation> {
		match &self.state {
			OperationState::Committed(completed) => Some(completed),
			_ => None,
		}
	}
	async fn acquire_inner(
		&mut self,
		holder: Option<HolderId>,
		rate_limit_cost: std::num::NonZeroU32,
	) -> Result<NativeThrottlerAcquisitionObservation, NativeThrottlerError> {
		if !matches!(self.state, OperationState::New) {
			return Err(NativeThrottlerError::OperationState);
		}
		let handle = self.handle.clone();
		let bindings = &handle.bindings;
		let runtime = self.owner.admission_tracker();
		// The acquisition waits for its turn holding no connection, so waiting for the key under
		// contention bounds nothing; the bound below starts with the turn.
		let _turn = runtime.take_record_turn(handle.identity.record()).await;
		let acquisition_started = Instant::now();
		require_confirmed_lifetime(&runtime)?;
		let mut pack = bindings
			.admission
			.register_throttler_acquisition(&self.owner, &handle.identity)?;
		let receipt = pack.receipt();
		self.register_attempt(OriginPurpose::Acquisition(acquisition_started), receipt)?;
		self.state = OperationState::AcquisitionPending(DeliveryTargetState::Unavailable);
		let registration = pack.take_registration();
		let result = execute_registered(
			bindings,
			registration,
			"fleet.native_throttler.acquire",
			OperationWait::ConfirmedLifetime(runtime.clone()),
			async |tx| {
				let contracts = pack
					.authenticate(tx, self.attempt.ordering_mut()?, None)
					.await?;
				let declaration = bindings.code.declaration();
				let writer = contracts
					.bind_throttler_writer(&declaration)
					.map_err(OwnedPrimitiveAdmissionError::from)?;
				let resource = writer.bind_resource(ThrottlerResourceWriteFields {
					identity: handle.identity.record(),
					key: handle.identity.resource(),
					settings: &handle.settings,
				})?;
				let proposal = resource
					.acquisition_proposal(holder, rate_limit_cost)
					.map_err(|error| {
						NativeThrottlerError::Preparation(Box::new(
							PrimitiveScopeBindingError::from(Box::new(error)),
						))
					})?;
				let identities = handle.identities(proposal.outcome_id());
				require_confirmed_lifetime(&runtime)?;
				let attempt = PgRecordGroupAttempt::acquire(
					&bindings.storage,
					tx,
					identities,
					bindings.group_budget,
				)
				.await??;
				require_confirmed_lifetime(&runtime)?;
				let prepared = writer.prepare_acquisition_group(ThrottlerAcquireScopeGroupFields {
					key: handle.identity.resource(),
					settings: &handle.settings,
					group: ThrottlerAcquireGroupFields {
						resource_identity: handle.identity.record(),
						snapshot: attempt.snapshot(),
						proposal,
					},
				});
				let prepared = match prepared {
					Ok(prepared) => prepared,
					Err(preparation) => {
						return match attempt.restore().await {
							Ok(()) => Err(preparation.into()),
							Err(cleanup) => Err(NativeThrottlerError::PreparationCleanup {
								preparation_error: Box::new(preparation),
								cleanup_error: Box::new(cleanup),
							}),
						};
					}
				};
				let (actions, result, target) = prepared.into_parts();
				let OperationState::AcquisitionPending(retained) = &mut self.state else {
					return Err(NativeThrottlerError::OperationState);
				};
				*retained = DeliveryTargetState::from_target(target);
				require_confirmed_lifetime(&runtime)?;
				attempt.apply_native(actions).await??;
				require_confirmed_lifetime(&runtime)?;
				Ok(result)
			},
		)
		.await?;
		let observation = match result {
			ThrottlerAcquisitionResult::Acquired(tentative) => {
				let OperationState::AcquisitionPending(target) =
					std::mem::replace(&mut self.state, OperationState::Complete)
				else {
					unreachable!("committed acquisition retains its pending target state");
				};
				self.state = OperationState::ConfirmingHeld {
					permit: tentative.into_permit_after_known_commit(),
					target,
				};
				self.wait_origin().await?;
				self.owner.confirm_held()?;
				let OperationState::ConfirmingHeld { permit, target } =
					std::mem::replace(&mut self.state, OperationState::Complete)
				else {
					unreachable!("local confirmation retains the committed permit");
				};
				self.state = OperationState::Held { permit, target };
				NativeThrottlerAcquisitionObservation::Acquired
			}
			ThrottlerAcquisitionResult::Throttled { retry_after } => {
				self.state =
					OperationState::Committed(CompletedThrottlerOperation::DeniedAcquisition {
						receipt: self
							.origin()
							.expect("actual acquisition finish has a retained receipt")
							.clone(),
					});
				self.settle_completed().await?;
				NativeThrottlerAcquisitionObservation::Denied(NativeThrottlerDenial::Throttled {
					retry_after,
				})
			}
			ThrottlerAcquisitionResult::CircuitOpen => {
				self.state =
					OperationState::Committed(CompletedThrottlerOperation::DeniedAcquisition {
						receipt: self
							.origin()
							.expect("actual acquisition finish has a retained receipt")
							.clone(),
					});
				self.settle_completed().await?;
				NativeThrottlerAcquisitionObservation::Denied(NativeThrottlerDenial::CircuitOpen)
			}
		};
		Ok(observation)
	}
	/// A trusted owned delivery/task boundary must establish actual caller execution first.
	/// This consumes the private outcome once; failures cannot enter undelivered cleanup.
	pub(crate) async fn consume_outcome(
		&mut self,
		outcome: ThrottlerTaskOutcome,
	) -> Result<(), Arc<NativeThrottlerError>> {
		self.begin_attempt();
		let result = self.consume_outcome_inner(outcome).await;
		self.retain_result(result, false)
	}
	async fn consume_outcome_inner(
		&mut self,
		outcome: ThrottlerTaskOutcome,
	) -> Result<(), NativeThrottlerError> {
		if !matches!(self.state, OperationState::Held { .. }) {
			return Err(NativeThrottlerError::OperationState);
		}
		let OperationState::Held { permit, .. } =
			std::mem::replace(&mut self.state, OperationState::Complete)
		else {
			unreachable!("caller outcome consumes its held permit");
		};
		self.state = OperationState::OutcomePending(permit.into_pending_outcome(outcome));
		if self.complete_rate_only_outcome()? {
			return Ok(());
		}
		self.history.prepare_outcome()?;
		self.dispatch_pending_outcome().await
	}
	fn complete_rate_only_outcome(&mut self) -> Result<bool, NativeThrottlerError> {
		let settings = self.handle.settings;
		if settings.concurrency().is_some() || settings.circuit().is_some() {
			return Ok(false);
		}
		if !matches!(self.state, OperationState::OutcomePending(_)) {
			return Err(NativeThrottlerError::OperationState);
		}
		self.owner.begin_closing()?;
		self.owner.complete()?;
		self.state = OperationState::Complete;
		Ok(true)
	}

	async fn dispatch_pending_outcome(&mut self) -> Result<(), NativeThrottlerError> {
		let handle = self.handle.clone();
		let bindings = &handle.bindings;
		if self.heartbeat_origin().is_some() {
			return self.dispatch_outcome_after_probe().await;
		}
		let runtime = self.owner.admission_tracker();
		require_confirmed_lifetime(&runtime)?;
		let mut pack = bindings
			.admission
			.register_throttler_outcome(&self.owner, &handle.identity)?;
		let receipt = pack.receipt();
		self.register_attempt(OriginPurpose::Outcome, receipt)?;
		let OperationState::OutcomePending(pending) = &self.state else {
			return Err(NativeThrottlerError::OperationState);
		};
		let registration = pack.take_registration();
		execute_registered(
			bindings,
			registration,
			"fleet.native_throttler.dispatch_outcome",
			OperationWait::ConfirmedLifetime(runtime.clone()),
			async |tx| {
				let contracts = pack
					.authenticate(tx, self.attempt.ordering_mut()?, None)
					.await?;
				apply_pending_outcome(
					bindings,
					&handle,
					tx,
					contracts,
					pending,
					OutcomePreparationWait::ConfirmedLifetime(&runtime),
				)
				.await
			},
		)
		.await?;
		self.record_outcome_completion();
		self.settle_completed().await?;
		Ok(())
	}
	fn record_outcome_completion(&mut self) {
		self.state = OperationState::Committed(CompletedThrottlerOperation::ConsumedOutcome {
			receipt: self
				.origin()
				.expect("actual outcome finish retains its receipt")
				.clone(),
		});
	}
	/// Reconciles only the exact consumed outcome; it creates no caller work or replacement
	/// purpose.
	pub(super) async fn reconcile_outcome(&mut self) -> Result<(), Arc<NativeThrottlerError>> {
		self.begin_attempt();
		let result = self.reconcile_outcome_inner().await;
		self.retain_result(result, true)
	}
	async fn reconcile_outcome_inner(&mut self) -> Result<(), NativeThrottlerError> {
		if self.has_completion() {
			return match self.settle_completed().await? {
				NativeThrottlerCompletionObservation::ConsumedOutcome => Ok(()),
				_ => Err(NativeThrottlerError::OperationState),
			};
		}
		if !matches!(self.state, OperationState::OutcomePending(_)) {
			return Err(NativeThrottlerError::OperationState);
		}
		if self.complete_rate_only_outcome()? {
			return Ok(());
		}
		if self.outcome_origin().is_none() {
			// No outcome registration occurred. The original exact purpose is still retained.
			return self.dispatch_pending_outcome().await;
		}
		self.wait_reconciliation_origins().await?;
		let origins = self.retain_reconciliation_origins()?;
		let handle = self.handle.clone();
		let bindings = &handle.bindings;
		let proof_deadline = deadline(bindings.timing.physical_proof_attempt_timeout)?;
		let (phase, receiver) =
			tokio::sync::watch::channel(OwnedPrimitiveCleanupPhase::OriginOrdering);
		let cleanup_timeout = bindings.timing.cleanup_attempt_timeout;
		let mut pack = bindings.admission.register_throttler_cleanup(
			&self.owner,
			&handle.identity,
			origins,
		)?;
		self.register_attempt(OriginPurpose::Reconciliation, pack.receipt())?;

		let OperationState::OutcomePending(pending) = &self.state else {
			return Err(NativeThrottlerError::OperationState);
		};
		let registration = pack.take_registration();
		let execution = execute_registered(
			bindings,
			registration,
			"fleet.native_throttler.reconcile_outcome",
			OperationWait::Cleanup {
				proof_deadline,
				receiver,
				cleanup_timeout,
			},
			async |tx| {
				let contracts = pack
					.authenticate(
						tx,
						self.attempt.ordering_mut()?,
						Some((&phase, proof_deadline)),
					)
					.await?;
				apply_pending_outcome(
					bindings,
					&handle,
					tx,
					contracts,
					pending,
					OutcomePreparationWait::Cleanup {
						phase: &phase,
						proof_deadline,
						cleanup_timeout,
					},
				)
				.await
			},
		)
		.await;
		self.clear_observed_origins();
		execution?;
		self.record_outcome_completion();
		self.settle_completed().await?;
		Ok(())
	}

	/// Exact preexisting NotExecuted cleanup is retryable only after each actual preceding ending.
	pub(crate) async fn cleanup_undelivered(&mut self) -> Result<(), Arc<NativeThrottlerError>> {
		self.begin_attempt();
		let result = self.cleanup_inner().await;
		self.retain_result(result, true)
	}
	async fn cleanup_inner(&mut self) -> Result<(), NativeThrottlerError> {
		if self.has_completion() {
			return self.settle_completed().await.map(|_| ());
		}
		match &self.state {
			OperationState::Complete => return Ok(()),
			OperationState::OutcomePending(_) => return Err(NativeThrottlerError::OperationState),
			OperationState::New => {
				self.owner.begin_closing()?;
				self.owner.complete()?;
				self.state = OperationState::Complete;
				return Ok(());
			}
			_ => {}
		}
		self.owner.begin_closing()?;
		self.wait_cleanup_origins().await?;
		// Only the original acquisition rollback rules out a persisted acquisition. A later
		// cleanup rollback cannot discard a grant from an earlier committed acquisition.
		let acquisition = self
			.acquisition_origin()
			.ok_or(NativeThrottlerError::OperationState)?;
		let ending = acquisition.wait_until_terminal().await;
		if ending.disposition() == TransactionEndingDisposition::KnownRollback {
			self.owner.complete()?;
			self.state = OperationState::Complete;
			return Ok(());
		}
		let origins = self.retain_reconciliation_origins()?;
		let handle = self.handle.clone();
		let bindings = &handle.bindings;
		let proof_deadline = deadline(bindings.timing.physical_proof_attempt_timeout)?;
		let (phase, receiver) =
			tokio::sync::watch::channel(OwnedPrimitiveCleanupPhase::OriginOrdering);
		let cleanup_timeout = bindings.timing.cleanup_attempt_timeout;
		let mut pack = bindings.admission.register_throttler_cleanup(
			&self.owner,
			&handle.identity,
			origins.clone(),
		)?;
		self.register_attempt(OriginPurpose::Reconciliation, pack.receipt())?;

		let target = match std::mem::replace(&mut self.state, OperationState::Complete) {
			OperationState::AcquisitionPending(target)
			| OperationState::ConfirmingHeld { target, .. }
			| OperationState::Held { target, .. }
			| OperationState::CleanupPending(target) => target,
			other => {
				self.state = other;
				return Err(NativeThrottlerError::OperationState);
			}
		};
		self.state = OperationState::CleanupPending(target);
		let OperationState::CleanupPending(target) = &self.state else {
			unreachable!("cleanup retains its target state");
		};
		let target = target.target();
		let registration = pack.take_registration();
		let execution = execute_registered(
			bindings,
			registration,
			"fleet.native_throttler.cleanup",
			OperationWait::Cleanup {
				proof_deadline,
				receiver,
				cleanup_timeout,
			},
			async |tx| {
				let contracts = pack
					.authenticate(
						tx,
						self.attempt.ordering_mut()?,
						Some((&phase, proof_deadline)),
					)
					.await?;
				let declaration = bindings.code.declaration();
				let writer = contracts
					.bind_throttler_writer(&declaration)
					.map_err(OwnedPrimitiveAdmissionError::from)?;
				require_cleanup_deadline(&phase, proof_deadline, cleanup_timeout)?;
				let Some(target) = target else {
					return Ok(());
				};
				let identities = handle.identities(target.outcome_id());
				let attempt = PgRecordGroupAttempt::acquire(
					&bindings.storage,
					tx,
					identities,
					bindings.group_budget,
				)
				.await??;
				require_cleanup_deadline(&phase, proof_deadline, cleanup_timeout)?;
				let prepared =
					writer.prepare_undelivered_group(ThrottlerUndeliveredScopeGroupFields {
						key: handle.identity.resource(),
						settings: &handle.settings,
						group: ThrottlerUndeliveredGroupFields {
							resource_identity: handle.identity.record(),
							snapshot: attempt.snapshot(),
							target,
						},
					});
				let prepared = match prepared {
					Ok(prepared) => prepared,
					Err(preparation) => {
						return match attempt.restore().await {
							Ok(()) => Err(preparation.into()),
							Err(cleanup) => Err(NativeThrottlerError::PreparationCleanup {
								preparation_error: Box::new(preparation),
								cleanup_error: Box::new(cleanup),
							}),
						};
					}
				};
				let (actions, _) = prepared.into_parts();
				require_cleanup_deadline(&phase, proof_deadline, cleanup_timeout)?;
				attempt.apply_native(actions).await??;
				require_cleanup_deadline(&phase, proof_deadline, cleanup_timeout)?;
				Ok(())
			},
		)
		.await;
		self.clear_observed_origins();
		execution?;
		let ordering = self
			.attempt
			.ordering_mut()?
			.take_proof()
			.ok_or(NativeThrottlerError::OperationState)?;
		if !ordering.matches(&origins) {
			return Err(NativeThrottlerError::OperationState);
		}
		self.state = OperationState::Committed(CompletedThrottlerOperation::UndeliveredCleanup {
			receipt: self
				.origin()
				.expect("actual cleanup finish has a retained receipt")
				.clone(),
			origins,
			ordering,
		});
		self.settle_completed().await.map(|_| ())
	}
	/// Settles only an actual recorded known successful finish; this method executes no SQL.
	pub(crate) async fn settle_known_completion(
		&mut self,
	) -> Result<NativeThrottlerCompletionObservation, Arc<NativeThrottlerError>> {
		let cleanup = matches!(
			self.state,
			OperationState::Committed(CompletedThrottlerOperation::UndeliveredCleanup { .. })
		);
		let result = self.settle_completed().await;
		self.retain_result(result, cleanup)
	}
	async fn settle_completed(
		&mut self,
	) -> Result<NativeThrottlerCompletionObservation, NativeThrottlerError> {
		let completed = self
			.completed()
			.ok_or(NativeThrottlerError::OperationState)?;
		if !self
			.origin()
			.is_some_and(|origin| origin.shares_ending(completed.receipt()))
		{
			return Err(NativeThrottlerError::OperationState);
		}
		if let CompletedThrottlerOperation::UndeliveredCleanup {
			origins, ordering, ..
		} = completed
			&& !ordering.matches(origins)
		{
			return Err(NativeThrottlerError::OperationState);
		}
		self.wait_origin().await?;
		let ending = completed.receipt().wait_until_terminal().await;
		if ending.disposition() != TransactionEndingDisposition::KnownCommit {
			return Err(NativeThrottlerError::OperationState);
		}
		self.owner.complete()?;
		let OperationState::Committed(completed) =
			std::mem::replace(&mut self.state, OperationState::Complete)
		else {
			unreachable!("successful completion retains its exact committed result");
		};
		Ok(match completed {
			CompletedThrottlerOperation::DeniedAcquisition { .. } => {
				NativeThrottlerCompletionObservation::DeniedAcquisition
			}
			CompletedThrottlerOperation::ConsumedOutcome { .. } => {
				NativeThrottlerCompletionObservation::ConsumedOutcome
			}
			CompletedThrottlerOperation::UndeliveredCleanup { .. } => {
				NativeThrottlerCompletionObservation::UndeliveredCleanup
			}
		})
	}
	async fn wait_cleanup_origins(&self) -> Result<(), NativeThrottlerError> {
		self.wait_reconciliation_origins().await
	}
	async fn wait_origin(&self) -> Result<(), NativeThrottlerError> {
		let origin = self.origin().ok_or(NativeThrottlerError::OperationState)?;
		let deadline = deadline(self.handle.bindings.timing.ending_wait_timeout)?;
		tokio::time::timeout_at(deadline, async {
			origin.wait_until_terminal().await;
			self.owner.wait_until_continuations_end().await
		})
		.await
		.map_err(|_| NativeThrottlerError::EndingPending)??;
		Ok(())
	}
	pub(super) fn bindings(&self) -> &NativeThrottlerBindings {
		&self.handle.bindings
	}
	pub(super) fn handle_settings(&self) -> ThrottlerSettings {
		self.handle.settings
	}
	pub(super) fn diagnostic(&self) -> Arc<NativeThrottlerDiagnosticError> {
		fn terminal(
			receipt: Option<&TransactionEndingReceipt>,
		) -> Option<Arc<crate::db::transaction_ending::TransactionEndingResult>> {
			match receipt.map(TransactionEndingReceipt::snapshot) {
				Some(crate::db::transaction_ending::TransactionEndingSnapshot::Terminal(
					ending,
				)) => Some(ending),
				_ => None,
			}
		}
		Arc::new(NativeThrottlerDiagnosticError {
			primary_error: self.primary_error.clone(),
			cleanup_error: self.cleanup_error.clone(),
			acquisition_ending: terminal(self.acquisition_origin()),
			outcome_ending: terminal(self.outcome_origin()),
			heartbeat_ending: terminal(self.heartbeat_origin()),
			heartbeat_error: self.heartbeat_error.clone(),
			latest_ending: terminal(self.origin()),
			unsettled_ending: self.pending_origins.ending(),
		})
	}
	pub(super) fn owner(&self) -> &RuntimeThrottlerWorkPermit {
		&self.owner
	}
	pub(super) fn is_complete(&self) -> bool {
		matches!(self.state, OperationState::Complete)
	}
	pub(super) fn has_completion(&self) -> bool {
		matches!(self.state, OperationState::Committed(_))
	}
	pub(super) fn has_pending_outcome(&self) -> bool {
		matches!(
			self.state,
			OperationState::OutcomePending(_)
				| OperationState::Committed(CompletedThrottlerOperation::ConsumedOutcome { .. })
		)
	}
	pub(super) fn reservation_kind(
		&self,
	) -> Option<crate::fleet::throttler::state::NativeThrottlerReservationKind> {
		self.held_permit()
			.map(NativeThrottlerPermit::reservation_kind)
	}
	pub(super) fn take_delivery_target(&mut self) -> Option<ThrottlerUndeliveredTarget> {
		match &mut self.state {
			OperationState::Held { target, .. } => target.take(),
			_ => None,
		}
	}
	pub(super) fn restore_delivery_target(&mut self, target: Option<ThrottlerUndeliveredTarget>) {
		let OperationState::Held {
			target: retained, ..
		} = &mut self.state
		else {
			unreachable!("delivery returns its target before held work ends");
		};
		*retained = DeliveryTargetState::from_target(target);
	}
	pub(super) async fn acquire(
		&mut self,
		holder: Option<HolderId>,
		rate_limit_cost: std::num::NonZeroU32,
	) -> Result<NativeThrottlerAcquisitionObservation, Arc<NativeThrottlerError>> {
		self.begin_attempt();
		let result = self.acquire_inner(holder, rate_limit_cost).await;
		self.retain_result(result, false)
	}
	/// When the acquisition took its record's turn, before its transaction began. The stored
	/// reservations of the acquisition start no earlier.
	pub(super) fn acquisition_started(&self) -> Option<Instant> {
		self.history.requested_at()
	}
	fn reconciliation_origins(&self) -> Result<OwnedPrimitiveOriginEndings, NativeThrottlerError> {
		let original = if matches!(
			self.state,
			OperationState::OutcomePending(_)
				| OperationState::Committed(CompletedThrottlerOperation::ConsumedOutcome { .. })
		) {
			self.outcome_origin().or(self.acquisition_origin())
		} else {
			self.acquisition_origin()
		}
		.ok_or(NativeThrottlerError::OperationState)?;
		let preceding = self
			.origin()
			.or(self.acquisition_origin())
			.ok_or(NativeThrottlerError::OperationState)?;
		let fields = ThrottlerOriginEndingsFields {
			original_operation: original,
			preceding_operation: preceding,
			pending_origins: &self.pending_origins,
		};
		Ok(if let Some(heartbeat) = self.heartbeat_origin() {
			OwnedPrimitiveOriginEndings::throttler_after_probe(ThrottlerProbeOriginEndingsFields {
				outcome: fields,
				acquisition: self
					.acquisition_origin()
					.ok_or(NativeThrottlerError::OperationState)?,
				heartbeat,
			})
		} else {
			OwnedPrimitiveOriginEndings::throttler(fields)
		})
	}
	fn retain_reconciliation_origins(
		&mut self,
	) -> Result<OwnedPrimitiveOriginEndings, NativeThrottlerError> {
		let origins = self.reconciliation_origins()?;
		self.pending_origins.retain(&origins)?;
		self.reconciliation_origins()
	}
	fn clear_observed_origins(&mut self) {
		if let Some(ordering) = self.attempt.ordering() {
			self.pending_origins.clear_observed(ordering);
		}
	}

	async fn wait_reconciliation_origins(&self) -> Result<(), NativeThrottlerError> {
		let origins = self.reconciliation_origins()?;
		let until = deadline(self.handle.bindings.timing.ending_wait_timeout)?;
		tokio::time::timeout_at(until, async {
			origins.wait_until_terminal().await;
			self.owner.wait_until_continuations_end().await
		})
		.await
		.map_err(|_| NativeThrottlerError::EndingPending)??;
		Ok(())
	}
	/// A final failed accounting attempt needs its own physical ordering before terminal
	/// diagnostics.
	pub(super) async fn prove_pending_origins(&mut self) -> Result<(), Arc<NativeThrottlerError>> {
		self.begin_attempt();
		let result = self.prove_pending_origins_inner().await;
		self.retain_result(result, true)
	}
	async fn prove_pending_origins_inner(&mut self) -> Result<(), NativeThrottlerError> {
		self.owner.begin_closing()?;
		self.wait_reconciliation_origins().await?;
		let origins = self.retain_reconciliation_origins()?;
		if origins.all_known()? {
			// Exact known driver endings already settle physical origin; no extra control SQL is required.
			let receipt = self
				.origin()
				.or(self.acquisition_origin())
				.ok_or(NativeThrottlerError::OperationState)?
				.clone();
			self.proof_settlement = OriginSettlement::Settled(SettledThrottlerOrigins {
				receipt,
				origins,
				ordering: SettledOriginOrdering::KnownEndings,
			});
			return Ok(());
		}
		let handle = self.handle.clone();
		let bindings = &handle.bindings;
		let until = deadline(bindings.timing.physical_proof_attempt_timeout)?;
		let mut pack = bindings.admission.register_throttler_cleanup(
			&self.owner,
			&handle.identity,
			origins.clone(),
		)?;
		self.register_attempt(OriginPurpose::Reconciliation, pack.receipt())?;
		let registration = pack.take_registration();
		let operation = execute_registered(
			bindings,
			registration,
			"fleet.native_throttler.prove_pending_origins",
			OperationWait::Protected(until),
			async |tx| {
				pack.prove_origins(tx, self.attempt.ordering_mut()?).await?;
				require_before_deadline(until)?;
				Ok(())
			},
		)
		.await;
		self.clear_observed_origins();
		let settlement = self.wait_origin().await;
		if settlement.is_ok()
			&& self
				.attempt
				.ordering()
				.is_some_and(|ordering| ordering.matches(&origins))
		{
			let receipt = self
				.origin()
				.expect("actual proof attempt retains its ending");
			let ending = receipt.wait_until_terminal().await;
			if matches!(
				ending.disposition(),
				TransactionEndingDisposition::KnownCommit
					| TransactionEndingDisposition::KnownRollback
			) {
				self.proof_settlement = OriginSettlement::Settled(SettledThrottlerOrigins {
					receipt: receipt.clone(),
					origins,
					ordering: SettledOriginOrdering::Proven(
						self.attempt
							.ordering_mut()?
							.take_proof()
							.expect("settled physical ordering retains its proof"),
					),
				});
			}
		}
		match (operation, settlement) {
			(Err(operation_error), Err(settlement_error)) => {
				Err(NativeThrottlerError::PhysicalSettlementFailed {
					operation_error: Box::new(operation_error),
					settlement_error: Box::new(settlement_error),
				})
			}
			(Err(error), _) | (_, Err(error)) => Err(error),
			(Ok(()), Ok(())) if matches!(self.proof_settlement, OriginSettlement::Settled(_)) => {
				Ok(())
			}
			_ => Err(NativeThrottlerError::EndingPending),
		}
	}
	fn begin_attempt(&mut self) {
		self.attempt = AttemptState::Unregistered;

		self.proof_settlement = OriginSettlement::Awaiting;
	}
	/// Counts only this actual registered, ordered and locally settled failed accounting attempt.
	pub(super) fn origin_was_ordered(&self) -> bool {
		let Some(attempt) = self.attempt.receipt() else {
			return false;
		};
		self.origin()
			.is_some_and(|origin| origin.shares_ending(attempt))
			&& self.attempt.ordering().is_some()
			&& matches!(
				attempt.snapshot(),
				crate::db::transaction_ending::TransactionEndingSnapshot::Terminal(_)
			) && self.owner.snapshot().is_ok_and(|entry| {
			!entry.continuation.is_retained() && entry.pending_caller_work_count == 0
		})
	}
	pub(super) fn finish_unresolved(
		&mut self,
		source: Arc<dyn std::error::Error + Send + Sync>,
	) -> Result<(), NativeThrottlerError> {
		let OriginSettlement::Settled(settled) = &self.proof_settlement else {
			return Err(NativeThrottlerError::EndingPending);
		};
		let current = self
			.origin()
			.or(self.acquisition_origin())
			.ok_or(NativeThrottlerError::OperationState)?;
		let ordered = match &settled.ordering {
			SettledOriginOrdering::KnownEndings => settled.origins.all_known()?,
			SettledOriginOrdering::Proven(ordering) => ordering.matches(&settled.origins),
		};
		if !settled.receipt.shares_ending(current) || !ordered {
			return Err(NativeThrottlerError::EndingPending);
		}
		self.owner.complete_unresolved(source)?;
		self.state = OperationState::Complete;
		Ok(())
	}
	pub(super) fn retain_driver_cause(&mut self, error: Arc<NativeThrottlerError>) {
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
		let _ = self.owner.retain_diagnostic(self.diagnostic());
	}
	pub(super) fn retain_driver_error(&mut self, error: NativeThrottlerError) {
		let cleanup = self.primary_error.is_some();
		let _ = self.retain_result::<()>(Err(error), cleanup);
	}
	fn retain_result<T>(
		&mut self,
		result: Result<T, NativeThrottlerError>,
		cleanup: bool,
	) -> Result<T, Arc<NativeThrottlerError>> {
		result.map_err(|error| {
			let error = Arc::new(error);
			if cleanup {
				self.cleanup_error = Some(Arc::clone(&error));
			} else if self.primary_error.is_none() {
				self.primary_error = Some(Arc::clone(&error));
			}
			let diagnostic = self.diagnostic();
			// Pending ownership remains pending. The registry retains typed causes if this
			// operation state is subsequently dropped or its client disappears.
			if !matches!(self.state, OperationState::Complete)
				&& let Err(retention_error) = self.owner.retain_diagnostic(diagnostic)
			{
				return Arc::new(NativeThrottlerError::DiagnosticRetention {
					operation_error: error,
					retention_error,
				});
			}
			error
		})
	}
}

async fn apply_pending_outcome(
	bindings: &NativeThrottlerBindings,
	handle: &PgNativeThrottler,
	tx: &mut WriteTx<'_>,
	contracts: crate::runtime::admission::NativeAdmittedContracts,
	pending: &ThrottlerPendingOutcome,
	wait: OutcomePreparationWait<'_>,
) -> Result<(), NativeThrottlerError> {
	let identities = handle.identities(pending.outcome_id());
	let declaration = bindings.code.declaration();
	let writer = contracts
		.bind_throttler_writer(&declaration)
		.map_err(OwnedPrimitiveAdmissionError::from)?;
	wait.require()?;
	let attempt =
		PgRecordGroupAttempt::acquire(&bindings.storage, tx, identities, bindings.group_budget)
			.await??;
	wait.require()?;
	let prepared = writer.prepare_pending_outcome_group(ThrottlerPendingOutcomeScopeGroupFields {
		key: handle.identity.resource(),
		settings: &handle.settings,
		group: ThrottlerPendingOutcomeGroupFields {
			resource_identity: handle.identity.record(),
			snapshot: attempt.snapshot(),
			pending,
		},
	});
	let prepared = match prepared {
		Ok(prepared) => prepared,
		Err(preparation) => {
			return match attempt.restore().await {
				Ok(()) => Err(preparation.into()),
				Err(cleanup) => Err(NativeThrottlerError::PreparationCleanup {
					preparation_error: Box::new(preparation),
					cleanup_error: Box::new(cleanup),
				}),
			};
		}
	};
	let (actions, _) = prepared.into_parts();
	wait.require()?;
	attempt.apply_native(actions).await??;
	wait.require()?;
	Ok(())
}

enum OutcomePreparationWait<'a> {
	ConfirmedLifetime(&'a RuntimeAdmissionTracker),
	Cleanup {
		phase: &'a tokio::sync::watch::Sender<OwnedPrimitiveCleanupPhase>,
		proof_deadline: Instant,
		cleanup_timeout: Duration,
	},
}
impl OutcomePreparationWait<'_> {
	fn require(&self) -> Result<(), NativeThrottlerError> {
		match self {
			Self::ConfirmedLifetime(runtime) => require_confirmed_lifetime(runtime),
			Self::Cleanup {
				phase,
				proof_deadline,
				cleanup_timeout,
			} => require_cleanup_deadline(phase, *proof_deadline, *cleanup_timeout),
		}
	}
}

/// The end of the runtime's confirmed lifetime, which renewal extends. Protected work that the
/// caller waits for must commit before it, and a fenced runtime or a finished lifetime ends the
/// work at once.
fn confirmed_lifetime_deadline(
	runtime: &RuntimeAdmissionTracker,
) -> Result<Instant, NativeThrottlerError> {
	let status = runtime.status();
	if status.lifetime_finished() {
		return Err(NativeThrottlerError::Local(
			RuntimeAdmissionError::LifetimeFinished,
		));
	}
	if status.stop() == Some(RuntimeAdmissionStop::Fenced) {
		return Err(NativeThrottlerError::Local(
			RuntimeAdmissionError::Stopped {
				stop: RuntimeAdmissionStop::Fenced,
			},
		));
	}
	Ok(status.confirmed_until)
}
fn require_confirmed_lifetime(
	runtime: &RuntimeAdmissionTracker,
) -> Result<(), NativeThrottlerError> {
	confirmed_lifetime_deadline(runtime).map(|_| ())
}

enum OperationWait {
	/// The work ends with the runtime's confirmed lifetime, which renewal extends.
	ConfirmedLifetime(RuntimeAdmissionTracker),
	Protected(Instant),
	Probe {
		fixed_deadline: Instant,
		runtime: RuntimeAdmissionTracker,
	},
	Cleanup {
		proof_deadline: Instant,
		receiver: tokio::sync::watch::Receiver<OwnedPrimitiveCleanupPhase>,
		cleanup_timeout: Duration,
	},
}
impl OperationWait {
	fn deadline(&self) -> Result<Instant, NativeThrottlerError> {
		match self {
			Self::ConfirmedLifetime(runtime) => confirmed_lifetime_deadline(runtime),
			Self::Protected(deadline) => Ok(*deadline),
			Self::Probe {
				fixed_deadline,
				runtime,
			} => probe::probe_deadline(*fixed_deadline, runtime),
			Self::Cleanup {
				proof_deadline,
				receiver,
				cleanup_timeout,
			} => cleanup_deadline(*receiver.borrow(), *proof_deadline, *cleanup_timeout),
		}
	}
	async fn changed(&mut self) {
		match self {
			Self::Protected(_) => std::future::pending::<()>().await,
			Self::ConfirmedLifetime(runtime) | Self::Probe { runtime, .. } => {
				let current = runtime.status();
				runtime.wait_for_status_change(current).await;
			}
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
) -> Result<Instant, NativeThrottlerError> {
	match phase {
		OwnedPrimitiveCleanupPhase::OriginOrdering => Ok(proof_deadline),
		OwnedPrimitiveCleanupPhase::AuthenticationAndMutation(start) => start
			.checked_add(cleanup_timeout)
			.ok_or(NativeThrottlerError::InvalidTiming),
	}
}
fn require_cleanup_deadline(
	phase: &tokio::sync::watch::Sender<OwnedPrimitiveCleanupPhase>,
	proof_deadline: Instant,
	cleanup_timeout: Duration,
) -> Result<(), NativeThrottlerError> {
	require_before_deadline(cleanup_deadline(
		*phase.borrow(),
		proof_deadline,
		cleanup_timeout,
	)?)
}
/// Bounds registered acquisition/BEGIN and protected work separately from owned ending.
async fn execute_registered<R>(
	bindings: &NativeThrottlerBindings,
	registration: OwnedTransactionRegistration,
	operation_label: &'static str,
	mut wait: OperationWait,
	work: impl AsyncFnOnce(&mut WriteTx<'_>) -> Result<R, NativeThrottlerError>,
) -> Result<R, NativeThrottlerError> {
	let mut tx = {
		let begin = bindings
			.execution_pool
			.begin_registered_transaction(registration, operation_label);
		tokio::pin!(begin);
		loop {
			let until = wait.deadline()?;
			tokio::select! {
				biased;
				result = &mut begin => break result.map_err(NativeThrottlerError::from_database_error)?,
				_ = wait.changed() => {},
				_ = tokio::time::sleep_until(until) => {
					if Instant::now() >= wait.deadline()? { return Err(NativeThrottlerError::ProtectedTimeout); }
				}
			}
		}
	};
	let marker = tx.statement_interruption_marker();
	let result = {
		let work = work(&mut tx);
		tokio::pin!(work);
		loop {
			let deadline = match wait.deadline() {
				Ok(deadline) => deadline,
				Err(error) => {
					marker.mark_interrupted();
					break Err(error);
				}
			};
			tokio::select! {
				biased;
				_ = tokio::time::sleep_until(deadline) => {
					// A completed full fetch can extend only the cleanup phase. Resample the
					// actual signal before declaring the earlier proof deadline elapsed.
					match wait.deadline() {
						Ok(current) if Instant::now() < current => continue,
						Err(error) => { marker.mark_interrupted(); break Err(error); },
						Ok(_) => {}
					}
					marker.mark_interrupted(); break Err(NativeThrottlerError::ProtectedTimeout);
				}
				result = &mut work => break result,
				_ = wait.changed() => {}
			}
		}
	};
	finish_pool_owned_write_transaction_and_preserve_rollback_error(
		operation_label,
		tx,
		result,
		NativeThrottlerError::from_database_error,
		|_, operation_error, rollback_error| NativeThrottlerError::RollbackFailed {
			operation_error: Box::new(operation_error),
			rollback_error: Box::new(rollback_error),
		},
	)
	.await
}
