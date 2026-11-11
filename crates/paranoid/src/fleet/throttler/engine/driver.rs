//! One same-registry task owns delivery, exact outcome accounting and physical reconciliation.

use super::delivery::{
	CallerDelivery, DeliveryAcknowledgement, InitialDelivery, InitialDeliveryState,
	NativeThrottlerClient, NativeThrottlerPendingDelivery, NativeThrottlerStartResult,
};
use super::options::NativeThrottlerProbeHeartbeatPolicy;
use super::{
	Arc, Duration, HolderId, Instant, NativeThrottlerAcquisitionObservation,
	NativeThrottlerCancellationReason, NativeThrottlerError, NativeThrottlerOperation,
	NativeThrottlerStartOptions, PgNativeThrottler, ThrottlerTaskOutcome, deadline,
};
use crate::fleet::throttler::state::outcome::ThrottlerUndeliveredTarget;
use crate::runtime::local_admission::membership::RuntimeResourceWorkState;
use crate::runtime::local_admission::throttler_work::{
	RuntimeThrottlerCallerWorkPermit, RuntimeThrottlerWorkId, RuntimeThrottlerWorkSnapshot,
};
use crate::runtime::local_admission::{
	RuntimeAdmissionStop, RuntimeAdmissionTracker, RuntimeWorkView,
};
use std::sync::Mutex;
use tokio::sync::{Notify, oneshot};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeThrottlerLocalLoss {
	RuntimeStopped,
	ReservationWindowElapsed,
	ClientClosed,
	ProbeExtensionFailed,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeThrottlerLocalObservation {
	Pending,
	ConfirmedLocally,
	Lost(NativeThrottlerLocalLoss),
	Closing,
	Finished,
}
#[derive(Clone)]
pub(crate) enum NativeThrottlerAccountingObservation {
	Pending,
	Complete,
	Unresolved,
	Abandoned,
}
mod state;
use state::{
	CallerResponse, ClientAvailability, ClosingPurpose, DriverPhase, DriverStage, ProbeHeartbeat,
	RescheduleProbe, ReservationBounds, TerminalAccounting,
};

struct DriverState {
	phase: DriverPhase,
	availability: ClientAvailability,
	loss: std::sync::OnceLock<NativeThrottlerLocalLoss>,
}
impl DriverState {
	fn reservation_until(&self) -> Option<Instant> {
		self.phase.bounds().and_then(ReservationBounds::until)
	}
	fn take_phase(&mut self) -> DriverPhase {
		std::mem::replace(
			&mut self.phase,
			DriverPhase::Closing {
				bounds: None,
				purpose: ClosingPurpose::Undelivered,
			},
		)
	}
}
impl DriverPhase {
	fn accounting(&self) -> NativeThrottlerAccountingObservation {
		match self {
			Self::Finished {
				accounting: TerminalAccounting::Complete(_diagnostic),
				..
			} => NativeThrottlerAccountingObservation::Complete,
			Self::Finished {
				accounting: TerminalAccounting::Unresolved(_diagnostic),
				..
			} => NativeThrottlerAccountingObservation::Unresolved,
			_ => NativeThrottlerAccountingObservation::Pending,
		}
	}
}
pub(super) struct DriverShared {
	state: Mutex<DriverState>,
	changed: Notify,
	runtime: RuntimeAdmissionTracker,
	work: RuntimeThrottlerWorkId,
}
impl DriverShared {
	fn lock(&self) -> std::sync::MutexGuard<'_, DriverState> {
		self.state
			.lock()
			.expect("native Throttler driver state is not poisoned")
	}
	pub(super) fn close(&self) {
		let mut state = self.lock();
		Self::close_locked(&mut state);
		self.changed.notify_waiters();
	}
	fn close_locked(state: &mut DriverState) {
		state.availability = ClientAvailability::Closed;
		let phase = state.take_phase();
		state.phase = match phase {
			DriverPhase::CallerRequested { bounds, .. }
			| DriverPhase::CallerQueued { bounds, .. } => DriverPhase::Closing {
				bounds: Some(bounds),
				purpose: ClosingPurpose::Outcome(ThrottlerTaskOutcome::NotExecuted),
			},
			other => other,
		};
	}
	fn acquisition_cancellation(&self) -> Option<NativeThrottlerCancellationReason> {
		let runtime = self.runtime.status();
		let mut state = self.lock();
		if runtime.stop().is_some() {
			if state.phase.stage() != DriverStage::Running {
				state
					.loss
					.get_or_init(|| NativeThrottlerLocalLoss::RuntimeStopped);
			}
			Self::close_locked(&mut state);
			self.changed.notify_waiters();
			return Some(NativeThrottlerCancellationReason::RuntimeStopped);
		}
		state
			.availability
			.is_closed()
			.then_some(NativeThrottlerCancellationReason::ClientClosed)
	}
	async fn wait_acquisition_cancellation(&self) -> NativeThrottlerCancellationReason {
		loop {
			let changed = self.changed.notified();
			tokio::pin!(changed);
			changed.as_mut().enable();
			let runtime = self.runtime.status();
			if let Some(reason) = self.acquisition_cancellation() {
				return reason;
			}
			tokio::select! { _ = changed => {}, _ = self.runtime.wait_for_status_change(runtime) => {} }
		}
	}
	async fn wait_probe_cancellation(&self) {
		loop {
			let changed = self.changed.notified();
			tokio::pin!(changed);
			changed.as_mut().enable();
			let (runtime, until, cancel) = {
				let mut state = self.lock();
				let (local, current) = self.observe_locked(&mut state);
				let cancel = local != NativeThrottlerLocalObservation::ConfirmedLocally
					|| state.phase.stage() == DriverStage::Closing
					|| (state.availability.is_closed()
						&& state.phase.stage() != DriverStage::Running)
					|| (current.stop().is_some()
						&& !(current.stop() == Some(RuntimeAdmissionStop::Shutdown)
							&& state.phase.stage() == DriverStage::Running));
				let until = state
					.reservation_until()
					.map_or(current.confirmed_until, |raw| {
						raw.min(current.confirmed_until)
					});
				(current, until, cancel)
			};
			if cancel {
				return;
			}
			tokio::select! { _=changed=>{}, _=self.runtime.wait_for_work_change(self.work, &runtime)=>{}, _=tokio::time::sleep_until(until)=>{} }
		}
	}
	fn observe_locked(
		&self,
		state: &mut DriverState,
	) -> (
		NativeThrottlerLocalObservation,
		RuntimeWorkView<RuntimeThrottlerWorkSnapshot>,
	) {
		let runtime = self.runtime.work_view(self.work);
		if state.loss.get().is_none() {
			if runtime.lifetime_finished() || runtime.stop() == Some(RuntimeAdmissionStop::Fenced) {
				state
					.loss
					.get_or_init(|| NativeThrottlerLocalLoss::RuntimeStopped);
			} else if state
				.reservation_until()
				.is_some_and(|until| Instant::now() >= until)
			{
				state
					.loss
					.get_or_init(|| NativeThrottlerLocalLoss::ReservationWindowElapsed);
			} else if state.availability.is_closed() && state.phase.stage() != DriverStage::Running
			{
				state
					.loss
					.get_or_init(|| NativeThrottlerLocalLoss::ClientClosed);
			}
		}
		let local = if let Some(loss) = state.loss.get() {
			NativeThrottlerLocalObservation::Lost(*loss)
		} else {
			match state.phase.stage() {
				DriverStage::Acquiring => NativeThrottlerLocalObservation::Pending,
				DriverStage::Closing => NativeThrottlerLocalObservation::Closing,
				DriverStage::Finished => NativeThrottlerLocalObservation::Finished,
				_ => NativeThrottlerLocalObservation::ConfirmedLocally,
			}
		};
		(local, runtime)
	}
	fn require_delivery(
		&self,
		state: &mut DriverState,
		expected: DriverStage,
	) -> Result<(), Arc<NativeThrottlerError>> {
		let (local, runtime) = self.observe_locked(state);
		let held = runtime.work.as_ref();
		if state.phase.stage() != expected
			|| state.availability.is_closed()
			|| local != NativeThrottlerLocalObservation::ConfirmedLocally
			|| runtime.stop().is_some()
			|| runtime.lifetime_finished()
			|| !held.is_some_and(|entry| {
				entry.state == RuntimeResourceWorkState::Held
					&& (expected == DriverStage::DeliveredIdle || !entry.continuation.is_retained())
			}) {
			return Err(Arc::new(NativeThrottlerError::DeliveryRejected));
		}
		if expected == DriverStage::CallerQueued
			&& !held.is_some_and(|entry| entry.pending_caller_work_count == 1)
		{
			return Err(Arc::new(NativeThrottlerError::DeliveryRejected));
		}
		Ok(())
	}
	pub(super) fn accept_initial(&self) -> Result<(), Arc<NativeThrottlerError>> {
		let mut state = self.lock();
		self.require_delivery(&mut state, DriverStage::Queued)?;
		let DriverPhase::Queued { bounds, .. } = state.take_phase() else {
			unreachable!("initial acceptance consumes queued delivery");
		};
		state.phase = DriverPhase::DeliveredIdle(bounds);
		self.changed.notify_waiters();
		Ok(())
	}
	pub(super) fn request_caller(
		&self,
		sender: CallerResponse,
	) -> Result<(), Arc<NativeThrottlerError>> {
		let mut state = self.lock();
		self.require_delivery(&mut state, DriverStage::DeliveredIdle)?;
		let DriverPhase::DeliveredIdle(bounds) = state.take_phase() else {
			unreachable!("caller request consumes idle delivery");
		};
		state.phase = DriverPhase::CallerRequested {
			bounds,
			response: sender,
		};
		self.changed.notify_waiters();
		Ok(())
	}
	pub(super) fn accept_caller(
		&self,
	) -> Result<RuntimeThrottlerCallerWorkPermit, Arc<NativeThrottlerError>> {
		let mut state = self.lock();
		self.require_delivery(&mut state, DriverStage::CallerQueued)?;
		let DriverPhase::CallerQueued { bounds, permit } = state.take_phase() else {
			unreachable!("caller acceptance consumes its exact queued permit");
		};
		state.phase = DriverPhase::Running(bounds);
		Ok(permit)
	}
	pub(super) fn finish_caller(&self, outcome: ThrottlerTaskOutcome) {
		let mut state = self.lock();
		if matches!(
			state.phase.stage(),
			DriverStage::CallerQueued | DriverStage::Running
		) {
			let previous = state.take_phase();
			state.availability = ClientAvailability::Closed;
			state.phase = DriverPhase::Closing {
				bounds: previous.into_bounds(),
				purpose: ClosingPurpose::Outcome(outcome),
			};
		}
		self.changed.notify_waiters();
	}
}
/// Read-only clones cannot create caller scopes, retain a grant or delay client closure.
#[derive(Clone)]
pub(crate) struct NativeThrottlerObserver(pub(super) Arc<DriverShared>);
impl NativeThrottlerObserver {
	pub(crate) fn snapshot(&self) -> NativeThrottlerLocalObservation {
		self.0.observe_locked(&mut self.0.lock()).0
	}
	pub(crate) fn accounting(&self) -> NativeThrottlerAccountingObservation {
		let state = self.0.lock();
		if matches!(
			state.phase.accounting(),
			NativeThrottlerAccountingObservation::Pending
		) && self
			.0
			.runtime
			.work_view(self.0.work)
			.work
			.is_some_and(|entry| entry.state == RuntimeResourceWorkState::Abandoned)
		{
			return NativeThrottlerAccountingObservation::Abandoned;
		}
		state.phase.accounting()
	}
	pub(crate) async fn wait_accounting(&self) -> NativeThrottlerAccountingObservation {
		loop {
			let changed = self.0.changed.notified();
			tokio::pin!(changed);
			changed.as_mut().enable();
			let runtime = self.0.runtime.work_view(self.0.work);
			let accounting = self.accounting();
			if !matches!(accounting, NativeThrottlerAccountingObservation::Pending) {
				return accounting;
			}
			tokio::select! { _ = changed => {}, _ = self.0.runtime.wait_for_work_change(self.0.work, &runtime) => {} }
		}
	}
}

