//! Probe-only extension and exact predecessor ordering use the existing registered ending owner.

use super::{
	Arc, Instant, NativeThrottlerError, NativeThrottlerOperation, NativeThrottlerPermit,
	OperationState, OperationWait, OriginPurpose, OutcomePreparationWait,
	OwnedPrimitiveAdmissionError, OwnedPrimitiveCleanupPhase, PrimitiveScopeBindingError,
	ThrottlerResourceWriteFields, TransactionEndingDisposition, apply_pending_outcome, deadline,
	execute_registered, require_before_deadline,
};
use crate::db::{DatabaseError, RetryClassification};
use crate::fleet::throttler::state::ThrottlerProbeExtensionInput;
use crate::records::{PointRecordLock, RecordMetadata};
use crate::runtime::local_admission::{RuntimeAdmissionStop, RuntimeAdmissionTracker};

impl NativeThrottlerOperation {
	pub(in crate::fleet::throttler::engine) async fn extend_probe_before(
		&mut self,
		until: Instant,
	) -> Result<bool, Arc<NativeThrottlerError>> {
		self.begin_attempt();
		let result = self.extend_probe_inner(until).await;
		match result {
			Ok(matched) => {
				self.heartbeat_error = None;
				Ok(matched)
			}
			Err(error) => {
				let error = Arc::new(error);
				self.heartbeat_error = Some(Arc::clone(&error));
				let _ = self.owner.retain_diagnostic(self.diagnostic());
				Err(error)
			}
		}
	}
	pub(in crate::fleet::throttler::engine) fn retain_probe_driver_cause(
		&mut self,
		error: Arc<NativeThrottlerError>,
	) {
		self.heartbeat_error = Some(error);
		let _ = self.owner.retain_diagnostic(self.diagnostic());
	}
	pub(in crate::fleet::throttler::engine) fn retain_probe_driver_error(
		&mut self,
		error: NativeThrottlerError,
	) {
		self.retain_probe_driver_cause(Arc::new(error));
	}
	async fn extend_probe_inner(&mut self, until: Instant) -> Result<bool, NativeThrottlerError> {
		if !matches!(self.state, OperationState::Held { .. }) {
			return Err(NativeThrottlerError::OperationState);
		}
		let authority = self
			.held_permit()
			.and_then(NativeThrottlerPermit::probe_extension_authority)
			.ok_or(NativeThrottlerError::OperationState)?;
		let handle = self.handle.clone();
		let bindings = &handle.bindings;
		let runtime = self.owner.admission_tracker();
		require_probe_deadline(until, &runtime)?;
		let mut pack = bindings
			.admission
			.register_throttler_probe_extension(&self.owner, &handle.identity)?;
		let receipt = pack.receipt();
		self.register_attempt(OriginPurpose::Probe, receipt)?;
		let registration = pack.take_registration();
		let matched = execute_registered(
			bindings,
			registration,
			"fleet.native_throttler.extend_probe",
			OperationWait::Probe {
				fixed_deadline: until,
				runtime: runtime.clone(),
			},
			async |tx| {
				let contracts = pack
					.authenticate(tx, self.attempt.ordering_mut()?, None)
					.await?;
				require_probe_deadline(until, &runtime)?;
				let declaration = bindings.code.declaration();
				let writer = contracts
					.bind_throttler_writer(&declaration)
					.map_err(OwnedPrimitiveAdmissionError::from)?;
				let resource = writer.bind_resource(ThrottlerResourceWriteFields {
					identity: handle.identity.record(),
					key: handle.identity.resource(),
					settings: &handle.settings,
				})?;
				let lock = bindings
					.storage
					.lock(
						tx,
						handle.identity.record(),
						bindings.group_budget.conflict_attempts,
					)
					.await?;
				require_probe_deadline(until, &runtime)?;
				let prepared = (|| {
					let snapshot = lock.snapshot();
					let Some(record) = snapshot.record() else {
						return Ok((None, false));
					};
					let mut state = resource
						.decode_record(record)
						.map_err(PrimitiveScopeBindingError::from)?;
					let matched = resource
						.extend_probe(
							&mut state,
							ThrottlerProbeExtensionInput {
								authority: &authority,
								observed_at: snapshot.observed_at(),
							},
						)
						.map_err(PrimitiveScopeBindingError::from)?;
					let action = if matched {
						Some(
							resource
								.encode_record(
									&state,
									RecordMetadata {
										expires_at: None,
										updated_at: snapshot.observed_at(),
									},
								)
								.map_err(PrimitiveScopeBindingError::from)?,
						)
					} else {
						None
					};
					Ok::<_, PrimitiveScopeBindingError>((action, matched))
				})();
				let (action, matched) = match prepared {
					Ok(prepared) => prepared,
					Err(preparation) => {
						let source = match lock.keep().await {
							Ok(()) => preparation.into(),
							Err(cleanup) => NativeThrottlerError::PointPreparationCleanup {
								preparation_error: Box::new(preparation),
								cleanup_error: Box::new(cleanup),
							},
						};
						return Err(NativeThrottlerError::Record {
							identity: handle.identity.record().clone(),
							source: Box::new(source),
						});
					}
				};
				require_probe_deadline(until, &runtime)?;
				match action {
					Some(record) => lock.write(record).await?,
					None => lock.keep().await?,
				}
				require_probe_deadline(until, &runtime)?;
				Ok(matched)
			},
		)
		.await?;
		// A caller timeout can abandon this waiter; the actual receipt and continuation remain owned.
		let settled = async {
			self.origin()
				.expect("probe dispatch retained its receipt")
				.wait_until_terminal()
				.await;
			self.owner.wait_until_continuations_end().await
		};
		tokio::time::timeout_at(until, settled)
			.await
			.map_err(|_| NativeThrottlerError::EndingPending)??;
		require_probe_deadline(until, &runtime)?;
		Ok(matched)
	}
	/// Retry proof is this attempt's actual rollback, never an earlier successful origin.
	pub(in crate::fleet::throttler::engine) fn probe_retry_is_known_rollback(
		&self,
		error: &NativeThrottlerError,
	) -> bool {
		let Some(receipt) = self.attempt.receipt() else {
			return false;
		};
		let crate::db::transaction_ending::TransactionEndingSnapshot::Terminal(ending) =
			receipt.snapshot()
		else {
			return false;
		};
		ending.disposition() == TransactionEndingDisposition::KnownRollback
			&& !ending.commit_attempted()
			&& self
				.owner
				.snapshot()
				.is_ok_and(|entry| !entry.continuation.is_retained())
			&& retryable_database_error(error)
	}
	pub(super) async fn dispatch_outcome_after_probe(
		&mut self,
	) -> Result<(), NativeThrottlerError> {
		self.wait_reconciliation_origins().await?;
		let origins = self.retain_reconciliation_origins()?;
		let handle = self.handle.clone();
		let bindings = &handle.bindings;
		let proof_deadline = deadline(bindings.timing.physical_proof_attempt_timeout)?;
		let cleanup_timeout = bindings.timing.cleanup_attempt_timeout;
		let (phase, receiver) =
			tokio::sync::watch::channel(OwnedPrimitiveCleanupPhase::OriginOrdering);
		let mut pack = bindings.admission.register_throttler_outcome_after_probe(
			&self.owner,
			&handle.identity,
			origins,
		)?;
		let receipt = pack.receipt();
		self.register_attempt(OriginPurpose::Outcome, receipt)?;
		let registration = pack.take_registration();
		let OperationState::OutcomePending(pending) = &self.state else {
			return Err(NativeThrottlerError::OperationState);
		};
		let execution = execute_registered(
			bindings,
			registration,
			"fleet.native_throttler.dispatch_outcome_after_probe",
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
}

pub(super) fn probe_deadline(
	fixed: Instant,
	runtime: &RuntimeAdmissionTracker,
) -> Result<Instant, NativeThrottlerError> {
	let current = runtime.status();
	if current.lifetime_finished() || current.stop() == Some(RuntimeAdmissionStop::Fenced) {
		return Err(NativeThrottlerError::ProbeExtensionLost);
	}
	// Runtime snapshots fence actual expiry and recompute timely participant renewal.
	Ok(fixed.min(current.confirmed_until))
}
fn require_probe_deadline(
	fixed: Instant,
	runtime: &RuntimeAdmissionTracker,
) -> Result<(), NativeThrottlerError> {
	require_before_deadline(probe_deadline(fixed, runtime)?)
}
fn retryable_database_error(error: &NativeThrottlerError) -> bool {
	fn retryable(error: &DatabaseError) -> bool {
		error.retry() != RetryClassification::NotRetryable
	}
	match error {
		NativeThrottlerError::Database(error) => retryable(error),
		NativeThrottlerError::Admission(error) => error.database_error().is_some_and(retryable),
		NativeThrottlerError::Group(error) => error.database_error().is_some_and(retryable),
		NativeThrottlerError::Storage(error) => error.database_error().is_some_and(retryable),
		NativeThrottlerError::Record { source, .. } => retryable_database_error(source),
		// Aggregated failed cleanup is not a confirmed clean retry even if one cause is retryable.
		_ => false,
	}
}
