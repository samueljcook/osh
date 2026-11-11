//! Actual immutable backend mapping and transaction-observed source routing.
//!
//! Imported transactions pin the source gate before native control. Routing is selected
//! only after the control reader and exact recorded backend registration are checked.

use std::sync::Arc;

use crate::fleet::CounterKey;
use crate::installation::contract::BoundStateSchema;
use crate::installation::control_state::InstallationControlState;
use crate::records::RecordNamespace;

use crate::db::{DatabaseError, SchemaError, TransactionFailure, Tx};
use crate::installation::control_storage::PgControlStorage;
use crate::installation::creation::{
	ImportedPhase, NativeInstallationError, PgBackendRegistration, PgBackendRegistrationCodecError,
	PgNativeStorageBindings, inspect_descriptor_imported_registration, verify_fresh_registration,
};
use crate::installation::v10::source_gate::NativeSourceGatePhase;
use crate::installation::v10::{PgV10SourceDescriptor, PgV10SourceRole, V10SourceBindingError};
use crate::records::postgres::PgPointStorage;
use crate::records::postgres::v10::relocating_point::{
	PgRelocatingPointStorage, RelocatingPointBinding,
};

mod fleet_scalar;

/// Immutable associations shared by ordinary admission and administrative participation.
pub(crate) enum PgInstallationAdmissionBinding {
	Fresh {
		native: Arc<PgNativeStorageBindings>,
	},
	Imported {
		native: Arc<PgNativeStorageBindings>,
		source: Arc<PgV10SourceDescriptor>,
	},
}

pub(crate) use crate::installation::control_state::InstallationStorageRoute;

/// Parsed diagnostics from one supplied control observation, without transaction authority.
pub(crate) struct InstallationControlRegistration {
	route: InstallationStorageRoute,
	backend: PgBackendRegistration,
}

impl InstallationControlRegistration {
	pub(crate) fn route(&self) -> InstallationStorageRoute {
		self.route
	}
	pub(crate) fn into_parts(self) -> (InstallationStorageRoute, PgBackendRegistration) {
		(self.route, self.backend)
	}
}

impl PgInstallationAdmissionBinding {
	pub(crate) fn queue_storage(&self) -> &crate::installation::creation::PgNativeQueueStorage {
		let native = match self {
			Self::Fresh { native } | Self::Imported { native, .. } => native,
		};
		native.queue()
	}

	pub(crate) fn check_scope_policies(
		&self,
		policies: &crate::scope::ScopePolicySet,
	) -> Result<(), InstallationAdmissionBindingError> {
		let native = match self {
			Self::Fresh { native } | Self::Imported { native, .. } => native,
		};
		Ok(native.check_scope_policies(policies)?)
	}

	/// Resolves immutable native placement without granting an application scope role.
	pub(crate) fn app_storage(
		&self,
		scope: &crate::scope::ScopeIdentity,
	) -> Result<PgPointStorage, InstallationAdmissionBindingError> {
		let native = match self {
			Self::Fresh { native } | Self::Imported { native, .. } => native,
		};
		Ok(native.app_storage(scope)?.clone())
	}

	pub(crate) fn verify_lease_source_storage(
		&self,
		adapter: &crate::fleet::lease::v10::source_store::PgLeaseSourceStorage,
		namespace: &RecordNamespace,
	) -> Result<(), InstallationAdmissionBindingError> {
		let Self::Imported { native, source } = self else {
			return Err(InstallationAdmissionBindingError::SourceMappingMismatch { role: "lease" });
		};
		if adapter.claim_table() != source.table(PgV10SourceRole::FleetClaims)
			|| adapter.fencing_counter_table() != source.table(PgV10SourceRole::FleetFencing)
			|| !adapter
				.native_storage()
				.shares_container(native.leases().storage())
			|| adapter.namespace() != native.leases().namespace()
			|| namespace != native.leases().namespace()
		{
			return Err(InstallationAdmissionBindingError::SourceMappingMismatch { role: "lease" });
		}
		Ok(())
	}