impl PgNativeThrottler {
	/// Registration and task reservation precede the first await. A committed acquisition is
	/// delivered whenever its caller still waits for it.
	pub(crate) fn start_owned(
		&self,
		options: NativeThrottlerStartOptions,
	) -> Result<NativeThrottlerPendingDelivery, NativeThrottlerError> {
		if options.settings != self.settings {
			return Err(NativeThrottlerError::InvalidProbeHeartbeatPolicy);
		}
		let owner = self
			.bindings
			.admission
			.register_throttler_work(self.identity.clone())?;
		let shared = Arc::new(DriverShared {
			state: Mutex::new(DriverState {
				phase: DriverPhase::Acquiring,
				availability: ClientAvailability::Open,
				loss: std::sync::OnceLock::new(),
			}),
			changed: Notify::new(),
			runtime: owner.admission_tracker(),
			work: owner.id(),
		});
		let (sender, receiver) = oneshot::channel();
		let pending = NativeThrottlerPendingDelivery {
			receiver,
			shared: Arc::clone(&shared),
			acknowledgement: DeliveryAcknowledgement::Awaiting,
		};
		let handle = self.clone();
		owner.spawn_owned_task(
			&self.bindings.originating_runtime,
			move |owner| async move {
				let mut driver = Driver {
					operation: NativeThrottlerOperation::new(handle, owner),
					shared,
					heartbeat: ProbeHeartbeat::AwaitingReservation(options.probe_heartbeat),
				};
				driver
					.run(options.holder, options.rate_limit_cost, sender)
					.await;
			},
		)?;
		Ok(pending)
	}
}
struct Driver {
	operation: NativeThrottlerOperation,
	shared: Arc<DriverShared>,
	heartbeat: ProbeHeartbeat,
}
impl Driver {
	async fn run(
		&mut self,
		holder: Option<HolderId>,
		rate_limit_cost: std::num::NonZeroU32,
		response: oneshot::Sender<InitialDelivery>,
	) {
		let initial = if let Some(reason) = self.shared.acquisition_cancellation() {
			Err(Arc::new(NativeThrottlerError::AcquisitionCancelled {
				reason,
			}))
		} else {
			let acquisition = self.operation.acquire(holder, rate_limit_cost);
			tokio::pin!(acquisition);
			tokio::select! {
				biased;
				result = &mut acquisition => result,
				reason = self.shared.wait_acquisition_cancellation() => Err(Arc::new(NativeThrottlerError::AcquisitionCancelled { reason })),
			}
		};
		if let Err(error) = &initial {
			self.operation.retain_driver_cause(Arc::clone(error));
		}
		// Cancellation drops only this registered waiter. Its actual target/receipt and
		// any dispatched owned BEGIN/ending remain pending for exact cleanup below.

		let value = match initial {
			Ok(NativeThrottlerAcquisitionObservation::Acquired) => {
				match self.reservation_bounds() {
					Ok(bounds) => {
						let target = self
							.operation
							.take_delivery_target()
							.expect("confirmed acquisition retains its delivery target");
						let mut state = self.shared.lock();
						state.phase = DriverPhase::Queued { bounds, target };
						Ok(NativeThrottlerStartResult::Acquired(
							NativeThrottlerClient::new(Arc::clone(&self.shared)),
						))
					}
					Err(error) => {
						let error = Arc::new(error);
						self.operation.retain_driver_cause(Arc::clone(&error));
						Err(error)
					}
				}
			}
			Ok(NativeThrottlerAcquisitionObservation::Denied(denial)) => {
				Ok(NativeThrottlerStartResult::Denied(denial))
			}
			Err(error) => Err(error),
		};
		let acquired = matches!(&value, Ok(NativeThrottlerStartResult::Acquired(_)));
		let _ = response.send(InitialDelivery {
			state: InitialDeliveryState::Queued(value),
			shared: Arc::clone(&self.shared),
		});
		if acquired {
			let bounds = *self
				.shared
				.lock()
				.phase
				.bounds()
				.expect("queued acquisition retains reservation bounds");
			let requested = self
				.operation
				.acquisition_started()
				.expect("confirmed acquisition retains its start");
			if let Err(error) = self.heartbeat.schedule(requested, bounds) {
				self.lose_probe(error);
			}
			self.wait_task().await;
		}
		let purpose = {
			let mut state = self.shared.lock();
			let (bounds, purpose, target) = state.take_phase().into_closing();
			if let Some(target) = target {
				self.operation.restore_delivery_target(Some(target));
			}
			state.phase = DriverPhase::Closing { bounds, purpose };
			purpose
		};
		if let ClosingPurpose::Outcome(outcome) = purpose {
			let _ = self.operation.consume_outcome(outcome).await;
		}
		self.finish_accounting().await;
	}
	/// The local ends of the acquisition's slot and probe reservations, measured from when the
	/// acquisition took its record's turn, which precedes the database's own start of them.
	fn reservation_bounds(&self) -> Result<ReservationBounds, NativeThrottlerError> {
		use crate::fleet::throttler::state::NativeThrottlerReservationKind as Kind;
		let kind = self
			.operation
			.reservation_kind()
			.ok_or(NativeThrottlerError::OperationState)?;
		let requested = self
			.operation
			.acquisition_started()
			.ok_or(NativeThrottlerError::OperationState)?;
		let settings = self.operation.handle_settings();
		let until = |duration: Duration| {
			requested
				.checked_add(duration)
				.ok_or(NativeThrottlerError::InvalidTiming)
		};
		let slot = || {
			until(
				settings
					.concurrency()
					.expect("slot acquisition retains concurrency settings")
					.max_hold_duration(),
			)
		};
		let probe = || {
			until(
				settings
					.circuit()
					.expect("probe acquisition retains circuit settings")
					.probe_window(),
			)
		};
		Ok(match kind {
			Kind::Unreserved => ReservationBounds::Unreserved,
			Kind::Slot => ReservationBounds::Slot(slot()?),
			Kind::Probe => ReservationBounds::Probe(probe()?),
			Kind::SlotAndProbe => ReservationBounds::SlotAndProbe {
				slot: slot()?,
				probe: probe()?,
			},
		})
	}
	fn lose_probe(&mut self, error: NativeThrottlerError) {
		self.operation.retain_probe_driver_error(error);
		let state = self.shared.lock();
		state
			.loss
			.get_or_init(|| NativeThrottlerLocalLoss::ProbeExtensionFailed);
		self.heartbeat = ProbeHeartbeat::Lost;
		self.shared.changed.notify_waiters();
	}
	async fn wait_task(&mut self) {
		loop {
			let shared = Arc::clone(&self.shared);
			let changed = shared.changed.notified();
			tokio::pin!(changed);
			changed.as_mut().enable();
			let runtime = self.shared.runtime.work_view(self.shared.work);
			if runtime.stop().is_some() {
				let mut state = self.shared.lock();
				let closing_changed = !state.availability.is_closed();
				if state.phase.stage() != DriverStage::Running {
					state
						.loss
						.get_or_init(|| NativeThrottlerLocalLoss::RuntimeStopped);
				}
				DriverShared::close_locked(&mut state);
				if closing_changed {
					self.shared.changed.notify_waiters();
				}
			}
			let mut send = None;
			let should_close = {
				let mut state = self.shared.lock();
				let (local, current) = self.shared.observe_locked(&mut state);
				if matches!(local, NativeThrottlerLocalObservation::Lost(_)) {
					DriverShared::close_locked(&mut state);
				}
				if state.phase.stage() == DriverStage::CallerRequested
					&& !state.availability.is_closed()
				{
					let DriverPhase::CallerRequested {
						bounds,
						response: sender,
					} = state.take_phase()
					else {
						unreachable!("caller request retains its exact response sender");
					};
					let result = if current.stop().is_none()
						&& local == NativeThrottlerLocalObservation::ConfirmedLocally
					{
						self.operation
							.owner()
							.try_start_caller_work()
							.map_err(|error| Arc::new(NativeThrottlerError::Local(error)))
					} else {
						Err(Arc::new(NativeThrottlerError::DeliveryRejected))
					};
					let result = match result {
						Ok(permit) => {
							state.phase = DriverPhase::CallerQueued { bounds, permit };
							Ok(CallerDelivery {
								shared: Arc::clone(&self.shared),
								acknowledgement: DeliveryAcknowledgement::Awaiting,
							})
						}
						Err(error) => {
							state.phase = DriverPhase::Closing {
								bounds: Some(bounds),
								purpose: ClosingPurpose::Outcome(ThrottlerTaskOutcome::NotExecuted),
							};
							state.availability = ClientAvailability::Closed;
							Err(error)
						}
					};
					send = Some((sender, result));
				}
				let caller_pending = current
					.work
					.as_ref()
					.is_some_and(|entry| entry.pending_caller_work_count != 0);
				(state.availability.is_closed()
					|| current.stop().is_some()
					|| matches!(local, NativeThrottlerLocalObservation::Lost(_)))
					&& !caller_pending
			};
			if let Some((sender, result)) = send {
				let _ = sender.send(result);
			}
			if should_close {
				return;
			}
			let reservation_until = self.shared.lock().reservation_until();
			let until = reservation_until.map_or(runtime.confirmed_until, |until| {
				until.min(runtime.confirmed_until)
			});
			if self.probe_due() {
				self.heartbeat_once().await;
				continue;
			}
			let until = self
				.eligible_probe_start()
				.map_or(until, |next| until.min(next));
			// Existing caller work has no library abort deadline. Its exact count/change ends this wait.
			let caller_pending = runtime
				.work
				.as_ref()
				.is_some_and(|entry| entry.pending_caller_work_count != 0);
			let wake_for_deadline = !caller_pending
				|| self.shared.lock().phase.stage() == DriverStage::CallerQueued
				|| self.eligible_probe_start().is_some();
			tokio::select! { _ = changed => {}, _ = self.shared.runtime.wait_for_work_change(self.shared.work, &runtime) => {}, _ = tokio::time::sleep_until(until), if wake_for_deadline => {} }
		}
	}
	fn eligible_probe_start(&self) -> Option<Instant> {
		let mut state = self.shared.lock();
		let (local, current) = self.shared.observe_locked(&mut state);
		if local != NativeThrottlerLocalObservation::ConfirmedLocally
			|| !matches!(
				state.phase.stage(),
				DriverStage::DeliveredIdle | DriverStage::Running
			) || (state.availability.is_closed() && state.phase.stage() != DriverStage::Running)
		{
			return None;
		}
		self.heartbeat.next().filter(|_| {
			current.stop().is_none()
				|| (current.stop() == Some(RuntimeAdmissionStop::Shutdown)
					&& state.phase.stage() == DriverStage::Running)
		})
	}
	fn probe_due(&self) -> bool {
		self.eligible_probe_start()
			.is_some_and(|next| Instant::now() >= next)
	}
	async fn heartbeat_once(&mut self) {
		let Some((policy, failures)) = self.heartbeat.scheduled() else {
			return;
		};
		let requested = Instant::now();
		let until = {
			let mut state = self.shared.lock();
			let (local, _) = self.shared.observe_locked(&mut state);
			if local != NativeThrottlerLocalObservation::ConfirmedLocally {
				return;
			}
			let Some(until) = state.reservation_until() else {
				return;
			};
			until
		};
		let result = {
			let extension = self.operation.extend_probe_before(until);
			tokio::pin!(extension);
			tokio::select! { biased;
				result = &mut extension => result,
				_ = self.shared.wait_probe_cancellation() => Err(Arc::new(NativeThrottlerError::ProbeExtensionLost)),
			}
		};
		match result {
			Ok(true) => {
				let duration = self
					.operation
					.handle_settings()
					.circuit()
					.expect("probe policy requires its circuit")
					.probe_window();
				let Some(probe_until) = requested.checked_add(duration) else {
					self.lose_probe(NativeThrottlerError::InvalidTiming);
					return;
				};
				let live = {
					let mut state = self.shared.lock();
					let (local, _) = self.shared.observe_locked(&mut state);
					if local == NativeThrottlerLocalObservation::ConfirmedLocally {
						state
							.phase
							.bounds_mut()
							.expect("confirmed probe retains reservation bounds")
							.extend_probe(probe_until);
						true
					} else {
						false
					}
				};
				if live {
					if let Err(error) = self.heartbeat.reschedule(RescheduleProbe {
						requested_at: requested,
						failures: 0,
					}) {
						self.lose_probe(error);
					}
				} else {
					self.heartbeat = ProbeHeartbeat::Lost;
				}
				self.shared.changed.notify_waiters();
			}
			Ok(false) => self.lose_probe(NativeThrottlerError::ProbeExtensionLost),
			Err(error) => {
				self.operation.retain_probe_driver_cause(Arc::clone(&error));
				let failures = failures.saturating_add(1);
				let live = self.shared.observe_locked(&mut self.shared.lock()).0
					== NativeThrottlerLocalObservation::ConfirmedLocally;
				if live
					&& failures < policy.max_consecutive_failures.get()
					&& self.operation.probe_retry_is_known_rollback(&error)
				{
					if let Err(error) = self.heartbeat.reschedule(RescheduleProbe {
						requested_at: Instant::now(),
						failures,
					}) {
						self.lose_probe(error);
					}
				} else {
					self.shared
						.lock()
						.loss
						.get_or_init(|| NativeThrottlerLocalLoss::ProbeExtensionFailed);
					self.heartbeat = ProbeHeartbeat::Lost;
					self.shared.changed.notify_waiters();
				}
			}
		}
	}
	async fn finish_accounting(&mut self) {
		let timing = &self.operation.bindings().driver_timing;
		let outcome_retry = timing.outcome_retry_interval;
		let cleanup_retry = timing.cleanup_retry_interval;
		let proof_retry = timing.physical_proof_retry_interval;
		let budget = if self.operation.has_pending_outcome() {
			timing.max_consecutive_outcome_failures.get()
		} else {
			timing.max_consecutive_cleanup_failures.get()
		};
		let mut failed = 0usize;
		loop {
			if self.operation.is_complete() {
				self.publish_complete();
				return;
			}
			let result = if self.operation.has_completion() {
				self.operation.settle_known_completion().await.map(|_| ())
			} else if self.operation.has_pending_outcome() {
				self.operation.reconcile_outcome().await.map(|_| ())
			} else {
				self.operation.cleanup_undelivered().await
			};
			match result {
				Ok(()) => {
					self.publish_complete();
					return;
				}
				Err(error) => {
					if matches!(
						error.as_ref(),
						NativeThrottlerError::InvalidTiming | NativeThrottlerError::OperationState
					) {
						self.finish_unresolved(proof_retry).await;
						return;
					}
					if self.operation.origin_was_ordered() && !self.operation.has_completion() {
						failed += 1;
					}
					if failed >= budget {
						self.finish_unresolved(proof_retry).await;
						return;
					}
				}
			}
			let interval = if self.operation.origin_was_ordered() {
				if self.operation.has_pending_outcome() {
					outcome_retry
				} else {
					cleanup_retry
				}
			} else {
				proof_retry
			};
			if let Err(error) = self.sleep_retry(interval).await {
				self.operation.retain_driver_error(error);
				self.finish_unresolved(proof_retry).await;
				return;
			}
		}
	}
	async fn sleep_retry(&self, interval: Duration) -> Result<(), NativeThrottlerError> {
		tokio::time::sleep_until(deadline(interval)?).await;
		Ok(())
	}
	async fn finish_unresolved(&mut self, retry: Duration) {
		loop {
			let _ = self.operation.prove_pending_origins().await;
			// The fixed operation checks its sealed receipt-backed settlement, including a
			// full physical fetch with a retained parse error and known rollback.
			let diagnostic = self.operation.diagnostic();
			if self
				.operation
				.finish_unresolved(
					Arc::clone(&diagnostic) as Arc<dyn std::error::Error + Send + Sync>
				)
				.is_ok()
			{
				let mut state = self.shared.lock();
				let previous = state.take_phase();
				state.phase = DriverPhase::Finished {
					bounds: previous.into_bounds(),
					accounting: TerminalAccounting::Unresolved(diagnostic),
				};
				self.shared.changed.notify_waiters();
				return;
			}
			if let Err(error) = self.sleep_retry(retry).await {
				self.operation.retain_driver_error(error);
				// Unproved ownership is abandoned and remains visible in the same registry.
				return;
			}
		}
	}
	fn publish_complete(&self) {
		let diagnostic = self.operation.diagnostic();
		let mut state = self.shared.lock();
		let previous = state.take_phase();
		state.phase = DriverPhase::Finished {
			bounds: previous.into_bounds(),
			accounting: TerminalAccounting::Complete(diagnostic),
		};
		self.shared.changed.notify_waiters();
	}
}
