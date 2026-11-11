//! Native control-pinned component execution authenticated by a private participant registration.
//!
//! The callback is library code and runs before this boundary releases its transaction borrow.
//! Internal runtime composition supplies committed registration and managed participation.

use std::sync::{Arc, Mutex};

use crate::installation::contract::{
	BoundComponentContract, ComponentDeclaration, ComponentPolicy, ContractStateReadError,
	StateContractError,
};
use crate::installation::control_state::{
	DecodedInstallationControl, InstallationControlError, InstallationControlState,
};
use crate::records::PointRecordRead;
use crate::runtime::local_admission::{
	RuntimeAdmissionError, RuntimeAdmissionStop, RuntimeAdmissionTracker,
};
use crate::runtime::participation::state::{
	ParticipantComponentSupport, ParticipantState, ParticipantStateError,
};
use crate::scope::ScopeError;

use crate::db::{DatabaseError, Tx, WriteTx};
use crate::installation::administration::{NativeAdministrationError, PgNativeAdministration};
use crate::installation::control_storage::{
	ControlPinError, PgControlStorage, PgSharedPinWithPoint, PinnedControlWithPoint,
};
use crate::installation::v10::source_gate::NativeSourceGatePhase;
use crate::records::postgres::{PgPointStorage, StateStorageError};
use crate::runtime::participation::transactions::CommittedParticipation;
use bindings::{
	InstallationAdmissionBindingError, InstallationStorageRoute, PgInstallationAdmissionBinding,
};

pub(crate) mod bindings;
mod document;
pub(crate) mod lease;
pub(crate) mod observation;
pub(crate) mod owned_primitive;
pub(crate) mod payload;
mod primitive;
pub(crate) mod queue;
mod raw;
pub(crate) mod semaphore;
pub(crate) mod throttler;
pub(crate) mod upgrade;

/// Explicit physical/semantic bindings and committed private registration for one runtime.
pub(crate) struct PgNativeAdmissionConfig {
	pub(crate) installation_binding: Arc<PgInstallationAdmissionBinding>,
	pub(crate) retired_source_phase: Arc<RetiredSourcePhase>,
	pub(crate) control_storage: PgControlStorage,
	pub(crate) administration: Arc<PgNativeAdministration>,
	pub(crate) participant_storage: PgPointStorage,
	pub(crate) registration: Arc<CommittedParticipation>,
	pub(crate) local_admission: RuntimeAdmissionTracker,
}

/// Immutable native bindings. Construction performs no installation creation or registration.
pub(crate) struct PgNativeAdmission {
	config: PgNativeAdmissionConfig,
	/// The control pin that also reads this runtime's participant registration.
	shared_pin_with_participant: PgSharedPinWithPoint,
	decoded_records: Mutex<Option<Arc<CachedAdmissionRecords>>>,
}

/// The v10 source gate's phase, once a transaction of the runtime has observed that the
/// installation retired its v10 data. Data retirement is final and the gate never changes after
/// it, so the runtime's later transactions do not pin the gate. The runtime's admission and its
/// participation share one value.
#[derive(Default)]
pub(crate) struct RetiredSourcePhase(std::sync::OnceLock<NativeSourceGatePhase>);

impl RetiredSourcePhase {
	/// Pins the v10 source gate of an imported installation, unless an earlier transaction of
	/// the runtime observed that the installation retired its v10 data.
	pub(crate) async fn pin_source(
		&self,
		installation: &PgInstallationAdmissionBinding,
		tx: &mut Tx<'_>,
	) -> Result<Option<NativeSourceGatePhase>, InstallationAdmissionBindingError> {
		match self.0.get() {
			Some(phase) => Ok(Some(*phase)),
			None => installation.pin_source(tx).await,
		}
	}

