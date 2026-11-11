//! Closed ordinary and fixed-upgrade origins share actual source placement checks.

use crate::installation::creation::PgNativeQueueStorage;
use crate::installation::v10::PgV10SourceDescriptor;
use crate::runtime::admission::bindings::InstallationStorageRoute;
use crate::runtime::admission::queue::NativeQueueSourceBindingError;
use crate::runtime::admission::upgrade::contracts::NativeUpgradeContracts;
use crate::runtime::admission::{NativeAdmissionError, NativeAdmittedContracts};

/// Neither variant supplies transaction ownership or independently transferable authority.
#[derive(Clone, Copy)]
pub(crate) enum QueueSourceContext<'context, 'current> {
	Ordinary(&'context NativeAdmittedContracts),
	Upgrade(&'context NativeUpgradeContracts<'current>),
}

impl QueueSourceContext<'_, '_> {
	pub(in crate::queue) fn require_queue_source_storage(
		self,
		descriptor: &PgV10SourceDescriptor,
		storage: &PgNativeQueueStorage,
	) -> Result<(), NativeQueueSourceBindingError> {
		let route = match self {
			Self::Ordinary(contracts) => contracts.storage_route(),
			Self::Upgrade(contracts) => contracts.storage_route(),
		};
		if route != InstallationStorageRoute::SourceRelocation {
			return Err(NativeAdmissionError::StorageRouteUnsupported { route }.into());
		}
		match self {
			Self::Ordinary(contracts) => {
				contracts.require_queue_source_storage(descriptor, storage)
			}
			Self::Upgrade(contracts) => contracts.require_queue_source_storage(descriptor, storage),
		}
	}
}
