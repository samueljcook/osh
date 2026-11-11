//! Failures to start an upgrade and failures of a running upgrade.

use std::{error::Error, fmt};

use crate::db::{DatabaseError, TransactionFailure};
use crate::kv::Key;
use crate::kv::document::schema::IndexContractIdentity;
use crate::kv::document::schema::managed::ManagedDocumentError;
use crate::records::RecordIdentity;
use crate::records::state_schema::StateSchemaError;
use crate::runtime::targets::NativeRuntimeTargetResolutionError;
use crate::runtime::upgrade::selection::UpgradeTargetSelectionError;
use crate::runtime::upgrade::{NativeRuntimeUpgradeServicesError, NativeRuntimeUpgradeStartError};
use crate::scope::ScopeIdentity;
use crate::scope::selection::ScopeSelectionError;
use crate::upgrade::engine::driver::error::NativeUpgradeDriverError;
use crate::upgrade::engine::execution::error::NativeUpgradeStepExecutionCauseError;
use crate::upgrade::engine::selection::PgUpgradeStep;
use crate::upgrade::progress::v10::source_raw::SourceRawConversionError;
use crate::upgrade::requests::NativeUpgradeRequestError;
use crate::upgrade::requests::batch::NativeUpgradeBatchError;

/// A failure of `PgRuntime::start_upgrade`, `retire_v10`, `resume_upgrade`, `inspect_upgrade`,
/// or `cancel_upgrade`: the runtime could not start the task that the method requests, so the
/// method returns no upgrade controller.
pub struct PgUpgradeStartError {
	source: Box<NativeRuntimeUpgradeStartError>,
}

/// The class of a `PgUpgradeStartError`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgUpgradeStartErrorKind {
	/// Installing or validating the Queue task-selection index failed.
	QueueTaskIndex,
	/// Installing or validating the Queue job-listing index failed.
	QueueListIndex,
	/// Installing or validating a Raw record expiry index failed.
	RecordExpiryIndex,
	/// Paranoid could not prepare the services that record the upgrade and inspect the
	/// installation's participants. When the error has a database error, the database
	/// failed.
	Services,
	/// This runtime's code for a component or a scope does not describe a valid upgrade target.
	/// `scope` names the scope when the failure belongs to one.
	Code,
	/// The upgrade target that Paranoid derives from this runtime's declarations is invalid.
	/// `selection_kind` and `scope_selection_error` describe the cause.
	Selection,
	/// The runtime could not prepare the upgrade's identity, for example because the local
	/// random source failed.
	Identity,
	/// The runtime could not start the task that runs the upgrade. `task_failure` describes
	/// the cause.
	Task,
	/// Reading the installation's recorded upgrade or its Queue recovery settings failed.
	Read,
	/// The installation has an unfinished upgrade; resume or cancel it first.
	Unfinished,
	/// The installation is retiring v10, and only `retire_v10` or `resume_upgrade` can continue
	/// that retirement.
	V10RetirementUnfinished,
	/// The installation did not import v10, so it has no v10 installation to retire.
	NotImportedFromV10,
	/// The installation already retired v10.
	V10AlreadyRetired,
}

/// The class of an upgrade target that Paranoid could not derive from this runtime's
/// declarations and v10 code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgUpgradeTargetSelectionErrorKind {
	/// This runtime's declarations do not form a valid target for a scope.
	/// `scope_selection_error` describes the cause.
	Scopes,
	/// This runtime's code for a declared scope cannot form a target for the installed
	/// policy. `scope` names the scope.
	ScopeCode,
	/// Paranoid could not form a target for its own storage. This is a defect in Paranoid.
	Internal,
}

