//! Bounded participant compatibility inspection, expired-registration fencing, and the fixed
//! upgrade steps that change the installation's control state: writer promotion, index
//! registration, Ready publication, scope addition, scope finalization, Queue job representation
//! changes, target completion, and v10 retirement.
//!
//! One owned transaction retains exclusive native control for the entire scan. Coverage
//! describes that interval only; committed counts grant no authority to promote policies.

use std::{num::NonZeroU32, sync::Arc, time::Duration};

use crate::installation::contract::{ContractStateReadError, StateContractError};
use crate::installation::control_state::{
	DecodedInstallationControl, InstallationControlError, InstallationControlState,
};
use crate::records::{
	EncodedRecord, PointRecordRead, RecordMetadata, RecordNamespace, StateTimestampMicros,
};
use crate::runtime::participation::state::ParticipantStateError;
use crate::scope::{ScopeError, ScopeIdentity};
use crate::upgrade::policy::{InstallationUpgradeCandidate, UpgradeCompatibilityError};
use crate::upgrade::wait::require_valid_wait;
use crate::upgrade::work::UpgradeWorkError;

use crate::db::{DatabaseError, TransactionFailure, WritePool, WriteTx};
use crate::installation::ProtocolError;
use crate::installation::administration::{NativeAdministrationError, PgNativeAdministration};
use crate::installation::control_storage::{ControlPinError, PgControlStorage};
use crate::installation::creation::PgBackendRegistration;
use crate::installation::v10::source_gate::NativeSourceGatePhase;
use crate::records::postgres::scan::RecordPageError;
use crate::records::postgres::{PgPointStorage, StateStorageError};
use crate::runtime::admission::bindings::{
	InstallationAdmissionBindingError, InstallationStorageRoute, PgInstallationAdmissionBinding,
};

pub(in crate::upgrade) mod control_lock;
pub(in crate::upgrade) use control_lock::v10::SourceDataInspectionFields;
pub(in crate::upgrade) use control_lock::{
	ParticipantInspectionFields, PgDocumentIndexInspectionError,
};
pub(crate) mod document_index;
pub(in crate::upgrade) mod document_readiness;
pub(crate) mod exclusive;
pub(in crate::upgrade) mod queue_job;
pub(crate) mod scope_addition;
pub(in crate::upgrade) mod scope_reader;
pub(in crate::upgrade) mod scope_writer;
pub(in crate::upgrade) mod target_completion;
pub(crate) mod v10;

/// Actual installed associations and an independent owned control-transaction pool.
pub(crate) struct PgNativeUpgradeParticipantsConfig {
	pub(crate) installation_binding: Arc<PgInstallationAdmissionBinding>,
	pub(crate) control_pool: WritePool,
	pub(crate) control_storage: PgControlStorage,
	pub(crate) administration: Arc<PgNativeAdministration>,
	pub(crate) participant_storage: PgPointStorage,
	pub(crate) participant_namespace: RecordNamespace,
	pub(crate) conflict_attempts: NonZeroU32,
	pub(crate) control_pin_wait_timeout: Duration,
}

/// Each page statement and inspected row consumes its respective explicit budget.
/// Point acquisition work is separately bounded by rows and configured conflict attempts.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ParticipantInspectionBudget {
	/// Maximum participant rows inspected in the same inspection.
	pub max_rows: NonZeroU32,
	/// Maximum page statements issued in the same inspection.
	pub max_page_queries: NonZeroU32,
	/// Maximum rows requested by each page statement.
	pub page_size: NonZeroU32,
}

/// Coverage is complete only when all rows were inspected in one exclusive interval.
/// No cursor survives transaction completion, and this result is not a promotion proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ParticipantInspectionCoverage {
	Complete,
	Incomplete {
		exhausted: ParticipantInspectionLimit,
	},
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ParticipantInspectionLimit {
	Rows,
	PageQueries,
}

/// A live registration defers adoption; its private body and registration identity are omitted.
#[derive(Debug)]
pub(crate) enum ParticipantUpgradeDeferral {
	LiveUnsupportedCore,
	LiveUnsupportedScope,
}

