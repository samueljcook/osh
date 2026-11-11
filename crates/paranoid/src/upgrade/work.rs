//! Durable immutable targets and pure controller status/generation transitions.
//!
//! Exact slot matching provides context consistency, not exclusive transaction authority,
//! writer exclusion, data coverage, physical drainage, or permission to promote. An executor
//! must establish those facts and atomically persist effective state and this work metadata.

use std::{fmt, num::NonZeroU64};

use primitives_rs::id::time_sortable::SortableId;
use serde_json::value::RawValue;

use crate::installation::control_state::InstallationControlState;

use super::policy::InstallationUpgradeCandidate;
use super::policy::request::{
	PolicyUpgradeRequest, PolicyUpgradeRequestError, PolicyUpgradeRequestIdentity,
};

pub(in crate::upgrade) mod document_checkpoint;
pub(in crate::upgrade) mod document_completion;
pub(in crate::upgrade) mod document_verification;
pub(in crate::upgrade) mod payload_checkpoint;
pub(in crate::upgrade) mod payload_completion;
pub(in crate::upgrade) mod queue_checkpoint;
pub(in crate::upgrade) mod target_completion;
pub(crate) mod v10;
mod wire;
use document_checkpoint::{DocumentCheckpointError, DocumentConversionCheckpoint};
use document_verification::{DocumentVerificationCheckpoint, DocumentVerificationError};
use payload_checkpoint::{PayloadCheckpointError, PayloadConversionCheckpoint};
use v10::source_checkpoint::{SourceCheckpointError, SourceConversionCheckpoint};

/// Explicit source retirement objective for an installation upgrade.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceGoal {
	/// Preserves the source phase.
	Unchanged,
	/// Requires durable exclusion of source writers while source data may remain.
	WritersRetired,
	/// Requires relocation and retirement of the selected source data.
	DataRetired,
}

/// The actual backend must supply this origin after checking its immutable registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UpgradeSourceOrigin {
	Fresh,
	Imported,
}

impl SourceGoal {
	pub(crate) fn require_origin(
		self,
		origin: UpgradeSourceOrigin,
	) -> Result<(), UpgradeWorkError> {
		if origin == UpgradeSourceOrigin::Fresh && self != Self::Unchanged {
			return Err(UpgradeWorkError::FreshSourceGoal);
		}
		Ok(())
	}
}

/// Policy shape is retained; executable declarations must bind again on every continuation.
#[derive(Clone)]
pub(crate) struct CompleteUpgradeTarget {
	request: PolicyUpgradeRequest,
	source_goal: SourceGoal,
}

impl CompleteUpgradeTarget {
	pub(crate) fn from_candidate(
		identity: PolicyUpgradeRequestIdentity,
		candidate: &InstallationUpgradeCandidate,
		source_goal: SourceGoal,
	) -> Self {
		Self {
			request: PolicyUpgradeRequest::from_candidate(identity, candidate),
			source_goal,
		}
	}

	pub(crate) fn request(&self) -> &PolicyUpgradeRequest {
		&self.request
	}
	pub(crate) fn source_goal(&self) -> SourceGoal {
		self.source_goal
	}

	pub(crate) fn matches_candidate(&self, candidate: &InstallationUpgradeCandidate) -> bool {
		self.request.matches_candidate(candidate)
	}

	pub(crate) fn matches_target(&self, other: &Self) -> bool {
		self.source_goal == other.source_goal && self.request.matches_request(&other.request)
	}

	pub(crate) fn require_origin(
		&self,
		origin: UpgradeSourceOrigin,
	) -> Result<(), UpgradeWorkError> {
		self.source_goal.require_origin(origin)
	}
}

impl fmt::Debug for CompleteUpgradeTarget {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("CompleteUpgradeTarget")
			.field("request", &self.request)
			.field("source_goal", &self.source_goal)
			.finish()
	}
}

/// The status of an upgrade in the installation's record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgUpgradeStatus {
	/// The installation records the upgrade, and no step has recorded progress for it.
	Queued,
	/// At least one step has recorded progress, and the upgrade has not ended.
	Building,
	/// The installation uses the upgrade's target policies, and the upgrade has ended.
	Completed,
	/// The upgrade was cancelled. Changes that its steps made before the cancellation remain.
	Cancelled,
}