impl PgUpgradeStartError {
	pub(crate) fn new(source: NativeRuntimeUpgradeStartError) -> Self {
		Self {
			source: Box::new(source),
		}
	}
	/// The class of the failure.
	pub fn kind(&self) -> PgUpgradeStartErrorKind {
		match self.source.as_ref() {
			NativeRuntimeUpgradeStartError::Services(_) => PgUpgradeStartErrorKind::Services,
			NativeRuntimeUpgradeStartError::RecordExpiryIndex(_) => {
				PgUpgradeStartErrorKind::RecordExpiryIndex
			}
			NativeRuntimeUpgradeStartError::QueueTaskIndex(_) => {
				PgUpgradeStartErrorKind::QueueTaskIndex
			}
			NativeRuntimeUpgradeStartError::QueueListIndex(_) => {
				PgUpgradeStartErrorKind::QueueListIndex
			}
			NativeRuntimeUpgradeStartError::Code(_) => PgUpgradeStartErrorKind::Code,
			NativeRuntimeUpgradeStartError::Selection(_) => PgUpgradeStartErrorKind::Selection,
			NativeRuntimeUpgradeStartError::Intent(_) => PgUpgradeStartErrorKind::Identity,
			NativeRuntimeUpgradeStartError::Driver(_) => PgUpgradeStartErrorKind::Task,
			NativeRuntimeUpgradeStartError::Read(_)
			| NativeRuntimeUpgradeStartError::QueueRecovery(_) => PgUpgradeStartErrorKind::Read,
			NativeRuntimeUpgradeStartError::Unfinished => PgUpgradeStartErrorKind::Unfinished,
			NativeRuntimeUpgradeStartError::V10RetirementUnfinished => {
				PgUpgradeStartErrorKind::V10RetirementUnfinished
			}
			NativeRuntimeUpgradeStartError::NotImportedFromV10 => {
				PgUpgradeStartErrorKind::NotImportedFromV10
			}
			NativeRuntimeUpgradeStartError::V10AlreadyRetired => {
				PgUpgradeStartErrorKind::V10AlreadyRetired
			}
			NativeRuntimeUpgradeStartError::Scopes(_) => PgUpgradeStartErrorKind::Selection,
		}
	}

	/// The class of an invalid upgrade target, when the kind is `Selection`.
	pub fn selection_kind(&self) -> Option<PgUpgradeTargetSelectionErrorKind> {
		use PgUpgradeTargetSelectionErrorKind as Kind;
		let source = match self.source.as_ref() {
			NativeRuntimeUpgradeStartError::Selection(source) => source,
			NativeRuntimeUpgradeStartError::Scopes(_) => return Some(Kind::Scopes),
			_ => return None,
		};
		Some(match source.as_ref() {
			UpgradeTargetSelectionError::Scopes(_) => Kind::Scopes,
			UpgradeTargetSelectionError::ScopeCode(_) => Kind::ScopeCode,
			UpgradeTargetSelectionError::ImportedSourceRequired
			| UpgradeTargetSelectionError::QueueSourceCodeMissing
			| UpgradeTargetSelectionError::DerivedComponentAbsent
			| UpgradeTargetSelectionError::Administration(_)
			| UpgradeTargetSelectionError::Counter(_)
			| UpgradeTargetSelectionError::Lease(_)
			| UpgradeTargetSelectionError::SourceLease(_)
			| UpgradeTargetSelectionError::Scalar(_)
			| UpgradeTargetSelectionError::Queue(_)
			| UpgradeTargetSelectionError::QueueSourcePolicy(_)
			| UpgradeTargetSelectionError::Contract(_)
			| UpgradeTargetSelectionError::Candidate(_) => Kind::Internal,
		})
	}

	/// Why this runtime's declarations do not form a valid target for a scope, when that is
	/// the cause.
	pub fn scope_selection_error(&self) -> Option<&ScopeSelectionError> {
		match self.source.as_ref() {
			NativeRuntimeUpgradeStartError::Selection(source) => match source.as_ref() {
				UpgradeTargetSelectionError::Scopes(source) => Some(source),
				_ => None,
			},
			NativeRuntimeUpgradeStartError::Scopes(source) => Some(source),
			_ => None,
		}
	}

