//! Owned native participation transactions on an explicitly configured control write pool.
//!
//! Registration becomes externally usable only after a known successful commit. These
//! operations do not start renewal tasks or establish runtime shutdown ordering; callers must
//! drain transaction permits and managed work before unregistering a runtime's participation.

use std::{fmt, num::NonZeroU32, sync::Arc};

use crate::installation::contract::{ContractStateReadError, StateContractError};
use crate::installation::control_state::{
	DecodedInstallationControl, InstallationControlError, InstallationControlState,
};
use crate::records::state_schema::StateSchemaError;
use crate::records::{
	PointRecordLock, RecordIdentity, RecordKey, RecordNamespace, StateTimestampMicros,
};
use crate::runtime::participation::state::catalog::{
	ParticipantSupportCatalog, ParticipantSupportCatalogFields, ParticipantUseReference,
};
use crate::runtime::participation::state::{
	ParticipantAuthority, ParticipantCapabilities, ParticipantComponent,
	ParticipantComponentFields, ParticipantComponentSupport, ParticipantState,
	ParticipantStateError, ParticipationDuration,
};
use crate::scope::payload::PayloadRoleSupport;
use crate::scope::{DocumentRoleSupport, PrimitiveScopeSupport, RawRoleSupport};

use crate::db::{DatabaseError, WritePool, WriteTx};
use crate::installation::administration::{NativeAdministrationError, PgNativeAdministration};
use crate::installation::control_storage::{ControlPinError, PgControlStorage};
use crate::installation::v10::source_gate::NativeSourceGatePhase;
use crate::records::postgres::{PgPointStorage, StateStorageError};
use crate::runtime::admission::RetiredSourcePhase;
use crate::runtime::admission::bindings::{
	InstallationAdmissionBindingError, PgInstallationAdmissionBinding,
};

mod v10;

/// Explicit native storage bindings and control credentials, independent of an application pool.
pub(crate) struct PgNativeParticipationConfig {
	pub(crate) installation_binding: Arc<PgInstallationAdmissionBinding>,
	pub(crate) retired_source_phase: Arc<RetiredSourcePhase>,
	pub(crate) control_pool: WritePool,
	pub(crate) control_storage: PgControlStorage,
	pub(crate) administration: Arc<PgNativeAdministration>,
	pub(crate) participant_storage: PgPointStorage,
	pub(crate) participant_namespace: RecordNamespace,
	pub(crate) conflict_attempts: NonZeroU32,
}

/// Administrative participation operations with their own commit boundary.
pub(crate) struct PgNativeParticipation {
	config: PgNativeParticipationConfig,
}

/// Core/component support derived from actual declarations and an explicit participation duration.
/// Administrative participation support is derived and added by the backend itself.
pub(crate) struct NativeRegistrationOptions {
	pub(crate) component_support: Vec<ParticipantComponentSupport>,
	pub(crate) document_roles: Vec<DocumentRoleSupport>,
	pub(crate) raw_roles: Vec<RawRoleSupport>,
	pub(crate) primitive_scopes: Vec<PrimitiveScopeSupport>,
	pub(crate) payload_roles: Vec<PayloadRoleSupport>,
	/// The optional initial uses. They never select or repeat the backend's mandatory
	/// administration.
	pub(crate) optional_uses: Vec<ParticipantUseReference>,
	pub(crate) duration: ParticipationDuration,
}

/// Private registration material published only after the backend knows registration committed.
pub(crate) struct CommittedParticipation {
	authority: ParticipantAuthority,
	component: ParticipantComponent,
}

impl CommittedParticipation {
	pub(crate) fn authority(&self) -> &ParticipantAuthority {
		&self.authority
	}

	pub(crate) fn component(&self) -> &ParticipantComponent {
		&self.component
	}
}

impl fmt::Debug for CommittedParticipation {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("CommittedParticipation")
			.finish_non_exhaustive()
	}
}

/// Pending private material cannot leave the backend before transaction completion succeeds.
struct PendingRegistration {
	authority: ParticipantAuthority,
	component: ParticipantComponent,
}

