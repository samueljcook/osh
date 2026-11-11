//! Pure advisory fixed-step selection from actual current observations and retained code.
//!
//! Selected steps and prerequisites grant no transaction, participant, coverage or policy authority.

use crate::runtime::targets::ResolvedNativeRuntimeTarget;

use std::{cmp::Ordering, fmt};

use crate::installation::creation::NativeInstallationError;
use crate::kv::document::postgres::index::managed::PgManagedDocumentIndexError;
use crate::kv::document::schema::IndexContractIdentity;
use crate::runtime::admission::bindings::{
	InstallationStorageRoute, PgInstallationAdmissionBinding,
};
use crate::scope::managed::{ManagedScopeRegistration, ManagedScopeRegistryError};
use crate::scope::{ScopeError, ScopeIdentity, ScopeOwner, ScopePolicy};
use crate::upgrade::policy::document_reader::BoxedDocumentReaderCompletionError;
use crate::upgrade::policy::document_readiness::DocumentReadinessStageError;
use crate::upgrade::policy::document_stage::BoxedDocumentStageError;
use crate::upgrade::policy::payload_reader::BoxedPrimitivePayloadReaderCompletionError;
use crate::upgrade::policy::payload_stage::BoxedPrimitivePayloadWriterStageError;
use crate::upgrade::requests::work::observation::NativeUpgradeControllerObservation;
use crate::upgrade::work::v10::source_checkpoint::PgV10ConversionProcedure;
use crate::upgrade::work::{PgUpgradeStatus, UpgradeSourceOrigin, UpgradeWorkError};

mod document;
mod payload;
mod queue;
mod v10;

pub(in crate::upgrade) struct NativeUpgradeSelectionFields<'a> {
	pub(crate) observation: &'a NativeUpgradeControllerObservation,
	pub(crate) resolved: &'a ResolvedNativeRuntimeTarget,
}

struct ScopeSelectionFields<'a, 'context> {
	context: &'a NativeUpgradeSelectionFields<'context>,
	current: &'a ScopePolicy,
	target: &'a ScopePolicy,
}
struct IndexOrder<'a> {
	left: &'a IndexContractIdentity,
	right: &'a IndexContractIdentity,
}
struct StepOrder<'a> {
	left: &'a PgUpgradeStep,
	right: &'a PgUpgradeStep,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// One step of an upgrade. Paranoid chooses and runs the steps; the value only reports which
/// step an upgrade reached or which step failed.
#[non_exhaustive]
pub enum PgUpgradeStep {
	/// Enables separate Queue payload records after every running deployment can read them.
	PromoteQueueJobWriter,
	/// Converts one bounded page of Queue jobs into separate metadata and payload records.
	ConvertQueueJobs,
	/// Retires the inline Queue payload reader after conversion completes.
	FinalizeQueueJobReaders,
	/// Stops v10 deployments from writing to the v10 installation.
	RetireV10Writers,
	/// Moves one page of v10 records into Paranoid's storage with the named procedure.
	MoveV10Records(PgV10ConversionProcedure),
	/// Records that every v10 record has moved, which means that Paranoid no longer reads v10
	/// storage.
	RetireV10Data,
	/// Adds a declared scope that the installation does not have yet.
	InstallAdditiveScope {
		/// The added scope.
		scope: ScopeIdentity,
	},
	/// Makes the scope's newest writer version the version that new document writes use,
	/// once every running deployment can read that version.
	PromoteDocumentWriter {
		/// The document scope.
		scope: ScopeIdentity,
	},
	/// Creates the database storage of one index.
	RegisterDocumentIndex {
		/// The document scope.
		scope: ScopeIdentity,
		/// The index.
		index: IndexContractIdentity,
	},
	/// Rewrites one page of documents in the scope's writer version.
	ConvertDocument {
		/// The document scope.
		scope: ScopeIdentity,
	},
	/// Checks that every document's stored index values equal the values that the index's
	/// projection function computes.
	VerifyDocument {
		/// The document scope.
		scope: ScopeIdentity,
		/// The index.
		index: IndexContractIdentity,
	},
	/// Makes the index available for queries, once every running deployment maintains it.
	PublishDocumentReady {
		/// The document scope.
		scope: ScopeIdentity,
		/// The index.
		index: IndexContractIdentity,
	},
	/// Makes the scope's newest payload writer version the version that new cache values or
	/// Topic payloads use, once every running deployment can read that version.
	PromotePayloadWriter {
		/// The cache or Topic scope.
		scope: ScopeIdentity,
	},
	/// Rewrites one page of cache values or Topic payloads in the scope's writer version.
	ConvertPayload {
		/// The cache or Topic scope.
		scope: ScopeIdentity,
	},
	/// Records the versions that every deployment of the scope must read from now on, which
	/// drops versions that no stored record uses any more.
	FinalizeScope {
		/// The scope.
		scope: ScopeIdentity,
	},
	/// Records the upgrade's target as the installation's state and ends the upgrade.
	CompleteTarget,
}