	/// Why the runtime could not start the task that runs the upgrade, when the kind is
	/// `Task`.
	pub fn task_failure(&self) -> Option<PgUpgradeFailure<'_>> {
		match self.source.as_ref() {
			NativeRuntimeUpgradeStartError::Driver(source) => Some(PgUpgradeFailure::new(source)),
			_ => None,
		}
	}

	/// The scope that the failure belongs to, when it belongs to one.
	pub fn scope(&self) -> Option<&ScopeIdentity> {
		match self.source.as_ref() {
			NativeRuntimeUpgradeStartError::Selection(source) => match source.as_ref() {
				UpgradeTargetSelectionError::Scopes(source) => source.scope(),
				UpgradeTargetSelectionError::ScopeCode(source) => Some(source.scope()),
				_ => None,
			},
			NativeRuntimeUpgradeStartError::Scopes(source) => source.scope(),
			NativeRuntimeUpgradeStartError::Code(source) => match source.as_ref().typed_source() {
				NativeRuntimeTargetResolutionError::Scope(source) => Some(source.scope()),
				_ => None,
			},
			NativeRuntimeUpgradeStartError::Driver(source) => PgUpgradeFailure::new(source).scope(),
			_ => None,
		}
	}

	/// The database error, when a database statement or a transaction's own control failed.
	/// When both a statement and the following rollback failed, this is the statement's
	/// error.
	pub fn database_error(&self) -> Option<&DatabaseError> {
		match self.source.as_ref() {
			NativeRuntimeUpgradeStartError::Services(source) => match source.as_ref() {
				NativeRuntimeUpgradeServicesError::Requests(source) => {
					request_database_error(source)
				}
				NativeRuntimeUpgradeServicesError::Participants(source) => source.database_error(),
			},
			NativeRuntimeUpgradeStartError::Driver(source) => driver_database_error(source),
			NativeRuntimeUpgradeStartError::Read(source) => request_database_error(source),
			NativeRuntimeUpgradeStartError::QueueRecovery(source) => source.database_error(),
			NativeRuntimeUpgradeStartError::QueueTaskIndex(source)
			| NativeRuntimeUpgradeStartError::QueueListIndex(source)
			| NativeRuntimeUpgradeStartError::RecordExpiryIndex(source) => source.database_error(),
			NativeRuntimeUpgradeStartError::Code(_)
			| NativeRuntimeUpgradeStartError::Selection(_)
			| NativeRuntimeUpgradeStartError::Intent(_)
			| NativeRuntimeUpgradeStartError::Unfinished
			| NativeRuntimeUpgradeStartError::V10RetirementUnfinished
			| NativeRuntimeUpgradeStartError::NotImportedFromV10
			| NativeRuntimeUpgradeStartError::V10AlreadyRetired
			| NativeRuntimeUpgradeStartError::Scopes(_) => None,
		}
	}

	/// Whether a transaction's own `COMMIT` failed, which means that its changes may or may not
	/// have been committed.
	pub fn commit_outcome_is_unknown(&self) -> bool {
		self.classifications().1
	}

	fn classifications(&self) -> (bool, bool) {
		match self.source.as_ref() {
			NativeRuntimeUpgradeStartError::Services(source) => match source.as_ref() {
				NativeRuntimeUpgradeServicesError::Requests(source) => (
					source.involves_database_failure(),
					source.commit_outcome_is_unknown(),
				),
				NativeRuntimeUpgradeServicesError::Participants(source) => (
					source.involves_database_failure(),
					source.commit_outcome_is_unknown(),
				),
			},
			NativeRuntimeUpgradeStartError::Driver(source) => driver_classifications(source),
			NativeRuntimeUpgradeStartError::Read(source) => (
				source.involves_database_failure(),
				source.commit_outcome_is_unknown(),
			),
			NativeRuntimeUpgradeStartError::QueueTaskIndex(source)
			| NativeRuntimeUpgradeStartError::QueueListIndex(source)
			| NativeRuntimeUpgradeStartError::RecordExpiryIndex(source) => (
				source.involves_database_failure(),
				source.commit_outcome_is_unknown(),
			),
			NativeRuntimeUpgradeStartError::QueueRecovery(source) => (
				TransactionFailure::involves_database_failure(source.as_ref()),
				TransactionFailure::commit_outcome_is_unknown(source.as_ref()),
			),
			NativeRuntimeUpgradeStartError::Code(_)
			| NativeRuntimeUpgradeStartError::Selection(_)
			| NativeRuntimeUpgradeStartError::Intent(_)
			| NativeRuntimeUpgradeStartError::Unfinished
			| NativeRuntimeUpgradeStartError::V10RetirementUnfinished
			| NativeRuntimeUpgradeStartError::NotImportedFromV10
			| NativeRuntimeUpgradeStartError::V10AlreadyRetired
			| NativeRuntimeUpgradeStartError::Scopes(_) => (false, false),
		}
	}
}

