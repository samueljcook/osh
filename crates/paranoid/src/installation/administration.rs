//! Complete actual PostgreSQL administrative code, without transaction or policy authority.

use std::sync::Arc;

use crate::installation::contract::{
	BoundComponentContract, ComponentDeclaration, ComponentPolicy, StateContractError,
};
use crate::installation::control_state::{InstallationControlComponent, InstallationControlError};
use crate::runtime::participation::state::{
	ParticipantSchema, ParticipantStateError, ParticipationSemantics,
};

use super::creation::{
	PgBackendRegistrationBindingError, PgBackendRegistrationSchema, PgNativeStorageBindings,
};

pub(crate) struct PgNativeAdministrationFields {
	pub(crate) participant: Arc<ParticipantSchema>,
	pub(crate) control: Arc<InstallationControlComponent>,
	pub(crate) native: Arc<PgNativeStorageBindings>,
}

pub(crate) struct PgInitialAdministrationPolicyFields<'a> {
	pub(crate) participant: &'a ParticipantSchema,
	pub(crate) control: &'a InstallationControlComponent,
	pub(crate) backend: &'a PgBackendRegistrationSchema,
}

/// All runtime/controller clones borrow these same actual codec registry instances.
pub(crate) struct PgNativeAdministration {
	participant: Arc<ParticipantSchema>,
	control: Arc<InstallationControlComponent>,
	native: Arc<PgNativeStorageBindings>,
}

impl PgNativeAdministration {
	pub(crate) fn new(
		fields: PgNativeAdministrationFields,
	) -> Result<Self, NativeAdministrationError> {
		if fields.control.identity() != fields.native.control().identity() {
			return Err(NativeAdministrationError::ControlBindingMismatch);
		}
		Ok(Self {
			participant: fields.participant,
			control: fields.control,
			native: fields.native,
		})
	}
	pub(crate) fn participant_schema(&self) -> &Arc<ParticipantSchema> {
		&self.participant
	}
	pub(crate) fn control_component(&self) -> &Arc<InstallationControlComponent> {
		&self.control
	}
	pub(crate) fn backend_schema(&self) -> &PgBackendRegistrationSchema {
		self.native.backend_schema()
	}
	pub(crate) fn native_bindings(&self) -> &Arc<PgNativeStorageBindings> {
		&self.native
	}
	pub(crate) fn declaration(&self) -> ComponentDeclaration<'_, ParticipationSemantics> {
		Self::declaration_from_schemas(&self.participant, &self.control, self.backend_schema())
	}
	pub(in crate::installation) fn declaration_from_schemas<'a>(
		participant: &'a ParticipantSchema,
		control: &'a InstallationControlComponent,
		backend: &'a PgBackendRegistrationSchema,
	) -> ComponentDeclaration<'a, ParticipationSemantics> {
		let mut fields = participant.declaration_fields(control);
		fields.fixed_schemas.push(backend.schema().capabilities());
		ComponentDeclaration::new(fields).expect(
			"actual PostgreSQL administration declares distinct participant/control/backend schemas",
		)
	}

	/// Derives complete initial administrative metadata from the actual three codec owners.
	pub(crate) fn initial_policy_from_schemas(
		fields: PgInitialAdministrationPolicyFields<'_>,
	) -> Result<ComponentPolicy, NativeAdministrationError> {
		let mut policy_fields = fields.participant.initial_policy_fields(fields.control)?;
		policy_fields
			.schemas
			.push(fields.backend.initial_schema_policy()?);
		let policy = ComponentPolicy::new(policy_fields)?;
		let declaration =
			Self::declaration_from_schemas(fields.participant, fields.control, fields.backend);
		let bound = declaration.bind(&policy)?;
		require_selected_writers(PgAdministrativeWriterFields {
			participant: fields.participant,
			control: fields.control,
			backend: fields.backend,
			bound: &bound,
		})?;
		Ok(policy)
	}

	pub(crate) fn require_bound_policy(
		&self,
		bound: &BoundComponentContract<'_, '_, ParticipationSemantics>,
	) -> Result<(), NativeAdministrationError> {
		require_selected_writers(PgAdministrativeWriterFields {
			participant: &self.participant,
			control: &self.control,
			backend: self.backend_schema(),
			bound,
		})
	}
}

struct PgAdministrativeWriterFields<'a, 'policy, 'declaration> {
	participant: &'a ParticipantSchema,
	control: &'a InstallationControlComponent,
	backend: &'a PgBackendRegistrationSchema,
	bound: &'a BoundComponentContract<'policy, 'declaration, ParticipationSemantics>,
}

fn require_selected_writers(
	fields: PgAdministrativeWriterFields<'_, '_, '_>,
) -> Result<(), NativeAdministrationError> {
	fields
		.participant
		.require_bound_writer(&fields.bound.bind_schema(fields.participant.schema())?)?;
	fields
		.control
		.require_bound_writer(&fields.bound.bind_schema(fields.control.schema())?)?;
	fields
		.backend
		.require_bound_writer(&fields.bound.bind_schema(fields.backend.schema())?)?;
	Ok(())
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeAdministrationError {
	#[error("administration control identity differs from native placement")]
	ControlBindingMismatch,
	#[error("administration declaration binding failed")]
	Contract(#[from] StateContractError),
	#[error("administration selected participant writer failed")]
	Participant(#[from] ParticipantStateError),
	#[error("administration selected control writer failed")]
	Control(#[from] InstallationControlError),
	#[error("administration selected backend writer failed")]
	Backend(#[from] PgBackendRegistrationBindingError),
}
