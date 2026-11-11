//! Pure retained intent and exact prewrite conditions for owned upgrade creation.
//!
//! These comparisons supply no transaction, ending, admission, or retry authority. The
//! owning executor must bind the target under canonical control authority before writing,
//! and must resolve the actual ending and local continuation before uncertain recovery.

use std::{fmt, num::NonZeroU64, sync::Arc};

use crate::runtime::local_admission::upgrade_work::RuntimeUpgradeWorkIdentity;
use crate::upgrade::work::{
	CompleteUpgradeTarget, SourceGoal, UpgradeSourceOrigin, UpgradeWork, UpgradeWorkError,
};
use crate::{
	installation::control_state::InstallationControlState,
	runtime::admission::bindings::PgInstallationAdmissionBinding,
	runtime::targets::NativeRuntimeTargetCode,
	upgrade::policy::{InstallationUpgradeCandidate, request::PolicyUpgradeRequestIdentity},
};

pub(crate) struct UpgradeCreationIntentFields {
	pub(crate) code: Arc<NativeRuntimeTargetCode>,
	pub(crate) candidate: Arc<InstallationUpgradeCandidate>,
	pub(crate) source_goal: SourceGoal,
}

/// One preselected request identity, without a fabricated base generation or target.
pub(crate) struct UpgradeCreationIntent {
	fields: UpgradeCreationIntentFields,
	identity: RuntimeUpgradeWorkIdentity,
	origin: UpgradeSourceOrigin,
}

impl UpgradeCreationIntent {
	pub(crate) fn new(fields: UpgradeCreationIntentFields) -> Result<Self, UpgradeCreationError> {
		let origin = match fields.code.installation().as_ref() {
			PgInstallationAdmissionBinding::Fresh { .. } => UpgradeSourceOrigin::Fresh,
			PgInstallationAdmissionBinding::Imported { .. } => UpgradeSourceOrigin::Imported,
		};
		fields.source_goal.require_origin(origin)?;
		let identity = RuntimeUpgradeWorkIdentity {
			installation_id: fields
				.code
				.administration()
				.control_component()
				.installation_id(),
			request_id: primitives_rs::id::time_sortable::SortableId::generate()
				.map_err(UpgradeCreationError::Random)?,
		};
		Ok(Self {
			fields,
			identity,
			origin,
		})
	}

	pub(crate) fn identity(&self) -> RuntimeUpgradeWorkIdentity {
		self.identity
	}
	pub(crate) fn code(&self) -> &Arc<NativeRuntimeTargetCode> {
		&self.fields.code
	}
	pub(crate) fn candidate(&self) -> &Arc<InstallationUpgradeCandidate> {
		&self.fields.candidate
	}
	pub(crate) fn origin(&self) -> UpgradeSourceOrigin {
		self.origin
	}

	/// Prepares an initial condition. Only an owning verified rollback decision may refresh
	/// an unresolved attempt through this method; absence cannot establish rollback.
	pub(crate) fn prepare(
		self: &Arc<Self>,
		current: &InstallationControlState,
	) -> Result<UpgradeCreationPreparation, UpgradeCreationError> {
		self.require_installation(current)?;
		let prior = match current
			.pending_upgrade()
			.map(UpgradeWork::decode_json)
			.transpose()?
		{
			None => PriorCreateSlot::Absent,
			Some(prior) => {
				if prior.target().request().identity().request_id == self.identity.request_id {
					self.require_existing_meaning(&prior)?;
					prior.require_control_context(current, self.origin)?;
					return Ok(UpgradeCreationPreparation::Existing(Box::new(prior)));
				}
				prior.require_control_context(current, self.origin)?;
				if prior.is_unfinished() {
					return Err(UpgradeCreationError::AlreadyPending);
				}
				PriorCreateSlot::Terminal(Box::new(prior))
			}
		};
		let observed_generation = current.generation();
		let target = CompleteUpgradeTarget::from_candidate(
			PolicyUpgradeRequestIdentity {
				request_id: self.identity.request_id,
				installation_id: self.identity.installation_id,
				base_generation: observed_generation,
			},
			&self.fields.candidate,
			self.fields.source_goal,
		);
		let work = UpgradeWork::queued(target, self.origin)?;
		Ok(UpgradeCreationPreparation::Prepared(Box::new(
			RetainedPreparedAttempt {
				intent: Arc::clone(self),
				observed_generation,
				origin: self.origin,
				prior,
				work: Arc::new(work),
			},
		)))
	}

	fn require_installation(
		&self,
		current: &InstallationControlState,
	) -> Result<(), UpgradeCreationError> {
		if current.installation_id() != self.identity.installation_id {
			return Err(UpgradeCreationError::InstallationMismatch);
		}
		Ok(())
	}

