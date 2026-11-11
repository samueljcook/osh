//! Exact runtime-owned installation services for explicit upgrade execution.

use std::{sync::Arc, time::Duration};

use crate::upgrade::engine::NativeUpgradeExecutionServices;
use crate::upgrade::engine::driver::error::NativeUpgradeDriverError;
use crate::upgrade::engine::driver::observer::NativeUpgradeDriverObserver;
use crate::upgrade::engine::driver::{
	NativeUpgradeDriverRequest, NativeUpgradeDriverStartFields, start_upgrade_driver,
};
use crate::upgrade::participants::{
	NativeUpgradeParticipantsError, PgNativeUpgradeParticipants, PgNativeUpgradeParticipantsConfig,
};
use crate::upgrade::requests::work::creation::{
	UpgradeCreationError, UpgradeCreationIntent, UpgradeCreationIntentFields,
};
use crate::upgrade::requests::{
	NativeUpgradeRequestError, PgNativeUpgradeRequests, PgNativeUpgradeRequestsConfig,
};

use super::targets::NativeRuntimeTargetError;
use super::{PgRuntime, parameters};
use crate::queue::QueueRecoveryPolicy;
use crate::runtime::admission::bindings::InstallationStorageRoute;
use crate::runtime::local_admission::upgrade_work::RuntimeUpgradeWorkIdentity;
use crate::scope::selection::ScopeSelections;
use crate::upgrade::requests::work::slot::NativeUpgradeSlotObservation;
use crate::upgrade::work::SourceGoal;
use crate::upgrade::{PgUpgradeController, PgUpgradeStartError};

pub(crate) mod selection;

pub(crate) struct NativeRuntimeUpgradeServicesFields {
	pub(crate) control_pin_wait_timeout: Duration,
}

/// The work that one upgrade driver performs.
pub(crate) enum PgUpgradeRequest {
	/// Creates a new upgrade with this target.
	Create(PgUpgradeCreateFields),
	/// Continues the recorded upgrade.
	Resume(RuntimeUpgradeWorkIdentity),
	/// Observes the recorded upgrade without requiring code for its target.
	Inspect(RuntimeUpgradeWorkIdentity),
	/// Cancels the recorded upgrade.
	Cancel(RuntimeUpgradeWorkIdentity),
}

