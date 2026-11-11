//! A [`PgRuntime`] is one participant of an installation. [`PgRuntime::start`] registers it and
//! renews the registration every 10 seconds. The runtime admits an operation only while the
//! registration is live by the database clock and by a conservative local bound.
//!
//! A runtime builds the handles for Raw values, documents, Counters, leases, Mutexes, Crons,
//! Once, coalescing caches, Semaphores, Throttlers, Topics, and the Queue without database work.
//! It migrates and validates the schemas of the application's own components. It starts
//! upgrades and the retirement of an imported v10 installation, and it resumes, inspects, and
//! cancels them.
//!
//! [`PgRuntime::initiate_shutdown`] stops new operations, waits until the runtime's work and
//! every transaction that ran one of its operations have ended, and then removes the
//! registration. When a piece of that work was abandoned, the shutdown does not remove the
//! registration, which then expires on its own. A runtime that is dropped without a shutdown
//! stops accepting operations and stops renewing, and its registration stops being live 30
//! seconds after the runtime last registered or renewed it.
//!
//! [`PgRuntimeFields`] and [`FleetUseIntent`] configure a runtime, [`PgRuntimeStartError`]
//! reports a failed start, [`PgRuntimeObserver`], [`PgRuntimeSnapshot`], and
//! [`PgRuntimeWorkSnapshot`] report its state and work in this process, and
//! [`PgRuntimeShutdown`] reports its shutdown.

// The lifecycle owner is separate from retained handles. Verified fresh or imported
// installation bindings select source routing under current admission pins. Managed
// workers, guards, and upgrade controllers share that runtime's admission and shutdown.

pub(crate) mod admission;
mod error;
mod lifecycle;
pub(crate) mod local_admission;
pub(crate) mod participation;
pub(crate) mod scope_handle_error;
pub(crate) mod v10;
mod work;

pub use error::{PgRuntimeStartError, PgRuntimeStartErrorKind};
pub use fleet::FleetUseIntent;
pub use lifecycle::{
	PgRuntimeAdmissionState, PgRuntimeAdmissionStop, PgRuntimeLocalSnapshot, PgRuntimeObserver,
	PgRuntimeRemovalResult, PgRuntimeRenewalEnd, PgRuntimeRenewalStatus, PgRuntimeShutdown,
	PgRuntimeShutdownResult, PgRuntimeShutdownState, PgRuntimeShutdownWaitError,
	PgRuntimeShutdownWaitErrorKind, PgRuntimeSnapshot,
};
pub use scope_handle_error::{PgScopeHandleError, PgScopeHandleErrorKind};
pub use work::{
	PgRuntimeWorkDiagnostic, PgRuntimeWorkKind, PgRuntimeWorkSnapshot, PgRuntimeWorkState,
};

use std::{num::NonZeroU32, sync::Arc};

use scope_handle_error::{PgScopeHandleErrorFields, ScopeHandleCause};

use crate::db::{DatabaseError, SchemaError, TransactionFailure, WritePool, run_transaction_read};
use crate::installation::ProtocolError;
use crate::installation::administration::{
	NativeAdministrationError, PgNativeAdministration, PgNativeAdministrationFields,
};
use crate::installation::creation::PgNativeInstallation;
use crate::installation::creation::v10::recorded_source::PgRecordedImportedInstallation;
use crate::records::{RecordIdentity, RecordKey};
use crate::runtime::admission::{
	NativeAdmissionError, NativeOperationError, PgNativeAdmission, PgNativeAdmissionConfig,
	RetiredSourcePhase,
};
use crate::runtime::participation::state::{ParticipantComponentSupport, ParticipantSchema};
use crate::runtime::participation::transactions::{
	PgNativeParticipation, PgNativeParticipationConfig,
};
use crate::runtime::participation::{
	NativeParticipationRuntimeStartOptions, NativeParticipationShutdown,
	NativeParticipationShutdownResult, NativeParticipationShutdownWaitError,
	NativeParticipationStartupError, PgNativeParticipationRuntime,
};
use crate::scope::managed::ManagedScopeRegistry;
use crate::upgrade::participants::scope_addition::DeclaredScopeAdditionRequest;
use crate::upgrade::participants::{
	NativeUpgradeParticipantsError, PgNativeUpgradeParticipants, PgNativeUpgradeParticipantsConfig,
};

use crate::runtime::admission::bindings::{
	InstallationStorageRoute, PgInstallationAdmissionBinding,
};
use crate::runtime::v10::{
	NativeSourcePreparationError, NativeSourcePreparationFields, RuntimeSourceAdapters,
};

mod fleet;
use fleet::{
	NativeRuntimeFleetScalar, NativeRuntimeFleetScalarDefinition, NativeRuntimeFleetScalarError,
	NativeRuntimeFleetScalarFields,
};
pub(crate) mod once;
pub(crate) mod parameters;
mod queue;
mod selection_indexes;
pub(crate) mod semaphore;
pub(crate) mod throttler;
use throttler::ThrottlerRuntimeConfig;
pub(crate) mod targets;
pub(crate) mod upgrade;
use crate::fleet::HolderPrefix;
use crate::fleet::counter::engine::{NativeCounterBindings, PgNativeCounter};
use crate::fleet::counter::state::CounterComponent;
use crate::fleet::counter::state::CounterRepresentationError;
use crate::fleet::lease::{
	LeaseComponent, LeaseKey, LeaseSourceBridge, NativeLeaseBindings, PgNativeLease,
};
use crate::installation::contract::StateStorageEnvelope;
use crate::kv::raw::core_component::CoreRawComponent;
use crate::kv::raw::engine::store::{NativeRawKvBindings, PgNativeRawKvStore};
use queue::{NativeRuntimeQueue, NativeRuntimeQueueDefs};
use semaphore::SemaphoreRuntimeConfig;
use targets::{
	NativeRuntimeTargetCode, NativeRuntimeTargetCodeFields, NativeRuntimeTargetError,
	NativeRuntimeTargetFleetScalarCode, NativeRuntimeTargetQueueCode,
};

/// Actual verified origin; each operation still checks the durable installation phase.
pub(crate) enum PgNativeRuntimeInstallation {
	Fresh(Box<PgNativeInstallation>),
	Imported(Box<PgRecordedImportedInstallation>),
}

