//! Exact immutable Queue placement binding, independent of route and transaction admission.

use super::{
	InstallationStorageRoute, NativeAdmissionError, NativeAdmittedContracts,
	PgInstallationAdmissionBinding,
};
use crate::installation::creation::PgNativeQueueStorage;
use crate::installation::v10::PgV10SourceDescriptor;

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeQueuePlacementError {
	#[error("native Queue placement differs from the installation mapping")]
	Mismatch,
	#[error("source Queue descriptor differs from the imported installation")]
	SourceMismatch,
}

pub(super) fn verify_native_queue_storage(
	binding: &PgInstallationAdmissionBinding,
	storage: &PgNativeQueueStorage,
) -> Result<(), NativeQueuePlacementError> {
	let native = match binding {
		PgInstallationAdmissionBinding::Fresh { native }
		| PgInstallationAdmissionBinding::Imported { native, .. } => native,
	};
	let actual = native.queue();
	if !storage.storage().shares_container(actual.storage())
		|| storage.jobs() != actual.jobs()
		|| storage.active_dedupe() != actual.active_dedupe()
		|| storage.dead_letters() != actual.dead_letters()
		|| storage.dead_letter_originals() != actual.dead_letter_originals()
		|| storage.pauses() != actual.pauses()
	{
		return Err(NativeQueuePlacementError::Mismatch);
	}
	Ok(())
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeQueueSourceBindingError {
	#[error("source Queue observation route was rejected")]
	Admission(#[from] NativeAdmissionError),
	#[error("source Queue placement was rejected")]
	Placement(#[from] NativeQueuePlacementError),
}

impl NativeAdmittedContracts {
	/// Checks this actual observation's route and origin before any source Queue IO.
	/// This check does not transfer transaction locks or admit another transaction.
	pub(crate) fn require_queue_source_storage(
		&self,
		descriptor: &PgV10SourceDescriptor,
		storage: &PgNativeQueueStorage,
	) -> Result<(), NativeQueueSourceBindingError> {
		let route = self.storage_route();
		if route != InstallationStorageRoute::SourceRelocation {
			return Err(NativeAdmissionError::StorageRouteUnsupported { route }.into());
		}
		verify_queue_source_storage(
			self.observation.installation_binding.as_ref(),
			descriptor,
			storage,
		)?;
		Ok(())
	}
}

fn verify_queue_source_storage(
	binding: &PgInstallationAdmissionBinding,
	descriptor: &PgV10SourceDescriptor,
	storage: &PgNativeQueueStorage,
) -> Result<(), NativeQueuePlacementError> {
	verify_native_queue_storage(binding, storage)?;
	let PgInstallationAdmissionBinding::Imported { source, .. } = binding else {
		return Err(NativeQueuePlacementError::SourceMismatch);
	};
	if !source.matches_descriptor(descriptor) {
		return Err(NativeQueuePlacementError::SourceMismatch);
	}
	Ok(())
}
impl PgInstallationAdmissionBinding {
	/// Immutable full source/native placement check; this does not observe route or admit IO.
	pub(crate) fn verify_queue_source_storage(
		&self,
		descriptor: &PgV10SourceDescriptor,
		storage: &PgNativeQueueStorage,
	) -> Result<(), NativeQueuePlacementError> {
		verify_queue_source_storage(self, descriptor, storage)
	}
}