struct ParticipationControlObservation {
	decoded: DecodedInstallationControl,
	source_phase: Option<NativeSourceGatePhase>,
}

impl ParticipationControlObservation {
	fn state(&self) -> &InstallationControlState {
		self.decoded.state()
	}
}

impl PgNativeParticipation {
	/// Binds already-validated native containers without registering or authenticating a runtime.
	pub(crate) fn new(
		config: PgNativeParticipationConfig,
	) -> Result<Self, NativeParticipationError> {
		if config.control_storage.identity() != config.administration.control_component().identity()
			|| !config
				.installation_binding
				.matches_control_storage(&config.control_storage)
		{
			return Err(NativeParticipationError::ControlBindingMismatch);
		}
		if !config
			.installation_binding
			.matches_participant_storage(&config.participant_storage, &config.participant_namespace)
		{
			return Err(NativeParticipationError::ParticipantStorageBindingMismatch);
		}
		let native = match config.installation_binding.as_ref() {
			PgInstallationAdmissionBinding::Fresh { native }
			| PgInstallationAdmissionBinding::Imported { native, .. } => native,
		};
		if !Arc::ptr_eq(native, config.administration.native_bindings()) {
			return Err(NativeParticipationError::AdministrationBindingMismatch);
		}
		Ok(Self { config })
	}

	/// Generates fresh private authority and writes only an absent participant value.
	/// An existing value is rejected even when its embedded participation has expired.
	pub(crate) async fn register(
		&self,
		options: NativeRegistrationOptions,
	) -> Result<CommittedParticipation, NativeParticipationError> {
		let mut tx = self.begin("db.native_participation.register").await?;
		let outcome = self.register_in_transaction(&mut tx, options).await;
		let pending = finish_transaction(tx, outcome).await?;
		Ok(CommittedParticipation {
			authority: pending.authority,
			component: pending.component,
		})
	}

	/// Renews only matching live private authority using database time observed after point
	/// locking. Selected catalog semantics/writer withdraw only the proved obsolete source bridge
	/// uses. Other support and private material remain intact; this operation never changes
	/// registration ID.
	pub(crate) async fn renew(
		&self,
		registration: &CommittedParticipation,
		duration: ParticipationDuration,
	) -> Result<(), NativeParticipationError> {
		let mut tx = self.begin("db.native_participation.renew").await?;
		let outcome = self
			.renew_in_transaction(&mut tx, registration, duration)
			.await;
		finish_transaction(tx, outcome).await?;
		Ok(())
	}

	/// Removes matching private authority, including an expired registration, without renewal.
	/// Absence returns false. Callers must drain permits and managed work before invoking removal.
	pub(crate) async fn unregister(
		&self,
		registration: &CommittedParticipation,
	) -> Result<bool, NativeParticipationError> {
		let mut tx = self.begin("db.native_participation.unregister").await?;
		let outcome = self.unregister_in_transaction(&mut tx, registration).await;
		finish_transaction(tx, outcome).await
	}