/// A status/generation observation; executable checkpoints additionally need an actual invariant,
/// procedure, and phase. These private fields grant no exclusive or maintenance authority.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct UpgradeWorkCheckpointContext {
	request_id: SortableId,
	installation_id: SortableId,
	base_generation: NonZeroU64,
	owned_generation: NonZeroU64,
	status: PgUpgradeStatus,
}

impl fmt::Debug for UpgradeWorkCheckpointContext {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("UpgradeWorkCheckpointContext")
			.field("base_generation", &self.base_generation)
			.field("owned_generation", &self.owned_generation)
			.field("status", &self.status)
			.finish_non_exhaustive()
	}
}

/// An executor must apply this generation and the encoded work atomically with its effective
/// change. The value is pure transition output and supplies no promotion proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UpgradeWorkGenerationChange {
	pub(crate) expected: NonZeroU64,
	pub(crate) next: NonZeroU64,
}

pub(crate) struct UpgradeWork {
	target: CompleteUpgradeTarget,
	state: UpgradeWorkState,
}

/// Queued work has no progress, and its generation is the target's base generation. Completed
/// work has consumed every checkpoint, so it holds only its generation. Building and cancelled
/// work hold their progress, which includes their generation.
enum UpgradeWorkState {
	Queued,
	Building(UpgradeWorkProgress),
	Completed { owned_generation: NonZeroU64 },
	Cancelled(UpgradeWorkProgress),
}

struct UpgradeWorkProgress {
	owned_generation: NonZeroU64,
	document_conversion_checkpoints: Vec<DocumentConversionCheckpoint>,
	document_verification_checkpoints: Vec<DocumentVerificationCheckpoint>,
	payload_checkpoints: Vec<PayloadConversionCheckpoint>,
	source_checkpoints: Vec<SourceConversionCheckpoint>,
	queue_job_checkpoint: Option<Box<queue_checkpoint::QueueJobConversionCheckpoint>>,
}

impl UpgradeWorkProgress {
	fn new(owned_generation: NonZeroU64) -> Self {
		Self {
			owned_generation,
			document_conversion_checkpoints: Vec::new(),
			document_verification_checkpoints: Vec::new(),
			payload_checkpoints: Vec::new(),
			source_checkpoints: Vec::new(),
			queue_job_checkpoint: None,
		}
	}
}

impl UpgradeWork {
	/// Constructs unpersisted queued work; installed slot acceptance remains an executor operation.
	pub(crate) fn queued(
		target: CompleteUpgradeTarget,
		origin: UpgradeSourceOrigin,
	) -> Result<Self, UpgradeWorkError> {
		target.require_origin(origin)?;
		Ok(Self {
			target,
			state: UpgradeWorkState::Queued,
		})
	}