impl PgUpgradeStep {
	/// The scope of the step, when the step concerns one scope.
	pub fn scope(&self) -> Option<&ScopeIdentity> {
		match self {
			Self::InstallAdditiveScope { scope }
			| Self::PromoteDocumentWriter { scope }
			| Self::RegisterDocumentIndex { scope, .. }
			| Self::ConvertDocument { scope }
			| Self::VerifyDocument { scope, .. }
			| Self::PublishDocumentReady { scope, .. }
			| Self::PromotePayloadWriter { scope }
			| Self::ConvertPayload { scope }
			| Self::FinalizeScope { scope } => Some(scope),
			_ => None,
		}
	}
	/// The index of the step, when the step concerns one index.
	pub fn index(&self) -> Option<&IndexContractIdentity> {
		match self {
			Self::RegisterDocumentIndex { index, .. }
			| Self::VerifyDocument { index, .. }
			| Self::PublishDocumentReady { index, .. } => Some(index),
			_ => None,
		}
	}
	fn rank(&self) -> u8 {
		match self {
			Self::RetireV10Writers => 0,
			Self::MoveV10Records(procedure) => 2 + source_rank(*procedure),
			Self::RetireV10Data => 8,
			Self::InstallAdditiveScope { .. } => 9,
			Self::PromoteDocumentWriter { .. } => 10,
			Self::RegisterDocumentIndex { .. } => 11,
			Self::ConvertDocument { .. } => 12,
			Self::VerifyDocument { .. } => 13,
			Self::PublishDocumentReady { .. } => 14,
			Self::PromotePayloadWriter { .. } => 15,
			Self::ConvertPayload { .. } => 16,
			Self::FinalizeScope { .. } => 17,
			Self::PromoteQueueJobWriter => 18,
			Self::ConvertQueueJobs => 19,
			Self::FinalizeQueueJobReaders => 20,
			Self::CompleteTarget => 21,
		}
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::upgrade) enum NativeUpgradeObservedTerminal {
	Cancelled,
	Completed,
}

pub(in crate::upgrade) struct NativeUpgradeSelection {
	steps: Vec<PgUpgradeStep>,
	prerequisites: Vec<NativeUpgradePrerequisiteError>,
	terminal: Option<NativeUpgradeObservedTerminal>,
}
impl fmt::Debug for NativeUpgradeSelection {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("NativeUpgradeSelection")
			.field("step_count", &self.steps.len())
			.field("prerequisite_count", &self.prerequisites.len())
			.field("terminal", &self.terminal)
			.finish()
	}
}
impl NativeUpgradeSelection {
	pub(crate) fn into_parts(
		self,
	) -> (
		Vec<PgUpgradeStep>,
		Vec<NativeUpgradePrerequisiteError>,
		Option<NativeUpgradeObservedTerminal>,
	) {
		(self.steps, self.prerequisites, self.terminal)
	}
	fn push_scope_prerequisite(
		&mut self,
		scope: &ScopeIdentity,
		source: NativeUpgradeScopePrerequisiteError,
	) {
		self.prerequisites
			.push(NativeUpgradePrerequisiteError::Scope {
				scope: scope.clone(),
				source: Box::new(source),
			});
	}
}