	async fn begin(
		&self,
		transaction_label: &'static str,
	) -> Result<WriteTx<'_>, NativeParticipationError> {
		self.config
			.control_pool
			.begin_labeled_transaction(transaction_label)
			.await
			.map_err(NativeParticipationError::Begin)
	}

	async fn read_control(
		&self,
		tx: &mut WriteTx<'_>,
	) -> Result<ParticipationControlObservation, NativeParticipationError> {
		let source_phase = self
			.config
			.retired_source_phase
			.pin_source(&self.config.installation_binding, tx)
			.await?;
		let snapshot = self.config.control_storage.read_pinned_shared(tx).await?;
		let decoded = self
			.config
			.administration
			.control_component()
			.decode_record(self.config.control_storage.identity(), &snapshot)?
			.ok_or(NativeParticipationError::ControlAbsent)?;
		Ok(ParticipationControlObservation {
			decoded,
			source_phase,
		})
	}

	fn participant_component(&self, authority: &ParticipantAuthority) -> ParticipantComponent {
		ParticipantComponent::new(
			ParticipantComponentFields {
				identity: self.participant_identity(authority),
				installation_id: authority.installation_id(),
				registration_id: authority.registration_id(),
			},
			Arc::clone(self.config.administration.participant_schema()),
		)
	}

	fn participant_identity(&self, authority: &ParticipantAuthority) -> RecordIdentity {
		RecordIdentity::new(
			self.config.participant_namespace.clone(),
			RecordKey::new(authority.registration_id().as_bytes().to_vec()),
		)
	}

	fn check_registration_binding(
		&self,
		control: &InstallationControlState,
		registration: &CommittedParticipation,
	) -> Result<(), NativeParticipationError> {
		if registration.authority.installation_id() != control.installation_id() {
			return Err(NativeParticipationError::WrongInstallation);
		}
		if registration.component.identity() != &self.participant_identity(&registration.authority)
			|| !Arc::ptr_eq(
				registration.component.participant_schema(),
				self.config.administration.participant_schema(),
			) {
			return Err(NativeParticipationError::ParticipantBindingMismatch);
		}
		Ok(())
	}

	async fn register_in_transaction(
		&self,
		tx: &mut WriteTx<'_>,
		options: NativeRegistrationOptions,
	) -> Result<PendingRegistration, NativeParticipationError> {
		let decoded_control = self.read_control(tx).await?;
		let control = decoded_control.state();
		let authority = ParticipantAuthority::generate(control.installation_id())
			.map_err(NativeParticipationError::Random)?;
		let component = self.participant_component(&authority);
		let declaration = self.config.administration.declaration();
		let policy = control
			.component(declaration.identifier())
			.ok_or(NativeParticipationError::ParticipationNotInstalled)?;
		let binding = declaration.bind(policy)?;
		self.config.administration.require_bound_policy(&binding)?;
		let control_schema =
			binding.bind_schema(self.config.administration.control_component().schema())?;
		self.config
			.administration
			.control_component()
			.require_bound_reader(&decoded_control.decoded, &control_schema)?;
		self.config
			.administration
			.control_component()
			.require_bound_writer(&control_schema)?;
		let route = self.config.installation_binding.verify_control(
			control,
			decoded_control.source_phase,
			&binding.bind_schema(self.config.administration.backend_schema().schema())?,
		)?;
		self.config
			.retired_source_phase
			.record(route, decoded_control.source_phase);
		let bound_schema = binding.bind_schema(component.schema())?;
		component.require_bound_writer(&bound_schema)?;
		let writer_version = bound_schema.policy().writer().version;
		let mut support = options.component_support;
		support.push(ParticipantComponentSupport::from_declaration(&declaration));
		let mut active_uses = options.optional_uses;
		if active_uses.iter().any(|reference| matches!(reference, ParticipantUseReference::Component { identifier } if identifier == declaration.identifier())) {
			return Err(ParticipantStateError::ReservedAdministrativeUse.into());
		}
		active_uses.push(ParticipantUseReference::Component {
			identifier: declaration.identifier().to_owned(),
		});
		let mut capabilities = ParticipantCapabilities::new(
			ParticipantSupportCatalog::new(ParticipantSupportCatalogFields {
				components: support,
				document_roles: options.document_roles,
				raw_roles: options.raw_roles,
				primitive_scopes: options.primitive_scopes,
				payload_roles: options.payload_roles,
			})?,
			active_uses,
		)?;
		if route == crate::runtime::admission::bindings::InstallationStorageRoute::NativeImported {
			binding
				.transitions()
				.normalize_retired_source_capabilities(&mut capabilities, &bound_schema)?;
		}
		capabilities.check_installed_policies(control.component_policies())?;
		capabilities.check_scope_policies(control.scope_policies())?;
		let point = self
			.config
			.participant_storage
			.lock(tx, component.identity(), self.config.conflict_attempts)
			.await?;
		if point.snapshot().record().is_some() {
			return Err(NativeParticipationError::RegistrationOccupied);
		}
		let now = point.snapshot().observed_at();
		let state =
			(binding.transitions().register_fn)(&authority, capabilities, options.duration, now)?;
		check_installation(control, &state)?;
		let encoded = component
			.encode_record(&state, writer_version, now)
			.map_err(NativeParticipationError::Representation)?;
		point.write(encoded).await?;
		Ok(PendingRegistration {
			authority,
			component,
		})
	}

	async fn renew_in_transaction(
		&self,
		tx: &mut WriteTx<'_>,
		registration: &CommittedParticipation,
		duration: ParticipationDuration,
	) -> Result<StateTimestampMicros, NativeParticipationError> {
		let decoded_control = self.read_control(tx).await?;
		let control = decoded_control.state();
		self.check_registration_binding(control, registration)?;
		let component = &registration.component;
		let declaration = self.config.administration.declaration();
		let policy = control
			.component(declaration.identifier())
			.ok_or(NativeParticipationError::ParticipationNotInstalled)?;
		let binding = declaration.bind(policy)?;
		self.config.administration.require_bound_policy(&binding)?;
		let control_schema =
			binding.bind_schema(self.config.administration.control_component().schema())?;
		self.config
			.administration
			.control_component()
			.require_bound_reader(&decoded_control.decoded, &control_schema)?;
		self.config
			.administration
			.control_component()
			.require_bound_writer(&control_schema)?;
		let route = self.config.installation_binding.verify_control(
			control,
			decoded_control.source_phase,
			&binding.bind_schema(self.config.administration.backend_schema().schema())?,
		)?;
		self.config
			.retired_source_phase
			.record(route, decoded_control.source_phase);
		let bound_schema = binding.bind_schema(component.schema())?;
		component.require_bound_writer(&bound_schema)?;
		let writer_version = bound_schema.policy().writer().version;
		let point = self
			.config
			.participant_storage
			.lock(tx, component.identity(), self.config.conflict_attempts)
			.await?;
		let mut state = component
			.decode_bound_record(component.identity(), point.snapshot(), &bound_schema)?
			.ok_or(NativeParticipationError::ParticipantAbsent)?;
		check_installation(control, &state)?;
		let now = point.snapshot().observed_at();
		state.authenticate(&registration.authority, now)?;
		v10::reconcile_retired_source_uses(
			&mut state,
			&registration.authority,
			route,
			&binding,
			&bound_schema,
			now,
		)?;
		state
			.capabilities()
			.check_installed_policies(control.component_policies())?;
		state
			.capabilities()
			.check_scope_policies(control.scope_policies())?;
		(binding.transitions().renew_fn)(&mut state, &registration.authority, duration, now)?;
		let expires_at = state.expires_at();
		let encoded = component
			.encode_record(&state, writer_version, now)
			.map_err(NativeParticipationError::Representation)?;
		point.write(encoded).await?;
		Ok(expires_at)
	}

	async fn unregister_in_transaction(
		&self,
		tx: &mut WriteTx<'_>,
		registration: &CommittedParticipation,
	) -> Result<bool, NativeParticipationError> {
		let decoded_control = self.read_control(tx).await?;
		let control = decoded_control.state();
		self.check_registration_binding(control, registration)?;
		let component = &registration.component;
		let declaration = self.config.administration.declaration();
		let policy = control
			.component(declaration.identifier())
			.ok_or(NativeParticipationError::ParticipationNotInstalled)?;
		let binding = declaration.bind(policy)?;
		self.config.administration.require_bound_policy(&binding)?;
		let control_schema =
			binding.bind_schema(self.config.administration.control_component().schema())?;
		self.config
			.administration
			.control_component()
			.require_bound_reader(&decoded_control.decoded, &control_schema)?;
		self.config
			.administration
			.control_component()
			.require_bound_writer(&control_schema)?;
		self.config.installation_binding.verify_control(
			control,
			decoded_control.source_phase,
			&binding.bind_schema(self.config.administration.backend_schema().schema())?,
		)?;
		let bound_schema = binding.bind_schema(component.schema())?;
		component.require_bound_writer(&bound_schema)?;
		let point = self
			.config
			.participant_storage
			.lock(tx, component.identity(), self.config.conflict_attempts)
			.await?;
		let Some(state) =
			component.decode_bound_record(component.identity(), point.snapshot(), &bound_schema)?
		else {
			point.keep().await?;
			return Ok(false);
		};
		check_installation(control, &state)?;
		(binding.transitions().authorize_removal_fn)(&state, &registration.authority)?;
		point.delete().await?;
		Ok(true)
	}
}

