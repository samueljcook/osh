//! Automatic progress, registered ending settlement and exact uncertain-operation recovery.

use primitives_rs::time::Deadline;
use std::{num::NonZeroU32, sync::Arc};

use crate::db::RetryClassification;
use crate::db::transaction_ending::TransactionEndingDisposition;
use crate::runtime::admission::upgrade::ending::{
	NativeUpgradeEndingWait, NativeUpgradeEndingWaitError, wait_upgrade_transaction_ending,
};
use crate::runtime::admission::upgrade::{NativeUpgradeEnrollment, NativeUpgradeEnrollmentFields};
use crate::upgrade::requests::work::creation_execution::{
	NativeCreateExecutionError, NativeUpgradeCreateRecoveryObservation,
	NativeUpgradeCreationObservation, NativeUpgradeWorkCreationRequest,
};
use crate::upgrade::requests::work::observation::{
	NativeUpgradeControllerObservation, NativeUpgradeControllerObservationFields,
};
use crate::upgrade::requests::work::recovery_observation::{
	NativeUpgradeRecoveryOrigin, NativeUpgradeRecoveryOriginFields,
	NativeUpgradeRecoveryReadRequest, NativeUpgradeRecoveryState,
};
use crate::upgrade::requests::{
	NativeUpgradeRequestError,
	work::{NativeUpgradeWorkCancellationRequest, NativeUpgradeWorkReadRequest},
};
use crate::upgrade::wait::tokio_instant;
use crate::upgrade::work::PgUpgradeStatus;

use super::super::execution::failure::{
	NativeUpgradeProofBudget, NativeUpgradeStepFailureReason, classify_request_error,
};
use super::super::execution::{NativeUpgradeStepExecutionFields, execute_upgrade_step};
use super::super::selection::{
	NativeUpgradeObservedTerminal, NativeUpgradeSelectionFields, PgUpgradeStep,
	select_upgrade_steps,
};
use super::error::NativeUpgradeDriverError;
use super::observer::PgUpgradePhase;
use super::state::{
	DispatchKind, DriverCompletion, DriverCreation, DriverExit, DriverRecovery, DriverTarget,
};
use super::{DeferredStep, Driver, DriverPurpose, RejectedContext};

enum Decision {
	Retry { transient: bool },
	Reobserve,
	Wait,
	Stop,
	Prerequisite,
}

fn decide(reason: NativeUpgradeStepFailureReason<'_>) -> Decision {
	match reason {
		NativeUpgradeStepFailureReason::Database(error) => match error.retry() {
			RetryClassification::NotRetryable => Decision::Prerequisite,
			RetryClassification::SerializationFailure
			| RetryClassification::Deadlock
			| RetryClassification::LockNotAvailable
			| RetryClassification::ConnectionFailure
			| RetryClassification::QueryCanceled
			| RetryClassification::ServerUnavailable
			| RetryClassification::InsufficientResources
			| RetryClassification::SystemError => Decision::Retry { transient: true },
		},
		NativeUpgradeStepFailureReason::LiveParticipants
		| NativeUpgradeStepFailureReason::ProtectedWait => Decision::Retry { transient: false },
		NativeUpgradeStepFailureReason::ReobserveExactRequest => Decision::Reobserve,
		NativeUpgradeStepFailureReason::WaitForEnding => Decision::Wait,
		NativeUpgradeStepFailureReason::OwnerStopped => Decision::Stop,
		NativeUpgradeStepFailureReason::ProofBudget(_)
		| NativeUpgradeStepFailureReason::OwnershipFailure
		| NativeUpgradeStepFailureReason::Prerequisite => Decision::Prerequisite,
	}
}