	pub(crate) fn target(&self) -> &CompleteUpgradeTarget {
		&self.target
	}
	pub(crate) fn status(&self) -> PgUpgradeStatus {
		match self.state {
			UpgradeWorkState::Queued => PgUpgradeStatus::Queued,
			UpgradeWorkState::Building(_) => PgUpgradeStatus::Building,
			UpgradeWorkState::Completed { .. } => PgUpgradeStatus::Completed,
			UpgradeWorkState::Cancelled(_) => PgUpgradeStatus::Cancelled,
		}
	}
	fn progress(&self) -> Option<&UpgradeWorkProgress> {
		match &self.state {
			UpgradeWorkState::Queued | UpgradeWorkState::Completed { .. } => None,
			UpgradeWorkState::Building(progress) | UpgradeWorkState::Cancelled(progress) => {
				Some(progress)
			}
		}
	}
	fn owned_generation(&self) -> NonZeroU64 {
		match &self.state {
			UpgradeWorkState::Queued => self.target.request.identity().base_generation,
			UpgradeWorkState::Completed { owned_generation } => *owned_generation,
			UpgradeWorkState::Building(progress) | UpgradeWorkState::Cancelled(progress) => {
				progress.owned_generation
			}
		}
	}
	fn building_progress_mut(&mut self) -> Result<&mut UpgradeWorkProgress, UpgradeWorkError> {
		if matches!(self.state, UpgradeWorkState::Queued) {
			self.state = UpgradeWorkState::Building(UpgradeWorkProgress::new(
				self.target.request.identity().base_generation,
			));
		}
		match &mut self.state {
			UpgradeWorkState::Building(progress) => Ok(progress),
			UpgradeWorkState::Completed { .. } | UpgradeWorkState::Cancelled(_) => {
				Err(UpgradeWorkError::TerminalWork)
			}
			UpgradeWorkState::Queued => unreachable!("queued work was moved to building"),
		}
	}
	fn take_unfinished_progress(&mut self) -> Result<UpgradeWorkProgress, UpgradeWorkError> {
		if !self.is_unfinished() {
			return Err(UpgradeWorkError::TerminalWork);
		}
		match std::mem::replace(&mut self.state, UpgradeWorkState::Queued) {
			UpgradeWorkState::Queued => Ok(UpgradeWorkProgress::new(
				self.target.request.identity().base_generation,
			)),
			UpgradeWorkState::Building(progress) => Ok(progress),
			UpgradeWorkState::Completed { .. } | UpgradeWorkState::Cancelled(_) => {
				unreachable!("terminal work was rejected")
			}
		}
	}
	pub(crate) fn checkpoint_context(&self) -> UpgradeWorkCheckpointContext {
		let identity = self.target.request.identity();
		UpgradeWorkCheckpointContext {
			request_id: identity.request_id,
			installation_id: identity.installation_id,
			base_generation: identity.base_generation,
			owned_generation: self.owned_generation(),
			status: self.status(),
		}
	}

	/// Semantic equality includes full checkpoint invariants and cursor states, with scope-set
	/// order ignored.
	pub(in crate::upgrade) fn matches_work(&self, other: &Self) -> bool {
		(match (self.queue_job_checkpoint(), other.queue_job_checkpoint()) {
			(None, None) => true,
			(Some(left), Some(right)) => left.matches(right),
			_ => false,
		}) && self.owned_generation() == other.owned_generation()
			&& self.status() == other.status()
			&& self.target.matches_target(&other.target)
			&& document_checkpoint::matches_checkpoints(
				self.document_conversion_checkpoints(),
				other.document_conversion_checkpoints(),
			) && document_verification::matches_checkpoints(
			self.document_verification_checkpoints(),
			other.document_verification_checkpoints(),
		) && payload_checkpoint::matches_checkpoints(
			payload_checkpoint::PayloadCheckpointComparisonFields {
				left: self.payload_checkpoints(),
				right: other.payload_checkpoints(),
			},
		) && v10::source_checkpoint::matches_checkpoints(
			self.source_checkpoints(),
			other.source_checkpoints(),
		)
	}

	/// Checks actual origin, installation, and owned generation without asserting executable
	/// support.
	pub(in crate::upgrade) fn require_control_context(
		&self,
		control: &InstallationControlState,
		origin: UpgradeSourceOrigin,
	) -> Result<(), UpgradeWorkError> {
		self.target.require_origin(origin)?;
		if control.installation_id() != self.target.request.identity().installation_id {
			return Err(UpgradeWorkError::InstallationMismatch);
		}
		if control.generation() != self.owned_generation() {
			return Err(UpgradeWorkError::GenerationMismatch);
		}
		Ok(())
	}

	/// Whether the work still owns the installation's policy changes: it is queued or building.
	pub(in crate::upgrade) fn is_unfinished(&self) -> bool {
		matches!(
			self.status(),
			PgUpgradeStatus::Queued | PgUpgradeStatus::Building
		)
	}

	/// Moves finished work to `generation`, the installation's next generation. The work then
	/// stays the recorded work of the control state that a change outside an upgrade writes. A
	/// scope addition at a runtime's start is that change.
	pub(in crate::upgrade) fn follow_generation_after_finish(
		&mut self,
		generation: NonZeroU64,
	) -> Result<(), UpgradeWorkError> {
		if self.is_unfinished() {
			return Err(UpgradeWorkError::UnfinishedWork);
		}
		if generation <= self.owned_generation() {
			return Err(UpgradeWorkError::GenerationMismatch);
		}
		match &mut self.state {
			UpgradeWorkState::Completed { owned_generation } => *owned_generation = generation,
			UpgradeWorkState::Cancelled(progress) => progress.owned_generation = generation,
			UpgradeWorkState::Queued | UpgradeWorkState::Building(_) => {
				unreachable!("unfinished work was rejected")
			}
		}
		Ok(())
	}

