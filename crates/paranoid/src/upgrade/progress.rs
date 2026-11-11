//! Restricted pending-work status updates under a shared relation pin and serialized row.
//!
//! This boundary supplies no participant admission, controller proof, promotion, or protected
//! record callback. Effective changes require the separate exclusive control boundary.

use std::{num::NonZeroU32, time::Duration};

use primitives_rs::time::Deadline;

use crate::installation::administration::{NativeAdministrationError, PgNativeAdministration};
use crate::installation::contract::StateContractError;
use crate::installation::control_state::{
	DecodedInstallationControl, InstallationControlError, InstallationControlState,
};
use crate::installation::creation::PgBackendRegistration;
use crate::records::{PointRecordRead, RecordMetadata};
use crate::runtime::admission::upgrade::{NativeUpgradeAdmissionError, NativeUpgradeEnrollment};
use crate::upgrade::observation::{
	NativeUpgradeObservedControl, NativeUpgradeObservedControlFields,
};
use crate::upgrade::policy::UpgradeCompatibilityError;
use crate::upgrade::requests::work::creation_execution::NativeCreateExecutionError;
use crate::upgrade::work::{UpgradeSourceOrigin, UpgradeWork, UpgradeWorkError};
use std::sync::Arc;

use crate::db::{DatabaseError, StateMutationCompletion, WriteTx};
use crate::installation::control_storage::{
	ControlPinError, PgControlRecordLock, PgControlStorage,
};
use crate::installation::v10::source_gate::NativeSourceGatePhase;
use crate::runtime::admission::bindings::{
	InstallationAdmissionBindingError, InstallationStorageRoute, PgInstallationAdmissionBinding,
};

pub(crate) mod creation;
pub(in crate::upgrade) mod document_conversion;
pub(in crate::upgrade) mod document_verification;
pub(in crate::upgrade) mod payload_conversion;
pub(in crate::upgrade) mod queue_job_conversion;
pub(crate) mod v10;

/// The bounds of one v10 conversion transaction. The transaction reads at most `limit`
/// v10 keys, converts them one at a time, and takes no further key after `take_until`, which
/// means that it commits within its protected-work wait even when each key is slow to convert. It always
/// converts the first key, so every committed transaction advances the conversion, and it
/// records one checkpoint after the last key that it converted.
#[derive(Clone, Copy, Debug)]
pub(in crate::upgrade) struct SourceConversionBatch {
	pub(crate) limit: NonZeroU32,
	pub(crate) take_until: Deadline,
}

impl SourceConversionBatch {
	fn takes_another(&self, converted: usize) -> bool {
		converted == 0 || tokio::time::Instant::now().into_std() < self.take_until.instant()
	}
}

/// Borrows the actual installation/control inputs; wait and conflict limits belong to each call.
pub(in crate::upgrade) struct PgControlProgressStorage<'bindings> {
	installation: &'bindings PgInstallationAdmissionBinding,
	storage: &'bindings PgControlStorage,
	administration: &'bindings Arc<PgNativeAdministration>,
}

/// No point guard, mutable state, or arbitrary encoded write is exposed. A conversion lends the
/// lock's transaction to admission, Queue, and lease code for the length of one call, and that
/// code does not write the control record. Drop, cancellation, and every failed update retain
/// mandatory rollback.
pub(in crate::upgrade) struct PgControlProgressLock<'a, 'tx> {
	progress_storage: &'a PgControlProgressStorage<'a>,
	physical: PgControlRecordLock<'a, 'tx>,
	control: InstallationControlState,
	route: InstallationStorageRoute,
	backend: PgBackendRegistration,
	origin: UpgradeSourceOrigin,
	completion: StateMutationCompletion,
}

