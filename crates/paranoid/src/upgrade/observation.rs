//! Parsed control and backend state retained by an actual installation operation.
//!
//! These values describe an observed or prepared state. They grant no authority and remain
//! tentative until the enclosing registered transaction has a known successful ending.

use std::fmt;

use crate::installation::control_state::InstallationControlState;

use crate::installation::creation::PgBackendRegistration;
use crate::runtime::admission::bindings::InstallationStorageRoute;

pub(in crate::upgrade) struct NativeUpgradeObservedControlFields {
	pub(crate) control: InstallationControlState,
	pub(crate) route: InstallationStorageRoute,
	pub(crate) backend: PgBackendRegistration,
}

pub(in crate::upgrade) struct NativeUpgradeObservedControl {
	control: InstallationControlState,
	route: InstallationStorageRoute,
	backend: PgBackendRegistration,
}

impl NativeUpgradeObservedControl {
	pub(crate) fn new(fields: NativeUpgradeObservedControlFields) -> Self {
		Self {
			control: fields.control,
			route: fields.route,
			backend: fields.backend,
		}
	}

	pub(crate) fn control(&self) -> &InstallationControlState {
		&self.control
	}

	pub(crate) fn route(&self) -> InstallationStorageRoute {
		self.route
	}

	pub(crate) fn backend(&self) -> &PgBackendRegistration {
		&self.backend
	}
}

impl fmt::Debug for NativeUpgradeObservedControl {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("NativeUpgradeObservedControl")
			.finish_non_exhaustive()
	}
}