#[derive(thiserror::Error)]
pub(crate) enum NativeUpgradePrerequisiteError {
	#[error("upgrade scoped procedure has an identified prerequisite")]
	Scope {
		scope: ScopeIdentity,
		#[source]
		source: Box<NativeUpgradeScopePrerequisiteError>,
	},
	#[error("source progress remains after physical source data retirement")]
	SourceProgressAfterRetirement { procedure: PgV10ConversionProcedure },
}
impl NativeUpgradePrerequisiteError {
	pub(crate) fn scope(&self) -> Option<&ScopeIdentity> {
		match self {
			Self::Scope { scope, .. } => Some(scope),
			_ => None,
		}
	}
	pub(crate) fn scope_source(&self) -> Option<&NativeUpgradeScopePrerequisiteError> {
		match self {
			Self::Scope { source, .. } => Some(source),
			_ => None,
		}
	}
}
impl fmt::Debug for NativeUpgradePrerequisiteError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("NativeUpgradePrerequisiteError")
			.field("scoped", &self.scope().is_some())
			.finish_non_exhaustive()
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeUpgradeScopePrerequisiteError {
	#[error("upgrade target scope is absent from current installed policy")]
	MissingCurrentScope(#[source] ScopeError),
	#[error("upgrade additive scope owner binding failed")]
	AdditiveScopeCode(#[source] ManagedScopeRegistryError),
	#[error("upgrade scope owner or meaning requires a separate adoption procedure")]
	UnsupportedTransition,
	#[error("upgrade payload maintenance requires a native storage route")]
	NativePayloadRoute { route: InstallationStorageRoute },
	#[error("upgrade Document stage binding failed")]
	DocumentStage(#[source] BoxedDocumentStageError),
	#[error("upgrade Document completion preparation failed")]
	DocumentCompletion(#[source] BoxedDocumentReaderCompletionError),
	#[error("upgrade Document Ready preparation failed")]
	DocumentReadiness(#[source] DocumentReadinessStageError),
	#[error("upgrade payload stage binding failed")]
	PayloadStage(#[source] BoxedPrimitivePayloadWriterStageError),
	#[error("upgrade payload completion preparation failed")]
	PayloadCompletion(#[source] BoxedPrimitivePayloadReaderCompletionError),
	#[error("upgrade requested Ready index has an identified prerequisite")]
	Index {
		index: IndexContractIdentity,
		#[source]
		source: Box<NativeUpgradeIndexPrerequisiteError>,
	},
	#[error("upgrade verification checkpoint has no targeted Ready index")]
	VerificationTargetMissing,
	#[error("upgrade verification checkpoint has no conversion checkpoint")]
	VerificationConversionMissing,
	#[error("upgrade installed stage conflicts with existing scoped progress")]
	StageProgressConflict,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeUpgradeIndexPrerequisiteError {
	#[error("upgrade physical index code/placement binding failed")]
	Binding(#[source] PgManagedDocumentIndexError),
	#[error("upgrade physical index ledger differs from its bound descriptor")]
	Ledger(#[source] NativeInstallationError),
	#[error("upgrade current Ready reference has no recorded physical index")]
	ReadyLedgerMissing,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeUpgradeSelectionError {
	#[error("Queue job upgrade policy is invalid")]
	QueuePolicy(#[from] crate::queue::upgrade::QueueJobUpgradeError),
	#[error("upgrade selection has no complete Work")]
	WorkAbsent,
	#[error("upgrade selection code belongs to another installation")]
	InstallationMismatch,
	#[error("upgrade selection observed route differs from code origin")]
	OriginMismatch,
	#[error("upgrade selection Work differs from the resolved complete target")]
	TargetMismatch,
	#[error("upgrade selection Work context or invariant failed")]
	Work(#[from] UpgradeWorkError),
}

pub(in crate::upgrade) fn select_upgrade_steps(
	fields: NativeUpgradeSelectionFields<'_>,
) -> Result<NativeUpgradeSelection, NativeUpgradeSelectionError> {
	let current = fields.observation.control();
	let code = fields.resolved.code();
	if current.installation_id() != code.administration().control_component().installation_id() {
		return Err(NativeUpgradeSelectionError::InstallationMismatch);
	}
	let origin = match (code.installation().as_ref(), fields.observation.route()) {
		(PgInstallationAdmissionBinding::Fresh { .. }, InstallationStorageRoute::NativeFresh) => {
			UpgradeSourceOrigin::Fresh
		}
		(
			PgInstallationAdmissionBinding::Imported { .. },
			InstallationStorageRoute::SourceCoexistence
			| InstallationStorageRoute::SourceRelocation
			| InstallationStorageRoute::NativeImported,
		) => UpgradeSourceOrigin::Imported,
		_ => return Err(NativeUpgradeSelectionError::OriginMismatch),
	};
	let work = fields
		.observation
		.work()
		.ok_or(NativeUpgradeSelectionError::WorkAbsent)?;
	if !work.target().matches_target(fields.resolved.target()) {
		return Err(NativeUpgradeSelectionError::TargetMismatch);
	}
	work.require_checkpoint_context(
		current,
		fields.resolved.candidate(),
		origin,
		&work.checkpoint_context(),
	)?;
	work.require_source_checkpoint_invariants(
		current.component_policies(),
		current.scope_policies(),
	)?;
	work.require_queue_job_checkpoint_invariant(current.component_policies())?;
	work.require_document_checkpoint_invariants(current.scope_policies())?;
	work.require_document_verification_invariants(current.scope_policies())?;
	work.require_payload_checkpoint_invariants(current.scope_policies())?;
	let mut selection = NativeUpgradeSelection {
		steps: Vec::new(),
		prerequisites: Vec::new(),
		terminal: None,
	};
	if work.status() == PgUpgradeStatus::Cancelled {
		selection.terminal = Some(NativeUpgradeObservedTerminal::Cancelled);
		return Ok(selection);
	}
	if work.status() == PgUpgradeStatus::Completed {
		selection.terminal = Some(NativeUpgradeObservedTerminal::Completed);
	} else {
		v10::collect(&mut selection, work, fields.observation.route());
		queue::collect(&mut selection, &fields)?;
		if let Some(targets) = work.target().request().scope_replacements() {
			let mut targets = targets.policies().iter().collect::<Vec<_>>();
			targets.sort_by(|left, right| left.identity().cmp(right.identity()));
			for target in targets {
				let scope = target.identity();
				let installed = match current.scope_policies().lookup(scope) {
					Ok(installed) => installed,
					Err(source) => {
						if matches!(
							target.owner(),
							ScopeOwner::Document(_) | ScopeOwner::Raw(_) | ScopeOwner::Primitive(_)
						) {
							match (code.scopes().lookup(scope), target.owner()) {
								(
									Ok(ManagedScopeRegistration::Document(_)),
									ScopeOwner::Document(_),
								)
								| (Ok(ManagedScopeRegistration::Raw(_)), ScopeOwner::Raw(_))
								| (
									Ok(ManagedScopeRegistration::Primitive(_)),
									ScopeOwner::Primitive(_),
								) => {
									selection.steps.push(PgUpgradeStep::InstallAdditiveScope {
										scope: scope.clone(),
									});
								}
								(Ok(_), _) => selection.push_scope_prerequisite(
									scope,
									NativeUpgradeScopePrerequisiteError::UnsupportedTransition,
								),
								(Err(source), _) => selection.push_scope_prerequisite(
									scope,
									NativeUpgradeScopePrerequisiteError::AdditiveScopeCode(source),
								),
							}
						} else {
							selection.push_scope_prerequisite(
								scope,
								NativeUpgradeScopePrerequisiteError::MissingCurrentScope(source),
							);
						}
						continue;
					}
				};
				let has_progress = work.document_conversion_checkpoint(scope).is_some()
					|| work.document_verification(scope).is_some()
					|| work.payload_checkpoint(scope).is_some();
				if installed.matches_policy(target) && !has_progress {
					continue;
				}
				match (installed.owner(), target.owner()) {
					(ScopeOwner::Document(_), ScopeOwner::Document(_)) => document::collect(
						&mut selection,
						ScopeSelectionFields {
							context: &fields,
							current: installed,
							target,
						},
					),
					(ScopeOwner::Primitive(_), ScopeOwner::Primitive(_)) => payload::collect(
						&mut selection,
						ScopeSelectionFields {
							context: &fields,
							current: installed,
							target,
						},
					),
					_ => selection.push_scope_prerequisite(
						scope,
						NativeUpgradeScopePrerequisiteError::UnsupportedTransition,
					),
				}
			}
		}
	}
	let scopes_final = work
		.target()
		.request()
		.scope_replacements()
		.is_none_or(|targets| {
			targets.policies().iter().all(|target| {
				current
					.scope_policies()
					.lookup(target.identity())
					.is_ok_and(|installed| installed.matches_policy(target))
			})
		});
	if scopes_final
		&& queue::is_final(&fields)
		&& work.queue_job_checkpoint().is_none()
		&& work.source_checkpoints().is_empty()
		&& work.document_conversion_checkpoints().is_empty()
		&& work.document_verification_checkpoints().is_empty()
		&& work.payload_checkpoints().is_empty()
		&& v10::goal_satisfied(work.target().source_goal(), fields.observation.route())
	{
		selection.steps.push(PgUpgradeStep::CompleteTarget);
	}
	selection
		.steps
		.sort_by(|left, right| compare_steps(StepOrder { left, right }));
	selection.steps.dedup();
	Ok(selection)
}

fn compare_steps(fields: StepOrder<'_>) -> Ordering {
	let StepOrder { left, right } = fields;
	let group = |step: &PgUpgradeStep| {
		if matches!(step, PgUpgradeStep::CompleteTarget) {
			2
		} else if step.scope().is_some() {
			1
		} else {
			0
		}
	};
	group(left)
		.cmp(&group(right))
		.then_with(|| left.scope().cmp(&right.scope()))
		.then_with(|| left.rank().cmp(&right.rank()))
		.then_with(|| match (left.index(), right.index()) {
			(Some(left), Some(right)) => compare_indexes(IndexOrder { left, right }),
			_ => Ordering::Equal,
		})
}
fn compare_indexes(fields: IndexOrder<'_>) -> Ordering {
	let IndexOrder { left, right } = fields;
	left.identifier()
		.as_bytes()
		.cmp(right.identifier().as_bytes())
		.then_with(|| left.version().cmp(&right.version()))
}
fn source_rank(procedure: PgV10ConversionProcedure) -> u8 {
	match procedure {
		PgV10ConversionProcedure::RawBytesRelocationV1 => 0,
		PgV10ConversionProcedure::FleetStateRelocationV1 => 1,
		PgV10ConversionProcedure::LeaseAggregateRelocationV1 => 2,
		PgV10ConversionProcedure::QueueJobRelocationV1 => 3,
		PgV10ConversionProcedure::QueueDeadLetterRelocationV1 => 4,
		PgV10ConversionProcedure::QueuePauseRelocationV1 => 5,
	}
}
