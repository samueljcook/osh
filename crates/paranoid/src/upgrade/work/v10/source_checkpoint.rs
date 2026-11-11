//! Pure source conversion metadata with actual parsed keys and complete installed policies.
//!
//! Checkpoints prove no route, page, source exclusion, conversion, or native coverage.

use std::fmt;

use crate::installation::contract::ComponentPolicy;
use crate::queue::{JobId, QueuePauseScope, source_pause_key};
use crate::scope::{ScopeError, ScopeIdentity, ScopePolicySet};
use crate::{
	fleet::lease,
	installation::control_state::InstallationControlState,
	kv,
	upgrade::policy::{InstallationUpgradeCandidate, document_readiness::DocumentReadinessStage},
};

use super::super::{
	PgUpgradeStatus, SourceGoal, UpgradeSourceOrigin, UpgradeWork, UpgradeWorkCheckpointContext,
	UpgradeWorkError,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// A procedure that moves one kind of v10 record into Paranoid's storage.
#[non_exhaustive]
pub enum PgV10ConversionProcedure {
	/// Moves v10 key-value records into the core Raw namespace.
	RawBytesRelocationV1,
	/// Moves the v10 Fleet resources that the Fleet source assignments name into their
	/// declared scopes.
	FleetStateRelocationV1,
	/// Moves v10 leases, including the history of their fencing tokens.
	LeaseAggregateRelocationV1,
	/// Moves v10 Queue jobs and reads their payloads with the v10 Queue payload encodings.
	QueueJobRelocationV1,
	/// Moves v10 Queue dead letters.
	QueueDeadLetterRelocationV1,
	/// Moves v10 Queue pauses.
	QueuePauseRelocationV1,
}

/// Exact published pause identity, independent of the native pause key representation.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct SourceConversionPauseKey {
	scope: QueuePauseScope,
	key: String,
}
impl SourceConversionPauseKey {
	pub(crate) fn from_scope(scope: QueuePauseScope) -> Self {
		let key = source_pause_key(&scope);
		Self { scope, key }
	}
	pub(crate) fn scope(&self) -> &QueuePauseScope {
		&self.scope
	}
	pub(crate) fn as_str(&self) -> &str {
		&self.key
	}
}
impl fmt::Debug for SourceConversionPauseKey {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("SourceConversionPauseKey")
			.field("key_byte_len", &self.key.len())
			.finish_non_exhaustive()
	}
}

/// Each variant retains its actual key domain; ordering uses the exact stored bytes.
#[derive(Clone, Eq, PartialEq)]
pub(crate) enum SourceConversionCursor {
	RawBytes(kv::Key),
	FleetState(kv::Key),
	LeaseAggregate(lease::LeaseKey),
	QueueJob(JobId),
	QueueDeadLetter(JobId),
	QueuePause(SourceConversionPauseKey),
}
impl SourceConversionCursor {
	pub(crate) fn procedure(&self) -> PgV10ConversionProcedure {
		match self {
			Self::RawBytes(_) => PgV10ConversionProcedure::RawBytesRelocationV1,
			Self::FleetState(_) => PgV10ConversionProcedure::FleetStateRelocationV1,
			Self::LeaseAggregate(_) => PgV10ConversionProcedure::LeaseAggregateRelocationV1,
			Self::QueueJob(_) => PgV10ConversionProcedure::QueueJobRelocationV1,
			Self::QueueDeadLetter(_) => PgV10ConversionProcedure::QueueDeadLetterRelocationV1,
			Self::QueuePause(_) => PgV10ConversionProcedure::QueuePauseRelocationV1,
		}
	}
	pub(crate) fn as_bytes(&self) -> &[u8] {
		match self {
			Self::RawBytes(key) | Self::FleetState(key) => key.as_str().as_bytes(),
			Self::LeaseAggregate(key) => key.as_str().as_bytes(),
			Self::QueueJob(id) | Self::QueueDeadLetter(id) => id.as_bytes(),
			Self::QueuePause(key) => key.as_str().as_bytes(),
		}
	}
}
impl fmt::Debug for SourceConversionCursor {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("SourceConversionCursor")
			.field("procedure", &self.procedure())
			.field("key_byte_len", &self.as_bytes().len())
			.finish()
	}
}