	/// Retains `source_phase` when `route`, which the caller verified from the control record
	/// that its transaction read, shows that the installation retired its v10 data.
	pub(crate) fn record(
		&self,
		route: InstallationStorageRoute,
		source_phase: Option<NativeSourceGatePhase>,
	) {
		if route == InstallationStorageRoute::NativeImported
			&& let Some(phase) = source_phase
		{
			// Another transaction may have recorded the same phase first.
			let _ = self.0.set(phase);
		}
	}
}

struct AdmissionObservation {
	installation_binding: Arc<PgInstallationAdmissionBinding>,
	route: InstallationStorageRoute,
	control: Arc<DecodedInstallationControl>,
	participant: Option<Arc<ParticipantState>>,
}

/// One successfully decoded pair for this runtime's immutable storage and codec bindings.
/// Equality permits parser reuse; it grants no transaction or participant authority.
struct CachedAdmissionRecords {
	pinned: PinnedControlWithPoint,
	control: Arc<DecodedInstallationControl>,
	participant: Arc<ParticipantState>,
}

/// A transaction-scoped view that binds every component required by one library operation.
/// It exposes no control body, participant secret, or independently selectable policy.
pub(crate) struct NativeAdmittedContracts {
	observation: AdmissionObservation,
}

impl NativeAdmittedContracts {
	pub(crate) fn storage_route(&self) -> InstallationStorageRoute {
		self.observation.route
	}

	/// Native-only handles reject source phases before inspecting or mutating protected data.
	pub(crate) fn require_native_storage(&self) -> Result<(), NativeAdmissionError> {
		match self.storage_route() {
			InstallationStorageRoute::NativeFresh | InstallationStorageRoute::NativeImported => {
				Ok(())
			}
			route => Err(NativeAdmissionError::StorageRouteUnsupported { route }),
		}
	}

	pub(crate) fn bind<'a, 'd, Transitions>(
		&'a self,
		declaration: &'a ComponentDeclaration<'d, Transitions>,
	) -> Result<BoundComponentContract<'a, 'd, Transitions>, NativeAdmissionError> {
		self.observation.bind(declaration)
	}
}

impl AdmissionObservation {
	fn participant(&self) -> Result<&ParticipantState, NativeAdmissionError> {
		self.participant
			.as_deref()
			.ok_or(NativeAdmissionError::RuntimeRequired)
	}

	fn bind<'a, 'd, Transitions>(
		&'a self,
		declaration: &'a ComponentDeclaration<'d, Transitions>,
	) -> Result<BoundComponentContract<'a, 'd, Transitions>, NativeAdmissionError> {
		let policy = self
			.control
			.state()
			.component(declaration.identifier())
			.ok_or(NativeAdmissionError::ComponentNotInstalled)?;
		match &self.participant {
			Some(participant) => {
				let support = participant
					.capabilities()
					.component(declaration.identifier())
					.ok_or(NativeAdmissionError::ComponentNotRegistered)?;
				bind_supported_component(policy, support, declaration)
			}
			None => declaration
				.bind(policy)
				.map_err(NativeAdmissionError::Contract),
		}
	}
}

/// Support selection belongs to the caller's actual active or fixed maintenance boundary.
/// This kernel checks the complete selected policy and binds its actual declaration.
fn bind_supported_component<'a, 'd, Transitions>(
	policy: &'a ComponentPolicy,
	support: &ParticipantComponentSupport,
	declaration: &'a ComponentDeclaration<'d, Transitions>,
) -> Result<BoundComponentContract<'a, 'd, Transitions>, NativeAdmissionError> {
	support
		.check_policy(policy)
		.map_err(NativeAdmissionError::Participant)?;
	declaration
		.bind(policy)
		.map_err(NativeAdmissionError::Contract)
}

impl PgNativeAdmission {
	/// Retains the actual immutable association; every operation still verifies fresh phase.
	pub(crate) fn installation_binding(&self) -> &Arc<PgInstallationAdmissionBinding> {
		&self.config.installation_binding
	}

