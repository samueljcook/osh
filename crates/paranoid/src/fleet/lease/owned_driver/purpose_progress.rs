//! Final Lease purposes retain preparation only after their transaction is dispatched.

pub(super) enum PurposeDispatch<T> {
	Registered,
	Dispatched,
	Prepared(T),
}

impl<T> PurposeDispatch<T> {
	pub(super) fn registration_transferred(&mut self) {
		assert!(
			matches!(self, Self::Registered),
			"a final purpose dispatches once"
		);
		*self = Self::Dispatched;
	}

	pub(super) fn prepare(&mut self, intended: T) {
		assert!(
			matches!(self, Self::Dispatched),
			"a dispatched final purpose prepares once"
		);
		*self = Self::Prepared(intended);
	}

	pub(super) fn is_dispatched(&self) -> bool {
		!matches!(self, Self::Registered)
	}

	pub(super) fn prepared(&self) -> Option<&T> {
		match self {
			Self::Prepared(intended) => Some(intended),
			Self::Registered | Self::Dispatched => None,
		}
	}
}
