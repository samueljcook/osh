//! Owned durable policy requests under current installation control.
//!
//! Requesting or cancelling does not activate policy or attest participant/data readiness.
//! Returned request identity is published only after a known successful owned commit.

use crate::upgrade::wait::{deadline_after_positive_wait, require_valid_wait, tokio_instant};
use primitives_rs::time::Deadline;
use std::{num::NonZeroU32, sync::Arc, time::Duration};

use crate::installation::contract::StateContractError;
use crate::installation::control_state::{DecodedInstallationControl, InstallationControlError};
use crate::records::PointRecordRead;
use crate::upgrade::policy::UpgradeCompatibilityError;

use crate::db::{DatabaseError, TransactionFailure, Tx, WritePool};
use crate::installation::ProtocolError;
use crate::installation::administration::{NativeAdministrationError, PgNativeAdministration};
use crate::installation::control_storage::{ControlPinError, PgControlStorage};
use crate::installation::creation::PgBackendRegistration;
use crate::installation::v10::source_gate::NativeSourceGatePhase;
use crate::runtime::admission::bindings::{
	InstallationAdmissionBindingError, InstallationStorageRoute, PgInstallationAdmissionBinding,
};
use crate::runtime::admission::upgrade::NativeUpgradeAdmissionError;

pub(crate) mod batch;
pub(crate) mod work;

pub(crate) struct PgNativeUpgradeRequestsConfig {
	pub(crate) installation_binding: Arc<PgInstallationAdmissionBinding>,
	pub(crate) control_pool: WritePool,
	pub(crate) control_storage: PgControlStorage,
	pub(crate) administration: Arc<PgNativeAdministration>,
	pub(crate) conflict_attempts: NonZeroU32,
	pub(crate) control_pin_wait_timeout: Duration,
}

pub(crate) struct PgNativeUpgradeRequests {
	config: PgNativeUpgradeRequestsConfig,
}

struct NativeUpgradeControlObservation {
	decoded: DecodedInstallationControl,
	route: InstallationStorageRoute,
	backend: PgBackendRegistration,
}

impl PgNativeUpgradeRequests {
	pub(crate) fn new(
		config: PgNativeUpgradeRequestsConfig,
	) -> Result<Self, NativeUpgradeRequestError> {
		require_valid_wait(
			config.control_pin_wait_timeout,
			NativeUpgradeRequestError::InvalidPinWaitTimeout,
		)?;
		if config.control_storage.identity() != config.administration.control_component().identity()
			|| !config
				.installation_binding
				.matches_control_storage(&config.control_storage)
		{
			return Err(NativeUpgradeRequestError::ControlBindingMismatch);
		}
		let native = match config.installation_binding.as_ref() {
			PgInstallationAdmissionBinding::Fresh { native }
			| PgInstallationAdmissionBinding::Imported { native, .. } => native,
		};
		if !Arc::ptr_eq(native, config.administration.native_bindings()) {
			return Err(NativeUpgradeRequestError::AdministrationBindingMismatch);
		}
		Ok(Self { config })
	}

	async fn read_control_observation(
		&self,
		tx: &mut Tx<'_>,
	) -> Result<NativeUpgradeControlObservation, NativeUpgradeRequestError> {
		let deadline = self.deadline()?;
		let interruption = tx.statement_interruption_marker();
		let (phase, snapshot) = tokio::time::timeout_at(tokio_instant(deadline), async {
			let phase = self.config.installation_binding.pin_source(tx).await?;
			let snapshot = self.config.control_storage.read_pinned_shared(tx).await?;
			Ok::<_, NativeUpgradeRequestError>((phase, snapshot))
		})
		.await
		.map_err(|_| {
			interruption.mark_interrupted();
			NativeUpgradeRequestError::PinWaitElapsed
		})??;
		self.verify_control_observation(&snapshot, phase)
	}

	fn deadline(&self) -> Result<Deadline, NativeUpgradeRequestError> {
		deadline_after_positive_wait(
			self.config.control_pin_wait_timeout,
			NativeUpgradeRequestError::InvalidPinWaitTimeout,
		)
	}

