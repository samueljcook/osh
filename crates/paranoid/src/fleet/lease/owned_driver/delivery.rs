//! A queued Lease result retains cancellation responsibility until actual acceptance.

use super::{Arc, DriverShared, Instant, NativeLeaseDriverError, oneshot};

pub(super) struct LeaseDelivery<T> {
	state: DeliveryState<T>,
	shared: Arc<DriverShared>,
}

enum DeliveryState<T> {
	Ready {
		deadline: LeaseDeliveryDeadline,
		value: T,
	},
	Rejected(Arc<NativeLeaseDriverError>),
	Accepted,
}

/// Operation results retain their original budget; caller scopes use current held authority.
#[derive(Clone, Copy)]
pub(super) enum LeaseDeliveryDeadline {
	OriginalOperationDeadline(Instant),
	FreshCurrentAuthority,
	HeldAuthority,
}

impl<T> LeaseDelivery<T> {
	pub(super) fn new(
		result: Result<T, Arc<NativeLeaseDriverError>>,
		deadline: Option<LeaseDeliveryDeadline>,
		shared: &Arc<DriverShared>,
	) -> Self {
		Self {
			state: match result {
				Ok(value) => DeliveryState::Ready {
					deadline: deadline
						.expect("a successful Lease delivery retains its authority policy"),
					value,
				},
				Err(error) => DeliveryState::Rejected(error),
			},
			shared: Arc::clone(shared),
		}
	}
	pub(super) fn deadline(&self) -> Option<LeaseDeliveryDeadline> {
		match self.state {
			DeliveryState::Ready { deadline, .. } => Some(deadline),
			_ => None,
		}
	}
	pub(super) fn value(&self) -> Option<&T> {
		match &self.state {
			DeliveryState::Ready { value, .. } => Some(value),
			_ => None,
		}
	}
	pub(super) fn accept(mut self) -> Result<T, Arc<NativeLeaseDriverError>> {
		match std::mem::replace(&mut self.state, DeliveryState::Accepted) {
			DeliveryState::Ready { value, .. } => Ok(value),
			DeliveryState::Rejected(error) => Err(error),
			DeliveryState::Accepted => unreachable!("a queued Lease result is accepted once"),
		}
	}
}

impl<T> Drop for LeaseDelivery<T> {
	fn drop(&mut self) {
		if !matches!(self.state, DeliveryState::Accepted) {
			self.shared.close();
		}
	}
}

pub(super) struct PendingDelivery(PendingDeliveryState);

enum PendingDeliveryState {
	Awaiting(Arc<DriverShared>),
	Accepted,
}

impl PendingDelivery {
	pub(super) fn new(shared: &Arc<DriverShared>) -> Self {
		Self(PendingDeliveryState::Awaiting(Arc::clone(shared)))
	}
	pub(super) fn accept(&mut self) {
		self.0 = PendingDeliveryState::Accepted;
	}
}

impl Drop for PendingDelivery {
	fn drop(&mut self) {
		if let PendingDeliveryState::Awaiting(shared) = &self.0 {
			shared.close();
		}
	}
}

pub(super) type LeaseResponse<T> = oneshot::Sender<LeaseDelivery<T>>;
