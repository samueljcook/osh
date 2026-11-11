//! Private owned lease work. Authority stays in one library task; caller effects stay local.

pub(crate) mod acquire;
pub(crate) mod cache_publication;
mod cleanup_progress;
pub(crate) mod close;
use cleanup_progress::{CleanupDeadline, CleanupFailureProgress};
mod delivery;
mod final_purpose;
use delivery::{LeaseDelivery, LeaseDeliveryDeadline, LeaseResponse, PendingDelivery};
mod purpose_progress;
use purpose_progress::PurposeDispatch;
mod ownership;
use ownership::{LeaseOwnership, RenewalAttemptEnding, RenewalSchedule};
mod shared_state;
use shared_state::{SharedLeasePhase, SharedLeaseState};
pub(crate) mod once_completion;
pub(crate) mod once_pair;
use crate::runtime::admission::owned_primitive::{
	OwnedPrimitiveOriginOrdering, OwnedPrimitivePendingOrigins,
};
use cache_publication::{NativeLeaseCachePublicationFields, NativeLeaseCallerScope};
use final_purpose::{FinalPurposeCleanupOriginFields, LeaseFinalPurposeOrigin};
use once_completion::NativeLeaseOnceCompletionFields;
pub(crate) mod denial;
pub(crate) mod loss;
use close::{NativeLeaseCloseObserver, NativeLeaseReleaseAccounting};
use denial::{NativeLeaseOwnedStart, NativeLeaseStartErrorWithObserver, SettledLeaseDenial};
pub(crate) mod options;
use loss::NativeLeaseLossReason;
use options::{NativeLeaseOwnerOptions, NativeLeaseRenewalPolicy};