	pub(crate) fn app_storage(
		&self,
		scope: &crate::scope::ScopeIdentity,
	) -> Result<PgPointStorage, NativeAdmissionError> {
		self.config
			.installation_binding
			.app_storage(scope)
			.map_err(Into::into)
	}

	pub(crate) fn verify_raw_source_storage(
		&self,
		source_storage: &crate::records::postgres::v10::relocating_point::PgRelocatingPointStorage,
		namespace: &crate::records::RecordNamespace,
	) -> Result<(), NativeAdmissionError> {
		self.config
			.installation_binding
			.verify_raw_source_storage(source_storage, namespace)
			.map_err(Into::into)
	}

	pub(crate) fn new(config: PgNativeAdmissionConfig) -> Result<Self, NativeAdmissionError> {
		if config.control_storage.identity() != config.administration.control_component().identity()
			|| !config
				.installation_binding
				.matches_control_storage(&config.control_storage)
		{
			return Err(NativeAdmissionError::ControlBindingMismatch);
		}
		let native = match config.installation_binding.as_ref() {
			PgInstallationAdmissionBinding::Fresh { native }
			| PgInstallationAdmissionBinding::Imported { native, .. } => native,
		};
		if !Arc::ptr_eq(native, config.administration.native_bindings())
			|| !Arc::ptr_eq(
				config.registration.component().participant_schema(),
				config.administration.participant_schema(),
			) {
			return Err(NativeAdmissionError::AdministrationBindingMismatch);
		}
		if !config.installation_binding.matches_participant_storage(
			&config.participant_storage,
			config.registration.component().identity().namespace(),
		) {
			return Err(NativeAdmissionError::ParticipantStorageBindingMismatch);
		}
		let shared_pin_with_participant = config.control_storage.shared_pin_with_point(
			&config.participant_storage,
			config.registration.component().identity().clone(),
		);
		Ok(Self {
			config,
			shared_pin_with_participant,
			decoded_records: Mutex::new(None),
		})
	}

	/// Authenticates once for a library read requiring multiple actual component declarations.
	/// Callbacks tag admission failures explicitly; such failure retains mandatory rollback.
	pub(crate) async fn execute_read_with_contracts<T, E>(
		&self,
		tx: &mut Tx<'_>,
		operation_fn: impl for<'a, 'tx> AsyncFnOnce(
			NativeAdmittedContracts,
			&'a mut Tx<'tx>,
		) -> Result<T, NativeOperationError<E>>,
	) -> Result<T, NativeOperationError<E>> {
		let permit =
			self.config.local_admission.try_admit().map_err(|error| {
				NativeOperationError::Admission(NativeAdmissionError::Local(error))
			})?;
		tx.retain_runtime_permit(permit);
		let mut completion = tx.track_state_mutation();
		let observation = self
			.observe(tx)
			.await
			.map_err(NativeOperationError::Admission)?;
		let result = operation_fn(NativeAdmittedContracts { observation }, tx).await;
		// Storage failures retain their own sticky rollback requirement. A completed pure
		// rejection can preserve the application's ability to commit its earlier work.
		if !matches!(&result, Err(NativeOperationError::Admission(_))) {
			completion.complete();
		}
		result
	}

	/// Authenticates once for a mutation requiring actual component declarations.
	/// Any outer callback error requires rollback because earlier writes may have succeeded.
	/// A completed sole-point kernel returns its typed result inside Ok; its caller flattens
	/// that result after this boundary, preserving completed rejection without bypassing
	/// the storage guard's sticky rollback requirement.
	pub(crate) async fn execute_write_with_contracts<T, E>(
		&self,
		tx: &mut WriteTx<'_>,
		operation_fn: impl for<'a, 'tx> AsyncFnOnce(
			NativeAdmittedContracts,
			&'a mut WriteTx<'tx>,
		) -> Result<T, NativeOperationError<E>>,
	) -> Result<T, NativeOperationError<E>> {
		let permit =
			self.config.local_admission.try_admit().map_err(|error| {
				NativeOperationError::Admission(NativeAdmissionError::Local(error))
			})?;
		tx.retain_runtime_permit(permit);
		let mut completion = tx.track_state_mutation();
		let observation = self
			.observe(tx)
			.await
			.map_err(NativeOperationError::Admission)?;
		let result = operation_fn(NativeAdmittedContracts { observation }, tx).await;
		if result.is_ok() {
			completion.complete();
		}
		result
	}