	/// Exact recorded slot and expected status checks supply no executable target binding.
	fn require_slot_context(
		&self,
		control: &InstallationControlState,
		origin: UpgradeSourceOrigin,
		expected: &UpgradeWorkCheckpointContext,
	) -> Result<(), UpgradeWorkError> {
		self.require_control_context(control, origin)?;
		if self.checkpoint_context() != *expected {
			return Err(UpgradeWorkError::CheckpointMismatch);
		}
		let metadata = control
			.pending_upgrade()
			.ok_or(UpgradeWorkError::WorkNotCurrent)?;
		let current = Self::decode_json(metadata)?;
		if !self.matches_work(&current) {
			return Err(UpgradeWorkError::WorkNotCurrent);
		}
		Ok(())
	}

	/// Rechecks full recorded work and actual policy target without changing status or generation.
	/// Source-phase, serializer, mapping, and readiness prerequisites belong to the later executor.
	pub(crate) fn require_checkpoint_context(
		&self,
		control: &InstallationControlState,
		candidate: &InstallationUpgradeCandidate,
		origin: UpgradeSourceOrigin,
		expected: &UpgradeWorkCheckpointContext,
	) -> Result<(), UpgradeWorkError> {
		self.require_slot_context(control, origin, expected)?;
		if !self.target.matches_candidate(candidate) {
			return Err(UpgradeWorkError::TargetMismatch);
		}
		Ok(())
	}

	/// Moves queued or building work to cancelled at its owned generation, with its progress.
	fn record_cancellation(&mut self) -> Result<UpgradeWorkGenerationChange, UpgradeWorkError> {
		if matches!(
			self.status(),
			PgUpgradeStatus::Completed | PgUpgradeStatus::Cancelled
		) {
			return Err(UpgradeWorkError::TerminalWork);
		}
		document_checkpoint::validate_checkpoints(
			&self.target,
			PgUpgradeStatus::Cancelled,
			self.document_conversion_checkpoints(),
		)?;
		document_verification::validate_checkpoints(
			&self.target,
			PgUpgradeStatus::Cancelled,
			self.document_verification_checkpoints(),
			self.document_conversion_checkpoints(),
		)?;
		v10::source_checkpoint::validate_checkpoints(
			&self.target,
			PgUpgradeStatus::Cancelled,
			self.source_checkpoints(),
		)?;
		payload_checkpoint::validate_checkpoints(
			&self.target,
			PgUpgradeStatus::Cancelled,
			self.payload_checkpoints(),
		)?;
		let generation = self.owned_generation();
		let progress = self.take_unfinished_progress()?;
		self.state = UpgradeWorkState::Cancelled(progress);
		Ok(UpgradeWorkGenerationChange {
			expected: generation,
			next: generation,
		})
	}

	fn next_build_generation(&self) -> Result<NonZeroU64, UpgradeWorkError> {
		self.owned_generation()
			.get()
			.checked_add(1)
			.and_then(NonZeroU64::new)
			.ok_or(UpgradeWorkError::GenerationOverflow)
	}

	/// Advances one effective build barrier directly from queued or building work.
	/// The executor must atomically persist the effective change, generation and work metadata.
	pub(in crate::upgrade) fn advance_effective_build(
		&mut self,
		control: &InstallationControlState,
		candidate: &InstallationUpgradeCandidate,
		origin: UpgradeSourceOrigin,
		expected: &UpgradeWorkCheckpointContext,
	) -> Result<UpgradeWorkGenerationChange, UpgradeWorkError> {
		self.require_checkpoint_context(control, candidate, origin, expected)?;
		if matches!(
			self.status(),
			PgUpgradeStatus::Completed | PgUpgradeStatus::Cancelled
		) {
			return Err(UpgradeWorkError::TerminalWork);
		}
		self.require_queue_job_checkpoint_invariant(control.component_policies())?;
		self.require_document_verification_invariants(control.scope_policies())?;
		self.require_payload_checkpoint_invariants(control.scope_policies())?;
		self.require_source_checkpoint_invariants(
			control.component_policies(),
			control.scope_policies(),
		)?;
		let result = UpgradeWorkGenerationChange {
			expected: self.owned_generation(),
			next: self.next_build_generation()?,
		};
		self.building_progress_mut()?.owned_generation = result.next;
		Ok(result)
	}

