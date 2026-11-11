//! Unique acknowledged delivery and caller-owned effects with independent release accounting.

use super::driver::{DriverShared, NativeSemaphoreAccountingObservation, NativeSemaphoreObserver};
use super::{Arc, NativeSemaphoreError};
use crate::fleet::SemaphoreClaimSnapshot;
use crate::runtime::local_admission::semaphore_work::RuntimeSemaphoreCallerWorkPermit;
use std::{
	future::Future,
	pin::Pin,
	task::{Context, Poll},
};
use tokio::sync::oneshot;

pub(crate) enum NativeSemaphoreStartResult {
	Acquired(NativeSemaphoreClient),
	Unavailable,
}
/// Both receiver and queued response retain closure until actual acknowledgment.
pub(crate) struct NativeSemaphorePendingDelivery {
	pub(super) receiver: oneshot::Receiver<InitialDelivery>,
	pub(super) shared: Arc<DriverShared>,
	pub(super) acknowledgement: DeliveryAcknowledgement,
}
impl NativeSemaphorePendingDelivery {
	pub(crate) async fn receive(
		mut self,
	) -> Result<NativeSemaphoreStartResult, Arc<NativeSemaphoreError>> {
		let mut delivery = (&mut self.receiver)
			.await
			.map_err(|_| Arc::new(NativeSemaphoreError::DriverClosed))?;
		let value = delivery.accept()?;
		self.acknowledgement = DeliveryAcknowledgement::Accepted;
		value
	}
}
impl Drop for NativeSemaphorePendingDelivery {
	fn drop(&mut self) {
		if matches!(self.acknowledgement, DeliveryAcknowledgement::Awaiting) {
			self.shared.close();
		}
	}
}
pub(super) enum DeliveryAcknowledgement {
	Awaiting,
	Accepted,
}

pub(super) enum InitialDeliveryState {
	Queued(Result<NativeSemaphoreStartResult, Arc<NativeSemaphoreError>>),
	Accepted,
}

pub(super) struct InitialDelivery {
	pub(super) state: InitialDeliveryState,
	pub(super) shared: Arc<DriverShared>,
}
impl InitialDelivery {
	fn accept(
		&mut self,
	) -> Result<
		Result<NativeSemaphoreStartResult, Arc<NativeSemaphoreError>>,
		Arc<NativeSemaphoreError>,
	> {
		if matches!(
			&self.state,
			InitialDeliveryState::Queued(Ok(NativeSemaphoreStartResult::Acquired(_)))
		) {
			self.shared.accept_initial()?;
		}
		match std::mem::replace(&mut self.state, InitialDeliveryState::Accepted) {
			InitialDeliveryState::Queued(value) => Ok(value),
			InitialDeliveryState::Accepted => {
				unreachable!("initial delivery is accepted exactly once")
			}
		}
	}
}
impl Drop for InitialDelivery {
	fn drop(&mut self) {
		if matches!(self.state, InitialDeliveryState::Queued(_)) {
			self.shared.close();
		}
	}
}
/// The unique client contains no claim, capability or free cleanup target.
pub(crate) struct NativeSemaphoreClient {
	shared: Arc<DriverShared>,
}
impl NativeSemaphoreClient {
	pub(super) fn new(shared: Arc<DriverShared>) -> Self {
		Self { shared }
	}
	pub(crate) fn observer(&self) -> NativeSemaphoreObserver {
		NativeSemaphoreObserver(Arc::clone(&self.shared))
	}
	/// Invokes one callback after actual caller admission without spawning or restricting its
	/// future.
	pub(crate) async fn run_task<TaskFn, F, T, E>(
		self,
		task_fn: TaskFn,
	) -> Result<NativeSemaphoreTaskResult<T, E>, Arc<NativeSemaphoreError>>
	where
		TaskFn: FnOnce(NativeSemaphoreCallerCtx) -> F,
		F: Future<Output = Result<T, E>>,
	{
		let (sender, receiver) = oneshot::channel();
		self.shared.request_caller(sender)?;
		let mut receive = CallerReceive {
			receiver,
			shared: Arc::clone(&self.shared),
			acknowledgement: DeliveryAcknowledgement::Awaiting,
		};
		let mut delivery = (&mut receive.receiver)
			.await
			.map_err(|_| Arc::new(NativeSemaphoreError::DriverClosed))??;
		let (permit, claim) = self.shared.accept_caller()?;
		delivery.acknowledgement = DeliveryAcknowledgement::Accepted;
		receive.acknowledgement = DeliveryAcknowledgement::Accepted;
		let observer = self.observer();
		let task = CallerTask {
			shared: Arc::clone(&self.shared),
			_permit: permit,
		};
		let future = task_fn(NativeSemaphoreCallerCtx {
			claim,
			observer: observer.clone(),
		});
		let mut work = SupportedCallerFuture {
			future: Box::pin(future),
			task,
		};
		let output = (&mut work).await;
		let SupportedCallerFuture { future, task } = work;
		drop(future);
		let local = observer.snapshot();
		drop(task);
		let accounting = observer.wait_accounting().await;
		Ok(NativeSemaphoreTaskResult {
			task: output,
			local,
			accounting,
		})
	}
}
impl Drop for NativeSemaphoreClient {
	fn drop(&mut self) {
		self.shared.close();
	}
}
/// These fields describe local confirmation and do not authorize release or prove reset exclusion.
pub(crate) struct NativeSemaphoreCallerCtx {
	pub(crate) claim: SemaphoreClaimSnapshot,
	pub(crate) observer: NativeSemaphoreObserver,
}
pub(crate) struct NativeSemaphoreTaskResult<T, E> {
	pub(crate) task: Result<T, E>,
	pub(crate) local: super::driver::NativeSemaphoreLocalObservation,
	pub(crate) accounting: NativeSemaphoreAccountingObservation,
}
struct CallerReceive {
	receiver: oneshot::Receiver<Result<CallerDelivery, Arc<NativeSemaphoreError>>>,
	shared: Arc<DriverShared>,
	acknowledgement: DeliveryAcknowledgement,
}
impl Drop for CallerReceive {
	fn drop(&mut self) {
		if matches!(self.acknowledgement, DeliveryAcknowledgement::Awaiting) {
			self.shared.close();
		}
	}
}
pub(super) struct CallerDelivery {
	pub(super) shared: Arc<DriverShared>,
	pub(super) acknowledgement: DeliveryAcknowledgement,
}
impl Drop for CallerDelivery {
	fn drop(&mut self) {
		if matches!(self.acknowledgement, DeliveryAcknowledgement::Awaiting) {
			self.shared.close();
		}
	}
}
struct CallerTask {
	shared: Arc<DriverShared>,
	_permit: RuntimeSemaphoreCallerWorkPermit,
}
impl Drop for CallerTask {
	fn drop(&mut self) {
		self.shared.finish_caller();
	}
}
/// Cancellation destroys caller work before releasing its real shared-runtime caller count.
struct SupportedCallerFuture<F> {
	future: Pin<Box<F>>,
	task: CallerTask,
}
impl<F: Future> Future for SupportedCallerFuture<F> {
	type Output = F::Output;
	fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		self.get_mut().future.as_mut().poll(cx)
	}
}