impl<'bindings> PgControlProgressStorage<'bindings> {
	pub(crate) fn new(
		installation: &'bindings PgInstallationAdmissionBinding,
		storage: &'bindings PgControlStorage,
		administration: &'bindings Arc<PgNativeAdministration>,
	) -> Result<Self, ControlProgressError> {
		if storage.identity() != administration.control_component().identity()
			|| !installation.matches_control_storage(storage)
		{
			return Err(ControlProgressError::ControlBindingMismatch);
		}
		let native = match installation {
			PgInstallationAdmissionBinding::Fresh { native }
			| PgInstallationAdmissionBinding::Imported { native, .. } => native,
		};
		if !Arc::ptr_eq(native, administration.native_bindings()) {
			return Err(ControlProgressError::AdministrationBindingMismatch);
		}
		Ok(Self {
			installation,
			storage,
			administration,
		})
	}

	/// Source pin precedes native shared pin and canonical point authority. The supplied wait
	/// covers acquisition only, before status writes; it is not a whole-operation/commit limit.
	pub(crate) async fn lock<'a, 'tx>(
		&'a self,
		tx: &'a mut WriteTx<'tx>,
		conflict_attempts: NonZeroU32,
		control_pin_wait_timeout: Duration,
	) -> Result<PgControlProgressLock<'a, 'tx>, ControlProgressError> {
		if control_pin_wait_timeout.is_zero() {
			return Err(ControlProgressError::InvalidPinWaitTimeout);
		}
		let deadline = Deadline::new_from_base(
			tokio::time::Instant::now().into_std(),
			control_pin_wait_timeout,
		)
		.map_err(|_| ControlProgressError::InvalidPinWaitTimeout)?;
		let completion = tx.track_state_mutation();
		let interruption = tx.statement_interruption_marker();
		let (phase, physical) =
			tokio::time::timeout_at(tokio::time::Instant::from_std(deadline.instant()), async {
				let phase = self.installation.pin_source(tx).await?;
				let physical = self.storage.lock_progress(tx, conflict_attempts).await?;
				Ok::<_, ControlProgressError>((phase, physical))
			})
			.await
			.map_err(|_| {
				interruption.mark_interrupted();
				ControlProgressError::PinWaitElapsed
			})??;
		let (decoded, route, backend) = match self.decode_control(physical.snapshot(), phase) {
			Ok(value) => value,
			Err(error) => return Err(cleanup_rejection(physical, error).await),
		};
		let origin = match route {
			InstallationStorageRoute::NativeFresh => UpgradeSourceOrigin::Fresh,
			InstallationStorageRoute::SourceCoexistence
			| InstallationStorageRoute::SourceRelocation
			| InstallationStorageRoute::NativeImported => UpgradeSourceOrigin::Imported,
		};
		Ok(PgControlProgressLock {
			progress_storage: self,
			physical,
			control: decoded.into_state(),
			route,
			backend,
			origin,
			completion,
		})
	}

	fn decode_control(
		&self,
		snapshot: &PointRecordRead,
		phase: Option<NativeSourceGatePhase>,
	) -> Result<
		(
			DecodedInstallationControl,
			InstallationStorageRoute,
			PgBackendRegistration,
		),
		ControlProgressError,
	> {
		let decoded = self
			.administration
			.control_component()
			.decode_record(self.storage.identity(), snapshot)?
			.ok_or(ControlProgressError::ControlAbsent)?;
		let declaration = self.administration.declaration();
		let policy = decoded
			.state()
			.component(declaration.identifier())
			.ok_or(ControlProgressError::AdministrativePolicyAbsent)?;
		let bound = declaration.bind(policy)?;
		self.administration.require_bound_policy(&bound)?;
		let schema = bound.bind_schema(self.administration.control_component().schema())?;
		self.administration
			.control_component()
			.require_bound_reader(&decoded, &schema)?;
		let registration = self.installation.verify_control_with_registration(
			decoded.state(),
			phase,
			&bound.bind_schema(self.administration.backend_schema().schema())?,
		)?;
		let (route, backend) = registration.into_parts();
		Ok((decoded, route, backend))
	}
}