/// The inputs of a runtime.
pub struct PgRuntimeFields {
	/// The pool for the app's operations. It must use separate client connection capacity
	/// from `control_pool`.
	pub app_pool: WritePool,
	/// The pool for the runtime's own work: registration as a participant, renewal, upgrades,
	/// cleanup, and the transactions of leases, Mutexes, Semaphores, and Throttlers. It must
	/// not share client connection capacity with `app_pool`. The
	/// runtime's work other than its participation holds at most all but one of the pool's
	/// connections at once, leaving one available for renewal; a pool of one connection keeps
	/// none free. Runtimes that share a control pool share this limit. This reserve applies to
	/// the client pool. Shared pooler or database connection limits can still delay renewal,
	/// so the deployment must also leave backend capacity for the runtime's control work.
	pub control_pool: WritePool,
	/// The start of the holder label of every lease that this runtime acquires, such as the
	/// process's host name and process ID. The label is observable and grants nothing.
	pub holder_prefix: HolderPrefix,
	/// The opened installation that the runtime uses.
	pub installation: crate::installation::PgInstallation,
	/// Whether the runtime uses Once, cache epochs, and Topics from its start.
	pub fleet_use_intent: FleetUseIntent,
}

/// One participant of an installation, and the source of every handle that the application uses
/// on that installation.
///
/// While the runtime runs, it renews its registration as a participant so that upgrades
/// can see which code versions are running. `initiate_shutdown` stops new operations, waits
/// for the runtime's work to end, and removes the runtime from the installation's
/// participants. Dropping the runtime without a shutdown stops new operations and renewal
/// only; the registration then expires on its own. The Tokio runtime that started the
/// runtime must stay alive until its shutdown finishes.
pub struct PgRuntime {
	participation: PgNativeParticipationRuntime,
	admission: Arc<PgNativeAdmission>,
	administration: Arc<PgNativeAdministration>,
	conflict_attempts: NonZeroU32,
	group_budget: crate::records::postgres::group_attempt::PgRecordGroupAttemptBudget,
	semaphore_config: SemaphoreRuntimeConfig,
	throttler_config: ThrottlerRuntimeConfig,
	counters: Arc<NativeCounterBindings>,
	component_ledger: Arc<crate::installation::component_ledger::NativeComponentLedgerBindings>,
	raw_kv: Arc<NativeRawKvBindings>,
	leases: Arc<NativeLeaseBindings>,
	lease_source_bridge: Arc<LeaseSourceBridge>,
	queue: NativeRuntimeQueue,
	fleet_scalar: NativeRuntimeFleetScalar,
	managed_scopes: Arc<ManagedScopeRegistry>,
	queue_source_imports: Option<Arc<crate::queue::QueueSourceImportDefs>>,
	app_pool: WritePool,
	holder_prefix: HolderPrefix,
}

/// How long a wait for a runtime or an upgrade lasts at most.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgWaitOptions {
	/// The longest time that the wait lasts.
	pub timeout: std::time::Duration,
}

/// Abandoned startup transfers known committed participation to owned shutdown.
struct NativeRuntimeStartupGuard {
	ownership: NativeRuntimeStartupOwnership,
}

enum NativeRuntimeStartupOwnership {
	Checking(Box<PgRuntime>),
	Transferred,
}

impl NativeRuntimeStartupGuard {
	fn runtime(&self) -> &PgRuntime {
		match &self.ownership {
			NativeRuntimeStartupOwnership::Checking(runtime) => runtime,
			NativeRuntimeStartupOwnership::Transferred => {
				unreachable!("startup transfers its runtime only when consuming the guard")
			}
		}
	}

	fn into_runtime(mut self) -> PgRuntime {
		match std::mem::replace(
			&mut self.ownership,
			NativeRuntimeStartupOwnership::Transferred,
		) {
			NativeRuntimeStartupOwnership::Checking(runtime) => *runtime,
			NativeRuntimeStartupOwnership::Transferred => {
				unreachable!("startup transfers its runtime exactly once")
			}
		}
	}
}

impl Drop for NativeRuntimeStartupGuard {
	fn drop(&mut self) {
		if let NativeRuntimeStartupOwnership::Checking(runtime) = &self.ownership {
			runtime.participation.initiate_shutdown();
		}
	}
}

