//! Exact Fleet root, physical role and native scalar key associations without admission.

use crate::fleet::scalar::NativeFleetScalarResource;
use crate::fleet::v10::source_state_keys::FleetSourceKey;
use crate::kv;
use crate::records::RecordIdentity;

use super::{
	InstallationAdmissionBindingError, PgInstallationAdmissionBinding, PgRelocatingPointStorage,
	PgV10SourceRole, RelocatingPointBinding,
};

impl PgInstallationAdmissionBinding {
	pub(crate) fn classify_fleet_source_key(
		&self,
		key: kv::Key,
	) -> Result<FleetSourceKey, InstallationAdmissionBindingError> {
		let Self::Imported { source, .. } = self else {
			return Err(InstallationAdmissionBindingError::SourceMappingMismatch {
				role: "Fleet scalar",
			});
		};
		FleetSourceKey::parse(source.fleet_root(), key)
			.map_err(|error| InstallationAdmissionBindingError::FleetSourceKey(Box::new(error)))
	}
	pub(crate) fn scalar_source_binding(
		&self,
		key: &kv::Key,
	) -> Result<(FleetSourceKey, RelocatingPointBinding), InstallationAdmissionBindingError> {
		let parsed = self.classify_fleet_source_key(key.clone())?;
		let native = match self {
			Self::Imported { native, .. } => native,
			Self::Fresh { .. } => unreachable!("classification required an imported association"),
		};
		let placement = native.fleet_scalar();
		let resource = NativeFleetScalarResource::from_source(parsed.kind()).ok_or(
			InstallationAdmissionBindingError::SourceMappingMismatch {
				role: "Fleet scalar",
			},
		)?;
		let namespace = match &resource {
			NativeFleetScalarResource::Once(_) => placement.once_completion(),
			NativeFleetScalarResource::CacheEpoch(_) => placement.cache_epoch(),
			NativeFleetScalarResource::TopicSequence(_) => placement.topic_sequence(),
			NativeFleetScalarResource::SubscriptionCursor { .. } => placement.subscription_cursor(),
		};
		let binding = RelocatingPointBinding::new(
			key.clone(),
			RecordIdentity::new(namespace.clone(), resource.record_key()),
		);
		Ok((parsed, binding))
	}
	/// Startup and each admitted operation use this same actual relation-pair kernel.
	pub(crate) fn verify_fleet_scalar_source_pair(
		&self,
		source_storage: &PgRelocatingPointStorage,
	) -> Result<(), InstallationAdmissionBindingError> {
		let Self::Imported { source, native } = self else {
			return Err(InstallationAdmissionBindingError::SourceMappingMismatch {
				role: "Fleet scalar",
			});
		};
		let placement = native.fleet_scalar();
		if source_storage.source_table() != source.table(PgV10SourceRole::FleetState)
			|| !source_storage
				.native_storage()
				.shares_container(placement.storage())
		{
			return Err(InstallationAdmissionBindingError::SourceMappingMismatch {
				role: "Fleet scalar",
			});
		}
		Ok(())
	}
	pub(crate) fn verify_fleet_scalar_source_storage(
		&self,
		source_storage: &PgRelocatingPointStorage,
		binding: &RelocatingPointBinding,
	) -> Result<FleetSourceKey, InstallationAdmissionBindingError> {
		self.verify_fleet_scalar_source_pair(source_storage)?;
		let (parsed, expected) = self.scalar_source_binding(binding.source_key())?;
		if expected.identity() != binding.identity() {
			return Err(InstallationAdmissionBindingError::SourceMappingMismatch {
				role: "Fleet scalar",
			});
		}
		Ok(parsed)
	}
}