impl PgControlProgressLock<'_, '_> {
	/// Cancellation preserves effective state and needs no executable codec for the pending target.
	pub(in crate::upgrade) async fn cancel_work(
		mut self,
		enrollment: &NativeUpgradeEnrollment<'_>,
		expected: &UpgradeWork,
	) -> Result<UpgradeWork, ControlProgressError> {
		let cancelled = (|| {
			let metadata = self
				.control
				.pending_upgrade()
				.ok_or(ControlProgressError::WorkAbsent)?;
			let mut work = UpgradeWork::decode_json(metadata)?;
			if !work.matches_work(expected) {
				return Err(UpgradeWorkError::WorkNotCurrent.into());
			}
			work.cancel(&self.control, self.origin, &expected.checkpoint_context())?;
			Ok::<_, ControlProgressError>(work)
		})();
		let work = match cancelled {
			Ok(work) => work,
			Err(error) => return Err(cleanup_rejection(self.physical, error).await),
		};
		if let Err(error) = self.authenticate_enrolled_work(enrollment, &work).await {
			return Err(cleanup_rejection(self.physical, error).await);
		}
		self.persist_work(&work).await?;
		Ok(work)
	}

	async fn authenticate_enrolled_work(
		&mut self,
		enrollment: &NativeUpgradeEnrollment<'_>,
		work: &UpgradeWork,
	) -> Result<(), ControlProgressError> {
		work.require_control_context(&self.control, self.origin)?;
		let identity = work.target().request().identity();
		let enrolled = enrollment.identity();
		if identity.installation_id != enrolled.installation_id
			|| identity.request_id != enrolled.request_id
		{
			return Err(ControlProgressError::EnrolledWorkMismatch);
		}
		enrollment
			.authenticate_current(self.physical.companion_tx(), &self.control)
			.await?;
		Ok(())
	}

	async fn persist_work(
		mut self,
		work: &UpgradeWork,
	) -> Result<NativeUpgradeObservedControl, ControlProgressError> {
		let pending = match work.encode_json() {
			Ok(pending) => pending,
			Err(error) => return Err(cleanup_rejection(self.physical, error.into()).await),
		};
		// Move every actual active field without recreating policies or backend registration.
		let mut fields = self.control.into_fields();
		fields.pending_upgrade = Some(pending);
		let state = match InstallationControlState::new(fields) {
			Ok(state) => state,
			Err(error) => return Err(cleanup_rejection(self.physical, error.into()).await),
		};
		let declaration = self.progress_storage.administration.declaration();
		let bound = match state.component(declaration.identifier()) {
			Some(policy) => match declaration.bind(policy) {
				Ok(bound) => bound,
				Err(error) => return Err(cleanup_rejection(self.physical, error.into()).await),
			},
			None => {
				return Err(cleanup_rejection(
					self.physical,
					ControlProgressError::AdministrativePolicyAbsent,
				)
				.await);
			}
		};
		if let Err(error) = self
			.progress_storage
			.administration
			.require_bound_policy(&bound)
		{
			return Err(cleanup_rejection(self.physical, error.into()).await);
		}
		let schema = match bound.bind_schema(
			self.progress_storage
				.administration
				.control_component()
				.schema(),
		) {
			Ok(schema) => schema,
			Err(error) => return Err(cleanup_rejection(self.physical, error.into()).await),
		};
		let writer = self
			.progress_storage
			.administration
			.control_component()
			.require_bound_writer(&schema);
		if let Err(error) = writer {
			return Err(cleanup_rejection(self.physical, error.into()).await);
		}
		let now = self.physical.observe_time().await?;
		let encoded = match self
			.progress_storage
			.administration
			.control_component()
			.encode_record(
				self.progress_storage.storage.identity(),
				&state,
				&schema,
				RecordMetadata {
					expires_at: None,
					updated_at: now,
				},
			) {
			Ok(encoded) => encoded,
			Err(error) => return Err(cleanup_rejection(self.physical, error.into()).await),
		};
		self.physical.write(encoded).await?;
		self.completion.complete();
		Ok(NativeUpgradeObservedControl::new(
			NativeUpgradeObservedControlFields {
				control: state,
				route: self.route,
				backend: self.backend,
			},
		))
	}

