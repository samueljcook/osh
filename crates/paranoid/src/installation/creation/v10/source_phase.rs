//! Fixed imported phase preparation through the actual complete backend schema.
//!
//! These pure serializers supply no source exclusion, emptiness, native consistency,
//! participant coverage or control mutation authority. The owning executor must establish
//! those facts and atomically write the resulting registration with exact Work and generation.

use super::super::{
	BackendOrigin, BoundStateSchema, ImportedPhase, InstallationControlState,
	NativeInstallationError, NativeMappingOrigin, PgBackendRegistration,
	PgBackendRegistrationCodecError, PgNativeStorageBindings, PgV10SourceDescriptor,
	PreparedPgBackendRegistration,
};

/// Encodes only the writer-retired phase of this exact recorded imported association.
/// The owning executor separately retains actual exclusive source and control authority.
pub(crate) fn source_writers_retired_registration(
	current: &InstallationControlState,
	native: &PgNativeStorageBindings,
	source: &PgV10SourceDescriptor,
	backend: &BoundStateSchema<'_, PgBackendRegistration, PgBackendRegistrationCodecError>,
	registration: PgBackendRegistration,
) -> Result<PreparedPgBackendRegistration, NativeInstallationError> {
	prepare_registration(
		current,
		native,
		source,
		backend,
		registration,
		PhaseTransition::WritersRetired,
	)
}

/// Encodes only SourceWritersRetired to SourceDataRetired, preserving the whole parsed ledger.
/// Physical source emptiness and native consistency remain owning executor prerequisites.
pub(crate) fn source_data_retired_registration(
	current: &InstallationControlState,
	native: &PgNativeStorageBindings,
	source: &PgV10SourceDescriptor,
	backend: &BoundStateSchema<'_, PgBackendRegistration, PgBackendRegistrationCodecError>,
	registration: PgBackendRegistration,
) -> Result<PreparedPgBackendRegistration, NativeInstallationError> {
	prepare_registration(
		current,
		native,
		source,
		backend,
		registration,
		PhaseTransition::DataRetired,
	)
}

enum PhaseTransition {
	WritersRetired,
	DataRetired,
}

impl PhaseTransition {
	fn prepare(self, recorded: ImportedPhase) -> Result<ImportedPhase, NativeInstallationError> {
		match self {
			Self::WritersRetired if recorded == ImportedPhase::Coexisting => {
				Ok(ImportedPhase::SourceWritersRetired)
			}
			Self::WritersRetired => {
				Err(NativeInstallationError::SourceWriterRetirementPhase { recorded })
			}
			Self::DataRetired if recorded == ImportedPhase::SourceWritersRetired => {
				Ok(ImportedPhase::SourceDataRetired)
			}
			Self::DataRetired => {
				Err(NativeInstallationError::SourceDataRetirementPhase { recorded })
			}
		}
	}
}

fn prepare_registration(
	current: &InstallationControlState,
	native: &PgNativeStorageBindings,
	source: &PgV10SourceDescriptor,
	backend: &BoundStateSchema<'_, PgBackendRegistration, PgBackendRegistrationCodecError>,
	mut registration: PgBackendRegistration,
	transition: PhaseTransition,
) -> Result<PreparedPgBackendRegistration, NativeInstallationError> {
	if registration.fingerprint != native.fingerprint(NativeMappingOrigin::ImportedV10) {
		return Err(NativeInstallationError::BindingFingerprintMismatch);
	}
	native.require_fleet_scalar_registration(&registration)?;
	native.check_scope_policies(current.scope_policies())?;
	native.check_document_index_associations(&registration, current.scope_policies())?;
	let BackendOrigin::ImportedV10 { descriptor, phase } = &mut registration.origin else {
		return Err(NativeInstallationError::WrongOrigin);
	};
	descriptor.check_native_separation(native)?;
	let next_phase = transition.prepare(*phase)?;
	if !source.matches_descriptor(descriptor) {
		return Err(NativeInstallationError::SourceBindingFingerprintMismatch);
	}
	*phase = next_phase;
	let encoded = native
		.backend_schema
		.encode_bound_document(&registration, backend)?;
	Ok(PreparedPgBackendRegistration {
		encoded,
		registration,
	})
}