/// The complete target of a new upgrade.
pub(crate) struct PgUpgradeCreateFields {
	pub(crate) core: selection::UpgradeCoreSelection,
	/// Replacement scope selections; unlisted installed scopes keep their meaning. Fleet
	/// assignments also contribute their destination policies.
	pub(crate) scopes: Option<ScopeSelections>,
	pub(crate) source_goal: SourceGoal,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeRuntimeUpgradeStartError {
	#[error("installing the Queue task-selection index failed")]
	QueueTaskIndex(
		#[source] Box<crate::installation::component_ledger::NativeComponentLedgerError>,
	),
	#[error("installing the Queue job-listing index failed")]
	QueueListIndex(
		#[source] Box<crate::installation::component_ledger::NativeComponentLedgerError>,
	),
	#[error("installing the record expiry indexes failed")]
	RecordExpiryIndex(
		#[source] Box<crate::installation::component_ledger::NativeComponentLedgerError>,
	),
	#[error("native runtime upgrade service construction failed")]
	Services(#[source] Box<NativeRuntimeUpgradeServicesError>),
	#[error("reading the installation's recorded upgrade failed")]
	Read(#[source] Box<NativeUpgradeRequestError>),
	#[error("reading the installed Queue recovery settings failed")]
	QueueRecovery(#[source] Box<crate::queue::engine::NativeQueueGroupError>),
	#[error("the installation has an unfinished upgrade, which must be resumed or cancelled first")]
	Unfinished,
	#[error("the installation is retiring v10, and that upgrade must finish first")]
	V10RetirementUnfinished,
	#[error("the installation did not import v10")]
	NotImportedFromV10,
	#[error("the installation already retired v10")]
	V10AlreadyRetired,
	#[error("this runtime's declarations do not form a valid upgrade target")]
	Scopes(#[source] Box<crate::scope::selection::ScopeSelectionError>),
	#[error("native runtime upgrade code is invalid")]
	Code(#[source] Box<NativeRuntimeTargetError>),
	#[error("native runtime upgrade target choices do not bind code")]
	Selection(#[source] Box<selection::UpgradeTargetSelectionError>),
	#[error("native runtime fresh upgrade intent is invalid")]
	Intent(#[source] Box<UpgradeCreationError>),
	#[error("native runtime upgrade task could not start")]
	Driver(#[source] Box<NativeUpgradeDriverError>),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeRuntimeUpgradeServicesError {
	#[error("native upgrade request service construction failed")]
	Requests(#[source] Box<NativeUpgradeRequestError>),
	#[error("native upgrade participant service construction failed")]
	Participants(#[source] Box<NativeUpgradeParticipantsError>),
}

impl PgRuntime {
	/// Starts an upgrade of the installation's scopes and library-owned representations. For each
	/// declared scope, the newest version that the code writes as JSON becomes the writer
	/// version, and the upgrade converts every record stored in another version to it, after
	/// which the installation requires only the writer version; an index becomes available for
	/// queries when the declaration queries it or the installation already has it available;
	/// and every other declared index is maintained. Scopes that this runtime does not declare
	/// keep their installed state.
	///
	/// On native storage, this upgrade separates Queue payloads from mutable job metadata.
	/// It waits for incompatible deployments to stop before enabling the new writer, converts
	/// existing jobs in bounded transactions, and then retires the inline payload reader.
	/// Public job reads still include payloads. Imported installations retain inline payloads
	/// until v10 data retirement, which also performs this Queue representation upgrade.
	///
	/// A declared scope that the installation does not have is added empty. A runtime adds
	/// such a scope itself when it starts, unless an upgrade is pending (see
	/// [`PgRuntime::start`]), so a release can declare a new scope's roles
	/// [`Active`](crate::scope::RoleUse::Active) at once. An index becomes ready for queries
	/// only through this upgrade, also an index of a scope that a starting runtime added, so a
	/// release that adds a query declares it [`Dormant`](crate::scope::InitialUse::Dormant)
	/// and runs this upgrade, and a later release declares it `Active`; see
	/// [`QueryUseIntent`](crate::scope::QueryUseIntent).
	///
	/// The code must read every version that the installation requires when the upgrade
	/// starts because records in those versions remain until the upgrade converts them. A
	/// release that stops reading a version must therefore follow a release with a completed
	/// upgrade.
	///
	/// Before starting the upgrade task, this method installs the additive Queue task-selection
	/// index, the Queue job-listing index, and the Raw record expiry indexes through the
	/// transactional schema ledger. Existing installations need this call to obtain those indexes.
	/// Index creation can wait for writers and blocks writes to each table while PostgreSQL
	/// builds the index; each lock wait is bounded to five seconds.
	/// Each index and its ledger entry commit independently of later scope conversion.
	///
	/// The upgrade runs as a task of this runtime. Dropping the controller does not stop the
	/// task; the Tokio runtime that started it must stay alive while the upgrade runs. The
	/// upgrade waits until every running deployment can use the new versions, so an older
	/// deployment must stop first. When the installation already has an unfinished upgrade,
	/// starting fails; resume or cancel that upgrade first. The upgrade is recorded in the
	/// installation, so it survives a process restart.
	pub async fn start_upgrade(&self) -> Result<PgUpgradeController, PgUpgradeStartError> {
		self.start_created_upgrade(UpgradeKind::Scopes)
			.await
			.map_err(PgUpgradeStartError::new)
	}

	/// Retires the published v10 installation that this runtime's installation imported: v10
	/// deployments can no longer write, and the v10 records move to Paranoid's storage. Key-value
	/// values, counters, Once completions, cache epochs, Queue jobs, and leases move; the v10
	/// Semaphore, Throttler, cache value, and Topic records are deleted because this runtime's
	/// scopes keep their own. The installation keeps its Queue recovery settings. Start it only
	/// after no v10 deployment remains because v10 deployments cannot register their presence.
	///
	/// While the retirement moves the v10 records, the Queue keeps working: workers claim,
	/// reclaim, and clean up jobs wherever the move has left them, the operator operations of
	/// `PgQueue` act on a job, a dead letter, or a pause wherever the move has left it, and
	/// `PgQueueReader::fetch_job`, `list_jobs`, and `list_dead_letters` read both stores.
	///
	/// Retirement ends every claim on a v10 Queue job, including the claim of a job that this
	/// runtime's worker is running. The worker's next operation on that job then fails with
	/// [`PgQueueErrorKind::ClaimLost`](crate::queue::PgQueueErrorKind::ClaimLost). Each such job
	/// returns to pending once its claim is older than the installation's stale threshold,
	/// without spending a retry, so it runs again.
	///
	/// Before starting retirement, this method installs the same additive selection indexes as
	/// [`PgRuntime::start_upgrade`], with the same locking and commit behavior.
	/// The retirement runs as a task of this runtime, like an upgrade.
	pub async fn retire_v10(&self) -> Result<PgUpgradeController, PgUpgradeStartError> {
		self.start_created_upgrade(UpgradeKind::RetireV10)
			.await
			.map_err(PgUpgradeStartError::new)
	}

	/// Continues the installation's unfinished upgrade or v10 retirement, for example after
	/// the deployment that started it stopped. `None` means that the installation has no
	/// unfinished upgrade.
	pub async fn resume_upgrade(&self) -> Result<Option<PgUpgradeController>, PgUpgradeStartError> {
		self.start_recorded(RecordedRequest::Resume, true)
			.await
			.map_err(PgUpgradeStartError::new)
	}

	/// Reports the installation's most recent upgrade without continuing it. `None` means that
	/// the installation records no upgrade.
	pub async fn inspect_upgrade(
		&self,
	) -> Result<Option<PgUpgradeController>, PgUpgradeStartError> {
		self.start_recorded(RecordedRequest::Inspect, false)
			.await
			.map_err(PgUpgradeStartError::new)
	}

	/// Cancels the installation's unfinished upgrade. The installed scope versions and the data
	/// that the upgrade already converted stay in effect. `None` means that the installation
	/// has no unfinished upgrade.
	pub async fn cancel_upgrade(&self) -> Result<Option<PgUpgradeController>, PgUpgradeStartError> {
		self.start_recorded(RecordedRequest::Cancel, true)
			.await
			.map_err(PgUpgradeStartError::new)
	}

	async fn read_upgrade_slot(
		&self,
	) -> Result<NativeUpgradeSlotObservation, NativeRuntimeUpgradeStartError> {
		self.upgrade_services(self.upgrade_services_fields())
			.map_err(|error| NativeRuntimeUpgradeStartError::Services(Box::new(error)))?
			.requests
			.read_upgrade_slot()
			.await
			.map_err(|error| NativeRuntimeUpgradeStartError::Read(Box::new(error)))
	}

	async fn start_created_upgrade(
		&self,
		kind: UpgradeKind,
	) -> Result<PgUpgradeController, NativeRuntimeUpgradeStartError> {
		let slot = self.read_upgrade_slot().await?;
		if slot.entry.as_ref().is_some_and(|entry| entry.unfinished) {
			return Err(NativeRuntimeUpgradeStartError::Unfinished);
		}
		let queue_recovery = self
			.queue
			.store()
			.installed_recovery_policy(&self.app_pool)
			.await
			.map_err(|error| NativeRuntimeUpgradeStartError::QueueRecovery(Box::new(error)))?;
		let (core, scopes, source_goal) = match kind {
			UpgradeKind::Scopes => {
				let scopes = self
					.managed_scopes
					.target_selections(&slot.scopes)
					.map_err(|error| NativeRuntimeUpgradeStartError::Scopes(Box::new(error)))?;
				(
					self.installed_core(slot.route, queue_recovery)?,
					(!scopes.is_empty()).then_some(scopes),
					SourceGoal::Unchanged,
				)
			}
			UpgradeKind::RetireV10 => {
				match slot.route {
					InstallationStorageRoute::NativeFresh => {
						return Err(NativeRuntimeUpgradeStartError::NotImportedFromV10);
					}
					InstallationStorageRoute::NativeImported => {
						return Err(NativeRuntimeUpgradeStartError::V10AlreadyRetired);
					}
					// No unfinished upgrade is recorded here, so a retirement that ended without
					// finishing, for example because it was cancelled after v10 writers retired,
					// left the installation on the relocation route; a new retirement finishes it.
					InstallationStorageRoute::SourceRelocation
					| InstallationStorageRoute::SourceCoexistence => {}
				}
				(
					self.native_core(queue_recovery),
					None,
					SourceGoal::DataRetired,
				)
			}
		};
		self.install_queue_task_index()
			.await
			.map_err(|error| NativeRuntimeUpgradeStartError::QueueTaskIndex(Box::new(error)))?;
		self.install_queue_list_index()
			.await
			.map_err(|error| NativeRuntimeUpgradeStartError::QueueListIndex(Box::new(error)))?;
		self.install_record_expiry_indexes()
			.await
			.map_err(|error| NativeRuntimeUpgradeStartError::RecordExpiryIndex(Box::new(error)))?;
		self.start_driver(PgUpgradeRequest::Create(PgUpgradeCreateFields {
			core,
			scopes,
			source_goal,
		}))
	}

	async fn start_recorded(
		&self,
		request: RecordedRequest,
		only_unfinished: bool,
	) -> Result<Option<PgUpgradeController>, NativeRuntimeUpgradeStartError> {
		let slot = self.read_upgrade_slot().await?;
		let Some(entry) = slot.entry else {
			return Ok(None);
		};
		if only_unfinished && !entry.unfinished {
			return Ok(None);
		}
		let identity = entry.identity;
		self.start_driver(match request {
			RecordedRequest::Resume => PgUpgradeRequest::Resume(identity),
			RecordedRequest::Inspect => PgUpgradeRequest::Inspect(identity),
			RecordedRequest::Cancel => PgUpgradeRequest::Cancel(identity),
		})
		.map(Some)
	}

	/// The core target that keeps the installation's installed meaning on its storage route.
	fn installed_core(
		&self,
		route: InstallationStorageRoute,
		queue_recovery: QueueRecoveryPolicy,
	) -> Result<selection::UpgradeCoreSelection, NativeRuntimeUpgradeStartError> {
		Ok(match route {
			InstallationStorageRoute::NativeFresh | InstallationStorageRoute::NativeImported => {
				self.native_core(queue_recovery)
			}
			InstallationStorageRoute::SourceRelocation => {
				return Err(NativeRuntimeUpgradeStartError::V10RetirementUnfinished);
			}
			InstallationStorageRoute::SourceCoexistence => selection::UpgradeCoreSelection {
				source: selection::CoreSourceRetention::RetainV10,
				queue_recovery,
			},
		})
	}

	fn native_core(&self, queue_recovery: QueueRecoveryPolicy) -> selection::UpgradeCoreSelection {
		selection::UpgradeCoreSelection {
			source: selection::CoreSourceRetention::NativeOnly,
			queue_recovery,
		}
	}

	fn upgrade_services_fields(&self) -> NativeRuntimeUpgradeServicesFields {
		NativeRuntimeUpgradeServicesFields {
			control_pin_wait_timeout: parameters::UPGRADE_CONTROL_PIN_WAIT,
		}
	}

	fn start_driver(
		&self,
		request: PgUpgradeRequest,
	) -> Result<PgUpgradeController, NativeRuntimeUpgradeStartError> {
		let observer = self.start_upgrade_driver(request)?;
		Ok(PgUpgradeController::new(observer))
	}

	fn start_upgrade_driver(
		&self,
		request: PgUpgradeRequest,
	) -> Result<NativeUpgradeDriverObserver, NativeRuntimeUpgradeStartError> {
		let services = self
			.upgrade_services(self.upgrade_services_fields())
			.map_err(|error| NativeRuntimeUpgradeStartError::Services(Box::new(error)))?;
		let code = self
			.target_code()
			.map_err(|error| NativeRuntimeUpgradeStartError::Code(Box::new(error)))?;
		let request = match request {
			PgUpgradeRequest::Create(PgUpgradeCreateFields {
				core,
				scopes,
				source_goal,
			}) => {
				let candidate =
					selection::assemble_target(selection::UpgradeTargetAssemblyFields {
						code: &code,
						core,
						scopes,
					})
					.map_err(|error| NativeRuntimeUpgradeStartError::Selection(Box::new(error)))?;
				let intent = UpgradeCreationIntent::new(UpgradeCreationIntentFields {
					code,
					candidate: Arc::new(candidate),
					source_goal,
				})
				.map_err(|error| NativeRuntimeUpgradeStartError::Intent(Box::new(error)))?;
				NativeUpgradeDriverRequest::Create(Arc::new(intent))
			}
			PgUpgradeRequest::Resume(identity) => {
				NativeUpgradeDriverRequest::Resume { identity, code }
			}
			PgUpgradeRequest::Inspect(identity) => {
				NativeUpgradeDriverRequest::Inspect { identity, code }
			}
			PgUpgradeRequest::Cancel(identity) => {
				NativeUpgradeDriverRequest::Cancel { identity, code }
			}
		};
		start_upgrade_driver(NativeUpgradeDriverStartFields {
			services,
			request,
			runtime: self.participation.originating_runtime(),
			options: parameters::upgrade_driver_options(),
			execution_limits: parameters::upgrade_execution_limits(),
		})
		.map_err(|error| NativeRuntimeUpgradeStartError::Driver(Box::new(error)))
	}

	/// Prepares associated services without starting work, changing active uses or performing IO.
	pub(crate) fn upgrade_services(
		&self,
		fields: NativeRuntimeUpgradeServicesFields,
	) -> Result<Arc<NativeUpgradeExecutionServices>, NativeRuntimeUpgradeServicesError> {
		let native = self.administration.native_bindings();
		let requests = PgNativeUpgradeRequests::new(PgNativeUpgradeRequestsConfig {
			installation_binding: Arc::clone(self.admission.installation_binding()),
			control_pool: self.leases.control_pool.clone(),
			control_storage: native.control().clone(),
			administration: Arc::clone(&self.administration),
			conflict_attempts: self.conflict_attempts,
			control_pin_wait_timeout: fields.control_pin_wait_timeout,
		})
		.map_err(|error| NativeRuntimeUpgradeServicesError::Requests(Box::new(error)))?;
		let participants = PgNativeUpgradeParticipants::new(PgNativeUpgradeParticipantsConfig {
			installation_binding: Arc::clone(self.admission.installation_binding()),
			control_pool: self.leases.control_pool.clone(),
			control_storage: native.control().clone(),
			administration: Arc::clone(&self.administration),
			participant_storage: native.participants().storage().clone(),
			participant_namespace: native.participants().namespace().clone(),
			conflict_attempts: self.conflict_attempts,
			control_pin_wait_timeout: fields.control_pin_wait_timeout,
		})
		.map_err(|error| NativeRuntimeUpgradeServicesError::Participants(Box::new(error)))?;
		Ok(Arc::new(NativeUpgradeExecutionServices {
			admission: Arc::clone(&self.admission),
			requests: Arc::new(requests),
			participants: Arc::new(participants),
		}))
	}
}

enum UpgradeKind {
	Scopes,
	RetireV10,
}

enum RecordedRequest {
	Resume,
	Inspect,
	Cancel,
}