#[derive(Debug, thiserror::Error)]
enum NativeRuntimeStartupError {
	#[error("native runtime application and control pools share connection capacity")]
	SharedConnectionCapacity,
	#[error("native runtime administration does not match its installation code")]
	Administration(#[source] NativeAdministrationError),
	#[error("native runtime phase-aware storage preparation failed")]
	SourcePreparation(#[source] NativeSourcePreparationError),
	#[error("native runtime installation of its declared scopes failed")]
	ScopeAddition(#[source] Box<NativeUpgradeParticipantsError>),
	#[error("native runtime participation startup failed")]
	Participation(#[source] NativeParticipationStartupError),
	#[error("native runtime application-capacity admission failed")]
	AppAdmission {
		#[source]
		operation: Box<NativeRuntimePreflightError>,
		shutdown: NativeParticipationShutdown,
		cleanup: Result<
			Arc<NativeParticipationShutdownResult>,
			Box<NativeParticipationShutdownWaitError>,
		>,
	},
}

impl PgRuntime {
	/// Registers the runtime as a participant of the installation, checks through the app pool
	/// that the installation's tables and installed policies match this runtime's code, and
	/// starts renewal.
	///
	/// Before it registers, the runtime adds each declared scope with a JSON writer that the
	/// installation does not have, empty, in one transaction, as the upgrade step that adds a scope does. The
	/// addition requires that no record is in the scope's namespace and that every running
	/// participant stays compatible, and it fails the start with
	/// [`PgRuntimeStartErrorKind::ScopeAddition`]
	/// otherwise. A running participant does not use a scope that the installation lacked, so it
	/// continues unchanged. A document scope added this way maintains every declared index and
	/// has none ready for queries. A scope with no JSON writer stays absent, so a dormant reader
	/// can register before another release installs it. While an upgrade is pending, the runtime
	/// adds no scope. A declared `Active` role in any absent scope fails the registration.
	///
	/// The two pools must not share connections. Startup does not create missing tables.
	/// When the check through the app pool fails after registration, the runtime shuts itself
	/// down, and the error returns the handle of that shutdown. When the registration's lifetime ends
	/// before startup finishes, the runtime removes its registration, and the error reports the
	/// removal. When the result of the registration's `COMMIT` is unknown, for example because
	/// registration timed out, the error returns no shutdown handle, and a registration that did
	/// commit expires on its own.
	pub async fn start(fields: PgRuntimeFields) -> Result<Self, error::PgRuntimeStartError> {
		Self::start_inner(fields)
			.await
			.map_err(error::PgRuntimeStartError::new)
	}

	/// Returns usable handles only after committed registration and admission on application
	/// capacity. Physical validation never creates or repairs a missing native container.
	async fn start_inner(config: PgRuntimeFields) -> Result<Self, NativeRuntimeStartupError> {
		if config
			.app_pool
			.shares_connection_capacity(&config.control_pool)
		{
			return Err(NativeRuntimeStartupError::SharedConnectionCapacity);
		}
		let application = config.installation.app_declarations.into_parts();
		// An imported installation keeps the v10 Queue readers for target binding, also after
		// its v10 data moved.
		let (
			bindings,
			control_component,
			installation_binding,
			queue_source_preparation,
			queue_source_imports,
		) = match config.installation.installation {
			PgNativeRuntimeInstallation::Fresh(installation) => {
				let (bindings, control) = installation.into_parts();
				let bindings = Arc::new(bindings);
				let association = PgInstallationAdmissionBinding::Fresh {
					native: Arc::clone(&bindings),
				};
				(bindings, control, Arc::new(association), None, None)
			}
			PgNativeRuntimeInstallation::Imported(installation) => {
				let imports = Arc::new(crate::queue::QueueSourceImportDefs::new());
				let queue_source_preparation = crate::runtime::v10::prepare_recorded_queue_source(
					&installation,
					Arc::clone(&imports),
				);
				let (source, bindings, control) = installation.into_parts();
				let bindings = Arc::new(bindings);
				let association = PgInstallationAdmissionBinding::Imported {
					native: Arc::clone(&bindings),
					source: Arc::new(source),
				};
				(
					bindings,
					control,
					Arc::new(association),
					queue_source_preparation,
					Some(imports),
				)
			}
		};
		let fleet_scalar =
			NativeRuntimeFleetScalarDefinition::new(NativeRuntimeFleetScalarFields {
				uses: config.fleet_use_intent,
			});
		let control_storage = bindings.control().clone();
		let participant = bindings.participants().clone();
		let counters = bindings.counters().clone();
		let component_schemas = bindings.component_schemas().clone();
		let raw_kv_binding = bindings.raw_kv().clone();
		let lease_binding = bindings.leases().clone();
		let containers = bindings
			.unique_containers()
			.into_iter()
			.cloned()
			.collect::<Vec<_>>();
		let lease_source_bridge = Arc::new(LeaseSourceBridge::new());
		let administration = Arc::new(
			PgNativeAdministration::new(PgNativeAdministrationFields {
				participant: Arc::new(ParticipantSchema::new()),
				control: Arc::new(control_component),
				native: Arc::clone(&bindings),
			})
			.map_err(NativeRuntimeStartupError::Administration)?,
		);
		let sources = RuntimeSourceAdapters::prepare(NativeSourcePreparationFields {
			pool: &config.control_pool,
			installation: &installation_binding,
			administration: &administration,
			lease_bridge: &lease_source_bridge,
			queue: queue_source_preparation,
		})
		.await
		.map_err(NativeRuntimeStartupError::SourcePreparation)?;
		// The handles of each family take that family's v10 adapter, which is absent when the
		// runtime reads no v10 data.
		let (counter_source, raw_kv_source, lease_source, queue_source, fleet_scalar_source) =
			match sources {
				RuntimeSourceAdapters::Native => (None, None, None, None, None),
				RuntimeSourceAdapters::Source(sources) => (
					Some(sources.counter),
					Some(sources.raw_kv),
					Some(sources.lease),
					Some(sources.queue),
					Some(sources.fleet_scalar),
				),
			};
		let fleet_scalar = fleet_scalar.into_runtime(fleet_scalar_source);

		let counter = CounterComponent::new();
		let ledger = crate::installation::component_ledger::ComponentLedgerComponent::new();
		let raw_kv = CoreRawComponent::new();
		let lease_component = LeaseComponent::new();
		let queue_defs = NativeRuntimeQueueDefs::new(bindings.queue().clone(), queue_source);
		let mut support = vec![
			ParticipantComponentSupport::from_declaration(&counter.declaration()),
			ParticipantComponentSupport::from_declaration(&ledger.declaration()),
			ParticipantComponentSupport::from_declaration(&raw_kv.declaration()),
			ParticipantComponentSupport::from_declaration(&lease_component.declaration()),
		];
		if let Some(source) = &lease_source {
			support.push(ParticipantComponentSupport::from_declaration(
				&source.bridge.declaration(),
			));
		}
		queue_defs.register_support(&mut support);
		// A declared scope that the installation lacks holds no records, so the runtime installs
		// it before it registers, with the policy that the upgrade step that adds a scope
		// installs.
		let limits = parameters::upgrade_execution_limits();
		PgNativeUpgradeParticipants::new(PgNativeUpgradeParticipantsConfig {
			installation_binding: Arc::clone(&installation_binding),
			control_pool: config.control_pool.clone(),
			control_storage: control_storage.clone(),
			administration: Arc::clone(&administration),
			participant_storage: participant.storage().clone(),
			participant_namespace: participant.namespace().clone(),
			conflict_attempts: parameters::CONFLICT_ATTEMPTS,
			control_pin_wait_timeout: parameters::UPGRADE_CONTROL_PIN_WAIT,
		})
		.map_err(|error| NativeRuntimeStartupError::ScopeAddition(Box::new(error)))?
		.install_absent_declared_scopes(DeclaredScopeAdditionRequest {
			registry: &application.managed_scopes,
			participant_budget: limits.participant_budget(),
			installation_wait_timeout: limits.protected_work_wait_timeout(),
		})
		.await
		.map_err(|error| NativeRuntimeStartupError::ScopeAddition(Box::new(error)))?;
		let retired_source_phase = Arc::new(RetiredSourcePhase::default());
		let backend = PgNativeParticipation::new(PgNativeParticipationConfig {
			installation_binding: Arc::clone(&installation_binding),
			retired_source_phase: Arc::clone(&retired_source_phase),
			control_pool: config.control_pool.clone(),
			control_storage: control_storage.clone(),
			administration: Arc::clone(&administration),
			participant_storage: participant.storage().clone(),
			participant_namespace: participant.namespace().clone(),
			conflict_attempts: parameters::CONFLICT_ATTEMPTS,
		})
		.expect("shared control bindings were checked before native registration");
		let mut initial_optional_uses = application.initial_optional_uses;
		initial_optional_uses.extend(support.iter().map(|component| {
			crate::runtime::participation::state::catalog::ParticipantUseReference::Component {
				identifier: component.identifier().to_owned(),
			}
		}));
		fleet_scalar.register_support_and_uses(&mut support, &mut initial_optional_uses);
		if lease_source.is_none() {
			// Retained code is advertised without an active source dependency or physical adapter.
			support.push(ParticipantComponentSupport::from_declaration(
				&lease_source_bridge.declaration(),
			));
		}
		let participation = PgNativeParticipationRuntime::start(
			backend,
			NativeParticipationRuntimeStartOptions {
				timing: parameters::participation_timing(),
				component_support: support,
				document_roles: application.document_roles,
				raw_roles: application.raw_roles,
				primitive_scopes: application.primitive_scopes,
				payload_roles: application.payload_roles,
				optional_uses: initial_optional_uses,
			},
		)
		.await
		.map_err(NativeRuntimeStartupError::Participation)?;
		let admission = PgNativeAdmission::new(PgNativeAdmissionConfig {
			installation_binding,
			retired_source_phase,
			control_storage: control_storage.clone(),
			administration: Arc::clone(&administration),
			participant_storage: participant.storage().clone(),
			registration: participation.registration(),
			local_admission: participation.admission_tracker(),
		})
		.expect("admission retains the same checked control bindings as registration");
		let admission = Arc::new(admission);
		let originating_runtime = participation.originating_runtime();
		let group_budget = crate::records::postgres::group_attempt::PgRecordGroupAttemptBudget {
			conflict_attempts: parameters::CONFLICT_ATTEMPTS,
			identity_count: parameters::GROUP_IDENTITY_COUNT,
			whole_set_attempts: parameters::WHOLE_SET_ATTEMPTS,
		};
		let queue = queue_defs
			.into_runtime(Arc::clone(&admission), group_budget)
			.expect("Queue handle uses the same parsed mapping as native admission");
		let runtime = Self {
			participation,
			administration,
			lease_source_bridge,
			admission: Arc::clone(&admission),
			conflict_attempts: parameters::CONFLICT_ATTEMPTS,
			group_budget,
			semaphore_config: parameters::semaphore(),
			throttler_config: parameters::throttler(),
			counters: Arc::new(NativeCounterBindings {
				source: counter_source,
				component: counter,
				storage: counters.storage().clone(),
				admission: crate::runtime::admission::observation::PgOperationAdmission::Runtime(
					Arc::clone(&admission),
				),
				conflict_attempts: parameters::CONFLICT_ATTEMPTS,
				namespace: counters.namespace().clone(),
			}),
			component_ledger: Arc::new(
				crate::installation::component_ledger::NativeComponentLedgerBindings {
					component: ledger,
					storage: component_schemas.storage().clone(),
					namespace: component_schemas.namespace().clone(),
					admission: Arc::clone(&admission),
					conflict_attempts: parameters::CONFLICT_ATTEMPTS,
				},
			),
			raw_kv: Arc::new(NativeRawKvBindings {
				source: raw_kv_source,
				component: raw_kv,
				storage: raw_kv_binding.storage().clone(),
				admission: Arc::clone(&admission),
				conflict_attempts: parameters::CONFLICT_ATTEMPTS,
				namespace: raw_kv_binding.namespace().clone(),
			}),
			leases: Arc::new(NativeLeaseBindings {
				source: lease_source,
				component: lease_component,
				storage: lease_binding.storage().clone(),
				namespace: lease_binding.namespace().clone(),
				admission,
				conflict_attempts: parameters::CONFLICT_ATTEMPTS,
				// The runtime's work other than participation leaves one control connection free,
				// so that renewal never waits for that work's connections.
				control_pool: config.control_pool.clone_leaving_one_connection_free(),
				originating_runtime,
				timing: parameters::lease_timing(),
			}),
			app_pool: config.app_pool,
			holder_prefix: config.holder_prefix,
			managed_scopes: application.managed_scopes,
			queue_source_imports,
			queue,
			fleet_scalar,
		};
		let startup = NativeRuntimeStartupGuard {
			ownership: NativeRuntimeStartupOwnership::Checking(Box::new(runtime)),
		};
		let runtime = startup.runtime();
		let preflight = run_transaction_read(
			&runtime.app_pool,
			"db.native_runtime.admit_app",
			async |tx| {
				let counter_declaration = runtime.counters.component.declaration();
				let raw_declaration = runtime.raw_kv.component.declaration();
				let lease_declaration = runtime.leases.component.declaration();
				runtime
					.counters
					.admission
					.execute_read_with_contracts(tx, async move |contracts, tx| {
						runtime.queue.require_policies(&contracts)?;
						runtime
							.fleet_scalar
							.require_active_policies(&contracts)
							.map_err(|error| {
								NativeOperationError::Operation(
									NativeRuntimePreflightError::FleetScalar(error),
								)
							})?;
						let counter = contracts
							.bind(&counter_declaration)
							.map_err(NativeOperationError::Admission)?;
						let schema =
							counter
								.transitions()
								.bind_schema(&counter)
								.map_err(|error| {
									NativeOperationError::Admission(NativeAdmissionError::Contract(
										error,
									))
								})?;
						match contracts.storage_route() {
							InstallationStorageRoute::SourceCoexistence => {
								schema.require_source_writer()
							}
							InstallationStorageRoute::NativeFresh
							| InstallationStorageRoute::SourceRelocation
							| InstallationStorageRoute::NativeImported => schema.require_native_writer(),
						}
						.map_err(|error| {
							NativeOperationError::Operation(NativeRuntimePreflightError::Counter(
								error,
							))
						})?;
						let ledger_declaration = runtime.component_ledger.component.declaration();
						let ledger = contracts
							.bind(&ledger_declaration)
							.map_err(NativeOperationError::Admission)?;
						runtime
							.component_ledger
							.component
							.require_bound_policy(&ledger)
							.map_err(|error| {
								NativeOperationError::Admission(NativeAdmissionError::Contract(
									error,
								))
							})?;
						contracts
							.bind(&raw_declaration)
							.map_err(NativeOperationError::Admission)?;
						let lease = contracts
							.bind(&lease_declaration)
							.map_err(NativeOperationError::Admission)?;
						let lease_schema = lease
							.bind_schema(runtime.leases.component.schema())
							.map_err(|error| {
								NativeOperationError::Admission(NativeAdmissionError::Contract(
									error,
								))
							})?;
						if lease_schema.policy().writer().envelope
							!= StateStorageEnvelope::NativeJson
						{
							return Err(NativeOperationError::Operation(
								NativeRuntimePreflightError::LeaseRepresentation,
							));
						}
						if matches!(
							contracts.storage_route(),
							InstallationStorageRoute::SourceCoexistence
								| InstallationStorageRoute::SourceRelocation
						) {
							// Startup prepares the v10 adapters of all families together, so the
							// lease adapter is present exactly when every family has its adapter.
							let source = runtime.leases.source.as_ref().ok_or(
								NativeOperationError::Operation(
									NativeRuntimePreflightError::SourceMappingsAbsent,
								),
							)?;
							let source_declaration = source.bridge.declaration();
							let source_binding = contracts
								.bind(&source_declaration)
								.map_err(NativeOperationError::Admission)?;
							if contracts.storage_route()
								== InstallationStorageRoute::SourceCoexistence
							{
								source_binding
									.transitions()
									.require_sql_execution(&source_binding)
									.map_err(|error| {
										NativeOperationError::Operation(
											NativeRuntimePreflightError::SourceLeaseExecutionDef(
												error,
											),
										)
									})?;
							}
						}
						for container in containers {
							container.validate_existing(tx).await.map_err(|error| {
								NativeOperationError::Operation(
									NativeRuntimePreflightError::Schema(error),
								)
							})?;
						}
						Ok(())
					})
					.await
					.map_err(NativeRuntimePreflightError::from_operation)
			},
		)
		.await
		.and_then(|()| {
			runtime
				.participation
				.admission_tracker()
				.try_admit()
				.map(drop)
				.map_err(|error| {
					NativeRuntimePreflightError::Admission(NativeAdmissionError::Local(error))
				})
		});
		if let Err(operation) = preflight {
			let shutdown = startup.runtime().participation.initiate_shutdown();
			let cleanup = shutdown
				.wait_until_finished(parameters::STARTUP_CLEANUP_WAIT)
				.await
				.map_err(Box::new);
			return Err(NativeRuntimeStartupError::AppAdmission {
				operation: Box::new(operation),
				shutdown,
				cleanup,
			});
		}
		// Startup does not convert, promote, or request an installed policy change.
		Ok(startup.into_runtime())
	}

	/// Shares actual callable owners without retaining this runtime's lifecycle owner.
	pub(crate) fn target_code(
		&self,
	) -> Result<Arc<NativeRuntimeTargetCode>, NativeRuntimeTargetError> {
		Ok(Arc::new(NativeRuntimeTargetCode::new(
			NativeRuntimeTargetCodeFields {
				administration: Arc::clone(&self.administration),
				installation: Arc::clone(self.admission.installation_binding()),
				counters: Arc::clone(&self.counters),
				raw_kv: Arc::clone(&self.raw_kv),
				leases: Arc::clone(&self.leases),
				lease_source_bridge: Arc::clone(&self.lease_source_bridge),
				queue: NativeRuntimeTargetQueueCode {
					jobs: Arc::clone(self.queue.jobs()),
					auxiliary: Arc::clone(self.queue.auxiliary()),
					store: self.queue.store(),
				},
				queue_source_imports: self.queue_source_imports.clone(),
				fleet_scalar: NativeRuntimeTargetFleetScalarCode {
					code: Arc::clone(self.fleet_scalar.code()),
					source: self.fleet_scalar.source().cloned(),
				},
				scopes: Arc::clone(&self.managed_scopes),
			},
		)?))
	}

	/// Builds the handle for the installation's Queue; construction performs no database work.
	pub fn queue(
		&self,
		fields: crate::queue::PgQueueFields,
	) -> Result<crate::queue::PgQueue, crate::queue::PgQueueError> {
		crate::queue::PgQueue::new(
			self.queue.store(),
			self.native_lease(
				LeaseKey::from_parts(["paranoid.native-queue-cleanup.v1"])
					.expect("the Queue cleanup lease key is a valid lease key"),
			),
			self.holder_prefix.clone(),
			fields,
		)
	}

	/// Builds a handle for one counter; construction performs no database work.
	pub fn counter(&self, fields: crate::fleet::PgCounterFields) -> crate::fleet::PgCounter {
		let key = fields.key;
		let identity = RecordIdentity::new(
			self.counters.namespace.clone(),
			RecordKey::new(key.as_str().as_bytes().to_vec()),
		);
		crate::fleet::PgCounter::new(
			key,
			PgNativeCounter::new(Arc::clone(&self.counters), identity),
		)
	}

	pub(crate) fn cache_epoch(
		&self,
		key: crate::fleet::CoalescingCacheKey,
	) -> Result<
		crate::fleet::cache::epoch::PgNativeCacheEpoch,
		crate::fleet::cache::epoch::NativeCacheEpochError,
	> {
		use crate::fleet::cache::epoch::{NativeCacheEpochFields, PgNativeCacheEpoch};
		let fleet = &self.fleet_scalar;
		PgNativeCacheEpoch::new(NativeCacheEpochFields {
			key,
			code: Arc::clone(fleet.code()),
			source: fleet.source().cloned(),
			admission: Arc::clone(&self.admission),
			conflict_attempts: self.conflict_attempts,
		})
	}

	/// Builds the handle of one declared document scope without database work. The handle uses
	/// the document code that the application declared for the scope when it opened the
	/// installation. Each operation checks the installed policy and this runtime's roles for the
	/// scope.
	pub fn document<T, C, P>(
		&self,
		scope: &crate::scope::DocumentScope<T, C, P>,
	) -> Result<crate::kv::document::handle::PgDocument<'_, T, C, P>, PgScopeHandleError>
	where
		T: 'static,
		C: 'static,
		P: 'static,
	{
		use crate::scope::DocumentScopeDeclarationFields;
		use crate::scope::managed::ManagedScopeRegistration;
		let identity = scope.identity();
		let fail = |source| {
			PgScopeHandleError::new(PgScopeHandleErrorFields {
				scope: identity,
				source,
			})
		};
		let registration = match self.managed_scopes.lookup(identity) {
			Ok(ManagedScopeRegistration::Document(registration)) => registration,
			Ok(_) => return Err(fail(ScopeHandleCause::DeclarationMismatch)),
			Err(_) => return Err(fail(ScopeHandleCause::NotDeclared)),
		};
		let Some(code) = registration.typed_code::<T, C, P>() else {
			return Err(fail(ScopeHandleCause::DeclarationMismatch));
		};
		let declaration =
			crate::scope::DocumentScopeDeclaration::new(DocumentScopeDeclarationFields {
				identity: registration.scope(),
				document: code.document_schema(),
			});
		crate::kv::document::handle::PgDocument::new(
			crate::kv::document::handle::PgDocumentFields {
				declaration,
				admission: Arc::clone(&self.admission),
				conflict_attempts: self.conflict_attempts,
			},
		)
		.map_err(|error| fail(ScopeHandleCause::Admission(error)))
	}

	pub(crate) fn raw_kv(&self) -> PgNativeRawKvStore {
		PgNativeRawKvStore::new(Arc::clone(&self.raw_kv))
	}
	/// Builds the handle for the installation's core Raw namespace without database work. The
	/// core namespace needs no declaration, and it also holds the bytes that a v10 import moved:
	/// each moved value has the record key that `RecordKey::from(&key)` builds from its v10
	/// [`Key`](crate::kv::Key). Each operation checks the installed policy.
	pub fn core_raw(&self) -> crate::kv::PgRaw<'_> {
		crate::kv::PgRaw::core(self.raw_kv())
	}

	/// Builds a handle for one declared Raw scope without database work. It fails when the
	/// scope is not a declared Raw scope. Each operation checks the installed policy and this
	/// runtime's roles in the scope.
	pub fn raw(
		&self,
		fields: crate::kv::PgRawFields,
	) -> Result<crate::kv::PgRaw<'_>, PgScopeHandleError> {
		self.raw_scope(fields).map(crate::kv::PgRaw::scoped)
	}

	pub(crate) fn raw_scope(
		&self,
		fields: crate::kv::PgRawFields,
	) -> Result<crate::kv::raw::engine::scope::PgNativeRawScope<'_>, PgScopeHandleError> {
		use crate::scope::managed::ManagedScopeRegistration;
		let scope = fields.scope;
		let fail = |source| {
			PgScopeHandleError::new(PgScopeHandleErrorFields {
				scope: &scope,
				source,
			})
		};
		let declaration = match self.managed_scopes.lookup(&scope) {
			Ok(ManagedScopeRegistration::Raw(declaration)) => declaration,
			Ok(_) => return Err(fail(ScopeHandleCause::DeclarationMismatch)),
			Err(_) => return Err(fail(ScopeHandleCause::NotDeclared)),
		};
		crate::kv::raw::engine::scope::PgNativeRawScope::new(
			declaration,
			Arc::clone(&self.admission),
			self.conflict_attempts,
		)
		.map_err(|error| fail(ScopeHandleCause::Admission(error)))
	}
	/// Builds the handle of one Topic without database work. The handle uses the event codecs
	/// that the application declared for the Topic scope when it opened the installation.
	pub fn topic<T, C>(
		&self,
		fields: crate::fleet::PgTopicFields<'_, T, C>,
	) -> Result<crate::fleet::PgTopic<'_, T, C>, crate::fleet::PgTopicError<std::convert::Infallible>>
	where
		T: 'static,
		C: 'static,
	{
		use crate::fleet::PgTopicError;
		use crate::fleet::payload::scope::retained::RetainedPayloadCode;
		use crate::fleet::primitive_scope::ManagedPrimitiveRegistration;
		use crate::scope::managed::ManagedScopeRegistration;
		use crate::scope::payload::{PayloadSchemaDeclaration, PayloadSchemaDeclarationFields};
		let identity = fields.scope.identity();
		let fail = |source| {
			PgTopicError::scope_handle(PgScopeHandleError::new(PgScopeHandleErrorFields {
				scope: identity,
				source,
			}))
		};
		let code = match self.managed_scopes.lookup(identity) {
			Ok(ManagedScopeRegistration::Primitive(ManagedPrimitiveRegistration::TopicEvent(
				code,
			))) => code,
			Ok(_) => return Err(fail(ScopeHandleCause::DeclarationMismatch)),
			Err(_) => return Err(fail(ScopeHandleCause::NotDeclared)),
		};
		let RetainedPayloadCode::Schema(payload) = code.payload_code();
		let Some(schema) = payload.typed_schema::<T, C>() else {
			return Err(fail(ScopeHandleCause::DeclarationMismatch));
		};
		let payload = PayloadSchemaDeclaration::new(PayloadSchemaDeclarationFields {
			scope: code.identity(),
			schema,
		});
		self.native_topic(crate::fleet::topic::engine::NativeTopicConstructionFields {
			key: fields.key,
			declaration: code.declaration(),
			event_ttl: fields.event_ttl,
		})
		.map(|native| crate::fleet::PgTopic::new(native, payload))
		.map_err(PgTopicError::construction)
	}
	/// Builds the handle of one coalescing cache without database work. The handle uses the
	/// value codecs that the application declared for the cache scope when it opened the
	/// installation.
	pub fn coalescing_cache<T, C>(
		&self,
		fields: crate::fleet::PgCoalescingCacheFields<'_, T, C>,
	) -> Result<
		crate::fleet::PgCoalescingCache<'_, T, C>,
		crate::fleet::PgCoalescingCacheError<std::convert::Infallible>,
	>
	where
		T: 'static,
		C: 'static,
	{
		use crate::fleet::PgCoalescingCacheError;
		use crate::fleet::payload::scope::retained::RetainedPayloadCode;
		use crate::fleet::primitive_scope::ManagedPrimitiveRegistration;
		use crate::scope::managed::ManagedScopeRegistration;
		use crate::scope::payload::{PayloadSchemaDeclaration, PayloadSchemaDeclarationFields};
		let identity = fields.scope.identity();
		let fail = |source| {
			PgCoalescingCacheError::scope_handle(PgScopeHandleError::new(
				PgScopeHandleErrorFields {
					scope: identity,
					source,
				},
			))
		};
		let code = match self.managed_scopes.lookup(identity) {
			Ok(ManagedScopeRegistration::Primitive(ManagedPrimitiveRegistration::CacheEntry(
				code,
			))) => code,
			Ok(_) => return Err(fail(ScopeHandleCause::DeclarationMismatch)),
			Err(_) => return Err(fail(ScopeHandleCause::NotDeclared)),
		};
		let RetainedPayloadCode::Schema(payload) = code.payload_code();
		let Some(schema) = payload.typed_schema::<T, C>() else {
			return Err(fail(ScopeHandleCause::DeclarationMismatch));
		};
		let payload = PayloadSchemaDeclaration::new(PayloadSchemaDeclarationFields {
			scope: code.identity(),
			schema,
		});
		self.native_cache(crate::fleet::cache::engine::NativeCacheConstructionFields {
			key: fields.key,
			declaration: code.declaration(),
			options: fields.options.native(&self.holder_prefix),
		})
		.map(|native| crate::fleet::PgCoalescingCache::new(native, payload, self.app_pool.clone()))
		.map_err(PgCoalescingCacheError::construction)
	}
	pub(crate) fn native_cache<'scope, 'code>(
		&self,
		fields: crate::fleet::cache::engine::NativeCacheConstructionFields<'scope, 'code>,
	) -> Result<
		crate::fleet::cache::engine::PgNativeCache<'scope, 'code>,
		crate::fleet::cache::engine::NativeCacheConstructionError,
	> {
		use crate::fleet::cache::engine::{
			NativeCacheConstructionCause, NativeCacheConstructionError, NativeCacheFields,
			PgNativeCache,
		};
		let entry = self.cache_entry(fields.declaration).map_err(|error| {
			NativeCacheConstructionError::new(NativeCacheConstructionCause::Entry(error))
		})?;
		let epoch = self.cache_epoch(fields.key.clone()).map_err(|error| {
			NativeCacheConstructionError::new(NativeCacheConstructionCause::Epoch(error))
		})?;
		PgNativeCache::new(NativeCacheFields {
			key: fields.key,
			entry,
			epoch,
			leases: Arc::clone(&self.leases),
			options: fields.options,
		})
	}
	pub(crate) fn cache_entry<'scope, 'code>(
		&self,
		declaration: crate::fleet::payload::scope::cache::CacheEntryScopeDeclaration<'scope, 'code>,
	) -> Result<
		crate::fleet::payload::point::cache_handle::PgNativeCacheEntry<'scope, 'code>,
		crate::fleet::payload::point::factory::NativePayloadConstructionError,
	> {
		crate::fleet::payload::point::cache_handle::PgNativeCacheEntry::new(
			crate::fleet::payload::point::cache_handle::NativeCacheEntryFields {
				declaration,
				registry: &self.managed_scopes,
				admission: Arc::clone(&self.admission),
			},
		)
	}
	pub(crate) fn native_topic<'scope, 'code>(
		&self,
		fields: crate::fleet::topic::engine::NativeTopicConstructionFields<'scope, 'code>,
	) -> Result<
		crate::fleet::topic::engine::PgNativeTopic<'scope, 'code>,
		crate::fleet::topic::engine::NativeTopicConstructionError,
	> {
		use crate::fleet::topic::engine::{
			NativeTopicConstructionCause, NativeTopicConstructionError, NativeTopicFields,
			PgNativeTopic,
		};
		let fleet = &self.fleet_scalar;
		let event = self.topic_event(fields.declaration).map_err(|error| {
			NativeTopicConstructionError::new(NativeTopicConstructionCause::Event(error))
		})?;
		PgNativeTopic::new(NativeTopicFields {
			key: fields.key,
			event,
			code: Arc::clone(fleet.code()),
			admission: Arc::clone(&self.admission),
			event_ttl: fields.event_ttl,
			budget: self.group_budget,
		})
	}

	pub(crate) fn topic_event<'scope, 'code>(
		&self,
		declaration: crate::fleet::payload::scope::topic::TopicEventScopeDeclaration<'scope, 'code>,
	) -> Result<
		crate::fleet::payload::point::topic_handle::PgNativeTopicEvent<'scope, 'code>,
		crate::fleet::payload::point::factory::NativePayloadConstructionError,
	> {
		crate::fleet::payload::point::topic_handle::PgNativeTopicEvent::new(
			crate::fleet::payload::point::topic_handle::NativeTopicEventFields {
				declaration,
				registry: &self.managed_scopes,
				admission: Arc::clone(&self.admission),
			},
		)
	}
	/// Migrates one application component schema in one transaction: Paranoid locks the
	/// component's ledger entry, records the new version, and runs the fresh install or the
	/// ordered migration steps and then the validation checks. A failed statement or check
	/// rolls back the whole transaction, including the recorded version. The ledger entry
	/// serializes concurrent instances. While a published v10 release still
	/// writes, the ledger entry is the v10 ledger row that the published release uses.
	/// A pool starts an owned transaction; a supplied transaction includes the migration
	/// in its own eventual commit or rollback.
	/// An explicitly compatible newer layout is validated without changing its ledger
	/// entry or running SQL migrations. Older layouts follow the declared migration
	/// chain to the installation target even when they are declared compatible.
	pub async fn migrate_component_schema(
		&self,
		database: impl crate::db::WriteDatabase,
		schema: &crate::installation::ComponentSchema<'_>,
	) -> Result<
		crate::installation::ComponentSchemaMigrationResult,
		crate::installation::PgComponentSchemaError,
	> {
		self.component_ledger
			.migrate(database, schema)
			.await
			.map_err(crate::installation::PgComponentSchemaError::new)
	}

	/// Checks that the ledger records the installation target or an explicitly compatible
	/// version and fingerprint, then runs that layout's checks without changing anything.
	/// Validation rechecks the ledger after physical checks and retries a changed
	/// version within five validation attempts. It reports an observation; it does not prevent
	/// a later component migration or protect subsequent arbitrary application SQL.
	pub async fn validate_component_schema(
		&self,
		database: impl crate::db::Database,
		schema: &crate::installation::ComponentSchema<'_>,
	) -> Result<(), crate::installation::PgComponentSchemaError> {
		self.component_ledger
			.validate(database, schema)
			.await
			.map_err(crate::installation::PgComponentSchemaError::new)
	}

	/// Reports the version and fingerprint that the ledger records for `schema`'s component
	/// instance, whatever version `schema` expects. It runs no validation check.
	pub async fn fetch_component_registration(
		&self,
		database: impl crate::db::Database,
		schema: &crate::installation::ComponentSchema<'_>,
	) -> Result<
		crate::installation::ComponentSchemaRegistration,
		crate::installation::PgComponentSchemaError,
	> {
		self.component_ledger
			.fetch(database, schema)
			.await
			.map_err(crate::installation::PgComponentSchemaError::new)
	}

	/// Builds the handle for one mutex; construction performs no database work. On an
	/// installation imported from v10, the mutex's lease key is the one that v10 uses for the
	/// same mutex key.
	pub fn mutex(
		&self,
		fields: crate::fleet::PgMutexFields,
	) -> Result<crate::fleet::PgMutex, crate::fleet::PgLeaseKeyError> {
		let lease_key = match self.admission.installation_binding().as_ref() {
			PgInstallationAdmissionBinding::Imported { source, .. } => {
				crate::fleet::namespace_keys::build_mutex_lease_key(
					source.fleet_root(),
					&fields.key,
				)
			}
			PgInstallationAdmissionBinding::Fresh { .. } => {
				LeaseKey::from_parts(["paranoid.native-mutex.v1", fields.key.as_str()])
			}
		}
		.map_err(crate::fleet::PgLeaseKeyError::new)?;
		Ok(crate::fleet::PgMutex::new(
			fields,
			self.native_lease(lease_key),
			self.holder_prefix.clone(),
		))
	}

	/// Builds the handle for one cron; construction performs no database work. On an
	/// installation imported from v10, the cron's lease key is the one that v10 uses for the
	/// same cron key.
	pub fn cron(
		&self,
		fields: crate::fleet::PgCronFields,
	) -> Result<crate::fleet::PgCron, crate::fleet::PgCronError> {
		let lease_key = match self.admission.installation_binding().as_ref() {
			PgInstallationAdmissionBinding::Imported { source, .. } => {
				crate::fleet::namespace_keys::build_cron_mutex_lease_key(
					source.fleet_root(),
					&fields.key,
				)
			}
			PgInstallationAdmissionBinding::Fresh { .. } => {
				LeaseKey::from_parts(["paranoid.native-cron.v1", fields.key.as_str()])
			}
		}
		.map_err(|error| {
			crate::fleet::PgCronError::LeaseKey(crate::fleet::PgLeaseKeyError::new(error))
		})?;
		crate::fleet::PgCron::new(
			fields,
			self.native_lease(lease_key),
			self.holder_prefix.clone(),
		)
	}

	/// Builds a handle for one lease; construction performs no database work.
	pub fn lease(&self, fields: crate::fleet::PgLeaseFields) -> crate::fleet::PgLease {
		crate::fleet::PgLease::new(self.native_lease(fields.key), self.holder_prefix.clone())
	}

	pub(crate) fn native_lease(&self, key: LeaseKey) -> PgNativeLease {
		PgNativeLease::new(Arc::clone(&self.leases), key)
	}

	pub(crate) fn app_pool(&self) -> &WritePool {
		&self.app_pool
	}
	/// An observer of the runtime's lifecycle. The observer does not keep the runtime running.
	pub fn observer(&self) -> lifecycle::PgRuntimeObserver {
		lifecycle::PgRuntimeObserver {
			observer: self.participation.observer(),
		}
	}

	/// Stops new operations, and starts a task that waits for the runtime's work to end and
	/// then removes the runtime from the installation's participants. A repeated call returns
	/// a handle to the same shutdown. Dropping a handle does not cancel the shutdown. A runtime
	/// that several tasks share, for example in an `Arc`, can be shut down from any of them;
	/// their later operations fail.
	pub fn initiate_shutdown(&self) -> lifecycle::PgRuntimeShutdown {
		lifecycle::PgRuntimeShutdown {
			shutdown: self.participation.initiate_shutdown(),
		}
	}
}

/// Startup admission and physical readiness errors retain actual transaction failure causes.
#[derive(Debug, thiserror::Error)]
enum NativeRuntimePreflightError {
	#[error("source lease coexistence policy does not select executable SQL semantics")]
	SourceLeaseExecutionDef(#[source] crate::fleet::lease::LeaseSourceBindingError),
	#[error("native runtime application admission failed")]
	Admission(#[source] NativeAdmissionError),
	#[error("native runtime Counter mapping cannot use its selected representation")]
	Counter(#[source] CounterRepresentationError),
	#[error("native runtime Queue policies cannot bind their implementations")]
	QueuePolicy(#[source] crate::queue::QueueGroupError),
	#[error("native runtime Fleet scalar active policies cannot bind their implementations")]
	FleetScalar(#[source] NativeRuntimeFleetScalarError),
	#[error("native runtime source Queue implementations cannot bind the installed route")]
	QueueSource(#[source] Box<crate::queue::engine::NativeQueueGroupError>),
	#[error("native runtime lease mapping requires an installed native JSON writer")]
	LeaseRepresentation,
	#[error("native runtime source phase requires its prepared source mappings")]
	SourceMappingsAbsent,
	#[error("native runtime physical schema validation failed")]
	Schema(#[source] SchemaError),
	#[error("native runtime preflight database work failed")]
	Database(#[source] DatabaseError),
	#[error("native runtime preflight transaction protocol failed")]
	Protocol(#[source] ProtocolError),
	#[error("native runtime preflight failed and rollback also failed")]
	RollbackFailed {
		#[source]
		operation_error: Box<Self>,
		rollback_error: DatabaseError,
	},
}

impl NativeRuntimePreflightError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Database(error) => Some(error),
			Self::Admission(error) => error.database_error(),
			Self::Schema(error) => error.database_error(),
			Self::QueueSource(error) => error.database_error(),
			Self::FleetScalar(NativeRuntimeFleetScalarError::Admission(error)) => {
				error.database_error()
			}
			Self::RollbackFailed {
				operation_error,
				rollback_error,
			} => operation_error.database_error().or(Some(rollback_error)),
			_ => None,
		}
	}
	fn from_operation(error: NativeOperationError<Self>) -> Self {
		match error {
			NativeOperationError::Admission(error) => Self::Admission(error),
			NativeOperationError::Operation(error) => error,
		}
	}
}

impl TransactionFailure for NativeRuntimePreflightError {
	fn from_protocol_error(error: ProtocolError) -> Self {
		Self::Protocol(error)
	}
	fn from_database_error(error: DatabaseError) -> Self {
		Self::Database(error)
	}
	fn from_rollback_failure(operation_error: Self, rollback_error: DatabaseError) -> Self {
		Self::RollbackFailed {
			operation_error: Box::new(operation_error),
			rollback_error,
		}
	}
	fn involves_database_failure(&self) -> bool {
		match self {
			Self::Database(_) | Self::RollbackFailed { .. } => true,
			Self::Admission(error) => error.involves_database_failure(),
			Self::Schema(error) => error.involves_database_failure(),
			Self::QueueSource(error) => error.involves_database_failure(),
			Self::FleetScalar(NativeRuntimeFleetScalarError::Admission(error)) => {
				error.involves_database_failure()
			}
			Self::FleetScalar(_) => false,
			Self::SourceLeaseExecutionDef(_)
			| Self::QueuePolicy(_)
			| Self::Counter(_)
			| Self::LeaseRepresentation
			| Self::SourceMappingsAbsent
			| Self::Protocol(_) => false,
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Database(error) => error.commit_outcome_is_unknown(),
			Self::Schema(error) => error.commit_outcome_is_unknown(),
			Self::QueueSource(error) => error.commit_outcome_is_unknown(),
			Self::FleetScalar(NativeRuntimeFleetScalarError::Admission(error)) => {
				error.commit_outcome_is_unknown()
			}
			Self::FleetScalar(_) => false,
			Self::RollbackFailed {
				operation_error, ..
			} => operation_error.commit_outcome_is_unknown(),
			Self::Admission(error) => error.commit_outcome_is_unknown(),
			Self::SourceLeaseExecutionDef(_)
			| Self::QueuePolicy(_)
			| Self::Counter(_)
			| Self::LeaseRepresentation
			| Self::SourceMappingsAbsent
			| Self::Protocol(_) => false,
		}
	}
}