	/// Every operation reads the control record under a shared pin, which keeps the installed
	/// policy and generation fixed until the transaction ends, and this runtime's participant
	/// registration, which proves by the database clock that an upgrade has not deleted the
	/// registration as expired. One statement reads both.
	async fn observe(&self, tx: &mut Tx<'_>) -> Result<AdmissionObservation, NativeAdmissionError> {
		let source_phase = self.pin_source(tx).await?;
		let cached = self
			.decoded_records
			.lock()
			.expect("admission cache mutex")
			.clone();
		let pinned = self
			.config
			.control_storage
			.read_pinned_shared_with_point(
				tx,
				&self.shared_pin_with_participant,
				cached.as_ref().map(|c| &c.pinned),
			)
			.await?;
		let matching = cached.as_ref().filter(|cached| {
			cached.pinned.control.record() == pinned.control.record()
				&& cached.pinned.point.record() == pinned.point.record()
		});
		let observation = self
			.authenticate_control(
				tx,
				source_phase,
				&pinned.control,
				Some(&pinned.point),
				matching.map(Arc::as_ref),
			)
			.await?;
		*self.decoded_records.lock().expect("admission cache mutex") =
			Some(Arc::new(CachedAdmissionRecords {
				pinned,
				control: Arc::clone(&observation.control),
				participant: Arc::clone(
					observation
						.participant
						.as_ref()
						.expect("runtime admission authenticated its participant"),
				),
			}));
		Ok(observation)
	}

	/// Pins the v10 source gate unless the runtime observed that the installation retired its
	/// v10 data.
	async fn pin_source(
		&self,
		tx: &mut Tx<'_>,
	) -> Result<Option<NativeSourceGatePhase>, InstallationAdmissionBindingError> {
		self.config
			.retired_source_phase
			.pin_source(&self.config.installation_binding, tx)
			.await
	}

	async fn authenticate_observation(
		&self,
		tx: &mut Tx<'_>,
		source_phase: Option<NativeSourceGatePhase>,
		control_snapshot: PointRecordRead,
	) -> Result<AdmissionObservation, NativeAdmissionError> {
		self.authenticate_control(tx, source_phase, &control_snapshot, None, None)
			.await
	}

	/// Reads the participant registration unless `participant_snapshot` holds it already.
	async fn authenticate_control(
		&self,
		tx: &mut Tx<'_>,
		source_phase: Option<NativeSourceGatePhase>,
		control_snapshot: &PointRecordRead,
		participant_snapshot: Option<&PointRecordRead>,
		cached: Option<&CachedAdmissionRecords>,
	) -> Result<AdmissionObservation, NativeAdmissionError> {
		let decoded_control = match cached {
			Some(cached) => Arc::clone(&cached.control),
			None => Arc::new(
				self.config
					.administration
					.control_component()
					.decode_record(self.config.control_storage.identity(), control_snapshot)?
					.ok_or(NativeAdmissionError::ControlAbsent)?,
			),
		};
		let control = decoded_control.state();
		let administrative_declaration = self.config.administration.declaration();
		let administrative_policy = control
			.component(administrative_declaration.identifier())
			.ok_or(NativeAdmissionError::ComponentNotInstalled)?;
		let administrative_binding = administrative_declaration
			.bind(administrative_policy)
			.map_err(NativeAdmissionError::Contract)?;
		let route = observation::verify_control_observation(
			&self.config.administration,
			&self.config.installation_binding,
			&decoded_control,
			source_phase,
		)?;
		let fetched_participant;
		let participant_snapshot = match participant_snapshot {
			Some(snapshot) => snapshot,
			None => {
				fetched_participant = self
					.config
					.participant_storage
					.read(tx, self.config.registration.component().identity())
					.await?;
				&fetched_participant
			}
		};
		let participant = self.authenticate_participant_snapshot(
			tx,
			control,
			&administrative_binding,
			participant_snapshot,
			cached.map(|cached| Arc::clone(&cached.participant)),
		)?;
		self.config.retired_source_phase.record(route, source_phase);
		Ok(AdmissionObservation {
			installation_binding: Arc::clone(&self.config.installation_binding),
			route,
			control: decoded_control,
			participant: Some(participant),
		})
	}