	/// Retains the exact unchanged parsed state without another control or backend read.
	async fn keep_observed(mut self) -> Result<NativeUpgradeObservedControl, ControlProgressError> {
		self.physical.keep().await?;
		self.completion.complete();
		Ok(NativeUpgradeObservedControl::new(
			NativeUpgradeObservedControlFields {
				control: self.control,
				route: self.route,
				backend: self.backend,
			},
		))
	}
}

async fn cleanup_rejection(
	physical: PgControlRecordLock<'_, '_>,
	error: ControlProgressError,
) -> ControlProgressError {
	match physical.keep().await {
		Ok(()) => error,
		Err(cleanup) => ControlProgressError::RejectionCleanup {
			operation_error: Box::new(error),
			cleanup_error: cleanup,
		},
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ControlProgressError {
	#[error("creation intent origin differs from the pinned installation")]
	CreateOriginMismatch,
	#[error("creation attempt or target preparation failed")]
	Creation(#[from] NativeCreateExecutionError),
	#[error("progress metadata private participation enrollment failed")]
	Enrollment(#[from] NativeUpgradeAdmissionError),
	#[error("progress metadata differs from its enrolled request identity")]
	EnrolledWorkMismatch,
	#[error("progress pending target differs from its candidate")]
	Candidate(#[from] UpgradeCompatibilityError),
	#[error("progress administration belongs to another native installation")]
	AdministrationBindingMismatch,
	#[error("progress administrative writer policy failed")]
	Administration(#[from] NativeAdministrationError),
	#[error("progress control binding differs from the installation mapping")]
	ControlBindingMismatch,
	#[error("progress control acquisition wait must be positive and fit the local clock")]
	InvalidPinWaitTimeout,
	#[error("progress control acquisition wait elapsed before any status write")]
	PinWaitElapsed,
	#[error("progress control record is absent")]
	ControlAbsent,
	#[error("progress control mandatory administrative policy is absent")]
	AdministrativePolicyAbsent,
	#[error("progress control pending work is absent")]
	WorkAbsent,
	#[error("progress control origin or phase validation failed")]
	Binding(#[from] InstallationAdmissionBindingError),
	#[error("progress control physical acquisition or mutation failed")]
	Pin(#[from] ControlPinError),
	#[error("progress control representation failed")]
	Control(#[from] InstallationControlError),
	#[error("progress control administrative binding failed")]
	Contract(#[from] StateContractError),
	#[error("progress control work context or status failed")]
	Work(#[from] UpgradeWorkError),
	#[error("progress control operation failed and reservation cleanup also failed")]
	RejectionCleanup {
		#[source]
		operation_error: Box<Self>,
		cleanup_error: ControlPinError,
	},
}

impl ControlProgressError {
	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::Enrollment(error) => error.involves_database_failure(),
			Self::Binding(error) => error.involves_database_failure(),
			Self::Pin(error) => error.database_error().is_some(),
			Self::RejectionCleanup {
				operation_error,
				cleanup_error,
			} => {
				operation_error.involves_database_failure()
					|| cleanup_error.database_error().is_some()
			}
			_ => false,
		}
	}

	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Enrollment(error) => error.commit_outcome_is_unknown(),
			Self::Binding(error) => error.commit_outcome_is_unknown(),
			Self::Pin(error) => error
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
			Self::RejectionCleanup {
				operation_error,
				cleanup_error,
			} => {
				operation_error.commit_outcome_is_unknown()
					|| cleanup_error
						.database_error()
						.is_some_and(DatabaseError::commit_outcome_is_unknown)
			}
			_ => false,
		}
	}

	/// Returns one diagnostic driver cause; classifiers separately inspect both cleanup branches.
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Enrollment(error) => error.database_error(),
			Self::Binding(error) => error.database_error(),
			Self::Pin(error) => error.database_error(),
			Self::RejectionCleanup {
				operation_error,
				cleanup_error,
			} => operation_error
				.database_error()
				.or_else(|| cleanup_error.database_error()),
			_ => None,
		}
	}
}