impl Driver {
	pub(super) async fn run(mut self) {
		let result = self.drive().await;
		// The failure of a runtime stop, which accompanies an unresolved recovery in place of the
		// uncertain operation's own failure.
		let (mut completion, runtime_stop) = match result {
			Ok(exit) => (DriverCompletion::Succeeded(exit), None),
			Err(source) if enrollment_stopped(&source) => {
				self.remember_failure(Arc::clone(&source));
				(
					DriverCompletion::Succeeded(DriverExit::Stopped),
					Some(source),
				)
			}
			Err(source) => {
				let failure = match self.observation.take_deferred_failure() {
					Some(primary_error) => Arc::new(NativeUpgradeDriverError::FollowingFailure {
						primary_error,
						following_error: source,
					}),
					None => source,
				};
				(DriverCompletion::failed(failure), None)
			}
		};
		self.set_phase(PgUpgradePhase::Stopping);
		if let Err(source) = self.settle().await {
			completion = completion.following_unresolved(source);
		}
		if let Some(recovery) = &self.recovery {
			completion = completion.unresolved_recovery(
				&recovery.state,
				runtime_stop.as_ref().or(recovery.origin_failure.as_ref()),
			);
		}
		let resolution = self
			.owner
			.begin_stopping()
			.and_then(|()| match completion.failure() {
				Some(source) => self.owner.complete_unresolved(
					Arc::clone(source) as Arc<dyn std::error::Error + Send + Sync>
				),
				None => self.owner.complete(),
			});
		if let Err(source) = resolution {
			completion = completion
				.following_unresolved(Arc::new(NativeUpgradeDriverError::Ownership(source)));
		}
		self.shared.update(|snapshot| {
			snapshot.phase = completion.phase();
			if let Some(source) = completion.failure() {
				snapshot.failure = Some(Arc::clone(source));
			}
		});
	}

	async fn drive(&mut self) -> Result<DriverExit, Arc<NativeUpgradeDriverError>> {
		loop {
			self.settle().await?;
			if self.stop_requested() {
				return Ok(DriverExit::Stopped);
			}
			if self.shared.take_cancel_request() {
				self.purpose = DriverPurpose::Cancel;
			}
			if self.recovery.is_some() {
				self.recover_ordering().await?;
				continue;
			}
			if self.creation.state().is_some() {
				self.create().await?;
				continue;
			}
			if self.observation.observed().is_none() {
				self.observe().await?;
				continue;
			}
			if let Some(source) = self.observation.take_deferred_failure() {
				return Err(source);
			}
			if self.purpose == DriverPurpose::Inspect {
				return Ok(DriverExit::Inspected);
			}
			if self.purpose == DriverPurpose::Cancel {
				if let Some(phase) = self.cancel().await? {
					return Ok(phase);
				}
				continue;
			}
			if self
				.observation
				.observed()
				.and_then(|observation| observation.work())
				.is_some_and(|work| work.status() == PgUpgradeStatus::Cancelled)
			{
				return Ok(DriverExit::Cancelled);
			}
			let selection = select_upgrade_steps(NativeUpgradeSelectionFields {
				observation: self
					.observation
					.observed()
					.expect("observed before selecting"),
				resolved: self
					.target
					.resolved()
					.expect("accepted observation resolves its target"),
			})
			.map_err(|error| Arc::new(NativeUpgradeDriverError::Selection(error)))?;
			let (steps, prerequisites, terminal) = selection.into_parts();
			let prerequisites = Arc::new(prerequisites);
			self.shared
				.update(|snapshot| snapshot.prerequisites = Arc::clone(&prerequisites));
			if terminal == Some(NativeUpgradeObservedTerminal::Cancelled) {
				return Ok(DriverExit::Cancelled);
			}
			let now = tokio::time::Instant::now().into_std();
			self.deferred.retain(|deferred| {
				deferred.until.instant() > now && steps.contains(&deferred.step)
			});
			let first = self
				.last_step
				.as_ref()
				.and_then(|last| steps.iter().position(|step| step == last))
				.map_or(0, |index| (index + 1) % steps.len());
			let selected = (0..steps.len())
				.map(|offset| &steps[(first + offset) % steps.len()])
				.find(|step| !self.deferred.iter().any(|deferred| &deferred.step == *step))
				.cloned();
			let Some(step) = selected else {
				if let Some(until) = self
					.deferred
					.iter()
					.map(|step| step.until)
					.min_by_key(|until| until.instant())
				{
					self.set_phase(PgUpgradePhase::WaitingForCompatibility);
					self.wait_until(until).await;
					continue;
				}
				return Err(Arc::new(if prerequisites.is_empty() {
					NativeUpgradeDriverError::NoProcedure
				} else {
					NativeUpgradeDriverError::Prerequisites { prerequisites }
				}));
			};
			if self.steps_in_pass >= self.options.max_steps_per_pass().get() {
				self.steps_in_pass = 0;
				self.backoff().await?;
				continue;
			}
			self.steps_in_pass += 1;
			self.last_step = Some(step.clone());
			self.set_phase(PgUpgradePhase::Executing);
			self.shared
				.update(|snapshot| snapshot.step = Some(step.clone()));
			self.transaction
				.begin_dispatch(DispatchKind::Mutation)
				.map_err(Arc::new)?;
			let result = execute_upgrade_step(NativeUpgradeStepExecutionFields {
				services: &self.services,
				observation: self
					.observation
					.observed()
					.expect("selected actual observation"),
				resolved: self.target.resolved().expect("selected actual target"),
				owner: &self.owner,
				ending: self.transaction.slot_mut(),
				step: &step,
				limits: &self.execution_limits.with_limit(self.limit),
			})
			.await;
			let issued = self.record_issued();
			match result {
				Ok(observation) => {
					self.settle().await?;
					self.accept_observation(observation)?;
					self.transient_failures = 0;
					self.last_rejected = None;
					self.limit = self
						.limit
						.saturating_mul(NonZeroU32::new(2).expect("nonzero"))
						.min(self.execution_limits.limit());
					if step == PgUpgradeStep::CompleteTarget {
						return Ok(DriverExit::Completed);
					}
				}
				Err(error) => {
					let error = Arc::new(error);
					let source = Arc::new(NativeUpgradeDriverError::Step(Arc::clone(&error)));
					self.remember_failure(Arc::clone(&source));
					let ending = self.settle().await?;
					let reason = error.driver_reason();
					if matches!(reason, NativeUpgradeStepFailureReason::ProtectedWait) {
						self.limit =
							NonZeroU32::new(self.limit.get() / 2).unwrap_or(NonZeroU32::MIN);
					}
					let inspection_limited = matches!(
						reason,
						NativeUpgradeStepFailureReason::ProofBudget(
							NativeUpgradeProofBudget::Participants
						)
					);
					let decision =
						if inspection_limited && self.delete_expired_participants(&source).await? {
							Decision::Retry { transient: false }
						} else {
							decide(reason)
						};
					if issued && ending == Some(TransactionEndingDisposition::KnownCommit)
						|| self.recovery.is_some()
					{
						self.observation.invalidate();
					}
					self.handle_failure(decision, source, Some(step)).await?;
				}
			}
		}
	}