/// A bounded observation retained by its exclusive interval; counts supply no later proof.
#[derive(Debug)]
pub(crate) struct ParticipantInspectionSummary {
	pub(crate) coverage: ParticipantInspectionCoverage,
	pub(crate) inspected_row_count: u32,
	pub(crate) page_query_count: u32,
	pub(crate) expired_fenced_count: u32,
	pub(crate) deferred: Vec<ParticipantUpgradeDeferral>,
}

/// Published only after a known successful commit, including committed expired fences.
/// Compatible observations describe recorded core and scoped roles, not data readiness.
#[derive(Debug)]
pub(in crate::upgrade) struct CommittedParticipantInspection {
	pub(crate) expired_fenced_count: u32,
}

/// Internal inspection and fixed owned installation upgrade procedures.
/// Diagnostic inspection changes only expired registrations; fixed procedures own effective writes.
pub(crate) struct PgNativeUpgradeParticipants {
	config: PgNativeUpgradeParticipantsConfig,
}

impl PgNativeUpgradeParticipants {
	pub(crate) fn new(
		config: PgNativeUpgradeParticipantsConfig,
	) -> Result<Self, NativeUpgradeParticipantsError> {
		require_valid_wait(
			config.control_pin_wait_timeout,
			NativeUpgradeParticipantsError::InvalidControlPinWaitTimeout,
		)?;
		if config.control_storage.identity() != config.administration.control_component().identity()
			|| !config
				.installation_binding
				.matches_control_storage(&config.control_storage)
		{
			return Err(NativeUpgradeParticipantsError::ControlBindingMismatch);
		}
		if !config
			.installation_binding
			.matches_participant_storage(&config.participant_storage, &config.participant_namespace)
		{
			return Err(NativeUpgradeParticipantsError::ParticipantBindingMismatch);
		}
		let native = match config.installation_binding.as_ref() {
			PgInstallationAdmissionBinding::Fresh { native }
			| PgInstallationAdmissionBinding::Imported { native, .. } => native,
		};
		if !Arc::ptr_eq(native, config.administration.native_bindings()) {
			return Err(NativeUpgradeParticipantsError::AdministrationBindingMismatch);
		}
		Ok(Self { config })
	}

	pub(in crate::upgrade) fn require_control_guard(
		&self,
		lock: &crate::installation::control_storage::PgControlRecordLock<'_, '_>,
	) -> Result<(), NativeUpgradeParticipantsError> {
		if !lock.matches_exclusive_control_storage(&self.config.control_storage)
			|| lock.identity() != self.config.administration.control_component().identity()
			|| !self
				.config
				.installation_binding
				.matches_control_storage(&self.config.control_storage)
		{
			return Err(NativeUpgradeParticipantsError::ControlBindingMismatch);
		}
		if !self
			.config
			.installation_binding
			.matches_participant_storage(
				&self.config.participant_storage,
				&self.config.participant_namespace,
			) {
			return Err(NativeUpgradeParticipantsError::ParticipantBindingMismatch);
		}
		Ok(())
	}

	fn decode_control(
		&self,
		snapshot: &PointRecordRead,
		source_phase: Option<NativeSourceGatePhase>,
	) -> Result<
		(DecodedInstallationControl, InstallationStorageRoute),
		NativeUpgradeParticipantsError,
	> {
		self.decode_control_with_registration(snapshot, source_phase)
			.map(|(decoded, route, _)| (decoded, route))
	}

	fn decode_control_with_registration(
		&self,
		snapshot: &PointRecordRead,
		source_phase: Option<NativeSourceGatePhase>,
	) -> Result<
		(
			DecodedInstallationControl,
			InstallationStorageRoute,
			PgBackendRegistration,
		),
		NativeUpgradeParticipantsError,
	> {
		let decoded = self
			.config
			.administration
			.control_component()
			.decode_record(self.config.control_storage.identity(), snapshot)?
			.ok_or(NativeUpgradeParticipantsError::ControlAbsent)?;
		let declaration = self.config.administration.declaration();
		let policy = decoded
			.state()
			.component(declaration.identifier())
			.ok_or(NativeUpgradeParticipantsError::AdministrativePolicyAbsent)?;
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
		let registration = self
			.config
			.installation_binding
			.verify_control_with_registration(
				decoded.state(),
				source_phase,
				&administration
					.bind_schema(self.config.administration.backend_schema().schema())?,
			)?;
		let (route, backend) = registration.into_parts();
		Ok((decoded, route, backend))
	}

