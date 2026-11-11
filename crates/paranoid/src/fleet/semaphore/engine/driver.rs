//! One same-registry task owns acquisition, acknowledged delivery and exact release accounting.

use super::delivery::{
	CallerDelivery, DeliveryAcknowledgement, InitialDelivery, InitialDeliveryState,
	NativeSemaphoreClient, NativeSemaphorePendingDelivery, NativeSemaphoreStartResult,
};
use super::{
	Arc, NativeSemaphoreAcquireFields, NativeSemaphoreAcquisitionChoice,
	NativeSemaphoreAcquisitionObservation, NativeSemaphoreCancellationReason,
	NativeSemaphoreCleanupObservation, NativeSemaphoreCompletionObservation,
	NativeSemaphoreDiagnosticError, NativeSemaphoreError, NativeSemaphoreExecutionBindings,
	NativeSemaphoreOperation, NativeSemaphoreStartOptions,
};
use crate::fleet::SemaphoreClaimSnapshot;
use crate::fleet::semaphore::state::SemaphoreUndeliveredTarget;
use crate::runtime::local_admission::membership::RuntimeResourceWorkState;
use crate::runtime::local_admission::semaphore_work::{
	RuntimeSemaphoreCallerWorkPermit, RuntimeSemaphoreWorkId, RuntimeSemaphoreWorkSnapshot,
};
use crate::runtime::local_admission::{
	RuntimeAdmissionStop, RuntimeAdmissionTracker, RuntimeWorkView,
};
use std::{sync::Mutex, time::Duration};
use tokio::{
	sync::{Notify, oneshot},
	time::Instant,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeSemaphoreLocalLoss {
	RuntimeStopped,
	HoldWindowElapsed,
	ClientClosed,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeSemaphoreLocalObservation {
	Pending,
	ConfirmedLocally,
	Lost(NativeSemaphoreLocalLoss),
	Closing,
	Finished,
}
#[derive(Clone)]
pub(crate) enum NativeSemaphoreAccountingObservation {
	Pending,
	Complete(NativeSemaphoreCleanupObservation),
	Unresolved,
	Abandoned,
}
type HeldObservation = super::pending::NativeSemaphoreHeldObservation;
type CallerResponse = oneshot::Sender<Result<CallerDelivery, Arc<NativeSemaphoreError>>>;

#[derive(Clone, Copy, Eq, PartialEq)]
enum DriverStage {
	Acquiring,
	Queued,
	DeliveredIdle,
	CallerRequested,
	CallerQueued,
	Running,
	Closing,
	Finished,
}

enum TerminalAccounting {
	Complete {
		result: NativeSemaphoreCleanupObservation,
		_diagnostic: Arc<NativeSemaphoreDiagnosticError>,
	},
	Unresolved(Arc<NativeSemaphoreDiagnosticError>),
}

enum DriverPhase {
	Acquiring,
	Queued {
		held: HeldObservation,
		target: SemaphoreUndeliveredTarget,
	},
	DeliveredIdle(HeldObservation),
	CallerRequested {
		held: HeldObservation,
		response: CallerResponse,
	},
	CallerQueued {
		held: HeldObservation,
		permit: RuntimeSemaphoreCallerWorkPermit,
	},
	Running(HeldObservation),
	Closing {
		held: Option<HeldObservation>,
	},
	Finished {
		held: Option<HeldObservation>,
		accounting: TerminalAccounting,
	},
}

impl DriverPhase {
	fn stage(&self) -> DriverStage {
		match self {
			Self::Acquiring => DriverStage::Acquiring,
			Self::Queued { .. } => DriverStage::Queued,
			Self::DeliveredIdle(_) => DriverStage::DeliveredIdle,
			Self::CallerRequested { .. } => DriverStage::CallerRequested,
			Self::CallerQueued { .. } => DriverStage::CallerQueued,
			Self::Running(_) => DriverStage::Running,
			Self::Closing { .. } => DriverStage::Closing,
			Self::Finished { .. } => DriverStage::Finished,
		}
	}
	fn held(&self) -> Option<&HeldObservation> {
		match self {
			Self::Acquiring => None,
			Self::Queued { held, .. }
			| Self::DeliveredIdle(held)
			| Self::CallerRequested { held, .. }
			| Self::CallerQueued { held, .. }
			| Self::Running(held) => Some(held),
			Self::Closing { held } | Self::Finished { held, .. } => held.as_ref(),
		}
	}
	fn into_held(self) -> Option<HeldObservation> {
		match self {
			Self::Acquiring => None,
			Self::Queued { held, .. }
			| Self::DeliveredIdle(held)
			| Self::CallerRequested { held, .. }
			| Self::CallerQueued { held, .. }
			| Self::Running(held) => Some(held),
			Self::Closing { held } | Self::Finished { held, .. } => held,
		}
	}
	fn accounting(&self) -> NativeSemaphoreAccountingObservation {
		match self {
			Self::Finished {
				accounting: TerminalAccounting::Complete { result, .. },
				..
			} => NativeSemaphoreAccountingObservation::Complete(*result),
			Self::Finished {
				accounting: TerminalAccounting::Unresolved(_diagnostic),
				..
			} => NativeSemaphoreAccountingObservation::Unresolved,
			_ => NativeSemaphoreAccountingObservation::Pending,
		}
	}
}

enum ClientClosure {
	Open,
	Closed(NativeSemaphoreCancellationReason),
}
impl ClientClosure {
	fn close(&mut self, reason: NativeSemaphoreCancellationReason) {
		if matches!(self, Self::Open) {
			*self = Self::Closed(reason);
		}
	}
	fn reason(&self) -> Option<NativeSemaphoreCancellationReason> {
		match self {
			Self::Open => None,
			Self::Closed(reason) => Some(*reason),
		}
	}
	fn is_closed(&self) -> bool {
		matches!(self, Self::Closed(_))
	}
}

struct DriverState {
	phase: DriverPhase,
	closure: ClientClosure,
	loss: std::sync::OnceLock<NativeSemaphoreLocalLoss>,
}
pub(super) struct DriverShared {
	state: Mutex<DriverState>,
	changed: Notify,
	runtime: RuntimeAdmissionTracker,
	work: RuntimeSemaphoreWorkId,
}
impl DriverShared {
	fn lock(&self) -> std::sync::MutexGuard<'_, DriverState> {
		self.state
			.lock()
			.expect("native Semaphore driver state is not poisoned")
	}
	pub(super) fn close(&self) {
		let mut state = self.lock();
		Self::close_locked(&mut state);
		self.changed.notify_waiters();
	}
	fn close_locked(state: &mut DriverState) {
		state
			.closure
			.close(NativeSemaphoreCancellationReason::ClientClosed);
		let phase = std::mem::replace(&mut state.phase, DriverPhase::Closing { held: None });
		state.phase = match phase {
			DriverPhase::CallerRequested { held, .. } | DriverPhase::CallerQueued { held, .. } => {
				DriverPhase::Closing { held: Some(held) }
			}
			other => other,
		};
	}
	fn acquisition_cancellation(&self) -> Option<NativeSemaphoreCancellationReason> {
		let mut state = self.lock();
		let runtime = self.runtime.status();
		if let Some(stop) = runtime.stop() {
			state
				.loss
				.get_or_init(|| NativeSemaphoreLocalLoss::RuntimeStopped);
			state
				.closure
				.close(NativeSemaphoreCancellationReason::RuntimeStopped(stop));
			Self::close_locked(&mut state);
			self.changed.notify_waiters();
		}
		state.closure.reason()
	}

	async fn wait_acquisition_cancellation(&self) -> NativeSemaphoreCancellationReason {
		loop {
			let changed = self.changed.notified();
			tokio::pin!(changed);
			changed.as_mut().enable();
			let runtime = self.runtime.status();
			if let Some(reason) = self.acquisition_cancellation() {
				return reason;
			}
			tokio::select! {
				_ = changed => {},
				_ = self.runtime.wait_for_status_change(runtime) => {},
			}
		}
	}
	/// Samples current runtime and local state together; a stale participant cap is never retained.
	fn observe_locked(
		&self,
		state: &mut DriverState,
	) -> (
		NativeSemaphoreLocalObservation,
		RuntimeWorkView<RuntimeSemaphoreWorkSnapshot>,
	) {
		let runtime = self.runtime.work_view(self.work);
		if state.loss.get().is_none() {
			if runtime.lifetime_finished()
				|| runtime.stop() == Some(RuntimeAdmissionStop::Fenced)
				|| (runtime.stop().is_some() && state.phase.stage() != DriverStage::Running)
			{
				state
					.loss
					.get_or_init(|| NativeSemaphoreLocalLoss::RuntimeStopped);
			} else if state
				.phase
				.held()
				.is_some_and(|held| Instant::now() >= held.hold_until)
			{
				state
					.loss
					.get_or_init(|| NativeSemaphoreLocalLoss::HoldWindowElapsed);
			} else if state.closure.is_closed() && state.phase.stage() != DriverStage::Running {
				state
					.loss
					.get_or_init(|| NativeSemaphoreLocalLoss::ClientClosed);
			}
		}
		let local = if let Some(loss) = state.loss.get() {
			NativeSemaphoreLocalObservation::Lost(*loss)
		} else {
			match state.phase.stage() {
				DriverStage::Acquiring => NativeSemaphoreLocalObservation::Pending,
				DriverStage::Closing => NativeSemaphoreLocalObservation::Closing,
				DriverStage::Finished => NativeSemaphoreLocalObservation::Finished,
				_ => NativeSemaphoreLocalObservation::ConfirmedLocally,
			}
		};
		(local, runtime)
	}
	fn require_delivery(
		&self,
		state: &mut DriverState,
		expected: DriverStage,
	) -> Result<(), Arc<NativeSemaphoreError>> {
		let (local, runtime) = self.observe_locked(state);
		let owner = runtime.work.as_ref();
		if state.phase.stage() != expected
			|| state.closure.is_closed()
			|| local != NativeSemaphoreLocalObservation::ConfirmedLocally
			|| runtime.stop().is_some()
			|| runtime.lifetime_finished()
			|| !owner.is_some_and(|entry| {
				entry.state == RuntimeResourceWorkState::Held
					&& !entry.continuation.is_retained()
					&& entry.pending_caller_work_count
						== usize::from(expected == DriverStage::CallerQueued)
			}) {
			return Err(Arc::new(NativeSemaphoreError::DeliveryRejected));
		}
		Ok(())
	}
	pub(super) fn accept_initial(&self) -> Result<(), Arc<NativeSemaphoreError>> {
		let mut state = self.lock();
		self.require_delivery(&mut state, DriverStage::Queued)?;
		let DriverPhase::Queued { held, .. } =
			std::mem::replace(&mut state.phase, DriverPhase::Closing { held: None })
		else {
			unreachable!("initial acceptance consumes queued delivery");
		};
		state.phase = DriverPhase::DeliveredIdle(held);
		self.changed.notify_waiters();
		Ok(())
	}
	pub(super) fn request_caller(
		&self,
		sender: CallerResponse,
	) -> Result<(), Arc<NativeSemaphoreError>> {
		let mut state = self.lock();
		self.require_delivery(&mut state, DriverStage::DeliveredIdle)?;
		let DriverPhase::DeliveredIdle(held) =
			std::mem::replace(&mut state.phase, DriverPhase::Closing { held: None })
		else {
			unreachable!("caller request consumes idle delivery");
		};
		state.phase = DriverPhase::CallerRequested {
			held,
			response: sender,
		};
		self.changed.notify_waiters();
		Ok(())
	}
	pub(super) fn accept_caller(
		&self,
	) -> Result<(RuntimeSemaphoreCallerWorkPermit, SemaphoreClaimSnapshot), Arc<NativeSemaphoreError>>
	{
		let mut state = self.lock();
		self.require_delivery(&mut state, DriverStage::CallerQueued)?;
		let DriverPhase::CallerQueued { held, permit } =
			std::mem::replace(&mut state.phase, DriverPhase::Closing { held: None })
		else {
			unreachable!("caller acceptance consumes the actual queued permit");
		};
		let claim = held.claim.clone();
		state.phase = DriverPhase::Running(held);
		Ok((permit, claim))
	}
	pub(super) fn finish_caller(&self) {
		let mut state = self.lock();
		if state.phase.stage() == DriverStage::Running {
			state
				.closure
				.close(NativeSemaphoreCancellationReason::ClientClosed);
			let phase = std::mem::replace(&mut state.phase, DriverPhase::Closing { held: None });
			state.phase = DriverPhase::Closing {
				held: phase.into_held(),
			};
		}
		self.changed.notify_waiters();
	}
}
/// Clones contain no caller count, capability, cleanup target or SQL permission.
#[derive(Clone)]
pub(crate) struct NativeSemaphoreObserver(pub(super) Arc<DriverShared>);
impl NativeSemaphoreObserver {
	pub(crate) fn snapshot(&self) -> NativeSemaphoreLocalObservation {
		self.0.observe_locked(&mut self.0.lock()).0
	}
	pub(crate) fn accounting(&self) -> NativeSemaphoreAccountingObservation {
		let state = self.0.lock();
		if matches!(
			state.phase.accounting(),
			NativeSemaphoreAccountingObservation::Pending
		) && self
			.0
			.runtime
			.work_view(self.0.work)
			.work
			.is_some_and(|entry| entry.state == RuntimeResourceWorkState::Abandoned)
		{
			return NativeSemaphoreAccountingObservation::Abandoned;
		}
		state.phase.accounting()
	}
	pub(crate) async fn wait_accounting(&self) -> NativeSemaphoreAccountingObservation {
		loop {
			let changed = self.0.changed.notified();
			tokio::pin!(changed);
			changed.as_mut().enable();
			let runtime = self.0.runtime.work_view(self.0.work);
			let result = self.accounting();
			if !matches!(result, NativeSemaphoreAccountingObservation::Pending) {
				return result;
			}
			tokio::select! { _=changed=>{}, _=self.0.runtime.wait_for_work_change(self.0.work, &runtime)=>{} }
		}
	}
}
impl NativeSemaphoreExecutionBindings {
	/// Reserves the same actual work/task before spawning on its retained originating runtime.
	/// A committed acquisition is delivered whenever its caller still waits for it.
	pub(crate) fn start_owned(
		self: &Arc<Self>,
		options: NativeSemaphoreStartOptions,
	) -> Result<NativeSemaphorePendingDelivery, NativeSemaphoreError> {
		let registration = self.register_owner()?;
		let identity = registration.identity;
		let owner = registration.owner;
		let shared = Arc::new(DriverShared {
			state: Mutex::new(DriverState {
				phase: DriverPhase::Acquiring,
				closure: ClientClosure::Open,
				loss: std::sync::OnceLock::new(),
			}),
			changed: Notify::new(),
			runtime: owner.admission_tracker(),
			work: owner.id(),
		});
		let (sender, receiver) = oneshot::channel();
		let pending = NativeSemaphorePendingDelivery {
			receiver,
			shared: Arc::clone(&shared),
			acknowledgement: DeliveryAcknowledgement::Awaiting,
		};
		let bindings = Arc::clone(self);
		owner.spawn_owned_task(&self.originating_runtime, move |owner| async move {
			let mut driver = Driver {
				operation: NativeSemaphoreOperation::new(bindings, identity, owner),
				shared,
			};
			driver.run(options, sender).await;
		})?;
		Ok(pending)
	}
}
struct Driver {
	operation: NativeSemaphoreOperation,
	shared: Arc<DriverShared>,
}
impl Driver {
	async fn run(
		&mut self,
		options: NativeSemaphoreStartOptions,
		response: oneshot::Sender<InitialDelivery>,
	) {
		let initial = self.acquire(options).await;
		if let Err(error) = &initial {
			self.operation.retain_driver_cause(Arc::clone(error));
		}
		let value = match initial {
			Ok(NativeSemaphoreAcquisitionObservation::Acquired) => {
				match self.operation.held_observation() {
					Ok(held) => {
						let target = self
							.operation
							.take_delivery_target()
							.expect("newly confirmed grant retains its delivery target");
						let mut state = self.shared.lock();
						state.phase = DriverPhase::Queued { held, target };
						Ok(NativeSemaphoreStartResult::Acquired(
							NativeSemaphoreClient::new(Arc::clone(&self.shared)),
						))
					}
					Err(error) => {
						let error = Arc::new(error);
						self.operation.retain_driver_cause(Arc::clone(&error));
						Err(error)
					}
				}
			}
			Ok(NativeSemaphoreAcquisitionObservation::Unavailable) => {
				Ok(NativeSemaphoreStartResult::Unavailable)
			}
			Err(error) => Err(error),
		};
		let acquired = matches!(&value, Ok(NativeSemaphoreStartResult::Acquired(_)));
		let _ = response.send(InitialDelivery {
			state: InitialDeliveryState::Queued(value),
			shared: Arc::clone(&self.shared),
		});
		let delivered = if acquired {
			self.wait_task().await
		} else {
			false
		};
		{
			let mut state = self.shared.lock();
			let previous = std::mem::replace(&mut state.phase, DriverPhase::Closing { held: None });
			let held = match previous {
				DriverPhase::Queued { held, target } => {
					self.operation.restore_delivery_target(Some(target));
					Some(held)
				}
				other => other.into_held(),
			};
			state.phase = DriverPhase::Closing { held };
		}
		if delivered && let Ok(result) = self.operation.consume_release().await {
			self.publish_complete(NativeSemaphoreCleanupObservation::Released(result));
			return;
		}
		self.finish_accounting().await;
	}
	async fn acquire(
		&mut self,
		options: NativeSemaphoreStartOptions,
	) -> Result<NativeSemaphoreAcquisitionObservation, Arc<NativeSemaphoreError>> {
		loop {
			if let Some(reason) = self.shared.acquisition_cancellation() {
				return Err(Arc::new(NativeSemaphoreError::AcquisitionCancelled {
					reason,
				}));
			}
			let fields = NativeSemaphoreAcquireFields {
				holder: options.holder.clone(),
			};
			let result = {
				let acquisition = self.operation.acquire_attempt(fields);
				tokio::pin!(acquisition);
				tokio::select! { biased;
					result=&mut acquisition=>result,
					reason=self.shared.wait_acquisition_cancellation()=>Err(Arc::new(NativeSemaphoreError::AcquisitionCancelled {reason})),
				}
			};
			match result? {
				NativeSemaphoreAcquisitionObservation::Acquired => {
					return Ok(NativeSemaphoreAcquisitionObservation::Acquired);
				}
				NativeSemaphoreAcquisitionObservation::Unavailable => match options.acquisition {
					NativeSemaphoreAcquisitionChoice::TryOnce => {
						return Ok(NativeSemaphoreAcquisitionObservation::Unavailable);
					}
					NativeSemaphoreAcquisitionChoice::Waiting(policy) => {
						let next = deadline(policy.acquire_retry_interval).map_err(Arc::new)?;
						tokio::select! { biased;
							reason=self.shared.wait_acquisition_cancellation()=>return Err(Arc::new(NativeSemaphoreError::AcquisitionCancelled {reason})),
							_=tokio::time::sleep_until(next)=>{},
						}
						self.operation
							.continue_waiting_after_denial()
							.map_err(Arc::new)?;
					}
				},
			}
		}
	}
	/// Returns whether actual initial acknowledgment disabled the undelivered purpose.
	async fn wait_task(&mut self) -> bool {
		loop {
			let shared = Arc::clone(&self.shared);
			let changed = shared.changed.notified();
			tokio::pin!(changed);
			changed.as_mut().enable();
			let mut response = None;
			let (runtime, until, caller_pending, close, delivered) = {
				let mut state = self.shared.lock();
				let (local, runtime) = self.shared.observe_locked(&mut state);
				if runtime.stop().is_some()
					|| matches!(local, NativeSemaphoreLocalObservation::Lost(_))
				{
					DriverShared::close_locked(&mut state);
				}
				if state.phase.stage() == DriverStage::CallerRequested && !state.closure.is_closed()
				{
					let DriverPhase::CallerRequested {
						held,
						response: sender,
					} = std::mem::replace(&mut state.phase, DriverPhase::Closing { held: None })
					else {
						unreachable!("caller request retains its response sender");
					};
					let result = match self.operation.try_start_caller_work() {
						Ok(permit) => {
							state.phase = DriverPhase::CallerQueued { held, permit };
							Ok(CallerDelivery {
								shared: Arc::clone(&self.shared),
								acknowledgement: DeliveryAcknowledgement::Awaiting,
							})
						}
						Err(error) => {
							state.phase = DriverPhase::Closing { held: Some(held) };
							DriverShared::close_locked(&mut state);
							Err(Arc::new(error))
						}
					};
					response = Some((sender, result));
				}
				let caller_pending = runtime
					.work
					.as_ref()
					.is_some_and(|entry| entry.pending_caller_work_count != 0)
					|| matches!(state.phase, DriverPhase::CallerQueued { .. });
				let until = state.phase.held().map_or(runtime.confirmed_until, |held| {
					held.hold_until.min(runtime.confirmed_until)
				});
				let close = state.closure.is_closed() && !caller_pending;
				let delivered = state.phase.stage() != DriverStage::Queued;
				(runtime, until, caller_pending, close, delivered)
			};
			if let Some((sender, result)) = response {
				if let Err(error) = &result {
					self.operation.retain_driver_cause(Arc::clone(error));
				}
				let _ = sender.send(result);
			}
			if close {
				return delivered;
			}
			if caller_pending && Instant::now() >= until {
				tokio::select! {_=changed=>{}, _=self.shared.runtime.wait_for_work_change(self.shared.work, &runtime)=>{}}
			} else {
				tokio::select! {_=changed=>{}, _=self.shared.runtime.wait_for_work_change(self.shared.work, &runtime)=>{}, _=tokio::time::sleep_until(until)=>{}}
			}
		}
	}
	async fn finish_accounting(&mut self) {
		let timing = self.operation.driver_timing();
		let mut failures =
			usize::from(self.operation.attempt_was_ordered() && !self.operation.has_completion());
		loop {
			let budget = if self.operation.has_pending_release() {
				timing.max_consecutive_release_failures
			} else {
				timing.max_consecutive_cleanup_failures
			};
			if failures >= budget.get() && !self.operation.has_completion() {
				self.finish_unresolved(timing.physical_proof_retry_interval)
					.await;
				return;
			}
			let result = if self.operation.has_completion() {
				self.operation
					.settle_known_completion()
					.await
					.map_err(Arc::new)
					.and_then(|result| match result {
						NativeSemaphoreCompletionObservation::DeniedAcquisition => self
							.operation
							.finish_denial()
							.map(|()| NativeSemaphoreCleanupObservation::NotAcquired)
							.map_err(Arc::new),
						NativeSemaphoreCompletionObservation::Released(result) => {
							Ok(NativeSemaphoreCleanupObservation::Released(result))
						}
					})
			} else if self.operation.has_pending_release() {
				self.operation
					.reconcile_release()
					.await
					.map(NativeSemaphoreCleanupObservation::Released)
			} else {
				self.operation.cleanup_undelivered().await
			};
			match result {
				Ok(result) => {
					self.publish_complete(result);
					return;
				}
				Err(error) => {
					self.operation.retain_driver_cause(Arc::clone(&error));
					if matches!(
						error.as_ref(),
						NativeSemaphoreError::InvalidTiming | NativeSemaphoreError::OperationState
					) {
						self.finish_unresolved(timing.physical_proof_retry_interval)
							.await;
						return;
					}
					if self.operation.attempt_was_ordered() && !self.operation.has_completion() {
						failures = failures.saturating_add(1);
					}
					let budget = if self.operation.has_pending_release() {
						timing.max_consecutive_release_failures
					} else {
						timing.max_consecutive_cleanup_failures
					};
					if failures >= budget.get() {
						self.finish_unresolved(timing.physical_proof_retry_interval)
							.await;
						return;
					}
				}
			}
			let interval =
				if !self.operation.attempt_was_ordered() || self.operation.has_completion() {
					timing.physical_proof_retry_interval
				} else if self.operation.has_pending_release() {
					timing.release_retry_interval
				} else {
					timing.cleanup_retry_interval
				};
			if let Err(error) = sleep_retry(interval).await {
				self.operation.retain_driver_cause(Arc::new(error));
				self.finish_unresolved(timing.physical_proof_retry_interval)
					.await;
				return;
			}
		}
	}
	async fn finish_unresolved(&mut self, retry: Duration) {
		loop {
			let _ = self.operation.prove_for_unresolved().await;
			if let Ok(diagnostic) = self.operation.finish_unresolved() {
				let mut state = self.shared.lock();
				let previous =
					std::mem::replace(&mut state.phase, DriverPhase::Closing { held: None });
				state.phase = DriverPhase::Finished {
					held: previous.into_held(),
					accounting: TerminalAccounting::Unresolved(diagnostic),
				};
				self.shared.changed.notify_waiters();
				return;
			}
			if let Err(error) = sleep_retry(retry).await {
				self.operation.retain_driver_cause(Arc::new(error));
				return;
			}
		}
	}
	fn publish_complete(&self, result: NativeSemaphoreCleanupObservation) {
		let mut state = self.shared.lock();
		let previous = std::mem::replace(&mut state.phase, DriverPhase::Closing { held: None });
		state.phase = DriverPhase::Finished {
			held: previous.into_held(),
			accounting: TerminalAccounting::Complete {
				result,
				_diagnostic: self.operation.diagnostic(),
			},
		};
		self.shared.changed.notify_waiters();
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
async fn sleep_retry(wait: Duration) -> Result<(), NativeSemaphoreError> {
	tokio::time::sleep_until(deadline(wait)?).await;
	Ok(())
}