	/// Shares private participant authentication after a fixed caller has retained actual control.
	async fn authenticate_pinned_participant(
		&self,
		tx: &mut Tx<'_>,
		control: &InstallationControlState,
		administration: &BoundComponentContract<
			'_,
			'_,
			crate::runtime::participation::state::ParticipationSemantics,
		>,
	) -> Result<Arc<ParticipantState>, NativeAdmissionError> {
		let participant_snapshot = self
			.config
			.participant_storage
			.read(tx, self.config.registration.component().identity())
			.await?;
		self.authenticate_participant_snapshot(
			tx,
			control,
			administration,
			&participant_snapshot,
			None,
		)
	}

	fn authenticate_participant_snapshot(
		&self,
		tx: &mut Tx<'_>,
		control: &InstallationControlState,
		administration: &BoundComponentContract<
			'_,
			'_,
			crate::runtime::participation::state::ParticipationSemantics,
		>,
		participant_snapshot: &PointRecordRead,
		cached: Option<Arc<ParticipantState>>,
	) -> Result<Arc<ParticipantState>, NativeAdmissionError> {
		let participant_identity = self.config.registration.component().identity();
		let administrative_policy = control
			.component(administration.policy().component_identifier())
			.ok_or(NativeAdmissionError::ComponentNotInstalled)?;
		let administrative_schema = administration
			.bind_schema(self.config.registration.component().schema())
			.map_err(NativeAdmissionError::Contract)?;
		self.config
			.registration
			.component()
			.require_bound_writer(&administrative_schema)
			.map_err(NativeAdmissionError::Participant)?;
		let participant = match cached {
			Some(cached) => Some(cached),
			None => self
				.config
				.registration
				.component()
				.decode_bound_record(
					participant_identity,
					participant_snapshot,
					&administrative_schema,
				)
				.map_err(NativeAdmissionError::ParticipantRepresentation)?
				.map(Arc::new),
		};
		let Some(participant) = participant else {
			self.config
				.local_admission
				.stop(RuntimeAdmissionStop::Fenced);
			return Err(NativeAdmissionError::ParticipantAbsent);
		};
		if participant.installation_id() != control.installation_id() {
			return Err(NativeAdmissionError::ParticipantInstallationMismatch);
		}
		if let Err(error) = participant.authenticate(
			self.config.registration.authority(),
			participant_snapshot.observed_at(),
		) {
			self.config
				.local_admission
				.stop(RuntimeAdmissionStop::Fenced);
			return Err(NativeAdmissionError::Participant(error));
		}
		participant
			.capabilities()
			.check_administrative_policy(administrative_policy)
			.map_err(NativeAdmissionError::Participant)?;
		(administration.transitions().require_capabilities_fn)(participant.capabilities())
			.map_err(NativeAdmissionError::Participant)?;
		if !tx.select_native_generation(control.installation_id(), control.generation()) {
			return Err(NativeAdmissionError::GenerationChanged);
		}
		Ok(participant)
	}
}