	/// Diagnostic inspection does not bind a pending request identity or base generation.
	/// Commits expired fences even if live actors defer the candidate or coverage is incomplete.
	/// A parse/storage failure rolls back all fences; uncertain commit returns no result.
	pub(in crate::upgrade) async fn inspect_and_fence(
		&self,
		candidate: &InstallationUpgradeCandidate,
		budget: ParticipantInspectionBudget,
	) -> Result<CommittedParticipantInspection, NativeUpgradeParticipantsError> {
		let mut tx = self
			.config
			.control_pool
			.begin_labeled_transaction("db.native_upgrade_participants.inspect_and_fence")
			.await
			.map_err(NativeUpgradeParticipantsError::Begin)?;
		let outcome = self
			.inspect_in_transaction(&mut tx, candidate, budget)
			.await;
		finish_transaction(tx, outcome).await
	}

	async fn inspect_in_transaction(
		&self,
		tx: &mut WriteTx<'_>,
		candidate: &InstallationUpgradeCandidate,
		budget: ParticipantInspectionBudget,
	) -> Result<ParticipantInspectionSummary, NativeUpgradeParticipantsError> {
		self.lock_diagnostic(tx, candidate)
			.await?
			.inspect_and_finish(budget)
			.await
	}
}

impl PgNativeUpgradeParticipants {
	fn require_control_writer(
		&self,
		current: &InstallationControlState,
	) -> Result<(), NativeUpgradeParticipantsError> {
		let declaration = self.config.administration.declaration();
		let policy = current
			.component(declaration.identifier())
			.ok_or(NativeUpgradeParticipantsError::AdministrativePolicyAbsent)?;
		let administration = declaration.bind(policy)?;
		self.config
			.administration
			.require_bound_policy(&administration)?;
		let schema =
			administration.bind_schema(self.config.administration.control_component().schema())?;
		self.config
			.administration
			.control_component()
			.require_bound_writer(&schema)?;

		Ok(())
	}

	fn encode_current_control(
		&self,
		state: &InstallationControlState,
		now: StateTimestampMicros,
	) -> Result<EncodedRecord, NativeUpgradeParticipantsError> {
		let declaration = self.config.administration.declaration();
		let policy = state
			.component(declaration.identifier())
			.ok_or(NativeUpgradeParticipantsError::AdministrativePolicyAbsent)?;
		let administrative = declaration.bind(policy)?;
		self.config
			.administration
			.require_bound_policy(&administrative)?;
		let schema =
			administrative.bind_schema(self.config.administration.control_component().schema())?;
		Ok(self
			.config
			.administration
			.control_component()
			.encode_record(
				self.config.control_storage.identity(),
				state,
				&schema,
				RecordMetadata {
					expires_at: None,
					updated_at: now,
				},
			)?)
	}
}

async fn finish_transaction(
	tx: WriteTx<'_>,
	outcome: Result<ParticipantInspectionSummary, NativeUpgradeParticipantsError>,
) -> Result<CommittedParticipantInspection, NativeUpgradeParticipantsError> {
	let result = crate::db::finish_pool_owned_write_transaction_and_preserve_rollback_error(
		"db.native_upgrade_participants.finish",
		tx,
		outcome,
		NativeUpgradeParticipantsError::Commit,
		|_, operation, rollback| NativeUpgradeParticipantsError::RollbackFailed {
			operation_error: Box::new(operation),
			rollback_error: rollback,
		},
	)
	.await?;
	Ok(CommittedParticipantInspection {
		expired_fenced_count: result.expired_fenced_count,
	})
}