impl fmt::Debug for PgUpgradeStartError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("PgUpgradeStartError")
			.field("kind", &self.kind())
			.field("selection_kind", &self.selection_kind())
			.finish_non_exhaustive()
	}
}
impl fmt::Display for PgUpgradeStartError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("the runtime could not start the requested upgrade task")
	}
}
impl Error for PgUpgradeStartError {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		Some(self.source.as_ref())
	}
}

/// A failure of the task that runs an upgrade. If the task had recorded the upgrade,
/// `PgRuntime::resume_upgrade` can continue it.
#[derive(Clone, Copy)]
pub struct PgUpgradeFailure<'a>(&'a NativeUpgradeDriverError);

/// The class of an upgrade task failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgUpgradeFailureKind {
	/// The task could not register its work with the installation.
	Admission,
	/// The runtime stopped accepting work, or its own accounting of the task failed.
	Ownership,
	/// Waiting for a transaction's end or for the task's release failed or exceeded its
	/// time limit.
	Ending,
	/// A mutation ended without a confirmed outcome, and its recovery did not finish.
	/// `source_failure` returns an accompanying operation failure, when one was recorded.
	UncertainMutation,
	/// Creating, reading, resuming, or cancelling the upgrade's record failed.
	Request,
	/// One step of the upgrade failed. `step` names the step.
	Step,
	/// A step read as many participant registrations as Paranoid allows, and deleting the
	/// expired registrations among them failed.
	ExpiredParticipantDeletion,
	/// This runtime's code could not form the upgrade's target.
	Target,
	/// Paranoid could not choose the upgrade's next step from the recorded state.
	Selection,
	/// The installation has no record of the upgrade.
	WorkAbsent,
	/// The installation records a different target for the upgrade than this runtime's
	/// code derives. A runtime with declarations that match the recorded upgrade must resume it,
	/// or the upgrade must be cancelled.
	TargetChanged,
	/// The next step cannot run until the conditions in the upgrade's prerequisites hold,
	/// for example until an older deployment stops.
	Prerequisites,
	/// The recorded state allows no next step, and the upgrade has not ended.
	NoProcedure,
	/// A wait deadline exceeds the range of the local monotonic clock.
	LocalClockDomain,
	/// Paranoid's limit of consecutive transient failures was reached. `source_failure`
	/// returns the last failure.
	MaxConsecutiveTransientFailuresReached,
	/// A step failed again on unchanged recorded state. `source_failure` returns the failure.
	RepeatedContext,
	/// Another failure happened while the task handled an earlier failure. `primary` returns
	/// the earlier failure, and `following` returns the later one.
	FollowingFailure,
}

impl<'a> PgUpgradeFailure<'a> {
	pub(crate) fn new(source: &'a NativeUpgradeDriverError) -> Self {
		Self(source)
	}