/// Admission errors contain typed categories and sanitized schema/parser failures.
#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeAdmissionError {
	#[error("the operation requires registered runtime participation")]
	RuntimeRequired,
	#[error("runtime backend mapping or source phase admission failed")]
	InstallationBinding(#[from] InstallationAdmissionBindingError),
	#[error("native-only operation cannot use installation storage route {route:?}")]
	StorageRouteUnsupported { route: InstallationStorageRoute },
	#[error("native runtime stopped local admission")]
	Local(#[source] RuntimeAdmissionError),
	#[error("native control physical and semantic bindings use different identities")]
	ControlBindingMismatch,
	#[error("native participant container or namespace does not match installation mapping")]
	ParticipantStorageBindingMismatch,
	#[error("native administration code differs from installation or registration owners")]
	AdministrationBindingMismatch,
	#[error("native administration selected policy is unavailable")]
	Administration(#[from] NativeAdministrationError),
	#[error("native lease operation does not retain this runtime admission binding")]
	LeaseOperationBindingMismatch,
	#[error("native installation control record is absent")]
	ControlAbsent,
	#[error("native participant registration is absent")]
	ParticipantAbsent,
	#[error("native participant registration belongs to another installation")]
	ParticipantInstallationMismatch,
	#[error("native component has no installed policy")]
	ComponentNotInstalled,
	#[error("native component has no declared participant use")]
	ComponentNotRegistered,
	#[error("fixed upgrade component has no advertised participant support")]
	ComponentNotAdvertised,
	#[error("native installation generation changed within one transaction")]
	GenerationChanged,
	#[error("native control pin failed")]
	ControlPin(#[from] ControlPinError),
	#[error("native installation control parsing failed")]
	Control(#[from] InstallationControlError),
	#[error("native participant physical read failed")]
	Storage(#[from] StateStorageError),
	#[error("native participant representation parsing failed")]
	ParticipantRepresentation(#[source] ContractStateReadError<ParticipantStateError>),
	#[error("native participant authentication or compatibility check failed")]
	Participant(#[source] ParticipantStateError),
	#[error("native component contract binding failed")]
	Contract(#[source] StateContractError),
	#[error("native scoped role binding failed")]
	Scope(#[from] ScopeError),
	#[error("native primitive family binding failed")]
	Primitive(#[from] crate::fleet::primitive_scope::PrimitiveScopeBindingError),
}

impl NativeAdmissionError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::InstallationBinding(error) => error.database_error(),
			Self::ControlPin(error) => error.database_error(),
			Self::Storage(error) => error.database_error(),
			Self::StorageRouteUnsupported { .. }
			| Self::Local(_)
			| Self::ControlBindingMismatch
			| Self::ParticipantStorageBindingMismatch
			| Self::AdministrationBindingMismatch
			| Self::Administration(_)
			| Self::LeaseOperationBindingMismatch
			| Self::ControlAbsent
			| Self::RuntimeRequired
			| Self::ParticipantAbsent
			| Self::ParticipantInstallationMismatch
			| Self::ComponentNotInstalled
			| Self::ComponentNotRegistered
			| Self::ComponentNotAdvertised
			| Self::GenerationChanged
			| Self::Control(_)
			| Self::ParticipantRepresentation(_)
			| Self::Participant(_)
			| Self::Contract(_)
			| Self::Scope(_)
			| Self::Primitive(_) => None,
		}
	}

	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::InstallationBinding(error) => error.involves_database_failure(),
			_ => self.database_error().is_some(),
		}
	}

	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::InstallationBinding(error) => error.commit_outcome_is_unknown(),
			_ => self
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
		}
	}
}

/// Separates admission rejection from the primitive's own completed operation result.
#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeOperationError<E> {
	#[error("native state operation admission failed")]
	Admission(#[source] NativeAdmissionError),
	#[error("native state operation failed")]
	Operation(#[source] E),
}