	fn require_existing_meaning(&self, work: &UpgradeWork) -> Result<(), UpgradeCreationError> {
		let target = work.target();
		let identity = target.request().identity();
		if identity.request_id != self.identity.request_id
			|| identity.installation_id != self.identity.installation_id
			|| target.source_goal() != self.fields.source_goal
			|| !target.matches_candidate(&self.fields.candidate)
		{
			return Err(UpgradeCreationError::IdentityConflict);
		}
		Ok(())
	}
}

impl fmt::Debug for UpgradeCreationIntent {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("UpgradeCreationIntent")
			.field("origin", &self.origin)
			.field("source_goal", &self.fields.source_goal)
			.finish_non_exhaustive()
	}
}

pub(crate) enum UpgradeCreationPreparation {
	Existing(Box<UpgradeWork>),
	Prepared(Box<RetainedPreparedAttempt>),
}

/// Exact prior condition, including all terminal target, generation, and checkpoint metadata.
pub(in crate::upgrade) enum PriorCreateSlot {
	Absent,
	Terminal(Box<UpgradeWork>),
}

/// Must be retained by the owning continuation before its first write await.
pub(crate) struct RetainedPreparedAttempt {
	intent: Arc<UpgradeCreationIntent>,
	observed_generation: NonZeroU64,
	origin: UpgradeSourceOrigin,
	prior: PriorCreateSlot,
	work: Arc<UpgradeWork>,
}

impl RetainedPreparedAttempt {
	pub(crate) fn work(&self) -> &UpgradeWork {
		&self.work
	}
	pub(in crate::upgrade) fn shared_work(&self) -> &Arc<UpgradeWork> {
		&self.work
	}

	/// A read-only result is diagnostic. A retrying executor must repeat this comparison
	/// under the same canonical row authority that it retains through the actual write.
	pub(in crate::upgrade) fn compare_recovery(
		&self,
		current: &InstallationControlState,
	) -> Result<UpgradeCreationRecovery, UpgradeCreationError> {
		self.intent.require_installation(current)?;
		let slot = match current
			.pending_upgrade()
			.map(UpgradeWork::decode_json)
			.transpose()?
		{
			Some(work)
				if work.target().request().identity().request_id
					== self.intent.identity.request_id =>
			{
				self.intent.require_existing_meaning(&work)?;
				if !self.work.target().matches_target(work.target()) {
					return Err(UpgradeCreationError::IdentityConflict);
				}
				work.require_control_context(current, self.origin)?;
				return Ok(UpgradeCreationRecovery::Existing(Box::new(work)));
			}
			other => other,
		};
		if current.generation() != self.observed_generation {
			return Err(UpgradeCreationError::HistoryAmbiguity {
				change: UpgradeCreationHistoryChange::Generation,
			});
		}
		let matches_prior = match (&self.prior, &slot) {
			(PriorCreateSlot::Absent, None) => true,
			(PriorCreateSlot::Terminal(prior), Some(current)) => prior.matches_work(current),
			_ => false,
		};
		if !matches_prior {
			return Err(UpgradeCreationError::HistoryAmbiguity {
				change: UpgradeCreationHistoryChange::PendingSlot,
			});
		}
		Ok(UpgradeCreationRecovery::RetrySameCondition)
	}
}

impl fmt::Debug for RetainedPreparedAttempt {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("RetainedPreparedAttempt")
			.field("observed_generation", &self.observed_generation)
			.field("origin", &self.origin)
			.field(
				"prior_terminal",
				&matches!(self.prior, PriorCreateSlot::Terminal(_)),
			)
			.finish_non_exhaustive()
	}
}

pub(in crate::upgrade) enum UpgradeCreationRecovery {
	Existing(Box<UpgradeWork>),
	RetrySameCondition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UpgradeCreationHistoryChange {
	Generation,
	PendingSlot,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum UpgradeCreationError {
	#[error("fresh upgrade request identity generation failed")]
	Random(#[source] primitives_rs::random::secure_bytes::RandomError),
	#[error("fresh upgrade request installation does not match control")]
	InstallationMismatch,
	#[error("an upgrade target or active work already occupies the pending slot")]
	AlreadyPending,
	#[error("the request identity has different recorded target meaning")]
	IdentityConflict,
	#[error("uncertain upgrade creation history changed: {change:?}")]
	HistoryAmbiguity {
		change: UpgradeCreationHistoryChange,
	},
	#[error("upgrade work context is invalid")]
	Work(#[from] UpgradeWorkError),
}