	/// Checks an adapter against actual imported role associations without granting admission.
	pub(crate) fn verify_raw_source_storage(
		&self,
		adapter: &PgRelocatingPointStorage,
		namespace: &RecordNamespace,
	) -> Result<(), InstallationAdmissionBindingError> {
		let Self::Imported { native, source } = self else {
			return Err(InstallationAdmissionBindingError::SourceMappingMismatch {
				role: "raw KV",
			});
		};
		if adapter.source_table() != source.table(PgV10SourceRole::RawKv)
			|| !adapter
				.native_storage()
				.shares_container(native.raw_kv().storage())
			|| namespace != native.raw_kv().namespace()
		{
			return Err(InstallationAdmissionBindingError::SourceMappingMismatch {
				role: "raw KV",
			});
		}
		Ok(())
	}

	/// Derives a source key only for an operation that needs the installed source representation.
	pub(crate) fn counter_source_binding(
		&self,
		identity: &crate::records::RecordIdentity,
	) -> Result<RelocatingPointBinding, InstallationAdmissionBindingError> {
		let Self::Imported { native, source } = self else {
			return Err(InstallationAdmissionBindingError::SourceMappingMismatch {
				role: "Counter",
			});
		};
		let text = std::str::from_utf8(identity.key().as_bytes())
			.map_err(|_| InstallationAdmissionBindingError::SourceCounterKeyInvalid)?;
		let key = CounterKey::new(text).map_err(V10SourceBindingError::Key)?;
		if identity.namespace() != native.counters().namespace() {
			return Err(InstallationAdmissionBindingError::SourceMappingMismatch {
				role: "Counter",
			});
		}
		Ok(RelocatingPointBinding::new(
			source.counter_source_key(&key)?,
			identity.clone(),
		))
	}

	/// Uses the actual source Counter key derivation as well as both physical role associations.
	pub(crate) fn verify_counter_source_storage(
		&self,
		adapter: &PgRelocatingPointStorage,
		binding: &RelocatingPointBinding,
	) -> Result<(), InstallationAdmissionBindingError> {
		let Self::Imported { native, source } = self else {
			return Err(InstallationAdmissionBindingError::SourceMappingMismatch {
				role: "Counter",
			});
		};
		if adapter.source_table() != source.table(PgV10SourceRole::FleetState)
			|| !adapter
				.native_storage()
				.shares_container(native.counters().storage())
		{
			return Err(InstallationAdmissionBindingError::SourceMappingMismatch {
				role: "Counter",
			});
		}
		let expected = self.counter_source_binding(binding.identity())?;
		if expected.identity() != binding.identity()
			|| expected.source_key() != binding.source_key()
		{
			return Err(InstallationAdmissionBindingError::SourceMappingMismatch {
				role: "Counter",
			});
		}
		Ok(())
	}

	pub(crate) fn matches_control_storage(&self, storage: &PgControlStorage) -> bool {
		let native = match self {
			Self::Fresh { native } | Self::Imported { native, .. } => native,
		};
		native.control().matches_storage(storage)
	}

	pub(crate) fn matches_participant_storage(
		&self,
		storage: &PgPointStorage,
		namespace: &RecordNamespace,
	) -> bool {
		let native = match self {
			Self::Fresh { native } | Self::Imported { native, .. } => native,
		};
		native.participants().storage().shares_container(storage)
			&& native.participants().namespace() == namespace
	}

	pub(crate) async fn pin_source(
		&self,
		tx: &mut Tx<'_>,
	) -> Result<Option<NativeSourceGatePhase>, InstallationAdmissionBindingError> {
		match self {
			Self::Fresh { .. } => Ok(None),
			Self::Imported { source, .. } => source
				.gate()
				.pin_shared(tx)
				.await
				.map(Some)
				.map_err(InstallationAdmissionBindingError::SourceGate),
		}
	}