fn check_installation(
	control: &InstallationControlState,
	participant: &ParticipantState,
) -> Result<(), NativeParticipationError> {
	if participant.installation_id() != control.installation_id() {
		return Err(NativeParticipationError::WrongInstallation);
	}
	Ok(())
}

/// Owned rollback cleans reservations and partial work; a failed rollback retains both errors.
async fn finish_transaction<T>(
	tx: WriteTx<'_>,
	outcome: Result<T, NativeParticipationError>,
) -> Result<T, NativeParticipationError> {
	crate::db::finish_pool_owned_write_transaction_and_preserve_rollback_error(
		"db.native_participation.finish",
		tx,
		outcome,
		NativeParticipationError::Commit,
		|_, operation, rollback| NativeParticipationError::RollbackFailed {
			operation_error: Box::new(operation),
			rollback_error: rollback,
		},
	)
	.await
}

/// Typed failure categories exclude participant identifiers, tokens, capabilities, and bodies.
#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeParticipationError {
	#[error("administration code owner differs from installation")]
	AdministrationBindingMismatch,
	#[error("native administrative writer policy failed")]
	Administration(#[from] NativeAdministrationError),
	#[error("participation backend mapping or source phase admission failed")]
	InstallationBinding(#[from] InstallationAdmissionBindingError),
	#[error("native participation control storage and parser bindings use different identities")]
	ControlBindingMismatch,
	#[error("native participant container or namespace does not match installation mapping")]
	ParticipantStorageBindingMismatch,
	#[error("native participation material uses a different canonical participant identity")]
	ParticipantBindingMismatch,
	#[error("native participation control record is absent")]
	ControlAbsent,
	#[error("native participation administrative policy is not installed")]
	ParticipationNotInstalled,
	#[error("native participation belongs to a different installation")]
	WrongInstallation,
	#[error("fresh native participation identity already contains a value")]
	RegistrationOccupied,
	#[error("native participation registration is absent")]
	ParticipantAbsent,
	#[error("native participation authority generation failed")]
	Random(#[source] primitives_rs::random::secure_bytes::RandomError),
	#[error("native participation transaction begin failed")]
	Begin(#[source] DatabaseError),
	#[error("native participation transaction commit failed")]
	Commit(#[source] DatabaseError),
	#[error("native participation operation and its owned rollback failed")]
	RollbackFailed {
		#[source]
		operation_error: Box<NativeParticipationError>,
		rollback_error: DatabaseError,
	},
	#[error("native participation control pin failed")]
	ControlPin(#[from] ControlPinError),
	#[error("native participation control parsing failed")]
	Control(#[from] InstallationControlError),
	#[error("native participation point storage failed")]
	Storage(#[from] StateStorageError),
	#[error("native participation representation parsing or encoding failed")]
	Representation(#[source] StateSchemaError<ParticipantStateError>),
	#[error("native participation reader policy or stored representation was rejected")]
	BoundRepresentation(#[from] ContractStateReadError<ParticipantStateError>),
	#[error("native participation authority or capabilities were rejected")]
	Participant(#[from] ParticipantStateError),
	#[error("native participation administrative contract binding failed")]
	Contract(#[from] StateContractError),
}

impl NativeParticipationError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Begin(error) | Self::Commit(error) => Some(error),
			Self::RollbackFailed {
				operation_error,
				rollback_error,
			} => operation_error.database_error().or(Some(rollback_error)),
			Self::InstallationBinding(error) => error.database_error(),
			Self::ControlPin(error) => error.database_error(),
			Self::Storage(error) => error.database_error(),
			_ => None,
		}
	}
}