	fn set_phase(&self, phase: PgUpgradePhase) {
		self.shared.update(|snapshot| snapshot.phase = phase);
	}
	fn stop_requested(&self) -> bool {
		self.shared.stop_requested() || self.runtime.status().stop().is_some()
	}
	fn record_issued(&mut self) -> bool {
		let issued = self.transaction.finish_dispatch();
		if let Some(receipt) = self.transaction.receipt() {
			self.shared
				.update(|snapshot| snapshot.ending = Some(receipt.clone()));
		}
		issued
	}

	fn remember_failure(&mut self, source: Arc<NativeUpgradeDriverError>) {
		self.failure = Some(Arc::clone(&source));
		self.shared
			.update(|snapshot| snapshot.failure = Some(source));
	}

	/// Every timeout retains the same owner and receipt and starts no next operation.
	async fn settle(
		&mut self,
	) -> Result<Option<TransactionEndingDisposition>, Arc<NativeUpgradeDriverError>> {
		self.transaction.finish_dispatch();
		let outcome = loop {
			let result = match self.transaction.receipt() {
				Some(receipt) => wait_upgrade_transaction_ending(NativeUpgradeEndingWait {
					owner: &self.owner,
					receipt,
					wait_timeout: self.options.ending_wait_timeout(),
				})
				.await
				.map(Some),
				None => {
					let deadline = Deadline::new_from_base(
						tokio::time::Instant::now().into_std(),
						self.options.ending_wait_timeout(),
					)
					.map_err(|_| Arc::new(NativeUpgradeDriverError::LocalClockDomain))?;
					match tokio::time::timeout_at(
						tokio_instant(deadline),
						self.owner.wait_until_continuations_end(),
					)
					.await
					{
						Ok(Ok(())) => Ok(None),
						Ok(Err(source)) => {
							return Err(Arc::new(NativeUpgradeDriverError::Ownership(source)));
						}
						Err(_) => {
							self.set_phase(PgUpgradePhase::WaitingForEnding);
							continue;
						}
					}
				}
			};
			match result {
				Ok(outcome) => break outcome,
				Err(NativeUpgradeEndingWaitError::DeadlineElapsed { .. }) => {
					self.set_phase(PgUpgradePhase::WaitingForEnding);
				}
				Err(source) => {
					return Err(Arc::new(NativeUpgradeDriverError::Ending(Arc::new(source))));
				}
			}
		};
		let disposition = outcome.as_ref().map(|outcome| outcome.disposition());
		if self.transaction.is_mutation_pending()
			&& matches!(
				disposition,
				Some(
					TransactionEndingDisposition::Unknown
						| TransactionEndingDisposition::UnconfirmedDisposal
						| TransactionEndingDisposition::TaskAbandoned
				)
			) {
			let origin = NativeUpgradeRecoveryOrigin::new(NativeUpgradeRecoveryOriginFields {
				identity: self.identity,
				receipt: self
					.transaction
					.receipt()
					.expect("issued mutation has its actual receipt")
					.clone(),
			})
			.map_err(|error| Arc::new(NativeUpgradeDriverError::Request(Arc::new(error))))?;
			// An operation returns its result only after a known `COMMIT`, so the operation with
			// this unknown ending recorded its failure before this settlement.
			self.recovery = Some(DriverRecovery {
				state: NativeUpgradeRecoveryState::Unordered(origin),
				origin_failure: self.failure.clone(),
			});
		}
		self.transaction.record_settled();
		Ok(disposition)
	}