/// The executor derives the required subset from actual declarations. This parsed set
/// carries no assertion that the subset is sufficient or that its code is supported.
pub(crate) struct SourceConversionInvariantFields {
	pub(crate) component_policies: Vec<ComponentPolicy>,
	pub(crate) scope_policies: ScopePolicySet,
}
#[derive(Clone)]
pub(crate) struct SourceConversionInvariant {
	component_policies: Vec<ComponentPolicy>,
	scope_policies: ScopePolicySet,
}
impl SourceConversionInvariant {
	pub(crate) fn new(
		fields: SourceConversionInvariantFields,
	) -> Result<Self, SourceCheckpointError> {
		let SourceConversionInvariantFields {
			component_policies: policies,
			scope_policies,
		} = fields;
		if policies.is_empty() && scope_policies.policies().is_empty() {
			return Err(SourceCheckpointError::EmptyInvariant);
		}
		for (index, policy) in policies.iter().enumerate() {
			if policies[..index]
				.iter()
				.any(|prior| prior.component_identifier() == policy.component_identifier())
			{
				return Err(SourceCheckpointError::DuplicateComponent { index });
			}
		}
		Ok(Self {
			component_policies: policies,
			scope_policies,
		})
	}
	pub(crate) fn component_policies(&self) -> &[ComponentPolicy] {
		&self.component_policies
	}
	pub(crate) fn scope_policies(&self) -> &ScopePolicySet {
		&self.scope_policies
	}
	pub(in crate::upgrade) fn matches_invariant(&self, other: &Self) -> bool {
		self.scope_policies.matches_policies(&other.scope_policies)
			&& self.component_policies.len() == other.component_policies.len()
			&& self.component_policies.iter().all(|policy| {
				other
					.component_policies
					.iter()
					.any(|other| policy.matches_policy(other))
			})
	}
	fn require_policies(
		&self,
		actual: &[ComponentPolicy],
		scopes: &ScopePolicySet,
	) -> Result<(), SourceCheckpointError> {
		for (policy_index, policy) in self.component_policies.iter().enumerate() {
			if !actual.iter().any(|actual| policy.matches_policy(actual)) {
				return Err(SourceCheckpointError::CurrentInvariantMismatch { policy_index });
			}
		}
		for required in self.scope_policies.policies() {
			if !scopes
				.lookup(required.identity())
				.is_ok_and(|actual| required.matches_policy(actual))
			{
				return Err(SourceCheckpointError::CurrentScopeInvariantMismatch {
					scope: required.identity().clone(),
				});
			}
		}
		Ok(())
	}
}
impl fmt::Debug for SourceConversionInvariant {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("SourceConversionInvariant")
			.field("component_policy_count", &self.component_policies.len())
			.field("scope_policy_count", &self.scope_policies.policies().len())
			.finish_non_exhaustive()
	}
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SourceScanProgress {
	Scanning {
		after_key: Option<SourceConversionCursor>,
	},
	Scanned {
		last_key: Option<SourceConversionCursor>,
	},
}

pub(in crate::upgrade) enum SourceScanUpdate {
	Advance {
		after_key: SourceConversionCursor,
	},
	Scanned {
		last_key: Option<SourceConversionCursor>,
	},
}

pub(crate) struct SourceConversionCheckpoint {
	pub(in crate::upgrade::work) procedure: PgV10ConversionProcedure,
	pub(in crate::upgrade::work) invariant: SourceConversionInvariant,
	pub(in crate::upgrade::work) progress: SourceScanProgress,
}
impl SourceConversionCheckpoint {
	pub(crate) fn procedure(&self) -> PgV10ConversionProcedure {
		self.procedure
	}
	pub(crate) fn invariant(&self) -> &SourceConversionInvariant {
		&self.invariant
	}
	pub(crate) fn progress(&self) -> &SourceScanProgress {
		&self.progress
	}
	fn matches(&self, other: &Self) -> bool {
		self.procedure == other.procedure
			&& self.progress == other.progress
			&& self.invariant.matches_invariant(&other.invariant)
	}
}
impl fmt::Debug for SourceConversionCheckpoint {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("SourceConversionCheckpoint")
			.field("procedure", &self.procedure)
			.field("progress", &self.progress)
			.field("invariant", &self.invariant)
			.finish()
	}
}

