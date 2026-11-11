//! Unique queued ownership and caller-owned work; diagnostics provide no task permission.

use super::driver::{DriverShared, NativeThrottlerObserver};
use super::{Arc, NativeThrottlerDenial, NativeThrottlerError, ThrottlerTaskOutcome};
use crate::runtime::local_admission::throttler_work::RuntimeThrottlerCallerWorkPermit;
use std::{
	future::Future,
	pin::Pin,
	task::{Context, Poll},
};
use tokio::sync::oneshot;

pub(crate) enum NativeThrottlerStartResult {
	Acquired(NativeThrottlerClient),
	Denied(NativeThrottlerDenial),
}
/// Receiving a queued acquisition is an ownership action, not a successful send acknowledgment.
pub(crate) struct NativeThrottlerPendingDelivery {
	pub(super) receiver: oneshot::Receiver<InitialDelivery>,
	pub(super) shared: Arc<DriverShared>,
	pub(super) acknowledgement: DeliveryAcknowledgement,
}
impl NativeThrottlerPendingDelivery {
	pub(crate) async fn receive(
		mut self,
	) -> Result<NativeThrottlerStartResult, Arc<NativeThrottlerError>> {
		let mut delivery = (&mut self.receiver)
			.await
			.map_err(|_| Arc::new(NativeThrottlerError::DriverClosed))?;
		let value = delivery.accept()?;
		self.acknowledgement = DeliveryAcknowledgement::Accepted;
		value
	}
}
impl Drop for NativeThrottlerPendingDelivery {
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
	Queued(Result<NativeThrottlerStartResult, Arc<NativeThrottlerError>>),
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
		Result<NativeThrottlerStartResult, Arc<NativeThrottlerError>>,
		Arc<NativeThrottlerError>,
	> {
		if matches!(
			&self.state,
			InitialDeliveryState::Queued(Ok(NativeThrottlerStartResult::Acquired(_)))
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

pub(crate) struct NativeThrottlerClient {
	pub(super) shared: Arc<DriverShared>,
}
impl NativeThrottlerClient {
	pub(super) fn new(shared: Arc<DriverShared>) -> Self {
		Self { shared }
	}
	pub(crate) fn observer(&self) -> NativeThrottlerObserver {
		NativeThrottlerObserver(Arc::clone(&self.shared))
	}
	/// The callback is invoked once only after actual caller admission and fresh local
	/// confirmation.
	pub(crate) async fn run_task<TaskFn, F, T, E>(
		self,
		task_fn: TaskFn,
	) -> Result<NativeThrottlerTaskResult<T, E>, Arc<NativeThrottlerError>>
	where
		TaskFn: FnOnce(NativeThrottlerObserver) -> F,
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
			.map_err(|_| Arc::new(NativeThrottlerError::DriverClosed))??;
		let permit = self.shared.accept_caller()?;
		delivery.acknowledgement = DeliveryAcknowledgement::Accepted;
		receive.acknowledgement = DeliveryAcknowledgement::Accepted;
		let observer = self.observer();
		// Install the established cancellation outcome before invoking application code.
		let task = CallerTask {
			shared: Arc::clone(&self.shared),
			_permit: permit,
			outcome: ThrottlerTaskOutcome::Failed,
		};
		let future = task_fn(observer.clone());
		let mut work = SupportedCallerFuture {
			future: Box::pin(future),
			task,
		};
		let output = (&mut work).await;
		// The observation precedes the task's drop, which starts closing the owner.
		let local = observer.snapshot();
		let SupportedCallerFuture { future, mut task } = work;
		task.outcome = if output.is_ok() {
			ThrottlerTaskOutcome::Succeeded
		} else {
			ThrottlerTaskOutcome::Failed
		};
		drop(future);
		drop(task);
		Ok(NativeThrottlerTaskResult {
			task: output,
			local,
			observer,
		})
	}
}
impl Drop for NativeThrottlerClient {
	fn drop(&mut self) {
		self.shared.close();
	}
}

/// Application output does not certify local reservation survival or database accounting.
pub(crate) struct NativeThrottlerTaskResult<T, E> {
	pub(crate) task: Result<T, E>,
	pub(crate) local: super::driver::NativeThrottlerLocalObservation,
	pub(crate) observer: NativeThrottlerObserver,
}
struct CallerReceive {
	receiver: oneshot::Receiver<Result<CallerDelivery, Arc<NativeThrottlerError>>>,
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
	_permit: RuntimeThrottlerCallerWorkPermit,
	outcome: ThrottlerTaskOutcome,
}
impl Drop for CallerTask {
	fn drop(&mut self) {
		self.shared.finish_caller(self.outcome);
	}
}
/// Drop destroys the caller future before releasing its actual caller-work count.
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