	/// The failure as an error, with its complete source chain.
	pub fn error(&self) -> &'a (dyn Error + Send + Sync + 'static) {
		self.0
	}

	/// The class of this failure. A failure that wraps other failures has its own class;
	/// `primary`, `following`, and `source_failure` return the wrapped failures.
	pub fn kind(&self) -> PgUpgradeFailureKind {
		use NativeUpgradeDriverError as E;
		match self.0 {
			E::Admission(_) => PgUpgradeFailureKind::Admission,
			E::Ownership(_) => PgUpgradeFailureKind::Ownership,
			E::Ending(_) => PgUpgradeFailureKind::Ending,
			E::UncertainMutation { .. } => PgUpgradeFailureKind::UncertainMutation,
			E::Request(_) => PgUpgradeFailureKind::Request,
			E::Step(_) => PgUpgradeFailureKind::Step,
			E::ExpiredParticipantDeletion(_) => PgUpgradeFailureKind::ExpiredParticipantDeletion,
			E::Target(_) => PgUpgradeFailureKind::Target,
			E::Selection(_) => PgUpgradeFailureKind::Selection,
			E::WorkAbsent => PgUpgradeFailureKind::WorkAbsent,
			E::TargetChanged => PgUpgradeFailureKind::TargetChanged,
			E::Prerequisites { .. } => PgUpgradeFailureKind::Prerequisites,
			E::NoProcedure => PgUpgradeFailureKind::NoProcedure,
			E::LocalClockDomain => PgUpgradeFailureKind::LocalClockDomain,
			E::MaxConsecutiveTransientFailuresReached { .. } => {
				PgUpgradeFailureKind::MaxConsecutiveTransientFailuresReached
			}
			E::RepeatedContext { .. } => PgUpgradeFailureKind::RepeatedContext,
			E::FollowingFailure { .. } => PgUpgradeFailureKind::FollowingFailure,
		}
	}

	/// The earlier failure, when the kind is `FollowingFailure`.
	pub fn primary(&self) -> Option<Self> {
		match self.0 {
			NativeUpgradeDriverError::FollowingFailure { primary_error, .. } => {
				Some(Self::new(primary_error))
			}
			_ => None,
		}
	}
	/// The later failure, when the kind is `FollowingFailure`.
	pub fn following(&self) -> Option<Self> {
		match self.0 {
			NativeUpgradeDriverError::FollowingFailure {
				following_error, ..
			} => Some(Self::new(following_error)),
			_ => None,
		}
	}
	/// The wrapped failure, when the kind is `MaxConsecutiveTransientFailuresReached`,
	/// `RepeatedContext`, or `UncertainMutation` with a recorded operation failure.
	pub fn source_failure(&self) -> Option<Self> {
		match self.0 {
			NativeUpgradeDriverError::MaxConsecutiveTransientFailuresReached { source }
			| NativeUpgradeDriverError::RepeatedContext { source } => Some(Self::new(source)),
			NativeUpgradeDriverError::UncertainMutation {
				source: Some(source),
				..
			} => Some(Self::new(source)),
			_ => None,
		}
	}
	fn primary_leaf(&self) -> &'a NativeUpgradeDriverError {
		let mut source = self.0;
		loop {
			source = match source {
				NativeUpgradeDriverError::FollowingFailure { primary_error, .. } => primary_error,
				NativeUpgradeDriverError::MaxConsecutiveTransientFailuresReached { source }
				| NativeUpgradeDriverError::RepeatedContext { source } => source,
				NativeUpgradeDriverError::UncertainMutation {
					source: Some(source),
					..
				} => source,
				_ => return source,
			};
		}
	}

	/// The step that failed, when the innermost earlier failure is a step failure.
	pub fn step(&self) -> Option<&'a PgUpgradeStep> {
		match self.primary_leaf() {
			NativeUpgradeDriverError::Step(source) => Some(source.step()),
			_ => None,
		}
	}
	/// The key of the v10 key-value item that could not move, when moving the v10 key-value
	/// items failed at one item with a valid [`Key`]. [`v10_key_bytes`](Self::v10_key_bytes)
	/// also names a stored key that is not one.
	pub fn v10_key(&self) -> Option<&'a Key> {
		self.source_raw_conversion()
			.and_then(SourceRawConversionError::source_key)
	}
	/// The stored bytes of the key of the v10 key-value item that could not move, when moving
	/// the v10 key-value items failed at one item. This includes a stored key that is not a
	/// valid [`Key`], such as a key that does not end with the key separator, which v10
	/// cannot write and only SQL outside v10 can store. The move stops at such a key, and the
	/// key must be corrected or deleted in the v10 table before the move can continue.
	pub fn v10_key_bytes(&self) -> Option<&'a [u8]> {
		self.source_raw_conversion()
			.and_then(SourceRawConversionError::stored_source_key)
	}
	fn source_raw_conversion(&self) -> Option<&'a SourceRawConversionError> {
		let NativeUpgradeDriverError::Step(source) = self.primary_leaf() else {
			return None;
		};
		let NativeUpgradeStepExecutionCauseError::Batch(batch) = source.as_ref().typed_source()
		else {
			return None;
		};
		let mut batch: &NativeUpgradeBatchError = batch;
		loop {
			batch = match batch {
				NativeUpgradeBatchError::Raw(source) => return Some(source),
				NativeUpgradeBatchError::RollbackFailed {
					operation_error, ..
				} => operation_error,
				_ => return None,
			};
		}
	}
	/// The failure of the app's document code in the innermost earlier failure's source chain.
	fn document_failure(&self) -> Option<&'a ManagedDocumentError> {
		let mut current: Option<&'a (dyn Error + 'static)> = Some(self.primary_leaf());
		while let Some(error) = current {
			if let Some(failure) = error.downcast_ref::<ManagedDocumentError>() {
				return Some(failure);
			}
			if let Some(failure) = error.downcast_ref::<Box<ManagedDocumentError>>() {
				return Some(failure);
			}
			current = error.source();
		}
		None
	}
	/// The identity of the stored document that a step could not convert or index, when the
	/// stored format, the app's decoder or encoder, or the app's projector rejected the
	/// document.
	pub fn document_record_identity(&self) -> Option<&'a RecordIdentity> {
		self.document_failure()?.record_identity()
	}
	/// The format failure of a stored document, when a step failed because the stored format
	/// could not be read or the app's decoder or encoder failed. `C` must be the codec error
	/// type of the document scope's code; for another type, the result is `None`. The
	/// `StateSchemaError::Codec` variant holds the app codec's own error.
	pub fn document_format_error<C: 'static>(&self) -> Option<&'a StateSchemaError<C>> {
		self.document_failure()?.format_error()
	}
	/// The app projector's error, when a step failed because the app's projector rejected a
	/// stored document. `P` must be the projector error type of the document scope's code; for
	/// another type, the result is `None`.
	pub fn document_projector_error<P: 'static>(&self) -> Option<&'a P> {
		self.document_failure()?.projector_error()
	}
	/// The scope of the failed step or target, when the failure belongs to one scope.
	pub fn scope(&self) -> Option<&'a ScopeIdentity> {
		match self.primary_leaf() {
			NativeUpgradeDriverError::Step(source) => source.step().scope(),
			NativeUpgradeDriverError::Target(source) => match source.typed_source() {
				NativeRuntimeTargetResolutionError::Scope(source) => Some(source.scope()),
				_ => None,
			},
			_ => None,
		}
	}
	/// The index of the failed step, when the step concerns one index.
	pub fn index(&self) -> Option<&'a IndexContractIdentity> {
		self.step().and_then(PgUpgradeStep::index)
	}
	/// The database error, when a database statement or a transaction's own control failed.
	/// An earlier failure's database error comes before a later failure's, and a statement's
	/// error comes before the following rollback's.
	pub fn database_error(&self) -> Option<&'a DatabaseError> {
		driver_database_error(self.0)
	}
	/// Whether a transaction's own `COMMIT` failed in this failure or a wrapped one, which means
	/// that its changes may or may not have been committed.
	pub fn commit_outcome_is_unknown(&self) -> bool {
		driver_classifications(self.0).1
	}
}