/// Actual source phase, code, and consuming data authority remain executor prerequisites.
pub(in crate::upgrade) struct SourceCheckpointContext<'a> {
	pub(crate) control: &'a InstallationControlState,
	pub(crate) candidate: &'a InstallationUpgradeCandidate,
	pub(crate) origin: UpgradeSourceOrigin,
	pub(crate) expected: &'a UpgradeWorkCheckpointContext,
}

impl UpgradeWork {
	pub(crate) fn source_checkpoints(&self) -> &[SourceConversionCheckpoint] {
		self.progress()
			.map_or(&[], |progress| progress.source_checkpoints.as_slice())
	}
	pub(crate) fn source_checkpoint(
		&self,
		procedure: PgV10ConversionProcedure,
	) -> Option<&SourceConversionCheckpoint> {
		self.source_checkpoints()
			.iter()
			.find(|checkpoint| checkpoint.procedure == procedure)
	}
	/// Compares one actual selected meaning and exhausted metadata, without proving source
	/// coverage.
	pub(in crate::upgrade) fn require_selected_source_checkpoint(
		&self,
		procedure: PgV10ConversionProcedure,
		invariant: &SourceConversionInvariant,
	) -> Result<(), SourceCheckpointError> {
		let checkpoint = self
			.source_checkpoint(procedure)
			.ok_or(SourceCheckpointError::RetirementCheckpointMissing { procedure })?;
		if !checkpoint.invariant().matches_invariant(invariant) {
			return Err(SourceCheckpointError::RetirementInvariantMismatch { procedure });
		}
		if !matches!(checkpoint.progress(), SourceScanProgress::Scanned { .. }) {
			return Err(SourceCheckpointError::RetirementScanIncomplete { procedure });
		}
		Ok(())
	}
	/// An effective guard must check its actual prepared next policies before preserving
	/// source progress. This check neither installs policies nor supplies coverage proof.
	pub(in crate::upgrade) fn require_source_checkpoint_invariants(
		&self,
		policies: &[ComponentPolicy],
		scopes: &ScopePolicySet,
	) -> Result<(), UpgradeWorkError> {
		for checkpoint in self.source_checkpoints() {
			checkpoint.invariant.require_policies(policies, scopes)?;
		}
		Ok(())
	}
	/// Prepares only selected scope entries affected by the opaque exact Ready addition.
	/// Every check and metadata copy precedes the owning Work's generation/status mutation.
	pub(in crate::upgrade::work) fn prepare_source_readiness_rebase(
		&self,
		control: &InstallationControlState,
		stage: &DocumentReadinessStage,
	) -> Result<Vec<(usize, SourceConversionInvariant)>, UpgradeWorkError> {
		self.require_source_checkpoint_invariants(
			control.component_policies(),
			control.scope_policies(),
		)?;
		let mut prepared = Vec::new();
		for (index, checkpoint) in self.source_checkpoints().iter().enumerate() {
			let Some(scope) = checkpoint
				.invariant
				.scope_policies
				.policies()
				.iter()
				.find(|scope| scope.identity() == stage.policy().identity())
			else {
				continue;
			};
			if !stage.writer_stage().matches_policy(scope) {
				return Err(SourceCheckpointError::CurrentScopeInvariantMismatch {
					scope: scope.identity().clone(),
				}
				.into());
			}
			let scopes = checkpoint
				.invariant
				.scope_policies
				.with_replaced_scope(stage.policy())
				.map_err(SourceCheckpointError::Scope)?;
			prepared.push((
				index,
				SourceConversionInvariant::new(SourceConversionInvariantFields {
					component_policies: checkpoint.invariant.component_policies.clone(),
					scope_policies: scopes,
				})?,
			));
		}
		Ok(prepared)
	}
	fn require_source_context(
		&self,
		context: &SourceCheckpointContext<'_>,
		invariant: &SourceConversionInvariant,
	) -> Result<(), UpgradeWorkError> {
		self.require_checkpoint_context(
			context.control,
			context.candidate,
			context.origin,
			context.expected,
		)?;
		if matches!(
			self.status(),
			PgUpgradeStatus::Completed | PgUpgradeStatus::Cancelled
		) {
			return Err(UpgradeWorkError::TerminalWork);
		}
		if context.origin != UpgradeSourceOrigin::Imported {
			return Err(SourceCheckpointError::ImportedRequired.into());
		}
		require_source_goal(&self.target)?;
		invariant.require_policies(
			context.control.component_policies(),
			context.control.scope_policies(),
		)?;
		Ok(())
	}
	/// All validation precedes mutation. The executor must commit its accounted data and
	/// this progress together; this pure update establishes no page or migration proof.
	pub(in crate::upgrade) fn record_source_scan(
		&mut self,
		context: SourceCheckpointContext<'_>,
		procedure: PgV10ConversionProcedure,
		invariant: &SourceConversionInvariant,
		update: SourceScanUpdate,
	) -> Result<(), UpgradeWorkError> {
		self.require_source_context(&context, invariant)?;
		let position = self
			.source_checkpoints()
			.iter()
			.position(|checkpoint| checkpoint.procedure == procedure);
		let after_key = match position {
			None => None,
			Some(index) => {
				let checkpoint = &self.source_checkpoints()[index];
				if !checkpoint.invariant.matches_invariant(invariant) {
					return Err(SourceCheckpointError::StoredInvariantMismatch.into());
				}
				let SourceScanProgress::Scanning { after_key } = &checkpoint.progress else {
					return Err(SourceCheckpointError::AlreadyScanned.into());
				};
				after_key.as_ref()
			}
		};
		let next = match update {
			SourceScanUpdate::Advance { after_key: next } => {
				require_cursor(procedure, Some(&next))?;
				if after_key.is_some_and(|previous| next.as_bytes() <= previous.as_bytes()) {
					return Err(SourceCheckpointError::CursorNotAdvanced.into());
				}
				SourceScanProgress::Scanning {
					after_key: Some(next),
				}
			}
			SourceScanUpdate::Scanned { last_key } => {
				require_cursor(procedure, last_key.as_ref())?;
				if after_key.is_some_and(|previous| {
					last_key
						.as_ref()
						.is_none_or(|last| last.as_bytes() < previous.as_bytes())
				}) {
					return Err(SourceCheckpointError::CursorRewind.into());
				}
				SourceScanProgress::Scanned { last_key }
			}
		};
		let progress = self.building_progress_mut()?;
		match position {
			Some(index) => progress.source_checkpoints[index].progress = next,
			None => progress
				.source_checkpoints
				.push(SourceConversionCheckpoint {
					procedure,
					invariant: invariant.clone(),
					progress: next,
				}),
		}
		Ok(())
	}
}