	fn verify_control_observation(
		&self,
		snapshot: &PointRecordRead,
		phase: Option<NativeSourceGatePhase>,
	) -> Result<NativeUpgradeControlObservation, NativeUpgradeRequestError> {
		let decoded = self
			.config
			.administration
			.control_component()
			.decode_record(self.config.control_storage.identity(), snapshot)?
			.ok_or(NativeUpgradeRequestError::ControlAbsent)?;
		let declaration = self.config.administration.declaration();
		let policy = decoded
			.state()
			.component(declaration.identifier())
			.ok_or(NativeUpgradeRequestError::AdministrativePolicyAbsent)?;
		let administration = declaration.bind(policy)?;
		self.config
			.administration
			.require_bound_policy(&administration)?;
		let schema =
			administration.bind_schema(self.config.administration.control_component().schema())?;
		self.config
			.administration
			.control_component()
			.require_bound_reader(&decoded, &schema)?;
		let (route, backend) = self
			.config
			.installation_binding
			.verify_control_with_registration(
				decoded.state(),
				phase,
				&administration
					.bind_schema(self.config.administration.backend_schema().schema())?,
			)?
			.into_parts();
		Ok(NativeUpgradeControlObservation {
			decoded,
			route,
			backend,
		})
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeUpgradeRequestError {
	#[error("upgrade physical recovery requires a terminal uncertain originating ending")]
	RecoveryOriginNotUncertain,
	#[error("upgrade physical recovery wait must be positive and fit the local clock")]
	InvalidRecoveryWaitTimeout,
	#[error("upgrade physical recovery protected wait elapsed")]
	RecoveryWaitElapsed,
	#[error("fresh upgrade creation or recovery failed")]
	Creation(#[from] work::creation_execution::NativeCreateExecutionError),
	#[error("upgrade metadata private participation enrollment failed")]
	Enrollment(#[from] NativeUpgradeAdmissionError),
	#[error("administration code owner differs from installation")]
	AdministrationBindingMismatch,
	#[error("native administrative writer policy failed")]
	Administration(#[from] NativeAdministrationError),
	#[error("upgrade target policies are not supported by their declarations")]
	Candidate(#[from] UpgradeCompatibilityError),
	#[error("upgrade request control pin wait must be positive and fit the local clock")]
	InvalidPinWaitTimeout,
	#[error("upgrade request source/control pin deadline elapsed before request writes")]
	PinWaitElapsed,
	#[error("upgrade request control container or identity differs from installation")]
	ControlBindingMismatch,
	#[error("upgrade request control record is absent")]
	ControlAbsent,
	#[error("upgrade request mandatory administrative policy is absent")]
	AdministrativePolicyAbsent,
	#[error("upgrade identity differs from the installation's pending request")]
	WrongRequest,
	#[error("upgrade request source mapping or phase was rejected")]
	Binding(#[from] InstallationAdmissionBindingError),
	#[error("upgrade request control pin failed")]
	Pin(#[from] ControlPinError),
	#[error("upgrade request control representation was rejected")]
	Control(#[from] InstallationControlError),
	#[error("upgrade request administrative policy binding was rejected")]
	Contract(#[from] StateContractError),
	#[error("upgrade work metadata or context was rejected")]
	Work(#[from] crate::upgrade::work::UpgradeWorkError),
	#[error("upgrade work progress update was rejected")]
	Progress(#[from] crate::upgrade::progress::ControlProgressError),
	#[error("upgrade request database work failed")]
	Database(#[source] DatabaseError),
	#[error("upgrade request transaction protocol failed")]
	Protocol(#[source] ProtocolError),
	#[error("upgrade request operation and rollback failed")]
	RollbackFailed {
		#[source]
		operation_error: Box<Self>,
		rollback_error: DatabaseError,
	},
}

impl TransactionFailure for NativeUpgradeRequestError {
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
			Self::Enrollment(error) => error.involves_database_failure(),
			Self::Binding(error) => error.involves_database_failure(),
			Self::Progress(error) => error.involves_database_failure(),
			Self::Pin(error) => error.database_error().is_some(),
			Self::Database(_) | Self::RollbackFailed { .. } => true,
			_ => false,
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Enrollment(error) => error.commit_outcome_is_unknown(),
			Self::Binding(error) => error.commit_outcome_is_unknown(),
			Self::Progress(error) => error.commit_outcome_is_unknown(),
			Self::Pin(error) => error
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
			Self::Database(error) => error.commit_outcome_is_unknown(),
			Self::RollbackFailed {
				operation_error,
				rollback_error,
			} => {
				operation_error.commit_outcome_is_unknown()
					|| rollback_error.commit_outcome_is_unknown()
			}
			_ => false,
		}
	}
}