	/// The caller must authenticate the observed control's exact retained reader first.
	/// This checks origin/mapping/phase before any participant or ordinary state access.
	pub(crate) fn verify_control(
		&self,
		control: &InstallationControlState,
		source_phase: Option<NativeSourceGatePhase>,
		backend: &BoundStateSchema<'_, PgBackendRegistration, PgBackendRegistrationCodecError>,
	) -> Result<InstallationStorageRoute, InstallationAdmissionBindingError> {
		Ok(self
			.verify_control_with_registration(control, source_phase, backend)?
			.route())
	}

	/// Retains the actual bound backend decode after the same origin, mapping and phase checks.
	pub(crate) fn verify_control_with_registration(
		&self,
		control: &InstallationControlState,
		source_phase: Option<NativeSourceGatePhase>,
		backend: &BoundStateSchema<'_, PgBackendRegistration, PgBackendRegistrationCodecError>,
	) -> Result<InstallationControlRegistration, InstallationAdmissionBindingError> {
		match self {
			Self::Fresh { native } => {
				if source_phase.is_some() {
					return Err(InstallationAdmissionBindingError::UnexpectedSourceObservation);
				}
				let backend = verify_fresh_registration(control, native, backend)?;
				Ok(InstallationControlRegistration {
					route: InstallationStorageRoute::NativeFresh,
					backend,
				})
			}
			Self::Imported { native, source } => {
				let source_phase = source_phase
					.ok_or(InstallationAdmissionBindingError::SourceObservationAbsent)?;
				let (backend, phase) = inspect_descriptor_imported_registration(
					control,
					native,
					source,
					source_phase,
					backend,
				)?;
				let route = match phase {
					ImportedPhase::Coexisting => InstallationStorageRoute::SourceCoexistence,
					ImportedPhase::SourceWritersRetired => {
						InstallationStorageRoute::SourceRelocation
					}
					ImportedPhase::SourceDataRetired => InstallationStorageRoute::NativeImported,
				};
				Ok(InstallationControlRegistration { route, backend })
			}
		}
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum InstallationAdmissionBindingError {
	#[error("source {role} adapter or key mapping differs from installation association")]
	SourceMappingMismatch { role: &'static str },
	#[error("source Counter logical key is not valid UTF-8")]
	SourceCounterKeyInvalid,
	#[error("source Fleet key does not match the recorded root and supported key shapes")]
	FleetSourceKey(#[source] Box<crate::fleet::v10::source_state_keys::FleetSourceKeyError>),
	#[error("source logical key cannot be derived from installation mapping")]
	SourceKeyMapping(#[from] V10SourceBindingError),
	#[error("imported installation source gate could not be pinned")]
	SourceGate(#[source] SchemaError),
	#[error("installation backend registration does not match runtime mapping")]
	Registration(#[from] NativeInstallationError),
	#[error("imported installation has no source-gate observation")]
	SourceObservationAbsent,
	#[error("fresh installation received a source-gate observation")]
	UnexpectedSourceObservation,
}

impl InstallationAdmissionBindingError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::SourceGate(error) => error.database_error(),
			Self::Registration(error) => error.database_error(),
			Self::SourceObservationAbsent
			| Self::SourceMappingMismatch { .. }
			| Self::SourceCounterKeyInvalid
			| Self::FleetSourceKey(_)
			| Self::SourceKeyMapping(_)
			| Self::UnexpectedSourceObservation => None,
		}
	}

	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::SourceGate(error) => error.involves_database_failure(),
			Self::Registration(error) => error.involves_database_failure(),
			Self::SourceObservationAbsent
			| Self::UnexpectedSourceObservation
			| Self::SourceMappingMismatch { .. }
			| Self::SourceCounterKeyInvalid
			| Self::FleetSourceKey(_)
			| Self::SourceKeyMapping(_) => false,
		}
	}

	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::SourceGate(error) => error.commit_outcome_is_unknown(),
			Self::Registration(error) => error.commit_outcome_is_unknown(),
			Self::SourceObservationAbsent
			| Self::UnexpectedSourceObservation
			| Self::SourceMappingMismatch { .. }
			| Self::SourceCounterKeyInvalid
			| Self::FleetSourceKey(_)
			| Self::SourceKeyMapping(_) => false,
		}
	}
}
