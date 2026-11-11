//! Managed native participation renewal and shared local lifetime shutdown continuation.
//!
//! This internal dependency owns participation transactions on separate control capacity.
//! It drains registered local work and transaction permits before removing participation.
//! Lease executors must retain that work through actual ending/cleanup proof; this dependency
//! neither executes lease cleanup nor proves physical server-side transaction drainage.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use primitives_rs::time::Deadline;
use tokio::runtime::Handle;
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until, timeout_at};

use crate::runtime::local_admission::{
	RuntimeAdmissionError, RuntimeAdmissionFenceReason, RuntimeAdmissionSnapshot,
	RuntimeAdmissionStop, RuntimeAdmissionTracker,
};
use crate::runtime::participation::state::{
	ParticipantComponentSupport, ParticipantStateError, ParticipationDuration,
};
use crate::scope::payload::PayloadRoleSupport;
use crate::scope::{DocumentRoleSupport, PrimitiveScopeSupport, RawRoleSupport};

use crate::runtime::participation::state::catalog::ParticipantUseReference;
use transactions::{
	CommittedParticipation, NativeParticipationError, NativeRegistrationOptions,
	PgNativeParticipation,
};

pub(crate) mod state;
pub(super) mod transactions;

/// Required lifecycle inputs. No interval, duration, or deadline is inferred.
/// Explicit participant lifetime, renewal cadence, and control-operation timeout. A renewal
/// that fails without ending the participation is retried after the renewal interval until the
/// confirmed lifetime elapses.
pub(crate) struct ParticipationTimingFields {
	/// The exact positive signed-microsecond participation lifetime.
	pub participation_duration: Duration,
	/// The positive renewal cadence, shorter than the participation lifetime.
	pub renewal_interval: Duration,
	/// The positive wait timeout for each participation control operation.
	pub control_operation_timeout: Duration,
}

/// One parsed duration drives both database participation and local conservative bounds.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ParticipationTiming {
	duration: Duration,
	database_duration: ParticipationDuration,
	renewal_interval: Duration,
	control_operation_timeout: Duration,
}

/// A participation timing input outside its duration, cadence or local-clock domain.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ParticipationTimingError {
	/// The lifetime is not positive exact signed microseconds.
	#[error("native participation duration must be positive signed microseconds")]
	ParticipationDuration,
	/// The renewal interval is zero or is not shorter than the lifetime.
	#[error("native participation renewal interval must be positive and less than its duration")]
	RenewalInterval,
	/// The control-operation timeout is zero.
	#[error("native participation control operation deadline must be positive")]
	ControlOperationTimeoutIsZero,
	/// The parsed lifetime or operation deadline exceeds the local clock domain.
	#[error("native participation timing exceeds the local monotonic clock domain")]
	LocalClockDomain,
}

impl ParticipationTiming {
	/// Parses explicit participation timing without selecting any settings or lifetime defaults.
	pub fn new(fields: ParticipationTimingFields) -> Result<Self, ParticipationTimingError> {
		let micros = i64::try_from(fields.participation_duration.as_micros())
			.map_err(|_| ParticipationTimingError::ParticipationDuration)?;
		if !fields
			.participation_duration
			.subsec_nanos()
			.is_multiple_of(1000)
		{
			return Err(ParticipationTimingError::ParticipationDuration);
		}
		let database_duration = ParticipationDuration::from_micros(micros)
			.map_err(|_| ParticipationTimingError::ParticipationDuration)?;
		if fields.renewal_interval.is_zero()
			|| fields.renewal_interval >= fields.participation_duration
		{
			return Err(ParticipationTimingError::RenewalInterval);
		}
		if fields.control_operation_timeout.is_zero() {
			return Err(ParticipationTimingError::ControlOperationTimeoutIsZero);
		}
		if Deadline::new_after(fields.participation_duration).is_err()
			|| Deadline::new_after(fields.control_operation_timeout).is_err()
		{
			return Err(ParticipationTimingError::LocalClockDomain);
		}
		Ok(Self {
			duration: fields.participation_duration,
			database_duration,
			renewal_interval: fields.renewal_interval,
			control_operation_timeout: fields.control_operation_timeout,
		})
	}
}