pub(in crate::upgrade::work) fn matches_checkpoints(
	left: &[SourceConversionCheckpoint],
	right: &[SourceConversionCheckpoint],
) -> bool {
	left.len() == right.len()
		&& left
			.iter()
			.all(|checkpoint| right.iter().any(|other| checkpoint.matches(other)))
}

pub(in crate::upgrade::work) fn validate_checkpoints(
	target: &super::super::CompleteUpgradeTarget,
	status: PgUpgradeStatus,
	checkpoints: &[SourceConversionCheckpoint],
) -> Result<(), SourceCheckpointError> {
	if !checkpoints.is_empty() {
		match status {
			PgUpgradeStatus::Queued => return Err(SourceCheckpointError::QueuedProgress),
			PgUpgradeStatus::Completed => return Err(SourceCheckpointError::CompletedProgress),
			PgUpgradeStatus::Building | PgUpgradeStatus::Cancelled => {}
		}
		require_source_goal(target)?;
	}
	for (index, checkpoint) in checkpoints.iter().enumerate() {
		if checkpoints[..index]
			.iter()
			.any(|prior| prior.procedure == checkpoint.procedure)
		{
			return Err(SourceCheckpointError::DuplicateProcedure { index });
		}
		let key = match &checkpoint.progress {
			SourceScanProgress::Scanning { after_key } => after_key,
			SourceScanProgress::Scanned { last_key } => last_key,
		};
		require_cursor(checkpoint.procedure, key.as_ref())?;
	}
	Ok(())
}
fn require_source_goal(
	target: &super::super::CompleteUpgradeTarget,
) -> Result<(), SourceCheckpointError> {
	if target.source_goal() != SourceGoal::DataRetired {
		return Err(SourceCheckpointError::DataRetiredGoalRequired);
	}
	Ok(())
}
fn require_cursor(
	procedure: PgV10ConversionProcedure,
	cursor: Option<&SourceConversionCursor>,
) -> Result<(), SourceCheckpointError> {
	if cursor.is_some_and(|cursor| cursor.procedure() != procedure) {
		return Err(SourceCheckpointError::CursorProcedureMismatch);
	}
	Ok(())
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SourceCheckpointError {
	#[error("source retirement has no checkpoint for {procedure:?}")]
	RetirementCheckpointMissing { procedure: PgV10ConversionProcedure },
	#[error("source retirement selected checkpoint invariant differs for {procedure:?}")]
	RetirementInvariantMismatch { procedure: PgV10ConversionProcedure },
	#[error("source retirement checkpoint is not exhausted for {procedure:?}")]
	RetirementScanIncomplete { procedure: PgV10ConversionProcedure },
	#[error("source checkpoint scope invariant differs from installed policy")]
	CurrentScopeInvariantMismatch { scope: ScopeIdentity },
	#[error("source checkpoint scope invariant is invalid")]
	Scope(#[source] ScopeError),
	#[error("source checkpoint invariant requires at least one selected component or scope policy")]
	EmptyInvariant,
	#[error("source checkpoint invariant component {index} is duplicated")]
	DuplicateComponent { index: usize },
	#[error("source checkpoint {index} repeats a procedure")]
	DuplicateProcedure { index: usize },
	#[error("queued work cannot contain source progress")]
	QueuedProgress,
	#[error("completed work cannot contain source progress")]
	CompletedProgress,
	#[error("source conversion requires an imported installation")]
	ImportedRequired,
	#[error("source conversion requires the explicit data-retirement target")]
	DataRetiredGoalRequired,
	#[error("source checkpoint invariant policy {policy_index} differs from installed policies")]
	CurrentInvariantMismatch { policy_index: usize },
	#[error("source checkpoint stored invariant differs from its selected current invariant")]
	StoredInvariantMismatch,
	#[error("source conversion cursor does not match its fixed procedure")]
	CursorProcedureMismatch,
	#[error("source conversion cursor must advance strictly bytewise")]
	CursorNotAdvanced,
	#[error("source conversion terminal cursor cannot rewind or discard a prior key")]
	CursorRewind,
	#[error("source conversion scan is already exhausted")]
	AlreadyScanned,
	#[error("source conversion KV cursor is invalid")]
	KvCursor(#[source] kv::KeyError),
	#[error("source conversion lease cursor lacks its final separator")]
	LeaseFinalSeparator,
	#[error("source conversion lease cursor is invalid")]
	LeaseCursor(#[source] lease::CoordinationKeyError),
	#[error("source conversion Queue ID cursor is invalid")]
	IdCursor(#[source] primitives_rs::id::time_sortable::SortableIdParseError),
	#[error("source conversion Queue task cursor is invalid")]
	TaskCursor(#[source] crate::queue::TaskNameError),
	#[error("source conversion pause cursor key/task association is invalid")]
	PauseCursor(#[source] crate::queue::QueueAuxiliarySourceImportError),
}