	async fn wait_until(&self, deadline: Deadline) {
		loop {
			let stopped = self.shared.stopped.notified();
			tokio::pin!(stopped);
			stopped.as_mut().enable();
			let runtime = self.runtime.status();
			if self.shared.stop_requested()
				|| self.shared.cancel_requested()
				|| runtime.stop().is_some()
			{
				return;
			}
			tokio::select! {
				_ = &mut stopped => return,
				_ = tokio::time::sleep_until(tokio_instant(deadline)) => return,
				changed = self.runtime.wait_for_status_change(runtime) => {
					if changed.stop().is_some() { return; }
				},
			}
		}
	}
	async fn backoff(&self) -> Result<(), Arc<NativeUpgradeDriverError>> {
		let until = Deadline::new_from_base(
			tokio::time::Instant::now().into_std(),
			self.options.retry_interval(),
		)
		.map_err(|_| Arc::new(NativeUpgradeDriverError::LocalClockDomain))?;
		self.wait_until(until).await;
		Ok(())
	}

	fn accept_observation(
		&mut self,
		observation: NativeUpgradeControllerObservation,
	) -> Result<(), Arc<NativeUpgradeDriverError>> {
		let work = observation
			.shared_work()
			.ok_or_else(|| Arc::new(NativeUpgradeDriverError::WorkAbsent))?;
		let identity = work.target().request().identity();
		if identity.installation_id != self.identity.installation_id
			|| identity.request_id != self.identity.request_id
		{
			return Err(Arc::new(NativeUpgradeDriverError::TargetChanged));
		}
		if let Some(target) = self.target.target() {
			if !target.matches_target(work.target()) {
				return Err(Arc::new(NativeUpgradeDriverError::TargetChanged));
			}
		} else if work.status() != PgUpgradeStatus::Cancelled {
			let target = Arc::new(work.target().clone());
			self.target =
				if self.purpose == DriverPurpose::Advance && !self.shared.cancel_requested() {
					DriverTarget::Resolved(
						self.code
							.resolve(target)
							.map_err(|error| Arc::new(NativeUpgradeDriverError::Target(error)))?,
					)
				} else {
					DriverTarget::Retained(target)
				};
		}
		self.shared
			.update(|snapshot| snapshot.work = Some(Arc::clone(work)));
		self.observation.accept(observation);
		Ok(())
	}