	/// Cancels recognized exact work without requiring executable support for its obsolete target.
	/// Effective installed state remains an executor-owned invariant and must be preserved.
	pub(crate) fn cancel(
		&mut self,
		control: &InstallationControlState,
		origin: UpgradeSourceOrigin,
		expected: &UpgradeWorkCheckpointContext,
	) -> Result<UpgradeWorkGenerationChange, UpgradeWorkError> {
		self.require_slot_context(control, origin, expected)?;
		self.record_cancellation()
	}

	pub(crate) fn encode_json(&self) -> Result<Box<RawValue>, UpgradeWorkError> {
		wire::encode_work(self)
	}
	pub(crate) fn decode_json(json: &RawValue) -> Result<Self, UpgradeWorkError> {
		wire::decode_work(json)
	}
}

impl fmt::Debug for UpgradeWork {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("UpgradeWork")
			.field("target", &self.target)
			.field("owned_generation", &self.owned_generation())
			.field("status", &self.status())
			.field(
				"document_checkpoint_count",
				&self.document_conversion_checkpoints().len(),
			)
			.field("source_checkpoint_count", &self.source_checkpoints().len())
			.field(
				"payload_checkpoint_count",
				&self.payload_checkpoints().len(),
			)
			.field(
				"document_verification_count",
				&self.document_verification_checkpoints().len(),
			)
			.finish()
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum UpgradeWorkError {
	#[error("upgrade work Queue job checkpoint is invalid")]
	QueueJobCheckpoint(#[from] queue_checkpoint::QueueJobCheckpointError),
	#[error("upgrade work complete target preparation failed")]
	TargetCompletion(#[source] Box<target_completion::TargetCompletionError>),
	#[error("upgrade work Primitive payload checkpoint is invalid")]
	PayloadCheckpoint(#[from] PayloadCheckpointError),
	#[error("upgrade work document verification checkpoint is invalid")]
	DocumentVerification(#[from] DocumentVerificationError),
	#[error("upgrade work source checkpoint is invalid")]
	SourceCheckpoint(#[from] SourceCheckpointError),
	#[error("upgrade work document checkpoint is invalid")]
	DocumentCheckpoint(#[from] DocumentCheckpointError),
	#[error("upgrade work policy target shape is invalid")]
	Request(#[from] PolicyUpgradeRequestError),
	#[error("upgrade work JSON is invalid: {category:?} error at line {line}, column {column}")]
	Json {
		category: primitives_rs::serialization::json::JsonFailureCategory,
		line: usize,
		column: usize,
	},
	#[error("upgrade work representation version {version} is unsupported")]
	UnsupportedVersion { version: u32 },
	#[error("upgrade work owned generation must be a canonical positive decimal u64")]
	InvalidOwnedGeneration,
	#[error("upgrade work status and owned generation are inconsistent with its immutable base")]
	InvalidGenerationState,
	#[error("fresh installation cannot request source retirement")]
	FreshSourceGoal,
	#[error("upgrade work checkpoint differs from its current status or generation")]
	CheckpointMismatch,
	#[error("upgrade work belongs to another installation")]
	InstallationMismatch,
	#[error("upgrade work owned generation differs from active control")]
	GenerationMismatch,
	#[error("upgrade work policy target differs from bound candidates")]
	TargetMismatch,
	#[error("upgrade work is not the current recorded slot")]
	WorkNotCurrent,
	#[error("upgrade work is terminal")]
	TerminalWork,
	#[error("upgrade work is unfinished")]
	UnfinishedWork,
	#[error("upgrade work generation cannot advance beyond u64")]
	GenerationOverflow,
}