use super::cleanup_target::LeaseCleanupTarget;
use super::driver_io::{self, LeaseCommittedCompletion, LeaseCompletionKind, LeaseOperationOrigin};
use super::engine::{NativeLeaseError, PgNativeLease};
use super::state::LeaseAcquisition;
use super::{Claim, ClaimDuration, HolderPrefix, HolderSnapshot, RandomnessError, Token};
use crate::db::transaction_ending::TransactionEndingDisposition;
use crate::runtime::admission::lease::{
	LeaseCleanupExecution, LeaseCleanupPhase, LeaseCleanupResult,
};
use crate::runtime::local_admission::lease_work::{
	RuntimeLeaseWorkCompletion, RuntimeLeaseWorkPermit, RuntimeLeaseWorkSnapshot,
};
use crate::runtime::local_admission::{
	RuntimeAdmissionError, RuntimeAdmissionStop, RuntimeAdmissionTracker, RuntimeWorkView,
};
use std::{
	future::Future,
	sync::{Arc, Mutex, OnceLock},
	time::Duration,
};
use tokio::{
	sync::{Notify, mpsc, oneshot, watch},
	time::{Instant, sleep, sleep_until},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeLeaseDriverState {
	Acquiring,
	Held,
	Closing,
	Finished,
	Unresolved,
}

#[derive(Clone, Debug)]
pub(crate) struct NativeLeaseDriverSnapshot {
	pub(crate) state: NativeLeaseDriverState,
	pub(crate) release: NativeLeaseReleaseAccounting,
	pub(crate) loss: Option<NativeLeaseLossReason>,
	/// Conservative lease duration bound; it is not combined participant authority.
	pub(crate) lease_confirmed_until: Option<Instant>,
	/// Fresh current minimum, absent whenever usable local authority is unavailable.
	pub(crate) usable_until: Option<Instant>,
	pub(crate) failures: Vec<Arc<NativeLeaseDriverError>>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum NativeLeaseWorkPhase {
	Acquire,
	CachePublication,
	OnceCompletion,
	Renew,
	PhysicalProof,
	Cleanup,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeLeaseDriverError {
	#[error("native owned lease operation failed")]
	Operation(#[source] NativeLeaseError),
	#[error("native owned lease ending failed after the caller stopped waiting")]
	Ending(#[source] NativeLeaseInterruptedEndingError),
	#[error("native owned lease local lifetime rejected work")]
	Local(#[source] RuntimeAdmissionError),
	#[error("native owned lease private material generation failed")]
	Randomness(#[source] RandomnessError),
	#[error("native owned lease {0:?} exceeded its configured attempt budget")]
	Timeout(NativeLeaseWorkPhase),
	#[error("native owned lease client delivery was cancelled")]
	DeliveryClosed,
	#[error("native owned lease confirmed authority is unavailable")]
	AuthorityLost,
	#[error("native owned lease timing exceeds the local monotonic clock domain")]
	LocalClockDomain,
}

/// Retains the actual owned ending cause without cloning or reconstructing its DatabaseError.
#[derive(Debug)]
pub(crate) struct NativeLeaseInterruptedEndingError(
	Arc<crate::db::transaction_ending::TransactionEndingResult>,
);
impl std::fmt::Display for NativeLeaseInterruptedEndingError {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str("native lease transaction ending failed after caller cancellation")
	}
}
impl std::error::Error for NativeLeaseInterruptedEndingError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		self.0
			.error()
			.map(|error| error as &(dyn std::error::Error + 'static))
	}
}

/// Exact bounded terminal causes, retained by the common registry after the client is gone.
#[derive(Debug)]
pub(crate) struct NativeLeaseUnresolvedError {
	pub(crate) failures: Vec<Arc<NativeLeaseDriverError>>,
}

impl std::fmt::Display for NativeLeaseUnresolvedError {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(
			formatter,
			"native lease release remains unresolved after {} recorded failures",
			self.failures.len()
		)
	}
}
impl std::error::Error for NativeLeaseUnresolvedError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		self.failures
			.first()
			.map(|error| error.as_ref() as &(dyn std::error::Error + 'static))
	}
}

impl NativeLeaseDriverError {
	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::Operation(error) => {
				crate::db::TransactionFailure::involves_database_failure(error)
			}
			Self::Ending(error) => error.0.error().is_some(),
			_ => false,
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Operation(error) => {
				crate::db::TransactionFailure::commit_outcome_is_unknown(error)
			}
			Self::Ending(error) => ending_commit_outcome_is_unknown(&error.0),
			_ => false,
		}
	}
}

struct DriverShared {
	runtime: RuntimeAdmissionTracker,
	work_id: crate::runtime::local_admission::lease_work::RuntimeLeaseWorkId,
	closing: OnceLock<()>,
	close_changed: Notify,
	state: Mutex<SharedLeaseState>,
	changed: watch::Sender<()>,
}
impl DriverShared {
	fn close(&self) {
		let _ = self.closing.set(());
		self.close_changed.notify_waiters();
		self.changed.send_replace(());
	}
	async fn wait_closed(&self) {
		loop {
			let changed = self.close_changed.notified();
			tokio::pin!(changed);
			changed.as_mut().enable();
			if self.closing.get().is_some() {
				return;
			}
			changed.await;
		}
	}
	fn update(&self, change: impl FnOnce(&mut SharedLeaseState)) {
		let mut snapshot = self
			.state
			.lock()
			.expect("lease driver snapshots contain no caller code");
		change(&mut snapshot);
		self.changed.send_replace(());
	}
	fn require_delivery(
		&self,
		kind: DeliveryAuthority,
		deadline: Option<LeaseDeliveryDeadline>,
	) -> Result<(), NativeLeaseDriverError> {
		self.delivery_deadline(kind, deadline).map(|_| ())
	}
	fn delivery_deadline(
		&self,
		kind: DeliveryAuthority,
		deadline: Option<LeaseDeliveryDeadline>,
	) -> Result<Instant, NativeLeaseDriverError> {
		loop {
			let snapshot = self
				.state
				.lock()
				.expect("lease delivery observations contain no caller code");
			let runtime = self.runtime.work_view(self.work_id);
			let entry = runtime
				.work
				.as_ref()
				.ok_or(NativeLeaseDriverError::AuthorityLost)?;
			let stop_allowed = runtime.stop().is_none()
				|| (matches!(kind, DeliveryAuthority::HeldRenewal)
					&& runtime.stop() == Some(RuntimeAdmissionStop::Shutdown)
					&& entry.pending_caller_work_count > 0);
			let now = Instant::now();
			let policy_rejects = match deadline {
				Some(LeaseDeliveryDeadline::OriginalOperationDeadline(until)) => now >= until,
				Some(LeaseDeliveryDeadline::FreshCurrentAuthority) => {
					entry.pending_caller_work_count == 0
				}
				Some(LeaseDeliveryDeadline::HeldAuthority) => false,
				None => true,
			};
			if policy_rejects
				|| !stop_allowed
				|| runtime.lifetime_finished()
				|| self.closing.get().is_some()
				|| snapshot.phase.state() != NativeLeaseDriverState::Held
				|| snapshot.loss.get().is_some()
				|| snapshot
					.phase
					.confirmed_until()
					.is_none_or(|until| now >= until)
			{
				return Err(NativeLeaseDriverError::AuthorityLost);
			}
			if now >= runtime.confirmed_until {
				// Resample after elapsed participant caps; timely renewal may have advanced them.
				// Actual participant expiry is fenced by the fresh runtime observation.
				drop(snapshot);
				continue;
			}
			let until = snapshot
				.phase
				.confirmed_until()
				.expect("confirmed delivery retains its Lease bound")
				.min(runtime.confirmed_until);
			return Ok(match deadline {
				Some(LeaseDeliveryDeadline::OriginalOperationDeadline(deadline)) => {
					until.min(deadline)
				}
				_ => until,
			});
		}
	}
	fn snapshot(&self) -> NativeLeaseDriverSnapshot {
		let runtime = self.runtime.work_view(self.work_id);
		let mut snapshot = self
			.state
			.lock()
			.expect("lease driver snapshots contain no caller code")
			.snapshot();
		let callers = runtime
			.work
			.as_ref()
			.map_or(0, |entry| entry.pending_caller_work_count);
		let continuing = runtime.stop().is_none()
			|| (runtime.stop() == Some(RuntimeAdmissionStop::Shutdown) && callers > 0);
		snapshot.usable_until = snapshot
			.lease_confirmed_until
			.map(|until| until.min(runtime.confirmed_until))
			.filter(|until| {
				snapshot.state == NativeLeaseDriverState::Held
					&& snapshot.loss.is_none()
					&& continuing && !runtime.lifetime_finished()
					&& self.closing.get().is_none()
					&& Instant::now() < *until
			});
		snapshot
	}
}

enum DeliveryAuthority {
	NewWork,
	HeldRenewal,
}
enum LeaseInitialResult {
	Acquired,
	Denied,
}

enum LeaseCmd {
	OnceCompletion {
		fields: Box<NativeLeaseOnceCompletionFields>,
		response: LeaseResponse<once_completion::PreparedLeaseOnceCompletion>,
	},
	CachePublication {
		fields: Box<NativeLeaseCachePublicationFields>,
		response: LeaseResponse<cache_publication::PreparedLeaseCachePublication>,
	},
	CallerWork {
		response: LeaseResponse<NativeLeaseCallerScope>,
	},
}

/// Unique client. Dropping the client requests exact cleanup; it never drops task ownership.
pub(crate) struct NativeOwnedLeaseClient {
	cmds: mpsc::Sender<LeaseCmd>,
	shared: Arc<DriverShared>,
}
impl Drop for NativeOwnedLeaseClient {
	fn drop(&mut self) {
		self.shared.close();
	}
}
impl NativeOwnedLeaseClient {
	async fn receive<T>(
		&self,
		kind: DeliveryAuthority,
		receiver: oneshot::Receiver<LeaseDelivery<T>>,
	) -> Result<T, Arc<NativeLeaseDriverError>> {
		let mut pending = PendingDelivery::new(&self.shared);
		let delivered = receiver
			.await
			.map_err(|_| Arc::new(NativeLeaseDriverError::DeliveryClosed))?;
		if delivered.value().is_some() {
			self.shared
				.require_delivery(kind, delivered.deadline())
				.map_err(Arc::new)?;
		}
		let value = delivered.accept();
		pending.accept();
		value
	}
	pub(crate) async fn begin_caller_work(
		&mut self,
	) -> Result<NativeLeaseCallerScope, Arc<NativeLeaseDriverError>> {
		let mut pending = PendingDelivery::new(&self.shared);
		let (response, receive) = oneshot::channel();
		self.cmds
			.send(LeaseCmd::CallerWork { response })
			.await
			.map_err(|_| Arc::new(NativeLeaseDriverError::DeliveryClosed))?;
		let permit = self.receive(DeliveryAuthority::NewWork, receive).await?;
		pending.accept();
		Ok(permit)
	}
}

pub(crate) struct NativeLeaseOwnedStartFields {
	pub(crate) options: NativeLeaseOwnerOptions,
	pub(crate) delivery_deadline: Option<Instant>,
}
impl PgNativeLease {
	pub(crate) async fn start_owned(
		&self,
		fields: NativeLeaseOwnedStartFields,
	) -> Result<NativeLeaseOwnedStart, NativeLeaseStartErrorWithObserver> {
		let requested_at = Instant::now();
		let NativeLeaseOwnedStartFields {
			options,
			delivery_deadline,
		} = fields;
		if delivery_deadline.is_some_and(|until| requested_at >= until) {
			return Err(NativeLeaseStartErrorWithObserver {
				source: Arc::new(NativeLeaseDriverError::Timeout(
					NativeLeaseWorkPhase::Acquire,
				)),
				observer: None,
			});
		}
		let owner = self
			.bindings
			.admission
			.register_lease_work(self)
			.map_err(|error| NativeLeaseStartErrorWithObserver {
				source: Arc::new(NativeLeaseDriverError::Operation(
					NativeLeaseError::Admission(error),
				)),
				observer: None,
			})?;
		let changes = watch::Sender::new(());
		let shared = Arc::new(DriverShared {
			runtime: owner.admission_tracker(),
			work_id: owner.id(),
			closing: OnceLock::new(),
			close_changed: Notify::new(),
			state: Mutex::new(SharedLeaseState::new()),
			changed: changes,
		});
		// A unique mutable client has at most one command awaiting a response.
		let (cmds, inbox) = mpsc::channel(1);
		let client = NativeOwnedLeaseClient {
			cmds,
			shared: Arc::clone(&shared),
		};
		let (response, receive) = oneshot::channel();
		let lease = self.clone();
		let prefix = options.holder_prefix;
		let duration = options.claim_duration;
		let renewal_policy = options.renewal_policy;
		let runtime = self.bindings.originating_runtime.clone();
		let observer = NativeLeaseCloseObserver::new(&shared);
		owner
			.spawn_owned_task(&runtime, move |owner| async move {
				LeaseDriver {
					lease,
					owner,
					duration,
					renewal_policy,
					renewal: RenewalSchedule::AwaitingClaim,
					shared,
					inbox,
					ownership: LeaseOwnership::New,
					origin: LeaseOperationOrigin::default(),
					renewal_attempt_ending: RenewalAttemptEnding::Unobserved,
				}
				.run(prefix, response)
				.await;
			})
			.map_err(|error| NativeLeaseStartErrorWithObserver {
				source: Arc::new(NativeLeaseDriverError::Local(error)),
				observer: Some(observer.clone()),
			})?;
		let mut pending = PendingDelivery::new(&client.shared);
		let received = match delivery_deadline {
			Some(deadline) => tokio::time::timeout_at(deadline, receive)
				.await
				.map_err(|_| NativeLeaseStartErrorWithObserver {
					source: Arc::new(NativeLeaseDriverError::Timeout(
						NativeLeaseWorkPhase::Acquire,
					)),
					observer: Some(observer.clone()),
				})?,
			None => receive.await,
		};
		let delivered = received.map_err(|_| NativeLeaseStartErrorWithObserver {
			source: Arc::new(NativeLeaseDriverError::DeliveryClosed),
			observer: Some(observer.clone()),
		})?;
		let initial_deadline = match (delivered.deadline(), delivery_deadline) {
			(Some(LeaseDeliveryDeadline::OriginalOperationDeadline(original)), Some(cap)) => Some(
				LeaseDeliveryDeadline::OriginalOperationDeadline(original.min(cap)),
			),
			(Some(LeaseDeliveryDeadline::HeldAuthority), Some(cap)) => {
				Some(LeaseDeliveryDeadline::OriginalOperationDeadline(cap))
			}
			(deadline, _) => deadline,
		};
		if matches!(delivered.value(), Some(LeaseInitialResult::Acquired)) {
			client
				.shared
				.require_delivery(DeliveryAuthority::NewWork, initial_deadline)
				.map_err(|error| NativeLeaseStartErrorWithObserver {
					source: Arc::new(error),
					observer: Some(observer.clone()),
				})?;
		}
		if delivery_deadline.is_some_and(|until| Instant::now() >= until) {
			return Err(NativeLeaseStartErrorWithObserver {
				source: Arc::new(NativeLeaseDriverError::Timeout(
					NativeLeaseWorkPhase::Acquire,
				)),
				observer: Some(observer.clone()),
			});
		}
		let value = delivered
			.accept()
			.map_err(|source| NativeLeaseStartErrorWithObserver {
				source,
				observer: Some(observer.clone()),
			})?;
		pending.accept();
		match value {
			LeaseInitialResult::Acquired => Ok(NativeLeaseOwnedStart::Acquired(client)),
			LeaseInitialResult::Denied => Ok(NativeLeaseOwnedStart::Denied(
				SettledLeaseDenial::new(observer),
			)),
		}
	}
}

struct LeaseDriver {
	lease: PgNativeLease,
	owner: RuntimeLeaseWorkPermit,
	duration: ClaimDuration,
	renewal_policy: NativeLeaseRenewalPolicy,
	renewal: RenewalSchedule,
	shared: Arc<DriverShared>,
	inbox: mpsc::Receiver<LeaseCmd>,
	ownership: LeaseOwnership,
	origin: LeaseOperationOrigin,
	renewal_attempt_ending: RenewalAttemptEnding,
}
impl LeaseDriver {
	fn remember_failure(&self, error: NativeLeaseDriverError) -> Arc<NativeLeaseDriverError> {
		let error = Arc::new(error);
		self.shared
			.update(|state| state.failures.push(Arc::clone(&error)));
		error
	}
	fn remember_completion(&mut self, completion: LeaseCommittedCompletion) {
		self.shared.update(|snapshot| {
			snapshot.release = match completion.kind {
				LeaseCompletionKind::AcquisitionDenied => NativeLeaseReleaseAccounting::NotAcquired,
				LeaseCompletionKind::Release(true) => NativeLeaseReleaseAccounting::Released,
				LeaseCompletionKind::Release(false) => NativeLeaseReleaseAccounting::NoMatch,
			}
		});
		self.ownership = LeaseOwnership::Committed(completion);
	}

	fn holder(claim: &Claim) -> HolderSnapshot {
		HolderSnapshot {
			key: claim.key.clone(),
			holder_id: claim.holder_id.clone(),
			fencing_token: claim.fencing_token,
			expires_at_unix_micros: claim.expires_at_unix_micros,
		}
	}
	fn operation_deadline(&self, requested: Instant) -> Result<Instant, NativeLeaseDriverError> {
		let until = requested
			.checked_add(self.lease.bindings.timing.operation_timeout)
			.ok_or(NativeLeaseDriverError::LocalClockDomain)?;
		Ok(self
			.ownership
			.until()
			.map_or(until, |prior| until.min(prior)))
	}
	fn confirm_claim(&mut self) -> Result<HolderSnapshot, NativeLeaseDriverError> {
		let (claim, requested) = self.ownership.candidate();
		let until = requested
			.checked_add(self.duration.as_duration())
			.ok_or(NativeLeaseDriverError::LocalClockDomain)?;
		let next_renewal = match self.renewal_policy {
			NativeLeaseRenewalPolicy::Automatic(policy) => requested
				.checked_add(policy.heartbeat_interval)
				.ok_or(NativeLeaseDriverError::LocalClockDomain)?,
		};
		let holder = Self::holder(claim);
		loop {
			let mut snapshot = self
				.shared
				.state
				.lock()
				.expect("lease confirmation observations contain no caller code");
			let runtime = self.owner.runtime_view();
			let callers = runtime
				.work
				.as_ref()
				.map_or(0, |entry| entry.pending_caller_work_count);
			let now = Instant::now();
			if self.shared.closing.get().is_some()
				|| snapshot.loss.get().is_some()
				|| runtime.stop() == Some(RuntimeAdmissionStop::Fenced)
				|| runtime.lifetime_finished()
				|| (runtime.stop() == Some(RuntimeAdmissionStop::Shutdown) && callers == 0)
				|| self.ownership.until().is_some_and(|prior| now >= prior)
				|| now >= until
			{
				return Err(NativeLeaseDriverError::AuthorityLost);
			}
			if now >= runtime.confirmed_until {
				// Keep actual lease bounds fixed while resampling an elapsed participant cap.
				drop(snapshot);
				continue;
			}
			self.renewal = RenewalSchedule::Scheduled {
				next: next_renewal,
				failures: 0,
			};
			self.ownership.confirm(until);
			snapshot.phase = SharedLeasePhase::Held {
				holder: holder.clone(),
				confirmed_until: until,
			};
			snapshot.renewal_failures.clear();
			self.shared.changed.send_replace(());
			return Ok(holder);
		}
	}
	async fn wait_origin(
		owner: &RuntimeLeaseWorkPermit,
		origin: &mut LeaseOperationOrigin,
	) -> Result<(), NativeLeaseDriverError> {
		if let Some(receipt) = origin.receipt() {
			let ending = receipt.wait_until_terminal().await;
			origin.observe_ending(ending);
		}
		owner
			.wait_until_continuations_end()
			.await
			.map_err(NativeLeaseDriverError::Local)
	}

	/// The current bound of an operation: its own `deadline`, when it has one, and the
	/// runtime's confirmed lifetime.
	fn current_operation_deadline(
		owner: &RuntimeLeaseWorkPermit,
		shared: &DriverShared,
		deadline: Option<Instant>,
		phase: NativeLeaseWorkPhase,
	) -> Result<(RuntimeWorkView<RuntimeLeaseWorkSnapshot>, Instant), NativeLeaseDriverError> {
		loop {
			let runtime = owner.runtime_view();
			let callers = runtime
				.work
				.as_ref()
				.map_or(0, |entry| entry.pending_caller_work_count);
			if runtime.stop() == Some(RuntimeAdmissionStop::Fenced)
				|| runtime.lifetime_finished()
				|| (runtime.stop() == Some(RuntimeAdmissionStop::Shutdown) && callers == 0)
				|| shared.snapshot().loss.is_some()
			{
				return Err(NativeLeaseDriverError::AuthorityLost);
			}
			if shared.closing.get().is_some() {
				return Err(NativeLeaseDriverError::DeliveryClosed);
			}
			if deadline.is_some_and(|until| Instant::now() >= until) {
				return Err(NativeLeaseDriverError::Timeout(phase));
			}
			if Instant::now() >= runtime.confirmed_until {
				// Resample expiry: a timely participant renewal may have changed this snapshot.
				// Actual expiry makes the tracker's fencing sticky on the next snapshot.
				continue;
			}
			let current = deadline.map_or(runtime.confirmed_until, |until| {
				until.min(runtime.confirmed_until)
			});
			return Ok((runtime, current));
		}
	}

	/// The operation and prior lease bounds stay fixed while participant renewal updates its cap.
	async fn wait_authorized_operation<T>(
		owner: &RuntimeLeaseWorkPermit,
		shared: &DriverShared,
		deadline: Option<Instant>,
		phase: NativeLeaseWorkPhase,
		work: impl Future<Output = Result<T, NativeLeaseDriverError>>,
		cancelled: impl Future<Output = ()>,
	) -> Result<T, NativeLeaseDriverError> {
		tokio::pin!(work, cancelled);
		loop {
			let (runtime, current) =
				Self::current_operation_deadline(owner, shared, deadline, phase)?;
			tokio::select! { biased;
				_ = shared.wait_closed() => return Err(NativeLeaseDriverError::DeliveryClosed),
				_ = &mut cancelled => return Err(NativeLeaseDriverError::DeliveryClosed),
				_ = owner.wait_for_runtime_change(&runtime) => continue,
				_ = sleep_until(current) => continue,
				result = &mut work => {
					let value = result?;
					Self::current_operation_deadline(owner, shared, deadline, phase)?;
					return Ok(value);
				},
			}
		}
	}

	async fn wait_deliverable_origin(
		&mut self,
		deadline: Option<Instant>,
		phase: NativeLeaseWorkPhase,
	) -> Result<(), NativeLeaseDriverError> {
		Self::wait_authorized_operation(
			&self.owner,
			&self.shared,
			deadline,
			phase,
			Self::wait_origin(&self.owner, &mut self.origin),
			std::future::pending(),
		)
		.await
	}
	async fn run(mut self, prefix: HolderPrefix, response: LeaseResponse<LeaseInitialResult>) {
		let initial = self.acquire(prefix).await;
		let acquired = matches!(initial, Ok(LeaseInitialResult::Acquired));
		let denied = matches!(initial, Ok(LeaseInitialResult::Denied));
		let result = initial.map_err(|error| self.remember_failure(error));
		let _ = response.send(LeaseDelivery::new(
			result,
			Some(LeaseDeliveryDeadline::HeldAuthority),
			&self.shared,
		));
		if denied {
			return;
		}
		if acquired {
			self.serve_cmds().await;
		}
		self.drain_pending_cmds().await;
		let runtime = self.owner.admission_tracker().status();
		let loss = if runtime.stop() == Some(RuntimeAdmissionStop::Fenced) {
			NativeLeaseLossReason::RuntimeFenced
		} else if self.shared.closing.get().is_some() {
			NativeLeaseLossReason::ClientClosed
		} else if self
			.ownership
			.until()
			.is_some_and(|until| Instant::now() >= until)
		{
			NativeLeaseLossReason::LeaseDeadlineElapsed
		} else if runtime.stop() == Some(RuntimeAdmissionStop::Shutdown) {
			NativeLeaseLossReason::ShutdownWithoutCallerWork
		} else if acquired {
			NativeLeaseLossReason::RenewalFailed
		} else {
			NativeLeaseLossReason::DriverClosing
		};
		self.shared.update(|state| {
			state.failures.append(&mut state.renewal_failures);
			state.phase = SharedLeasePhase::Closing;
			state.loss.get_or_init(|| loss);
		});
		self.renewal = RenewalSchedule::Stopped;
		if let Err(error) = self.owner.begin_closing() {
			self.remember_failure(NativeLeaseDriverError::Local(error));
			return;
		}
		self.cleanup().await;
	}
	/// Acquires the lease. The acquisition waits for its record's turn holding no connection, and
	/// only the runtime's confirmed lifetime bounds it after the turn so that waiting for the key
	/// under contention is no failure.
	async fn acquire(
		&mut self,
		prefix: HolderPrefix,
	) -> Result<LeaseInitialResult, NativeLeaseDriverError> {
		let admission = self.owner.admission_tracker();
		let _turn = Self::wait_authorized_operation(
			&self.owner,
			&self.shared,
			None,
			NativeLeaseWorkPhase::Acquire,
			async { Ok(admission.take_record_turn(self.lease.identity()).await) },
			std::future::pending(),
		)
		.await?;
		let started = Instant::now();
		let proposal = LeaseAcquisition {
			holder_id: prefix
				.generate_holder_id()
				.map_err(NativeLeaseDriverError::Randomness)?,
			lease_token: Token::generate().map_err(NativeLeaseDriverError::Randomness)?,
			duration: self.duration,
		};
		self.ownership =
			LeaseOwnership::Acquiring(LeaseCleanupTarget::acquisition(self.lease.key(), &proposal));
		let result = Self::wait_authorized_operation(
			&self.owner,
			&self.shared,
			None,
			NativeLeaseWorkPhase::Acquire,
			async {
				driver_io::acquire(&self.lease, &self.owner, proposal, &mut self.origin)
					.await
					.map_err(NativeLeaseDriverError::Operation)
			},
			std::future::pending(),
		)
		.await;
		self.origin.capture_interrupted_ending(matches!(
			&result,
			Err(NativeLeaseDriverError::Timeout(_)
				| NativeLeaseDriverError::DeliveryClosed
				| NativeLeaseDriverError::AuthorityLost)
		));
		let grant = match result? {
			Some(grant) => grant,
			None => {
				if let Some(receipt) = self.origin.receipt() {
					receipt.wait_until_terminal().await;
				}
				let completion = self
					.origin
					.committed_completion()
					.ok_or(NativeLeaseDriverError::AuthorityLost)?;
				if !matches!(completion.kind, LeaseCompletionKind::AcquisitionDenied) {
					return Err(NativeLeaseDriverError::AuthorityLost);
				}
				self.remember_completion(completion);
				Self::wait_origin(&self.owner, &mut self.origin).await?;
				if !self.finish(RuntimeLeaseWorkCompletion::Finished).await {
					return Err(NativeLeaseDriverError::AuthorityLost);
				}
				return Ok(LeaseInitialResult::Denied);
			}
		};
		self.ownership
			.retain_candidate(grant.into_claim_after_known_commit(), started);
		self.wait_deliverable_origin(None, NativeLeaseWorkPhase::Acquire)
			.await?;
		self.owner
			.confirm_held()
			.map_err(NativeLeaseDriverError::Local)?;
		// The stored claim starts when the transaction observes the database's time, after this
		// local start, so the local end of the claim is never later than the stored end.
		self.confirm_claim()?;
		Ok(LeaseInitialResult::Acquired)
	}
	async fn serve_cmds(&mut self) {
		loop {
			let runtime = self.owner.runtime_view();
			let callers = runtime
				.work
				.as_ref()
				.map_or(0, |entry| entry.pending_caller_work_count);
			let usable_until = self
				.ownership
				.until()
				.map(|until| until.min(runtime.confirmed_until));
			if self.shared.closing.get().is_some()
				|| self.shared.snapshot().loss.is_some()
				|| runtime.stop() == Some(RuntimeAdmissionStop::Fenced)
				|| (runtime.stop() == Some(RuntimeAdmissionStop::Shutdown) && callers == 0)
				|| runtime.lifetime_finished()
				|| self
					.ownership
					.until()
					.is_none_or(|until| Instant::now() >= until)
			{
				return;
			}
			if usable_until.is_some_and(|until| Instant::now() >= until) {
				continue;
			}
			let cmd = tokio::select! { biased;
				_ = self.shared.wait_closed() => return,
				_ = sleep_until(usable_until.expect("held driver has both confirmed bounds")) => continue,
				_ = self.owner.wait_for_runtime_change(&runtime) => continue,
				_ = wait_renewal_schedule(self.renewal.next()) => { if !self.automatic_renew().await { return; } continue; },
				cmd = self.inbox.recv() => cmd,
			};
			match cmd {
				Some(LeaseCmd::OnceCompletion { fields, response }) => {
					let result = self.prepare_once_completion(*fields);
					let deadline = result.as_ref().ok().map(|prepared| prepared.deadline);
					let result = result.map_err(|error| self.remember_failure(error));
					let _ = response.send(LeaseDelivery::new(
						result,
						deadline.map(LeaseDeliveryDeadline::OriginalOperationDeadline),
						&self.shared,
					));
					if let Some(deadline) = deadline {
						self.wait_final_purpose(deadline, NativeLeaseWorkPhase::OnceCompletion)
							.await;
					}
					return;
				}
				Some(LeaseCmd::CachePublication { fields, response }) => {
					let result = self.prepare_cache_publication(*fields);
					let deadline = result.as_ref().ok().map(|prepared| prepared.deadline);
					let result = result.map_err(|error| self.remember_failure(error));
					let _ = response.send(LeaseDelivery::new(
						result,
						deadline.map(LeaseDeliveryDeadline::OriginalOperationDeadline),
						&self.shared,
					));
					if let Some(deadline) = deadline {
						self.wait_final_purpose(deadline, NativeLeaseWorkPhase::CachePublication)
							.await;
					}
					return;
				}
				None => return,
				Some(LeaseCmd::CallerWork { response }) => {
					let confirmed = self.shared.snapshot().usable_until;
					let result = if confirmed.is_none() {
						Err(Arc::new(NativeLeaseDriverError::AuthorityLost))
					} else {
						self.owner
							.try_start_caller_work()
							.map(|caller| NativeLeaseCallerScope::new(caller, &self.shared))
							.map_err(|error| Arc::new(NativeLeaseDriverError::Local(error)))
					};
					let _ = response.send(LeaseDelivery::new(
						result,
						Some(LeaseDeliveryDeadline::FreshCurrentAuthority),
						&self.shared,
					));
				}
			}
		}
	}

	async fn automatic_renew(&mut self) -> bool {
		let NativeLeaseRenewalPolicy::Automatic(policy) = self.renewal_policy;
		let requested = Instant::now();
		match self.renew(requested, None).await {
			Ok(_) => true,
			Err(error) => {
				let retryable = matches!(&error, NativeLeaseDriverError::Operation(operation) if actual_retryable_database_failure(operation));
				// PostgreSQL commits nothing that was not sent COMMIT, so a renewal that
				// ended its transaction before sending COMMIT left the claim unchanged, whether or not
				// the rollback was confirmed.
				let unchanged = self.renewal_attempt_ending.commit_was_not_attempted();
				let error = Arc::new(error);
				let failures = self.renewal.record_failure();
				self.shared
					.update(|snapshot| snapshot.renewal_failures.push(Arc::clone(&error)));
				let attempts_left = policy
					.max_consecutive_renewal_failures
					.get()
					.saturating_sub(failures);
				if retryable
					&& unchanged && attempts_left > 0
					&& self.shared.snapshot().usable_until.is_some()
				{
					self.ownership.restore_unchanged_renewal();
					// The remaining attempts are spread over the claim's remaining time so that
					// each of them runs before the claim expires.
					let now = Instant::now();
					let remaining = self
						.ownership
						.until()
						.map_or(Duration::ZERO, |until| until.saturating_duration_since(now));
					let delay = policy
						.heartbeat_interval
						.min(remaining / (attempts_left + 1));
					let Some(next) = now.checked_add(delay) else {
						self.remember_failure(NativeLeaseDriverError::LocalClockDomain);
						return false;
					};
					self.renewal.reschedule(next);
					true
				} else {
					// The finite window becomes the exact terminal operation history before cleanup.
					self.shared.update(|snapshot| {
						snapshot.failures.append(&mut snapshot.renewal_failures)
					});
					false
				}
			}
		}
	}

	async fn renew(
		&mut self,
		requested: Instant,
		mut response: Option<&mut LeaseResponse<HolderSnapshot>>,
	) -> Result<HolderSnapshot, NativeLeaseDriverError> {
		self.renewal_attempt_ending = RenewalAttemptEnding::Unobserved;
		let deadline = self.operation_deadline(requested)?;
		if Instant::now() >= deadline {
			return Err(NativeLeaseDriverError::AuthorityLost);
		}
		self.wait_deliverable_origin(Some(deadline), NativeLeaseWorkPhase::Renew)
			.await?;
		let token = Token::generate().map_err(NativeLeaseDriverError::Randomness)?;
		self.ownership.begin_renewal(&token);
		let claim = self
			.ownership
			.claim()
			.expect("renewal retains its original private claim");
		let mut attempt = LeaseOperationOrigin::default();
		let result = Self::wait_authorized_operation(
			&self.owner,
			&self.shared,
			Some(deadline),
			NativeLeaseWorkPhase::Renew,
			async {
				driver_io::renew(
					&self.lease,
					&self.owner,
					claim,
					token,
					self.duration,
					&mut attempt,
				)
				.await
				.map_err(NativeLeaseDriverError::Operation)
			},
			async {
				match response.as_mut() {
					Some(response) => response.closed().await,
					None => std::future::pending().await,
				}
			},
		)
		.await;
		self.renewal_attempt_ending =
			attempt
				.receipt()
				.map_or(RenewalAttemptEnding::Unobserved, |receipt| {
					match receipt.snapshot() {
						crate::db::transaction_ending::TransactionEndingSnapshot::Terminal(
							ending,
						) => RenewalAttemptEnding::Observed(ending),
						_ => RenewalAttemptEnding::Unobserved,
					}
				});
		attempt.capture_interrupted_ending(matches!(
			&result,
			Err(NativeLeaseDriverError::Timeout(_)
				| NativeLeaseDriverError::DeliveryClosed
				| NativeLeaseDriverError::AuthorityLost)
		));
		if attempt.receipt().is_some() {
			self.origin = attempt;
		}
		let grant = result?.ok_or(NativeLeaseDriverError::AuthorityLost)?;
		self.ownership
			.retain_candidate(grant.into_claim_after_known_commit(), requested);
		self.wait_deliverable_origin(Some(deadline), NativeLeaseWorkPhase::Renew)
			.await?;
		self.confirm_claim()
	}
	async fn drain_pending_cmds(&mut self) {
		self.inbox.close();
		while let Some(cmd) = self.inbox.recv().await {
			drop(cmd);
		}
	}

	async fn wait_caller_work(&self) -> Result<(), NativeLeaseDriverError> {
		loop {
			let snapshot = self.owner.runtime_view();
			let entry = snapshot
				.work
				.as_ref()
				.ok_or(NativeLeaseDriverError::AuthorityLost)?;
			if entry.pending_caller_work_count == 0 {
				return Ok(());
			}
			self.owner.wait_for_runtime_change(&snapshot).await;
		}
	}
	async fn finish(&mut self, completion: RuntimeLeaseWorkCompletion) -> bool {
		if let Err(error) = self.owner.wait_until_continuations_end().await {
			self.remember_failure(NativeLeaseDriverError::Local(error));
			return false;
		}
		if let Some(known) = self.ownership.completion()
			&& !matches!(known.receipt.snapshot(), crate::db::transaction_ending::TransactionEndingSnapshot::Terminal(ending) if ending.disposition() == TransactionEndingDisposition::KnownCommit)
		{
			self.remember_failure(NativeLeaseDriverError::AuthorityLost);
			return false;
		}

		let completion_result = match completion {
			RuntimeLeaseWorkCompletion::Finished => self.owner.complete(completion),
			RuntimeLeaseWorkCompletion::Unresolved => {
				let snapshot = self.shared.snapshot();
				let outcome = Arc::new(NativeLeaseUnresolvedError {
					failures: snapshot.failures,
				});
				self.shared
					.update(|snapshot| snapshot.release = NativeLeaseReleaseAccounting::Unresolved);
				self.owner.complete_unresolved(outcome)
			}
		};
		if let Err(error) = completion_result {
			self.remember_failure(NativeLeaseDriverError::Local(error));
			return false;
		}
		self.shared.update(|state| {
			state.phase = match completion {
				RuntimeLeaseWorkCompletion::Finished => SharedLeasePhase::Finished,
				RuntimeLeaseWorkCompletion::Unresolved => SharedLeasePhase::Unresolved,
			};
		});
		self.ownership = LeaseOwnership::Finished;
		true
	}
	async fn cleanup(&mut self) {
		if let Some(publication) = self.ownership.final_purpose() {
			publication.wait_until_terminal().await;
			if let Some(ending) = publication.ending_failure() {
				self.remember_failure(NativeLeaseDriverError::Ending(
					NativeLeaseInterruptedEndingError(ending),
				));
			}
		}
		if let Some(completion) = self
			.ownership
			.final_purpose()
			.and_then(|purpose| purpose.committed_completion())
		{
			self.remember_completion(completion);
		}
		if let Err(error) = self.wait_caller_work().await {
			self.remember_failure(error);
			return;
		}
		if let Some(receipt) = self.origin.receipt() {
			receipt.wait_until_terminal().await;
		}
		if let Some(completion) = self.origin.committed_completion() {
			self.remember_completion(completion);
		}
		if let Err(error) = Self::wait_origin(&self.owner, &mut self.origin).await {
			self.remember_failure(error);
			return;
		}
		if self.ownership.completion().is_some_and(|completion| {
			matches!(completion.kind, LeaseCompletionKind::AcquisitionDenied)
		}) {
			self.finish(RuntimeLeaseWorkCompletion::Finished).await;
			return;
		}
		if self
			.ownership
			.completion()
			.is_some_and(|completion| matches!(completion.kind, LeaseCompletionKind::Release(_)))
		{
			self.finish(RuntimeLeaseWorkCompletion::Finished).await;
			return;
		}
		let Some(taken) = self.origin.take_receipt() else {
			// No protected lease transaction was dispatched.
			self.shared
				.update(|snapshot| snapshot.release = NativeLeaseReleaseAccounting::NotAcquired);
			self.finish(RuntimeLeaseWorkCompletion::Finished).await;
			return;
		};
		if self.ownership.target().is_none() {
			self.finish(RuntimeLeaseWorkCompletion::Finished).await;
			return;
		}
		let mut origin = taken.receipt;
		let mut unrecorded_ending = taken.interrupted_ending;
		let lease_operation = origin.clone();
		let mut purpose_pending_origins = OwnedPrimitivePendingOrigins::new();
		let timing = self.lease.bindings.timing;
		let mut failures = CleanupFailureProgress::Ready { failures: 0 };
		loop {
			let ending = origin.wait_until_terminal().await;
			if let Err(error) = self.owner.wait_until_continuations_end().await {
				self.remember_failure(NativeLeaseDriverError::Local(error));
				return;
			}
			let purpose_origins = self.ownership.final_purpose().and_then(|publication| {
				publication.cleanup_origins(FinalPurposeCleanupOriginFields {
					lease_operation: &lease_operation,
					preceding_operation: &origin,
					pending_origins: &purpose_pending_origins,
				})
			});
			if let Some(origins) = &purpose_origins {
				origins.wait_until_terminal().await;
				if let Err(error) = purpose_pending_origins.retain(origins) {
					self.remember_failure(NativeLeaseDriverError::Operation(
						crate::runtime::admission::lease::map_lease_ordering_error(error),
					));
					return;
				}
			}

			let known = if let Some(origins) = &purpose_origins {
				match origins.all_known() {
					Ok(known) => known,
					Err(error) => {
						self.remember_failure(NativeLeaseDriverError::Operation(
							crate::runtime::admission::lease::map_lease_ordering_error(error),
						));
						return;
					}
				}
			} else {
				matches!(
					ending.disposition(),
					TransactionEndingDisposition::KnownCommit
						| TransactionEndingDisposition::KnownRollback
				)
			};
			if known && let Some(ending) = unrecorded_ending.take() {
				self.shared.update(|state| {
					state.failures.push(Arc::new(NativeLeaseDriverError::Ending(
						NativeLeaseInterruptedEndingError(ending),
					)))
				});
			}
			if known && let Some(error) = failures.settle() {
				self.shared.update(|state| state.failures.push(error));
			}
			if known && failures.is_exhausted(timing.max_consecutive_cleanup_failures.get()) {
				self.finish(RuntimeLeaseWorkCompletion::Unresolved).await;
				return;
			}
			// An uncertain final failure needs a physical barrier, without another release mutation.
			let proof_only =
				failures.needs_final_proof(timing.max_consecutive_cleanup_failures.get());
			let execution = if proof_only {
				LeaseCleanupExecution::PhysicalProofOnly
			} else {
				LeaseCleanupExecution::Release
			};
			let phase = if known {
				LeaseCleanupPhase::AuthenticationAndRelease(Instant::now())
			} else {
				LeaseCleanupPhase::OriginOrdering
			};
			let (phase_send, mut phase_read) = watch::channel(phase);
			let mut attempt = LeaseOperationOrigin::default();
			let mut proof =
				crate::runtime::admission::owned_primitive::OriginOrderingProgress::Awaiting;
			let mut purpose_ordering =
				crate::runtime::admission::owned_primitive::OriginOrderingProgress::<
					OwnedPrimitiveOriginOrdering,
				>::Awaiting;
			let mut deadline = match CleanupDeadline::new(phase, &timing) {
				Ok(deadline) => deadline,
				Err(error) => {
					self.remember_failure(error);
					return;
				}
			};
			let result = {
				let invocation = crate::runtime::admission::lease::LeaseCleanupInvocation {
					owner: &self.owner,
					lease: &self.lease,
					target: self
						.ownership
						.target()
						.expect("cleanup retains actual material"),
					origin: &origin,
					proof: &mut proof,
					phase: &phase_send,
					execution,
					final_purpose: purpose_origins.as_ref().map(|origins| {
						crate::runtime::admission::lease::LeaseFinalPurposeCleanupInvocation {
							origins,
							ordering: &mut purpose_ordering,
						}
					}),
				};
				let work = driver_io::cleanup(invocation, &mut attempt);
				tokio::pin!(work);
				loop {
					let ready = tokio::select! { biased;
						changed = phase_read.changed(), if deadline.is_ordering() => {
							if changed.is_ok() {
								let observed = *phase_read.borrow_and_update();
								if let Err(error) = deadline.observe(observed, timing.cleanup_attempt_timeout) {
									break Err(error);
								}
							}
							continue;
						}
						_ = sleep_until(deadline.until()) => break Err(NativeLeaseDriverError::Timeout(deadline.phase())),
						result = &mut work => result.map_err(NativeLeaseDriverError::Operation),
					};
					// The concrete operation can complete on the poll that publishes its phase.
					// Use that actual phase instant even when its notification was not polled first.
					if let Err(error) =
						deadline.observe(*phase_read.borrow(), timing.cleanup_attempt_timeout)
					{
						break Err(error);
					}
					if ready.is_ok() && Instant::now() >= deadline.until() {
						break Err(NativeLeaseDriverError::Timeout(deadline.phase()));
					}
					break ready;
				}
			};

			attempt.capture_interrupted_ending(matches!(
				&result,
				Err(NativeLeaseDriverError::Timeout(_))
			));
			if let Some(receipt) = attempt.receipt() {
				receipt.wait_until_terminal().await;
			}
			if let Some(completion) = attempt.committed_completion() {
				self.remember_completion(completion);
			}
			if let Err(error) = Self::wait_origin(&self.owner, &mut attempt).await {
				self.remember_failure(error);
				return;
			}
			let origin_proved = known || proof.is_proven() || purpose_ordering.is_proven();
			if let Some(ordering) = purpose_ordering.proof() {
				purpose_pending_origins.clear_observed(ordering);
			}
			if origin_proved && let Some(ending) = unrecorded_ending.take() {
				self.shared.update(|state| {
					state.failures.push(Arc::new(NativeLeaseDriverError::Ending(
						NativeLeaseInterruptedEndingError(ending),
					)))
				});
			}

			if origin_proved && let Some(error) = failures.settle() {
				self.shared.update(|state| state.failures.push(error));
			}
			if self.ownership.completion().is_some_and(|completion| {
				matches!(completion.kind, LeaseCompletionKind::Release(_))
			}) {
				if let Err(error) = result {
					self.remember_failure(error);
				}

				self.finish(RuntimeLeaseWorkCompletion::Finished).await;
				return;
			}
			if matches!(
			 &result,
			 Err(NativeLeaseDriverError::Operation(NativeLeaseError::Registered(crate::runtime::admission::lease::LeaseRegistrationError::Admission(error))))
			  if matches!(error.as_ref(), crate::runtime::admission::NativeAdmissionError::Local(RuntimeAdmissionError::LeaseWorkStillActive))
			) {
				// Ending publication may precede permit disposal. This level wait is not a failed
				// cleanup attempt and cannot consume the configured failure count.
				if let Err(error) = self.owner.wait_until_continuations_end().await {
					self.remember_failure(NativeLeaseDriverError::Local(error));
					return;
				}
				if let Err(error) = self.wait_caller_work().await {
					self.remember_failure(error);
					return;
				}
				continue;
			}
			if proof_only && origin_proved {
				// The proof-only transaction has no lease mutation, and its own local ending/count
				// is settled above. This reports unresolved release, not confirmed backend drainage.
				self.finish(RuntimeLeaseWorkCompletion::Unresolved).await;
				return;
			}
			match result {
				Ok(LeaseCleanupResult::PhysicalProofOnly) => {
					self.finish(RuntimeLeaseWorkCompletion::Unresolved).await;
					return;
				}
				Ok(LeaseCleanupResult::Release(_)) => {
					self.remember_failure(NativeLeaseDriverError::AuthorityLost);
					self.finish(RuntimeLeaseWorkCompletion::Unresolved).await;
					return;
				}
				Err(_) if !origin_proved => {
					// No release path ran, so the attempt counts as no cleanup failure.
					// Without a final purpose, the next ordering attempt orders the same origin.
					// With a final purpose, it orders this attempt's transaction.
					if purpose_origins.is_some()
						&& let Some(taken) = attempt.take_receipt()
					{
						origin = taken.receipt;
					}
					sleep(timing.physical_proof_retry_interval).await;
				}
				Err(error) => {
					failures.record(Arc::new(error));
					if let Some(taken) = attempt.take_receipt() {
						unrecorded_ending = taken.interrupted_ending;
						origin = taken.receipt;
					}
					sleep(timing.cleanup_retry_interval).await;
				}
			}
		}
	}
}

async fn wait_renewal_schedule(next: Option<Instant>) {
	match next {
		Some(next) => sleep_until(next).await,
		None => std::future::pending().await,
	}
}
fn actual_retryable_database_failure(error: &NativeLeaseError) -> bool {
	let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
	while let Some(error) = source {
		if let Some(database) = error.downcast_ref::<crate::db::DatabaseError>() {
			return database.retry() != crate::db::RetryClassification::NotRetryable;
		}
		source = error.source();
	}
	false
}

fn ending_commit_outcome_is_unknown(
	ending: &crate::db::transaction_ending::TransactionEndingResult,
) -> bool {
	ending
		.error()
		.is_some_and(crate::db::DatabaseError::commit_outcome_is_unknown)
		|| (ending.commit_attempted()
			&& matches!(
				ending.disposition(),
				TransactionEndingDisposition::Unknown | TransactionEndingDisposition::TaskAbandoned
			))
}