	async fn observe(&mut self) -> Result<(), Arc<NativeUpgradeDriverError>> {
		self.set_phase(PgUpgradePhase::Observing);
		self.transaction
			.begin_dispatch(DispatchKind::Read)
			.map_err(Arc::new)?;
		let enrollment = NativeUpgradeEnrollment::new(NativeUpgradeEnrollmentFields {
			admission: &self.services.admission,
			owner: &self.owner,
			identity: self.identity,
			ending: self.transaction.slot_mut(),
		})
		.map_err(|error| Arc::new(NativeUpgradeDriverError::Admission(error)))?;
		let result = self
			.services
			.requests
			.read_pending_work_observation(NativeUpgradeWorkReadRequest { enrollment })
			.await;
		self.record_issued();
		match result {
			Ok(observation) => {
				self.settle().await?;
				self.accept_observation(observation)
			}
			Err(error) => {
				let error = Arc::new(error);
				let source = Arc::new(NativeUpgradeDriverError::Request(Arc::clone(&error)));
				self.remember_failure(Arc::clone(&source));
				self.settle().await?;
				let decision = decide(classify_request_error(&error));
				self.handle_failure(decision, source, None).await
			}
		}
	}

	async fn cancel(&mut self) -> Result<Option<DriverExit>, Arc<NativeUpgradeDriverError>> {
		let work = Arc::clone(
			self.observation
				.observed()
				.and_then(NativeUpgradeControllerObservation::shared_work)
				.ok_or_else(|| Arc::new(NativeUpgradeDriverError::WorkAbsent))?,
		);
		match work.status() {
			PgUpgradeStatus::Completed => return Ok(Some(DriverExit::Completed)),
			PgUpgradeStatus::Cancelled => return Ok(Some(DriverExit::Cancelled)),
			_ => {}
		}
		self.set_phase(PgUpgradePhase::Executing);
		self.transaction
			.begin_dispatch(DispatchKind::Mutation)
			.map_err(Arc::new)?;
		let enrollment = NativeUpgradeEnrollment::new(NativeUpgradeEnrollmentFields {
			admission: &self.services.admission,
			owner: &self.owner,
			identity: self.identity,
			ending: self.transaction.slot_mut(),
		})
		.map_err(|error| Arc::new(NativeUpgradeDriverError::Admission(error)))?;
		let result = self
			.services
			.requests
			.cancel_pending_work(NativeUpgradeWorkCancellationRequest {
				enrollment,
				expected: &work,
			})
			.await;
		let issued = self.record_issued();
		match result {
			Ok(committed) => {
				self.shared
					.update(|snapshot| snapshot.work = Some(committed.into_work()));
				self.settle().await?;
				Ok(Some(DriverExit::Cancelled))
			}
			Err(error) => {
				let error = Arc::new(error);
				let source = Arc::new(NativeUpgradeDriverError::Request(Arc::clone(&error)));
				self.remember_failure(Arc::clone(&source));
				let ending = self.settle().await?;
				if issued && ending == Some(TransactionEndingDisposition::KnownCommit)
					|| self.recovery.is_some()
				{
					self.observation.invalidate();
				}
				self.handle_failure(decide(classify_request_error(&error)), source, None)
					.await?;
				Ok(None)
			}
		}
	}

