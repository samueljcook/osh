use super::{
	Arc, DriverShared, HolderSnapshot, Instant, NativeLeaseDriverError, NativeLeaseDriverState,
	NativeOwnedLeaseClient, RuntimeAdmissionStop, sleep_until, watch,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NativeLeaseLossReason {
	LeaseDeadlineElapsed,
	RuntimeFenced,
	RuntimeLifetimeFinished,
	ShutdownWithoutCallerWork,
	ClientClosed,
	DriverClosing,
	RenewalFailed,
}

#[derive(Clone, Debug)]
pub(crate) enum NativeLeaseAuthorityObservation {
	ConfirmedLocally {
		holder: HolderSnapshot,
		usable_until: Instant,
	},
	Lost {
		reason: NativeLeaseLossReason,
	},
}

/// Read-only observation does not retain caller work, private Claim, or SQL permission.
#[derive(Clone)]
pub(crate) struct NativeLeaseLossObserver {
	pub(super) shared: Arc<DriverShared>,
	changes: watch::Receiver<()>,
}
impl NativeLeaseLossObserver {
	pub(super) fn new(shared: &Arc<DriverShared>) -> Self {
		Self {
			shared: Arc::clone(shared),
			changes: shared.changed.subscribe(),
		}
	}
	/// Computes fresh bounds and publishes sticky loss before returning any local authority.
	pub(crate) fn observe(&self) -> NativeLeaseAuthorityObservation {
		loop {
			let snapshot = self
				.shared
				.state
				.lock()
				.expect("lease observations contain no caller code");
			let runtime = self.shared.runtime.work_view(self.shared.work_id);
			let callers = runtime
				.work
				.as_ref()
				.map_or(0, |entry| entry.pending_caller_work_count);
			let loss = if runtime.lifetime_finished() {
				Some(NativeLeaseLossReason::RuntimeLifetimeFinished)
			} else if runtime.stop() == Some(RuntimeAdmissionStop::Fenced) {
				Some(NativeLeaseLossReason::RuntimeFenced)
			} else if self.shared.closing.get().is_some() {
				Some(NativeLeaseLossReason::ClientClosed)
			} else if runtime.stop() == Some(RuntimeAdmissionStop::Shutdown) && callers == 0 {
				Some(NativeLeaseLossReason::ShutdownWithoutCallerWork)
			} else if snapshot
				.phase
				.confirmed_until()
				.is_some_and(|until| Instant::now() >= until)
			{
				Some(NativeLeaseLossReason::LeaseDeadlineElapsed)
			} else if snapshot.phase.state() != NativeLeaseDriverState::Held {
				Some(NativeLeaseLossReason::DriverClosing)
			} else {
				None
			};
			if snapshot.loss.get().is_none()
				&& let Some(loss) = loss
			{
				let _ = snapshot.loss.set(loss);
				self.shared.changed.send_replace(());
			}
			if let Some(reason) = snapshot.loss.get() {
				return NativeLeaseAuthorityObservation::Lost {
					reason: reason.clone(),
				};
			}
			let until = snapshot
				.phase
				.confirmed_until()
				.expect("held authority retains its lease bound")
				.min(runtime.confirmed_until);
			if Instant::now() >= until {
				// Resample both bounds before sticky loss; participant renewal can change its cap.
				drop(snapshot);
				continue;
			}
			return NativeLeaseAuthorityObservation::ConfirmedLocally {
				holder: snapshot
					.phase
					.holder()
					.cloned()
					.expect("held authority retains diagnostic holder"),
				usable_until: until,
			};
		}
	}
	/// The watch version is observed before the level; expiry/runtime changes also wake this wait.
	pub(crate) async fn wait_until_lost(&mut self) -> NativeLeaseLossReason {
		loop {
			self.changes.borrow_and_update();
			let runtime = self.shared.runtime.work_view(self.shared.work_id);
			let until = match self.observe() {
				NativeLeaseAuthorityObservation::Lost { reason } => return reason,
				NativeLeaseAuthorityObservation::ConfirmedLocally { usable_until, .. } => {
					usable_until
				}
			};
			tokio::select! {
				_ = self.changes.changed() => {},
				_ = self.shared.runtime.wait_for_work_change(self.shared.work_id, &runtime) => {},
				_ = sleep_until(until) => {},
			}
		}
	}
}

pub(crate) struct NativeLeaseCallerCtx {
	pub(crate) holder: HolderSnapshot,
	pub(crate) loss: NativeLeaseLossObserver,
}

/// Effect output, including any caller error, remains separate from observed local authority.
pub(crate) struct NativeLeaseCallerWorkResult<O> {
	pub(crate) output: O,
	pub(crate) authority_at_completion: NativeLeaseAuthorityObservation,
}

impl NativeOwnedLeaseClient {
	pub(crate) fn loss_observer(&self) -> NativeLeaseLossObserver {
		NativeLeaseLossObserver::new(&self.shared)
	}
	/// Invokes the callback once after scope admission and a fresh confirmed check.
	/// Caller work is never spawned or aborted, and its output has no Send/static/Error bound.
	pub(crate) async fn run_effect<O>(
		&mut self,
		effect_fn: impl AsyncFnOnce(NativeLeaseCallerCtx) -> O,
	) -> Result<NativeLeaseCallerWorkResult<O>, Arc<NativeLeaseDriverError>> {
		let permit = self.begin_caller_work().await?;
		let observation = self.loss_observer();
		let holder = match observation.observe() {
			NativeLeaseAuthorityObservation::ConfirmedLocally { holder, .. } => holder,
			NativeLeaseAuthorityObservation::Lost { .. } => {
				return Err(Arc::new(NativeLeaseDriverError::AuthorityLost));
			}
		};
		let output = effect_fn(NativeLeaseCallerCtx {
			holder,
			loss: observation.clone(),
		})
		.await;
		let authority_at_completion = observation.observe();
		permit.finish();
		Ok(NativeLeaseCallerWorkResult {
			output,
			authority_at_completion,
		})
	}
}
