//! Actual optional Queue declarations and admitted native group handles for one runtime.

use super::{
	Arc, InstallationStorageRoute, NativeOperationError, NativeRuntimePreflightError,
	ParticipantComponentSupport, PgNativeAdmission,
};
use crate::installation::creation::PgNativeQueueStorage;
use crate::queue::engine::{
	NativeQueueBindings, NativeQueueGroupError, NativeQueueSourceBindings, PgNativeQueueGroup,
};
use crate::queue::{QueueAuxiliaryComponent, QueueGroupError, QueueJobComponent};
use crate::records::postgres::group_attempt::PgRecordGroupAttemptBudget;

pub(super) struct NativeRuntimeQueueDefs {
	storage: PgNativeQueueStorage,
	source: Option<Arc<NativeQueueSourceBindings>>,
	jobs: Arc<QueueJobComponent>,
	auxiliary: Arc<QueueAuxiliaryComponent>,
}
pub(super) struct NativeRuntimeQueue {
	store: PgNativeQueueGroup,
	source: Option<Arc<NativeQueueSourceBindings>>,
	jobs: Arc<QueueJobComponent>,
	auxiliary: Arc<QueueAuxiliaryComponent>,
}
impl NativeRuntimeQueueDefs {
	pub(super) fn new(
		storage: PgNativeQueueStorage,
		source: Option<Arc<NativeQueueSourceBindings>>,
	) -> Self {
		Self {
			storage,
			source,
			jobs: Arc::new(QueueJobComponent::new()),
			auxiliary: Arc::new(QueueAuxiliaryComponent::new()),
		}
	}
	pub(super) fn register_support(&self, support: &mut Vec<ParticipantComponentSupport>) {
		if let Some(source) = &self.source {
			support.push(ParticipantComponentSupport::from_declaration(
				&source.imports.job_declaration(),
			));
			support.push(ParticipantComponentSupport::from_declaration(
				&source.imports.auxiliary_declaration(),
			));
		}
		support.push(ParticipantComponentSupport::from_declaration(
			&self.jobs.declaration(),
		));
		support.push(ParticipantComponentSupport::from_declaration(
			&self.auxiliary.declaration(),
		));
	}
	pub(super) fn into_runtime(
		self,
		admission: Arc<PgNativeAdmission>,
		attempt_budget: PgRecordGroupAttemptBudget,
	) -> Result<NativeRuntimeQueue, NativeQueueGroupError> {
		let store = PgNativeQueueGroup::new(NativeQueueBindings {
			source: self.source.clone(),
			jobs: Arc::clone(&self.jobs),
			auxiliary: Arc::clone(&self.auxiliary),
			storage: self.storage,
			admission: crate::runtime::admission::observation::PgOperationAdmission::Runtime(
				admission,
			),
			attempt_budget,
		})?;
		Ok(NativeRuntimeQueue {
			store,
			source: self.source,
			jobs: self.jobs,
			auxiliary: self.auxiliary,
		})
	}
}
impl NativeRuntimeQueue {
	pub(super) fn store(&self) -> PgNativeQueueGroup {
		self.store.clone()
	}
	pub(super) fn jobs(&self) -> &Arc<QueueJobComponent> {
		&self.jobs
	}
	pub(super) fn auxiliary(&self) -> &Arc<QueueAuxiliaryComponent> {
		&self.auxiliary
	}

	pub(super) fn require_policies(
		&self,
		contracts: &crate::runtime::admission::NativeAdmittedContracts,
	) -> Result<(), NativeOperationError<NativeRuntimePreflightError>> {
		if matches!(
			contracts.storage_route(),
			InstallationStorageRoute::SourceCoexistence
				| InstallationStorageRoute::SourceRelocation
		) {
			let source = self.source.as_ref().ok_or_else(|| {
				NativeOperationError::Operation(NativeRuntimePreflightError::QueueSource(Box::new(
					NativeQueueGroupError::SourceMappingAbsent,
				)))
			})?;
			let job_declaration = source.imports.job_declaration();
			let job_binding = contracts
				.bind(&job_declaration)
				.map_err(NativeOperationError::Admission)?;
			let auxiliary_declaration = source.imports.auxiliary_declaration();
			let auxiliary_binding = contracts
				.bind(&auxiliary_declaration)
				.map_err(NativeOperationError::Admission)?;
			source
				.imports
				.require_bound_pair(
					&job_binding,
					&auxiliary_binding,
					source.storage.descriptor().layout().queue_layout(),
				)
				.map_err(|error| {
					NativeOperationError::Operation(NativeRuntimePreflightError::QueueSource(
						Box::new(NativeQueueGroupError::SourceObservation(error.into())),
					))
				})?;
		}
		let declaration = self.jobs.declaration();
		let binding = contracts
			.bind(&declaration)
			.map_err(NativeOperationError::Admission)?;
		let job_check = self.jobs.require_bound_policy(&binding);
		job_check.map_err(|error| {
			NativeOperationError::Operation(NativeRuntimePreflightError::QueuePolicy(
				QueueGroupError::JobBinding(error),
			))
		})?;
		let declaration = self.auxiliary.declaration();
		let binding = contracts
			.bind(&declaration)
			.map_err(NativeOperationError::Admission)?;
		self.auxiliary
			.require_bound_policy(&binding)
			.map_err(|error| {
				NativeOperationError::Operation(NativeRuntimePreflightError::QueuePolicy(error))
			})
	}
}