	async fn create(&mut self) -> Result<(), Arc<NativeUpgradeDriverError>> {
		self.set_phase(if self.creation.is_recovering() {
			PgUpgradePhase::Recovering
		} else {
			PgUpgradePhase::Starting
		});
		self.transaction
			.begin_dispatch(if self.creation.is_recovering() {
				DispatchKind::Read
			} else {
				DispatchKind::Mutation
			})
			.map_err(Arc::new)?;
		let enrollment = NativeUpgradeEnrollment::new(NativeUpgradeEnrollmentFields {
			admission: &self.services.admission,
			owner: &self.owner,
			identity: self.identity,
			ending: self.transaction.slot_mut(),
		})
		.map_err(|error| Arc::new(NativeUpgradeDriverError::Admission(error)))?;
		let recovering = self.creation.is_recovering();
		let request = NativeUpgradeWorkCreationRequest {
			enrollment,
			state: self
				.creation
				.state_mut()
				.expect("creation retains actual state"),
		};
		let result = if recovering {
			self.services
				.requests
				.recover_creation_observation(request)
				.await
				.map(|recovered| match recovered {
					NativeUpgradeCreateRecoveryObservation::Existing(observation) => {
						Some(*observation)
					}
					NativeUpgradeCreateRecoveryObservation::RetrySameCondition => None,
				})
		} else {
			self.services
				.requests
				.request_work_observation(request)
				.await
				.map(Some)
		};
		self.record_issued();
		match result {
			Ok(observation) => {
				self.settle().await?;
				self.creation.permit_direct();
				if let Some(observation) = observation {
					let NativeUpgradeCreationObservation { observed, work } = observation;
					self.accept_observation(NativeUpgradeControllerObservation::new(
						NativeUpgradeControllerObservationFields {
							observed,
							work: Some(work),
						},
					))?;
					self.creation = DriverCreation::NotCreating;
					self.transient_failures = 0;
				}
				Ok(())
			}
			Err(error) => {
				let recovery_required = matches!(
					primary_creation_error(&error),
					Some(NativeCreateExecutionError::RecoveryRequired)
				);
				let error = Arc::new(error);
				let source = Arc::new(NativeUpgradeDriverError::Request(Arc::clone(&error)));
				self.remember_failure(Arc::clone(&source));
				self.settle().await?;
				let decision = decide(classify_request_error(&error));
				if self.recovery.is_some()
					&& self
						.creation
						.state()
						.is_some_and(|state| state.prepared_attempt().is_some())
				{
					self.creation.require_recovery();
				}
				if recovery_required {
					self.creation.require_recovery();
					return Ok(());
				}
				self.handle_failure(decision, source, None).await
			}
		}
	}

	async fn recover_ordering(&mut self) -> Result<(), Arc<NativeUpgradeDriverError>> {
		self.set_phase(PgUpgradePhase::Recovering);

		self.transaction
			.begin_dispatch(DispatchKind::Read)
			.map_err(Arc::new)?;
		let enrollment = NativeUpgradeEnrollment::new(NativeUpgradeEnrollmentFields {
			admission: &self.services.admission,
			owner: &self.owner,
			identity: self.identity,
			ending: self.transaction.slot_mut(),
		})
		.map_err(|error| Arc::new(NativeUpgradeDriverError::Admission(error)))?;
		let result = self
			.services
			.requests
			.read_pending_work_recovery_observation(NativeUpgradeRecoveryReadRequest {
				enrollment,
				recovery: &mut self
					.recovery
					.as_mut()
					.expect("recovery retains its actual origin")
					.state,
				recovery_wait_timeout: self.options.recovery_wait_timeout(),
			})
			.await;
		self.record_issued();
		self.settle().await?;
		let ordered = self
			.recovery
			.as_ref()
			.is_some_and(|recovery| recovery.state.is_ordered());
		if ordered {
			self.recovery = None;
		}
		match result {
			Ok(observation) => {
				// Creation recovery must compare its retained baseline even after physical ordering.
				if self.creation.state().is_none() {
					self.accept_observation(observation)?;
				}
				Ok(())
			}
			Err(error) => {
				let decision = decide(classify_request_error(&error));
				let source = Arc::new(NativeUpgradeDriverError::Request(Arc::new(error)));
				self.remember_failure(Arc::clone(&source));
				if matches!(decision, Decision::Prerequisite | Decision::Reobserve) {
					return Err(source);
				}
				if ordered {
					self.observation.invalidate();
				}
				self.handle_failure(decision, source, None).await
			}
		}
	}

