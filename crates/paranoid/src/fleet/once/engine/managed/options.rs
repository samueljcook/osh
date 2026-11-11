//! A Once run's lease, acquisition, and close-wait choices.

use crate::fleet::lease::owned_driver::{
	close::NativeLeaseCloseWait,
	options::{NativeLeaseAcquisitionChoice, NativeLeaseOwnerOptions},
};

pub(crate) struct NativeOnceManagedOptionsFields {
	pub(crate) lease: NativeLeaseOwnerOptions,
	pub(crate) acquisition: NativeLeaseAcquisitionChoice,
	pub(crate) close_wait: NativeLeaseCloseWait,
}
pub(crate) struct NativeOnceManagedOptions {
	pub(super) lease: NativeLeaseOwnerOptions,
	pub(super) acquisition: NativeLeaseAcquisitionChoice,
	pub(super) close_wait: NativeLeaseCloseWait,
}
impl NativeOnceManagedOptions {
	pub(crate) fn new(fields: NativeOnceManagedOptionsFields) -> Self {
		Self {
			lease: fields.lease,
			acquisition: fields.acquisition,
			close_wait: fields.close_wait,
		}
	}
}