impl fmt::Debug for PgUpgradeFailure<'_> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("PgUpgradeFailure")
			.field("kind", &self.kind())
			.finish_non_exhaustive()
	}
}
impl fmt::Display for PgUpgradeFailure<'_> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("the upgrade task failed")
	}
}

fn request_database_error(source: &NativeUpgradeRequestError) -> Option<&DatabaseError> {
	match source {
		NativeUpgradeRequestError::Enrollment(source) => source.database_error(),
		NativeUpgradeRequestError::Binding(source) => source.database_error(),
		NativeUpgradeRequestError::Progress(source) => source.database_error(),
		NativeUpgradeRequestError::Pin(source) => source.database_error(),
		NativeUpgradeRequestError::Database(source) => Some(source),
		NativeUpgradeRequestError::RollbackFailed {
			operation_error,
			rollback_error,
		} => request_database_error(operation_error).or(Some(rollback_error)),
		_ => None,
	}
}

fn driver_database_error(source: &NativeUpgradeDriverError) -> Option<&DatabaseError> {
	match source {
		NativeUpgradeDriverError::Admission(source) => source.database_error(),
		NativeUpgradeDriverError::Request(source) => request_database_error(source),
		NativeUpgradeDriverError::Step(source) => source.database_error(),
		NativeUpgradeDriverError::ExpiredParticipantDeletion(source) => source.database_error(),
		NativeUpgradeDriverError::UncertainMutation { origin, source } => source
			.as_deref()
			.and_then(driver_database_error)
			.or_else(|| origin.ending().error()),
		NativeUpgradeDriverError::MaxConsecutiveTransientFailuresReached { source }
		| NativeUpgradeDriverError::RepeatedContext { source } => driver_database_error(source),
		NativeUpgradeDriverError::FollowingFailure {
			primary_error,
			following_error,
		} => driver_database_error(primary_error).or_else(|| driver_database_error(following_error)),
		NativeUpgradeDriverError::Ownership(_)
		| NativeUpgradeDriverError::Ending(_)
		| NativeUpgradeDriverError::Target(_)
		| NativeUpgradeDriverError::Selection(_)
		| NativeUpgradeDriverError::WorkAbsent
		| NativeUpgradeDriverError::TargetChanged
		| NativeUpgradeDriverError::Prerequisites { .. }
		| NativeUpgradeDriverError::NoProcedure
		| NativeUpgradeDriverError::LocalClockDomain => None,
	}
}