	/// A step's participant inspection reads at most a fixed number of registrations, and the step
	/// deletes the expired ones only if it commits, so registrations that processes leave behind
	/// when they stop without shutting down could otherwise block every later upgrade. This
	/// commits the deletion of the expired registrations among those that one inspection reads, and
	/// returns whether it deleted any, in which case the step can run again. A failure follows
	/// `step_failure`.
	async fn delete_expired_participants(
		&self,
		step_failure: &Arc<NativeUpgradeDriverError>,
	) -> Result<bool, Arc<NativeUpgradeDriverError>> {
		let resolved = self
			.target
			.resolved()
			.expect("a step runs only after its target is resolved");
		let inspection = self
			.services
			.participants
			.inspect_and_fence(
				resolved.candidate(),
				self.execution_limits.participant_budget(),
			)
			.await
			.map_err(|error| {
				Arc::new(NativeUpgradeDriverError::FollowingFailure {
					primary_error: Arc::clone(step_failure),
					following_error: Arc::new(
						NativeUpgradeDriverError::ExpiredParticipantDeletion(Arc::new(error)),
					),
				})
			})?;
		Ok(inspection.expired_fenced_count > 0)
	}

	async fn handle_failure(
		&mut self,
		decision: Decision,
		source: Arc<NativeUpgradeDriverError>,
		step: Option<PgUpgradeStep>,
	) -> Result<(), Arc<NativeUpgradeDriverError>> {
		match decision {
			Decision::Stop => Ok(()),
			Decision::Prerequisite => {
				if self.recovery.is_some()
					|| self.observation.observed().is_none() && step.is_some()
				{
					self.observation.defer_failure(source);
					Ok(())
				} else {
					Err(source)
				}
			}
			Decision::Reobserve => {
				if let Some(step) = step {
					if let Some(work) = self.shared.snapshot().work {
						if self.last_rejected.as_ref().is_some_and(|rejected| {
							rejected.step == step && rejected.work.matches_work(&work)
						}) {
							return Err(Arc::new(NativeUpgradeDriverError::RepeatedContext {
								source,
							}));
						}
						self.last_rejected = Some(RejectedContext { step, work });
					}
					self.observation.invalidate();
				} else {
					if !self.observation.request_reobservation() {
						return Err(Arc::new(NativeUpgradeDriverError::RepeatedContext {
							source,
						}));
					}
				}
				Ok(())
			}
			Decision::Retry { transient } => {
				if transient {
					self.count_transient(Arc::clone(&source))?;
				}
				if let Some(step) = step {
					let until = Deadline::new_from_base(
						tokio::time::Instant::now().into_std(),
						self.options.retry_interval(),
					)
					.map_err(|_| Arc::new(NativeUpgradeDriverError::LocalClockDomain))?;
					self.deferred.retain(|deferred| deferred.step != step);
					self.deferred.push(DeferredStep { step, until });
					Ok(())
				} else {
					self.backoff().await
				}
			}
			Decision::Wait => {
				self.count_transient(source)?;
				self.settle().await?;
				self.backoff().await
			}
		}
	}

	fn count_transient(
		&mut self,
		source: Arc<NativeUpgradeDriverError>,
	) -> Result<(), Arc<NativeUpgradeDriverError>> {
		if self.transient_failures >= self.options.max_consecutive_transient_failures().get() {
			let source = Arc::new(
				NativeUpgradeDriverError::MaxConsecutiveTransientFailuresReached { source },
			);
			return Err(source);
		}
		self.transient_failures += 1;
		Ok(())
	}
}

fn enrollment_stopped(error: &NativeUpgradeDriverError) -> bool {
	matches!(
		error,
		NativeUpgradeDriverError::Admission(
			crate::runtime::admission::upgrade::NativeUpgradeAdmissionError::Local(
				crate::runtime::local_admission::RuntimeAdmissionError::Stopped { .. }
			)
		)
	)
}

fn primary_creation_error(
	error: &NativeUpgradeRequestError,
) -> Option<&NativeCreateExecutionError> {
	match error {
		NativeUpgradeRequestError::Creation(source) => Some(source),
		NativeUpgradeRequestError::Progress(
			crate::upgrade::progress::ControlProgressError::Creation(source),
		) => Some(source),
		NativeUpgradeRequestError::RollbackFailed {
			operation_error, ..
		} => primary_creation_error(operation_error),
		_ => None,
	}
}
