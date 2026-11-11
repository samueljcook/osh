//! Transaction-scoped observation without participant registration or mutation authority.

use super::{
	AdmissionObservation, Arc, DecodedInstallationControl, InstallationStorageRoute,
	NativeAdmissionError, NativeAdmittedContracts, NativeOperationError, NativeSourceGatePhase,
	PgControlStorage, PgInstallationAdmissionBinding, PgNativeAdministration, PgNativeAdmission,
	Tx, WriteTx, queue,
};

pub(crate) struct PgNativeObservation {
	pub(crate) installation_binding: Arc<PgInstallationAdmissionBinding>,
	pub(crate) control_storage: PgControlStorage,
	pub(crate) administration: Arc<PgNativeAdministration>,
}

/// Verifies the current control reader, administrative policy, backend mapping,
/// and source phase. Runtime admission additionally authenticates its participant.
pub(super) fn verify_control_observation(
	administration: &PgNativeAdministration,
	installation: &PgInstallationAdmissionBinding,
	decoded: &DecodedInstallationControl,
	source_phase: Option<NativeSourceGatePhase>,
) -> Result<InstallationStorageRoute, NativeAdmissionError> {
	let declaration = administration.declaration();
	let state = decoded.state();
	let policy = state
		.component(declaration.identifier())
		.ok_or(NativeAdmissionError::ComponentNotInstalled)?;
	let bound = declaration
		.bind(policy)
		.map_err(NativeAdmissionError::Contract)?;
	let control = bound
		.bind_schema(administration.control_component().schema())
		.map_err(NativeAdmissionError::Contract)?;
	administration
		.control_component()
		.require_bound_reader(decoded, &control)?;
	administration.require_bound_policy(&bound)?;
	let backend = bound
		.bind_schema(administration.backend_schema().schema())
		.map_err(NativeAdmissionError::Contract)?;
	Ok(installation.verify_control(state, source_phase, &backend)?)
}

impl PgNativeObservation {
	pub(crate) async fn execute_read_with_contracts<T, E>(
		&self,
		tx: &mut Tx<'_>,
		operation_fn: impl for<'a, 'tx> AsyncFnOnce(
			NativeAdmittedContracts,
			&'a mut Tx<'tx>,
		) -> Result<T, NativeOperationError<E>>,
	) -> Result<T, NativeOperationError<E>> {
		let mut completion = tx.track_state_mutation();
		let observation = self
			.observe(tx)
			.await
			.map_err(NativeOperationError::Admission)?;
		let result = operation_fn(NativeAdmittedContracts { observation }, tx).await;
		if !matches!(&result, Err(NativeOperationError::Admission(_))) {
			completion.complete();
		}
		result
	}

	async fn observe(&self, tx: &mut Tx<'_>) -> Result<AdmissionObservation, NativeAdmissionError> {
		let source_phase = self.installation_binding.pin_source(tx).await?;
		let snapshot = self.control_storage.read_pinned_shared(tx).await?;
		let control = Arc::new(
			self.administration
				.control_component()
				.decode_record(self.control_storage.identity(), &snapshot)?
				.ok_or(NativeAdmissionError::ControlAbsent)?,
		);
		let route = verify_control_observation(
			&self.administration,
			&self.installation_binding,
			&control,
			source_phase,
		)?;
		if !tx.select_native_generation(
			control.state().installation_id(),
			control.state().generation(),
		) {
			return Err(NativeAdmissionError::GenerationChanged);
		}
		Ok(AdmissionObservation {
			installation_binding: Arc::clone(&self.installation_binding),
			route,
			control,
			participant: None,
		})
	}
}

/// Shared read implementations accept either runtime or detached admission.
/// Mutation remains conditional on actual runtime participation.
#[derive(Clone)]
pub(crate) enum PgOperationAdmission {
	Runtime(Arc<PgNativeAdmission>),
	Observation(Arc<PgNativeObservation>),
}

impl PgOperationAdmission {
	pub(crate) fn installation_binding(&self) -> &Arc<PgInstallationAdmissionBinding> {
		match self {
			Self::Runtime(runtime) => runtime.installation_binding(),
			Self::Observation(observer) => &observer.installation_binding,
		}
	}

	pub(crate) async fn execute_read_with_contracts<T, E>(
		&self,
		tx: &mut Tx<'_>,
		operation_fn: impl for<'a, 'tx> AsyncFnOnce(
			NativeAdmittedContracts,
			&'a mut Tx<'tx>,
		) -> Result<T, NativeOperationError<E>>,
	) -> Result<T, NativeOperationError<E>> {
		match self {
			Self::Runtime(runtime) => runtime.execute_read_with_contracts(tx, operation_fn).await,
			Self::Observation(observer) => {
				observer.execute_read_with_contracts(tx, operation_fn).await
			}
		}
	}

	pub(crate) async fn execute_write_with_contracts<T, E>(
		&self,
		tx: &mut WriteTx<'_>,
		operation_fn: impl for<'a, 'tx> AsyncFnOnce(
			NativeAdmittedContracts,
			&'a mut WriteTx<'tx>,
		) -> Result<T, NativeOperationError<E>>,
	) -> Result<T, NativeOperationError<E>> {
		match self {
			Self::Runtime(runtime) => runtime.execute_write_with_contracts(tx, operation_fn).await,
			Self::Observation(_) => Err(NativeOperationError::Admission(
				NativeAdmissionError::RuntimeRequired,
			)),
		}
	}

	pub(crate) fn counter_source_binding(
		&self,
		identity: &crate::records::RecordIdentity,
	) -> Result<
		crate::records::postgres::v10::relocating_point::RelocatingPointBinding,
		NativeAdmissionError,
	> {
		Ok(self
			.installation_binding()
			.counter_source_binding(identity)?)
	}
	pub(crate) fn verify_counter_source_storage(
		&self,
		storage: &crate::records::postgres::v10::relocating_point::PgRelocatingPointStorage,
		binding: &crate::records::postgres::v10::relocating_point::RelocatingPointBinding,
	) -> Result<(), NativeAdmissionError> {
		Ok(self
			.installation_binding()
			.verify_counter_source_storage(storage, binding)?)
	}
	pub(crate) fn verify_queue_storage(
		&self,
		storage: &crate::installation::creation::PgNativeQueueStorage,
	) -> Result<(), queue::NativeQueuePlacementError> {
		queue::verify_native_queue_storage(self.installation_binding(), storage)
	}
	pub(crate) fn verify_queue_source_storage(
		&self,
		descriptor: &crate::installation::v10::PgV10SourceDescriptor,
		storage: &crate::installation::creation::PgNativeQueueStorage,
	) -> Result<(), queue::NativeQueuePlacementError> {
		self.installation_binding()
			.verify_queue_source_storage(descriptor, storage)
	}
}