fn driver_classifications(source: &NativeUpgradeDriverError) -> (bool, bool) {
	match source {
		NativeUpgradeDriverError::Admission(source) => (
			source.involves_database_failure(),
			source.commit_outcome_is_unknown(),
		),
		NativeUpgradeDriverError::Request(source) => (
			source.involves_database_failure(),
			source.commit_outcome_is_unknown(),
		),
		NativeUpgradeDriverError::Step(source) => (
			source.involves_database_failure(),
			source.commit_outcome_is_unknown(),
		),
		NativeUpgradeDriverError::ExpiredParticipantDeletion(source) => (
			source.involves_database_failure(),
			source.commit_outcome_is_unknown(),
		),
		NativeUpgradeDriverError::UncertainMutation { origin, source } => {
			let preceding = source
				.as_deref()
				.map_or((false, false), driver_classifications);
			(
				origin.ending().error().is_some() || preceding.0,
				origin.ending().commit_attempted() || preceding.1,
			)
		}
		NativeUpgradeDriverError::MaxConsecutiveTransientFailuresReached { source }
		| NativeUpgradeDriverError::RepeatedContext { source } => driver_classifications(source),
		NativeUpgradeDriverError::FollowingFailure {
			primary_error,
			following_error,
		} => {
			let first = driver_classifications(primary_error);
			let next = driver_classifications(following_error);
			(first.0 || next.0, first.1 || next.1)
		}
		NativeUpgradeDriverError::Ownership(_)
		| NativeUpgradeDriverError::Ending(_)
		| NativeUpgradeDriverError::Target(_)
		| NativeUpgradeDriverError::Selection(_)
		| NativeUpgradeDriverError::WorkAbsent
		| NativeUpgradeDriverError::TargetChanged
		| NativeUpgradeDriverError::Prerequisites { .. }
		| NativeUpgradeDriverError::NoProcedure
		| NativeUpgradeDriverError::LocalClockDomain => (false, false),
	}
}