/// Typed errors expose no participant keys, private capabilities, or encoded records.
#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeUpgradeParticipantsError {
	#[error("Queue job policy transition failed")]
	QueueJobPolicy(#[from] crate::queue::upgrade::QueueJobUpgradeError),
	#[error("Queue job representation transition requires retired source data")]
	QueueJobNativeRouteRequired,
	#[error(
		"Queue job representation transition requires a complete compatible participant inspection"
	)]
	QueueJobInspection {
		summary: Box<ParticipantInspectionSummary>,
	},
	#[error("fixed additive scope installation failed")]
	ScopeAddition(#[source] Box<scope_addition::ScopeAdditionError>),
	#[error("fixed source importer component withdrawal failed")]
	SourceComponents(#[source] Box<v10::source_components::SourceComponentWithdrawalError>),
	#[error("complete target promotion preparation failed")]
	TargetCompletion(#[source] Box<target_completion::TargetCompletionError>),
	#[error("fixed scope reader retirement failed")]
	ScopeReader(#[source] Box<scope_reader::ScopeFinalizationError>),
	#[error("fixed source-data retirement failed")]
	SourceData(#[source] Box<v10::source_data::SourceDataRetirementCauseError>),
	#[error("fixed document index registration failed")]
	DocumentIndex(#[source] Box<document_index::DocumentIndexRegistrationError>),
	#[error("fixed document Ready publication failed")]
	DocumentReadiness(#[source] Box<document_readiness::DocumentReadyError>),
	#[error("administration code owner differs from installation")]
	AdministrationBindingMismatch,
	#[error("native administrative writer policy failed")]
	Administration(#[from] NativeAdministrationError),
	#[error("upgrade runtime private participation admission failed")]
	Admission(#[from] crate::runtime::admission::upgrade::NativeUpgradeAdmissionError),
	#[error("source writer retirement requires an imported installation")]
	ImportedInstallationRequired,
	#[error("source writer retirement requires an explicit retirement goal")]
	SourceRetirementGoalRequired,
	#[error("source writer retirement stage does not bind its code and policy")]
	SourceWriterStage(#[source] Box<crate::upgrade::policy::v10::SourceWriterRetirementStageError>),
	#[error("source writer retirement backend phase cannot be encoded from its association")]
	SourceWriterRegistration(#[source] Box<crate::installation::creation::NativeInstallationError>),
	#[error("source writer retirement requires a complete compatible participant inspection")]
	SourceWriterInspection {
		summary: Box<ParticipantInspectionSummary>,
	},
	#[error("document writer stage does not bind its code and policy")]
	DocumentStage(#[source] Box<crate::upgrade::policy::document_stage::BoxedDocumentStageError>),
	#[error("document writer scope is absent from the recorded document target")]
	DocumentScopeNotTargeted { scope: ScopeIdentity },
	#[error("document writer current or replacement scope is unavailable")]
	DocumentScope {
		scope: ScopeIdentity,
		#[source]
		source: ScopeError,
	},
	#[error("document writer scope conflicts with its conversion checkpoint")]
	DocumentCheckpointConflict { scope: ScopeIdentity },
	#[error("document writer promotion requires a complete compatible participant inspection")]
	DocumentWriterInspection {
		scope: ScopeIdentity,
		summary: Box<ParticipantInspectionSummary>,
	},
	#[error("Primitive payload writer stage does not bind its code and policy")]
	PayloadStage(
		#[source] Box<crate::upgrade::policy::payload_stage::BoxedPrimitivePayloadWriterStageError>,
	),
	#[error("Primitive payload writer scope is absent from the recorded target")]
	PayloadScopeNotTargeted { scope: ScopeIdentity },
	#[error("Primitive payload writer current or replacement scope is unavailable")]
	PayloadScope {
		scope: ScopeIdentity,
		#[source]
		source: ScopeError,
	},
	#[error("Primitive payload writer scope conflicts with its conversion checkpoint")]
	PayloadCheckpointConflict { scope: ScopeIdentity },
	#[error(
		"Primitive payload writer promotion requires a complete compatible participant inspection"
	)]
	PayloadWriterInspection {
		scope: ScopeIdentity,
		summary: Box<ParticipantInspectionSummary>,
	},
	#[error("participant control transaction database work failed")]
	Database(#[source] DatabaseError),
	#[error("participant control transaction protocol failed")]
	Protocol(#[source] ProtocolError),
	#[error(
		"participant inspection control pin wait timeout must be positive and within the monotonic deadline domain"
	)]
	InvalidControlPinWaitTimeout,
	#[error("participant inspection source/control pin wait expired before participant access")]
	ControlPinWaitElapsed,
	#[error("participant inspection control binding differs from installation mapping")]
	ControlBindingMismatch,
	#[error("participant inspection container or namespace differs from installation mapping")]
	ParticipantBindingMismatch,
	#[error("participant inspection control record is absent")]
	ControlAbsent,
	#[error("participant inspection mandatory administrative policy is absent")]
	AdministrativePolicyAbsent,
	#[error("participant disappeared between exclusive scan and canonical locked recheck")]
	ParticipantDisappeared,
	#[error("participant inspection current complete work is absent")]
	WorkAbsent,
	#[error("participant inspection complete work context was rejected")]
	Work(#[from] UpgradeWorkError),
	#[error("participant inspection may run only once in an exclusive interval")]
	InspectionAlreadyPerformed,
	#[error("participant inspection has no successfully completed scan")]
	InspectionNotCompleted,
	#[error("participant inspection transaction begin failed")]
	Begin(#[source] DatabaseError),
	#[error("participant inspection transaction commit failed")]
	Commit(#[source] DatabaseError),
	#[error("participant inspection operation and its owned rollback failed")]
	RollbackFailed {
		#[source]
		operation_error: Box<NativeUpgradeParticipantsError>,
		rollback_error: DatabaseError,
	},
	#[error("participant inspection installation binding or source phase was rejected")]
	InstallationBinding(#[from] InstallationAdmissionBindingError),
	#[error("participant inspection exclusive control pin failed")]
	ControlPin(#[from] ControlPinError),
	#[error("participant inspection control record parsing failed")]
	Control(#[from] InstallationControlError),
	#[error("participant inspection administrative contract binding failed")]
	Contract(#[from] StateContractError),
	#[error("participant inspection bounded physical page failed")]
	Page(#[from] RecordPageError),
	#[error("participant inspection point storage failed")]
	Storage(#[from] StateStorageError),
	#[error("participant inspection canonical identity or private representation was rejected")]
	Participant(#[from] ParticipantStateError),
	#[error("participant inspection reader or encoded representation was rejected")]
	BoundRepresentation(#[from] ContractStateReadError<ParticipantStateError>),
	#[error("participant inspection candidate assessment failed")]
	Candidate(#[from] UpgradeCompatibilityError),
}

impl NativeUpgradeParticipantsError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::ScopeAddition(source) => source.database_error(),
			Self::SourceData(source) => source.database_error(),
			Self::Admission(error) => error.database_error(),
			Self::DocumentIndex(error) => error.database_error(),
			Self::DocumentReadiness(error) => error.database_error(),
			Self::Begin(error) | Self::Commit(error) | Self::Database(error) => Some(error),
			Self::SourceWriterRegistration(error) => error.database_error(),
			Self::RollbackFailed {
				operation_error,
				rollback_error,
			} => operation_error.database_error().or(Some(rollback_error)),
			Self::InstallationBinding(error) => error.database_error(),
			Self::ControlPin(error) => error.database_error(),
			Self::Storage(error) => error.database_error(),
			Self::Page(RecordPageError::Database(error)) => Some(error),
			Self::Page(RecordPageError::Record(error)) => error.database_error(),
			_ => None,
		}
	}

	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::ScopeAddition(source) => source.commit_outcome_is_unknown(),
			Self::SourceData(source) => source.commit_outcome_is_unknown(),
			Self::Admission(error) => error.commit_outcome_is_unknown(),
			Self::DocumentIndex(error) => error.commit_outcome_is_unknown(),
			Self::DocumentReadiness(error) => error.commit_outcome_is_unknown(),
			Self::InstallationBinding(error) => error.commit_outcome_is_unknown(),
			Self::SourceWriterRegistration(error) => error.commit_outcome_is_unknown(),
			Self::RollbackFailed {
				operation_error,
				rollback_error,
			} => {
				operation_error.commit_outcome_is_unknown()
					|| rollback_error.commit_outcome_is_unknown()
			}
			_ => self
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
		}
	}

	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::ScopeAddition(source) => source.involves_database_failure(),
			Self::SourceData(source) => source.involves_database_failure(),
			Self::Admission(error) => error.involves_database_failure(),
			Self::DocumentIndex(error) => error.involves_database_failure(),
			Self::DocumentReadiness(error) => error.involves_database_failure(),
			Self::InstallationBinding(error) => error.involves_database_failure(),
			Self::SourceWriterRegistration(error) => error.involves_database_failure(),
			Self::RollbackFailed { .. } => true,
			_ => self.database_error().is_some(),
		}
	}
}

impl TransactionFailure for NativeUpgradeParticipantsError {
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
		self.involves_database_failure()
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		self.commit_outcome_is_unknown()
	}
}