/// Typed terminal local fencing remains observable without exposing private authority.
#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeParticipationFenceError {
	#[error("native participation confirmed lifetime elapsed")]
	ConfirmedLifetimeElapsed,
	#[error("native participation renewal exceeded its operation deadline")]
	RenewalTimedOut,
	#[error("native participation renewal commit completion is unknown")]
	RenewalCommitUnknown(#[source] Arc<NativeParticipationError>),
	#[error("native participation private authority is unavailable")]
	AuthorityLost(#[source] Arc<NativeParticipationError>),
	#[error("native participation local admission was fenced")]
	AdmissionFenced,
	#[error("native participation runtime was dropped before shutdown initiation")]
	RuntimeAbandoned,
	#[error("native participation renewal task ended abnormally")]
	RenewalTaskAbandoned,
	#[error("native participation timing exceeds the local monotonic clock domain")]
	LocalClockDomain,
	#[error("native participation local lifetime confirmation was rejected")]
	LocalConfirmationRejected(#[source] RuntimeAdmissionError),
}

#[derive(Clone, Debug)]
pub(crate) enum NativeParticipationRenewalStatus {
	Running {
		last_failure: Option<Arc<NativeParticipationError>>,
	},
	Stopped,
	Fenced(Arc<NativeParticipationFenceError>),
}

#[derive(Clone, Debug)]
pub(crate) struct NativeParticipationRuntimeSnapshot {
	pub(crate) admission: RuntimeAdmissionSnapshot,
	pub(crate) renewal: NativeParticipationRenewalStatus,
	pub(crate) shutdown: NativeParticipationShutdownStatus,
}

/// The observable stage of a runtime's shutdown.
#[derive(Clone, Debug)]
pub(crate) enum NativeParticipationShutdownStatus {
	NotStarted,
	Running,
	Finished(Arc<NativeParticipationShutdownResult>),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeParticipationRenewalCompletionError {
	#[error("native participation renewal fenced local admission")]
	Fenced(#[source] Arc<NativeParticipationFenceError>),
	#[error("native participation renewal task did not complete normally")]
	TaskAbandoned,
	#[error("native participation shutdown continuation did not complete normally")]
	ShutdownContinuationAbandoned,
}

#[derive(Debug)]
pub(crate) enum NativeParticipationRemovalResult {
	Removed { was_present: bool },
	Failed(Arc<NativeParticipationError>),
	TimedOut,
	LocalLifetimeRejected(RuntimeAdmissionError),
	DrainageRejected(RuntimeAdmissionError),
	ContinuationAbandoned,
	LocalClockDomain,
}

/// Renewal and removal outcomes are retained independently; neither failure is swallowed.
#[derive(Debug)]
pub(crate) struct NativeParticipationShutdownResult {
	pub(crate) local_drainage: RuntimeAdmissionSnapshot,
	pub(crate) renewal: Result<(), NativeParticipationRenewalCompletionError>,
	pub(crate) removal: NativeParticipationRemovalResult,
}

struct RuntimeState {
	renewal: NativeParticipationRenewalStatus,
	shutdown: ShutdownState,
}

enum ShutdownState {
	StartingRenewal,
	AwaitingInitiation(RenewalTaskHandle),
	Running,
	Finished(Arc<NativeParticipationShutdownResult>),
}

impl ShutdownState {
	fn is_started(&self) -> bool {
		matches!(self, Self::Running | Self::Finished(_))
	}
	fn result(&self) -> Option<&Arc<NativeParticipationShutdownResult>> {
		match self {
			Self::Finished(result) => Some(result),
			_ => None,
		}
	}
	fn status(&self) -> NativeParticipationShutdownStatus {
		match self {
			Self::StartingRenewal | Self::AwaitingInitiation(_) => {
				NativeParticipationShutdownStatus::NotStarted
			}
			Self::Running => NativeParticipationShutdownStatus::Running,
			Self::Finished(result) => {
				NativeParticipationShutdownStatus::Finished(Arc::clone(result))
			}
		}
	}
}

#[derive(Clone, Copy)]
enum RenewalCommand {
	Continue,
	Stop,
}

enum TaskCompletionState {
	Pending,
	Completed,
}

struct SharedRuntime {
	state: Mutex<RuntimeState>,
	changed: Notify,
}

impl SharedRuntime {
	fn lock(&self) -> std::sync::MutexGuard<'_, RuntimeState> {
		self.state
			.lock()
			.expect("native participation state is not poisoned")
	}

	fn fence(
		&self,
		admission: &RuntimeAdmissionTracker,
		failure: NativeParticipationFenceError,
	) -> Arc<NativeParticipationFenceError> {
		admission.stop(RuntimeAdmissionStop::Fenced);
		let mut state = self.lock();
		let failure = match &state.renewal {
			NativeParticipationRenewalStatus::Fenced(existing) => Arc::clone(existing),
			_ => Arc::new(failure),
		};
		state.renewal = NativeParticipationRenewalStatus::Fenced(Arc::clone(&failure));
		self.changed.notify_waiters();
		failure
	}

	fn finish_shutdown(&self, outcome: NativeParticipationShutdownResult) {
		self.lock().shutdown = ShutdownState::Finished(Arc::new(outcome));
		self.changed.notify_waiters();
	}
}

/// The task that renews a runtime's participation until shutdown or a fence.
type RenewalTaskHandle = JoinHandle<Result<(), Arc<NativeParticipationFenceError>>>;

/// Owned renewal plus the same committed material and tracker used by ordinary admission.
/// Drop is nonblocking. Explicit shutdown initiation transfers cleanup to an owned continuation.
/// The originating Tokio runtime must remain alive to execute that continuation; a retained
/// Handle permits off-context spawning but does not keep the originating runtime alive.
pub(crate) struct PgNativeParticipationRuntime {
	originating_runtime: Handle,
	participation: Arc<PgNativeParticipation>,
	registration: Arc<CommittedParticipation>,
	admission: RuntimeAdmissionTracker,
	timing: ParticipationTiming,
	shared: Arc<SharedRuntime>,
	stop_renewal: watch::Sender<RenewalCommand>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeParticipationStartupError {
	#[error("native participation startup requires an available Tokio runtime")]
	RuntimeUnavailable(#[source] tokio::runtime::TryCurrentError),
	#[error("native participation registration failed")]
	Registration(#[source] NativeParticipationError),
	#[error("native participation registration timed out with uncertain completion")]
	RegistrationTimedOut,
	#[error("native participation registration exhausted its conservative lifetime before startup")]
	ConfirmedLifetimeElapsed {
		removal: NativeParticipationRemovalResult,
	},
	#[error("native participation startup timing exceeds the local clock domain")]
	LocalClockDomain,
}

/// Explicit initial declarations and timing for one managed participation startup.
pub(crate) struct NativeParticipationRuntimeStartOptions {
	pub(crate) timing: ParticipationTiming,
	pub(crate) component_support: Vec<ParticipantComponentSupport>,
	pub(crate) document_roles: Vec<DocumentRoleSupport>,
	pub(crate) raw_roles: Vec<RawRoleSupport>,
	pub(crate) primitive_scopes: Vec<PrimitiveScopeSupport>,
	pub(crate) payload_roles: Vec<PayloadRoleSupport>,
	pub(crate) optional_uses: Vec<ParticipantUseReference>,
}

impl PgNativeParticipationRuntime {
	/// Publishes admission material only after known registration commit and a live local bound.
	pub(crate) async fn start(
		participation: PgNativeParticipation,
		options: NativeParticipationRuntimeStartOptions,
	) -> Result<Self, NativeParticipationStartupError> {
		let NativeParticipationRuntimeStartOptions {
			timing,
			component_support,
			document_roles,
			raw_roles,
			primitive_scopes,
			payload_roles,
			optional_uses,
		} = options;
		let originating_runtime =
			Handle::try_current().map_err(NativeParticipationStartupError::RuntimeUnavailable)?;
		let request_start = Instant::now();
		let confirmed_until = request_start
			.checked_add(timing.duration)
			.ok_or(NativeParticipationStartupError::LocalClockDomain)?;
		let operation_until = request_start
			.checked_add(timing.control_operation_timeout)
			.ok_or(NativeParticipationStartupError::LocalClockDomain)?;
		let registered = timeout_at(
			operation_until,
			participation.register(NativeRegistrationOptions {
				component_support,
				document_roles,
				raw_roles,
				primitive_scopes,
				payload_roles,
				optional_uses,
				duration: timing.database_duration,
			}),
		)
		.await
		.map_err(|_| NativeParticipationStartupError::RegistrationTimedOut)?
		.map_err(NativeParticipationStartupError::Registration)?;
		if Instant::now() >= confirmed_until {
			let removal = remove_participation(&participation, &registered, timing).await;
			return Err(NativeParticipationStartupError::ConfirmedLifetimeElapsed { removal });
		}
		let participation = Arc::new(participation);
		let registration = Arc::new(registered);
		let admission = RuntimeAdmissionTracker::new(confirmed_until);
		let shared = Arc::new(SharedRuntime {
			state: Mutex::new(RuntimeState {
				renewal: NativeParticipationRenewalStatus::Running { last_failure: None },
				shutdown: ShutdownState::StartingRenewal,
			}),
			changed: Notify::new(),
		});
		let (stop_renewal, stop_receiver) = watch::channel(RenewalCommand::Continue);
		let renewal_task = originating_runtime.spawn(run_renewal(RenewalTask {
			completion: RenewalTaskCompletion {
				shared: Arc::clone(&shared),
				admission: admission.clone(),
				state: TaskCompletionState::Pending,
			},
			participation: Arc::clone(&participation),
			registration: Arc::clone(&registration),
			admission: admission.clone(),
			shared: Arc::clone(&shared),
			timing,
			confirmed_until,
			next_attempt: request_start + timing.renewal_interval,
			stop_receiver,
		}));
		shared.lock().shutdown = ShutdownState::AwaitingInitiation(renewal_task);
		Ok(Self {
			originating_runtime,
			participation,
			registration,
			admission,
			timing,
			shared,
			stop_renewal,
		})
	}

	pub(crate) fn registration(&self) -> Arc<CommittedParticipation> {
		Arc::clone(&self.registration)
	}
	/// Uses the same runtime for owned library work without keeping that runtime alive.
	pub(crate) fn originating_runtime(&self) -> Handle {
		self.originating_runtime.clone()
	}

	pub(crate) fn admission_tracker(&self) -> RuntimeAdmissionTracker {
		self.admission.clone()
	}

	/// Retains typed status even when the owning runtime is dropped.
	pub(crate) fn observer(&self) -> NativeParticipationRuntimeObserver {
		NativeParticipationRuntimeObserver {
			shared: Arc::clone(&self.shared),
			admission: self.admission.clone(),
		}
	}

	/// Stops new admission immediately and owns drainage through renewal join and removal.
	/// Repeated initiation returns a handle to the same continuation and terminal outcome.
	/// This call needs no ambient Tokio context. The originating runtime must still execute
	/// tasks; cancellation of the continuation records its typed abandoned outcome.
	pub(crate) fn initiate_shutdown(&self) -> NativeParticipationShutdown {
		self.admission.stop(RuntimeAdmissionStop::Shutdown);
		let renewal_task = {
			let mut state = self.shared.lock();
			match std::mem::replace(&mut state.shutdown, ShutdownState::Running) {
				ShutdownState::AwaitingInitiation(task) => Some(task),
				ShutdownState::StartingRenewal => {
					unreachable!("startup retains the renewal handle before exposing the runtime")
				}
				other => {
					state.shutdown = other;
					None
				}
			}
		};
		if let Some(renewal_task) = renewal_task {
			self.originating_runtime.spawn(run_shutdown(ShutdownTask {
				completion: ShutdownTaskCompletion {
					shared: Arc::clone(&self.shared),
					admission: self.admission.clone(),
					stop_renewal: self.stop_renewal.clone(),
					state: TaskCompletionState::Pending,
				},
				participation: Arc::clone(&self.participation),
				registration: Arc::clone(&self.registration),
				admission: self.admission.clone(),
				shared: Arc::clone(&self.shared),
				timing: self.timing,
				stop_renewal: self.stop_renewal.clone(),
				renewal_task,
			}));
		}
		NativeParticipationShutdown {
			shared: Arc::clone(&self.shared),
			admission: self.admission.clone(),
		}
	}
}

/// A retained read-only observation of local lifecycle and terminal fencing.
#[derive(Clone)]
pub(crate) struct NativeParticipationRuntimeObserver {
	shared: Arc<SharedRuntime>,
	admission: RuntimeAdmissionTracker,
}

impl NativeParticipationRuntimeObserver {
	pub(crate) fn snapshot(&self) -> NativeParticipationRuntimeSnapshot {
		snapshot(&self.shared, &self.admission)
	}

	/// Waits until renewal is no longer running and returns its final status. A fence that the
	/// admission records, such as an elapsed confirmed lifetime, becomes the renewal status when
	/// a snapshot reads it, so the wait also wakes on every change of the admission's status.
	pub(crate) async fn wait_until_renewal_ends(&self) -> NativeParticipationRenewalStatus {
		loop {
			let changed = self.shared.changed.notified();
			tokio::pin!(changed);
			changed.as_mut().enable();
			let admission = self.admission.status();
			let renewal = self.snapshot().renewal;
			if !matches!(renewal, NativeParticipationRenewalStatus::Running { .. }) {
				return renewal;
			}
			tokio::select! {
				() = changed => {}
				_ = self.admission.wait_for_status_change(admission) => {}
			}
		}
	}
}

impl Drop for PgNativeParticipationRuntime {
	fn drop(&mut self) {
		if !self.shared.lock().shutdown.is_started() {
			self.shared.fence(
				&self.admission,
				NativeParticipationFenceError::RuntimeAbandoned,
			);
			self.stop_renewal.send_replace(RenewalCommand::Stop);
		}
	}
}

/// A shared completion handle; wait cancellation or timeout leaves owned shutdown active.
#[derive(Clone)]
pub(crate) struct NativeParticipationShutdown {
	shared: Arc<SharedRuntime>,
	admission: RuntimeAdmissionTracker,
}

impl std::fmt::Debug for NativeParticipationShutdown {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str("NativeParticipationShutdown { .. }")
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeParticipationShutdownWaitError {
	#[error(
		"native participation shutdown wait timeout must be positive and within the local monotonic clock domain"
	)]
	InvalidWait,
	#[error("native participation shutdown remains pending")]
	Pending {
		snapshot: Box<NativeParticipationRuntimeSnapshot>,
	},
}

impl NativeParticipationShutdown {
	pub(crate) fn snapshot(&self) -> NativeParticipationRuntimeSnapshot {
		snapshot(&self.shared, &self.admission)
	}

	pub(crate) async fn wait(&self) -> Arc<NativeParticipationShutdownResult> {
		loop {
			let changed = self.shared.changed.notified();
			tokio::pin!(changed);
			changed.as_mut().enable();
			if let Some(outcome) = self.shared.lock().shutdown.result() {
				return Arc::clone(outcome);
			}
			changed.await;
		}
	}

	/// Bounds only this wait. The continuation still renews during registered local work drainage.
	pub(crate) async fn wait_until_finished(
		&self,
		timeout: Duration,
	) -> Result<Arc<NativeParticipationShutdownResult>, NativeParticipationShutdownWaitError> {
		if timeout.is_zero() {
			return Err(NativeParticipationShutdownWaitError::InvalidWait);
		}
		let wait_until = Instant::now()
			.checked_add(timeout)
			.ok_or(NativeParticipationShutdownWaitError::InvalidWait)?;
		timeout_at(wait_until, self.wait()).await.map_err(|_| {
			NativeParticipationShutdownWaitError::Pending {
				snapshot: Box::new(self.snapshot()),
			}
		})
	}
}

fn snapshot(
	shared: &SharedRuntime,
	admission: &RuntimeAdmissionTracker,
) -> NativeParticipationRuntimeSnapshot {
	let admission = admission.snapshot();
	let mut state = shared.lock();
	if admission.stop() == Some(RuntimeAdmissionStop::Fenced)
		&& !matches!(state.renewal, NativeParticipationRenewalStatus::Fenced(_))
	{
		let failure = match admission.fence_reason() {
			Some(RuntimeAdmissionFenceReason::ConfirmedLifetimeElapsed) => {
				NativeParticipationFenceError::ConfirmedLifetimeElapsed
			}
			_ => NativeParticipationFenceError::AdmissionFenced,
		};
		state.renewal = NativeParticipationRenewalStatus::Fenced(Arc::new(failure));
	}
	NativeParticipationRuntimeSnapshot {
		admission,
		renewal: state.renewal.clone(),
		shutdown: state.shutdown.status(),
	}
}

struct RenewalTask {
	completion: RenewalTaskCompletion,
	participation: Arc<PgNativeParticipation>,
	registration: Arc<CommittedParticipation>,
	admission: RuntimeAdmissionTracker,
	shared: Arc<SharedRuntime>,
	timing: ParticipationTiming,
	confirmed_until: Instant,
	next_attempt: Instant,
	stop_receiver: watch::Receiver<RenewalCommand>,
}

struct RenewalTaskCompletion {
	shared: Arc<SharedRuntime>,
	admission: RuntimeAdmissionTracker,
	state: TaskCompletionState,
}

impl Drop for RenewalTaskCompletion {
	fn drop(&mut self) {
		if matches!(self.state, TaskCompletionState::Pending) {
			self.shared.fence(
				&self.admission,
				NativeParticipationFenceError::RenewalTaskAbandoned,
			);
		}
	}
}

async fn run_renewal(mut task: RenewalTask) -> Result<(), Arc<NativeParticipationFenceError>> {
	let mut completion = task.completion;
	let result = loop {
		let admission = task.admission.status();
		if admission.stop() == Some(RuntimeAdmissionStop::Fenced) {
			let failure = match admission.fence_reason() {
				Some(RuntimeAdmissionFenceReason::ConfirmedLifetimeElapsed) => {
					NativeParticipationFenceError::ConfirmedLifetimeElapsed
				}
				_ => NativeParticipationFenceError::AdmissionFenced,
			};
			break Err(task.shared.fence(&task.admission, failure));
		}
		if matches!(*task.stop_receiver.borrow(), RenewalCommand::Stop) {
			break Ok(());
		}
		tokio::select! {
			biased;
			_ = task.stop_receiver.changed() => { continue; }
			_ = sleep_until(task.confirmed_until) => {
				break Err(task.shared.fence(&task.admission, NativeParticipationFenceError::ConfirmedLifetimeElapsed));
			}
			_ = sleep_until(task.next_attempt) => {}
		}
		let request_start = Instant::now();
		if request_start >= task.confirmed_until {
			break Err(task.shared.fence(
				&task.admission,
				NativeParticipationFenceError::ConfirmedLifetimeElapsed,
			));
		}
		let Some(operation_until) =
			request_start.checked_add(task.timing.control_operation_timeout)
		else {
			break Err(task.shared.fence(
				&task.admission,
				NativeParticipationFenceError::LocalClockDomain,
			));
		};
		let renewal = timeout_at(
			operation_until.min(task.confirmed_until),
			task.participation
				.renew(&task.registration, task.timing.database_duration),
		)
		.await;
		match renewal {
			Err(_) => {
				break Err(task.shared.fence(
					&task.admission,
					NativeParticipationFenceError::RenewalTimedOut,
				));
			}
			Ok(Err(error)) => {
				let error = Arc::new(error);
				let failure = if commit_outcome_is_unknown(&error) {
					Some(NativeParticipationFenceError::RenewalCommitUnknown(
						Arc::clone(&error),
					))
				} else if authority_lost(&error) {
					Some(NativeParticipationFenceError::AuthorityLost(Arc::clone(
						&error,
					)))
				} else if Instant::now() >= task.confirmed_until {
					Some(NativeParticipationFenceError::ConfirmedLifetimeElapsed)
				} else if Instant::now() >= operation_until {
					Some(NativeParticipationFenceError::RenewalTimedOut)
				} else {
					None
				};
				if let Some(failure) = failure {
					break Err(task.shared.fence(&task.admission, failure));
				}
				let mut state = task.shared.lock();
				if !matches!(state.renewal, NativeParticipationRenewalStatus::Fenced(_)) {
					state.renewal = NativeParticipationRenewalStatus::Running {
						last_failure: Some(error),
					};
				}
			}
			Ok(Ok(_)) => {
				if Instant::now() >= task.confirmed_until {
					break Err(task.shared.fence(
						&task.admission,
						NativeParticipationFenceError::ConfirmedLifetimeElapsed,
					));
				}
				if Instant::now() >= operation_until {
					break Err(task.shared.fence(
						&task.admission,
						NativeParticipationFenceError::RenewalTimedOut,
					));
				}
				let Some(confirmed_until) = request_start.checked_add(task.timing.duration) else {
					break Err(task.shared.fence(
						&task.admission,
						NativeParticipationFenceError::LocalClockDomain,
					));
				};
				if Instant::now() >= confirmed_until {
					break Err(task.shared.fence(
						&task.admission,
						NativeParticipationFenceError::ConfirmedLifetimeElapsed,
					));
				}
				if let Err(error) = task.admission.confirm_until(confirmed_until) {
					let failure = match task.admission.status().fence_reason() {
						Some(RuntimeAdmissionFenceReason::ConfirmedLifetimeElapsed) => {
							NativeParticipationFenceError::ConfirmedLifetimeElapsed
						}
						_ => NativeParticipationFenceError::LocalConfirmationRejected(error),
					};
					break Err(task.shared.fence(&task.admission, failure));
				}
				task.confirmed_until = confirmed_until;
				let mut state = task.shared.lock();
				if !matches!(state.renewal, NativeParticipationRenewalStatus::Fenced(_)) {
					state.renewal =
						NativeParticipationRenewalStatus::Running { last_failure: None };
				}
			}
		}
		let Some(next_attempt) = request_start.checked_add(task.timing.renewal_interval) else {
			break Err(task.shared.fence(
				&task.admission,
				NativeParticipationFenceError::LocalClockDomain,
			));
		};
		task.next_attempt = next_attempt;
	};
	if result.is_ok() {
		let mut state = task.shared.lock();
		if !matches!(state.renewal, NativeParticipationRenewalStatus::Fenced(_)) {
			state.renewal = NativeParticipationRenewalStatus::Stopped;
		}
		drop(state);
		task.shared.changed.notify_waiters();
	}
	completion.state = TaskCompletionState::Completed;
	result
}

pub(crate) fn commit_outcome_is_unknown(error: &NativeParticipationError) -> bool {
	match error {
		NativeParticipationError::Commit(error) => error.commit_outcome_is_unknown(),
		NativeParticipationError::RollbackFailed {
			operation_error, ..
		} => commit_outcome_is_unknown(operation_error),
		_ => false,
	}
}

fn authority_lost(error: &NativeParticipationError) -> bool {
	match error {
		NativeParticipationError::ParticipantAbsent
		| NativeParticipationError::ControlAbsent
		| NativeParticipationError::WrongInstallation
		| NativeParticipationError::ParticipantBindingMismatch
		| NativeParticipationError::ControlBindingMismatch
		| NativeParticipationError::Participant(ParticipantStateError::AuthorityUnavailable) => true,
		NativeParticipationError::RollbackFailed {
			operation_error, ..
		} => authority_lost(operation_error),
		_ => false,
	}
}

struct ShutdownTask {
	completion: ShutdownTaskCompletion,
	participation: Arc<PgNativeParticipation>,
	registration: Arc<CommittedParticipation>,
	admission: RuntimeAdmissionTracker,
	shared: Arc<SharedRuntime>,
	timing: ParticipationTiming,
	stop_renewal: watch::Sender<RenewalCommand>,
	renewal_task: JoinHandle<Result<(), Arc<NativeParticipationFenceError>>>,
}

struct ShutdownTaskCompletion {
	shared: Arc<SharedRuntime>,
	admission: RuntimeAdmissionTracker,
	stop_renewal: watch::Sender<RenewalCommand>,
	state: TaskCompletionState,
}

impl Drop for ShutdownTaskCompletion {
	fn drop(&mut self) {
		if matches!(self.state, TaskCompletionState::Pending) {
			self.admission.stop(RuntimeAdmissionStop::Fenced);
			self.stop_renewal.send_replace(RenewalCommand::Stop);
			self.shared
				.finish_shutdown(NativeParticipationShutdownResult {
					local_drainage: self.admission.snapshot(),
					renewal: Err(
						NativeParticipationRenewalCompletionError::ShutdownContinuationAbandoned,
					),
					removal: NativeParticipationRemovalResult::ContinuationAbandoned,
				});
		}
	}
}

async fn run_shutdown(task: ShutdownTask) {
	let mut completion = task.completion;
	// Local drainage retains registered lease/caller work, holds no control relation authority,
	// and never ends caller-owned transactions. Abandoned ownership prohibits unregister.
	let drained = task.admission.wait_until_drained().await;
	if drained.is_err() {
		task.admission.stop(RuntimeAdmissionStop::Fenced);
	}
	task.stop_renewal.send_replace(RenewalCommand::Stop);
	let renewal = match task.renewal_task.await {
		Ok(Ok(())) => Ok(()),
		Ok(Err(failure)) => Err(NativeParticipationRenewalCompletionError::Fenced(failure)),
		Err(_) => Err(NativeParticipationRenewalCompletionError::TaskAbandoned),
	};
	if let Err(error) = drained {
		task.shared
			.finish_shutdown(NativeParticipationShutdownResult {
				local_drainage: task.admission.snapshot(),
				renewal,
				removal: NativeParticipationRemovalResult::DrainageRejected(error),
			});
		completion.state = TaskCompletionState::Completed;
		return;
	}
	if let Err(error) = task.admission.finish_local_lifetime() {
		task.shared
			.finish_shutdown(NativeParticipationShutdownResult {
				local_drainage: task.admission.snapshot(),
				renewal,
				removal: NativeParticipationRemovalResult::LocalLifetimeRejected(error),
			});
		completion.state = TaskCompletionState::Completed;
		return;
	}
	let removal = remove_participation(&task.participation, &task.registration, task.timing).await;
	task.shared
		.finish_shutdown(NativeParticipationShutdownResult {
			local_drainage: task.admission.snapshot(),
			renewal,
			removal,
		});
	completion.state = TaskCompletionState::Completed;
}

async fn remove_participation(
	participation: &PgNativeParticipation,
	registration: &CommittedParticipation,
	timing: ParticipationTiming,
) -> NativeParticipationRemovalResult {
	let Some(operation_until) = Instant::now().checked_add(timing.control_operation_timeout) else {
		return NativeParticipationRemovalResult::LocalClockDomain;
	};
	match timeout_at(operation_until, participation.unregister(registration)).await {
		Ok(Ok(was_present)) => NativeParticipationRemovalResult::Removed { was_present },
		Ok(Err(error)) => NativeParticipationRemovalResult::Failed(Arc::new(error)),
		Err(_) => NativeParticipationRemovalResult::TimedOut,
	}
}
