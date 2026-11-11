//! Fixed installation of an absent Document, Raw, or primitive scope without adopting existing
//! values: by an upgrade step, or by a runtime that starts with declarations of scopes that the
//! installation does not have.

use crate::upgrade::wait::{
	deadline_after_positive_wait, require_before_deadline, require_valid_wait, tokio_instant,
};
use primitives_rs::time::Deadline;
use std::{sync::Arc, time::Duration};

use crate::db::WriteTx;
use crate::db::transaction::run_registered_transaction_write;
use crate::installation::control_state::InstallationControlState;
use crate::installation::creation::NativeInstallationError;
use crate::records::RecordIdentity;
use crate::records::postgres::scan::{RecordMaximumValuedKey, RecordPageError};
use crate::runtime::admission::bindings::InstallationStorageRoute;
use crate::runtime::admission::upgrade::NativeUpgradeInvocation;
use crate::runtime::targets::ResolvedNativeRuntimeTarget;
use crate::scope::managed::{
	ManagedScopeRegistration, ManagedScopeRegistry, ManagedScopeRegistryError,
};
use crate::scope::selection::ScopeSelectionError;
use crate::scope::{
	DocumentScopePolicy, ScopeError, ScopeIdentity, ScopeOwner, ScopePolicy, ScopePolicySet,
};
use crate::upgrade::observation::{
	NativeUpgradeObservedControl, NativeUpgradeObservedControlFields,
};
use crate::upgrade::participants::ParticipantInspectionFields;
use crate::upgrade::policy::UpgradeCompatibilityError;
use crate::upgrade::policy::assessment::{
	ParticipantPolicyAssessment, ParticipantPolicyAssessmentFields,
};
use crate::upgrade::policy::control_replacement::EffectiveControlReplacement;
use crate::upgrade::work::{PgUpgradeStatus, UpgradeSourceOrigin, UpgradeWork, UpgradeWorkError};

use super::{
	NativeUpgradeParticipantsError, ParticipantInspectionBudget, ParticipantInspectionCoverage,
	ParticipantInspectionSummary, PgNativeUpgradeParticipants,
};

pub(in crate::upgrade) struct ScopeAdditionRequest<'a> {
	pub(crate) invocation: NativeUpgradeInvocation<'a>,
	pub(crate) resolved: &'a ResolvedNativeRuntimeTarget,
	pub(crate) scope: &'a ScopeIdentity,
	pub(crate) participant_budget: ParticipantInspectionBudget,
	/// Bounds protected work after owned BEGIN; transaction ending retains its own lifetime.
	pub(crate) installation_wait_timeout: Duration,
}

/// Both outcomes require known owned COMMIT and attribute no prior uncertain operation.
#[derive(Debug)]
pub(in crate::upgrade) enum ScopeAdditionResult {
	Installed {
		work: UpgradeWork,
		observed: NativeUpgradeObservedControl,
	},
	AlreadyInstalled {
		work: UpgradeWork,
		observed: NativeUpgradeObservedControl,
	},
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ScopeAdditionError {
	#[error("additive scope resolved code differs from the installation")]
	ResolvedCodeAssociationMismatch,
	#[error("additive scope protected wait must be positive and fit the local clock")]
	InvalidInstallationWaitTimeout,
	#[error("additive scope protected work exceeded its configured wait")]
	InstallationWaitElapsed,
	#[error("additive scope is absent from the complete target")]
	ScopeNotTargeted { scope: ScopeIdentity },
	#[error("additive scope requires a Document, Raw, or primitive owner")]
	UnsupportedOwner { scope: ScopeIdentity },
	#[error("additive scope already has different installed ownership or policy")]
	ExistingScopeRequiresProcedure { scope: ScopeIdentity },
	#[error("additive scope code is unavailable")]
	Code {
		scope: ScopeIdentity,
		#[source]
		source: Box<ManagedScopeRegistryError>,
	},
	#[error("additive scope parsed policy or overlay failed")]
	Scope {
		scope: ScopeIdentity,
		#[source]
		source: ScopeError,
	},
	#[error("additive scope conflicts with the backend association")]
	Backend(#[source] Box<NativeInstallationError>),
	#[error("additive scope namespace observation failed")]
	Namespace {
		scope: ScopeIdentity,
		#[source]
		source: RecordPageError,
	},
	#[error("additive scope namespace contains an existing valued record")]
	PreexistingData {
		scope: ScopeIdentity,
		identity: RecordIdentity,
	},
	#[error(
		"additive scope requires a complete compatible current and proposed participant inspection"
	)]
	ParticipantInspection {
		scope: ScopeIdentity,
		summary: Box<ParticipantInspectionSummary>,
	},
	#[error("the target policy of a declared scope that the installation lacks cannot be selected")]
	DeclaredScopeSelection(#[source] Box<ScopeSelectionError>),
	#[error("the installation generation cannot advance")]
	GenerationOverflow,
}
impl ScopeAdditionError {
	pub(crate) fn database_error(&self) -> Option<&crate::db::DatabaseError> {
		match self {
			Self::Backend(source) => source.database_error(),
			Self::Namespace {
				source: RecordPageError::Database(source),
				..
			} => Some(source),
			Self::Namespace {
				source: RecordPageError::Record(source),
				..
			} => source.database_error(),
			_ => None,
		}
	}
	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::Backend(source) => {
				crate::db::TransactionFailure::involves_database_failure(source.as_ref())
			}
			_ => self.database_error().is_some(),
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Backend(source) => {
				crate::db::TransactionFailure::commit_outcome_is_unknown(source.as_ref())
			}
			_ => self
				.database_error()
				.is_some_and(crate::db::DatabaseError::commit_outcome_is_unknown),
		}
	}
}
impl From<ScopeAdditionError> for NativeUpgradeParticipantsError {
	fn from(source: ScopeAdditionError) -> Self {
		Self::ScopeAddition(Box::new(source))
	}
}

impl PgNativeUpgradeParticipants {
	pub(in crate::upgrade) async fn install_additive_scope(
		&self,
		mut request: ScopeAdditionRequest<'_>,
	) -> Result<ScopeAdditionResult, NativeUpgradeParticipantsError> {
		request.invocation.require_bindings(
			&self.config.installation_binding,
			&self.config.control_storage,
			&self.config.administration,
		)?;
		if !Arc::ptr_eq(
			request.resolved.code().installation(),
			&self.config.installation_binding,
		) || !Arc::ptr_eq(
			request.resolved.code().administration(),
			&self.config.administration,
		) {
			return Err(ScopeAdditionError::ResolvedCodeAssociationMismatch.into());
		}
		if !request
			.invocation
			.target()
			.matches_target(request.resolved.target())
			|| !request
				.invocation
				.target()
				.matches_candidate(request.resolved.candidate())
		{
			return Err(UpgradeWorkError::TargetMismatch.into());
		}
		require_valid_wait(
			request.installation_wait_timeout,
			ScopeAdditionError::InvalidInstallationWaitTimeout,
		)?;
		let registration = request.invocation.prepare_transaction()?;
		run_registered_transaction_write(
			&self.config.control_pool,
			"db.native_upgrade_participants.install_additive_scope",
			registration,
			async |tx| {
				request.invocation.require_transaction(tx)?;
				let mut completion = tx.track_state_mutation();
				let deadline = deadline_after_positive_wait(
					request.installation_wait_timeout,
					ScopeAdditionError::InvalidInstallationWaitTimeout,
				)?;
				let interruption = tx.statement_interruption_marker();
				let outcome = tokio::time::timeout_at(
					tokio_instant(deadline),
					self.install_additive_scope_in_transaction(tx, &request, deadline),
				)
				.await
				.map_err(|_| {
					interruption.mark_interrupted();
					ScopeAdditionError::InstallationWaitElapsed
				})??;
				require_before_deadline(deadline, ScopeAdditionError::InstallationWaitElapsed)?;
				completion.complete();
				Ok(outcome)
			},
		)
		.await
	}

	async fn install_additive_scope_in_transaction(
		&self,
		tx: &mut WriteTx<'_>,
		request: &ScopeAdditionRequest<'_>,
		deadline: Deadline,
	) -> Result<ScopeAdditionResult, NativeUpgradeParticipantsError> {
		require_before_deadline(deadline, ScopeAdditionError::InstallationWaitElapsed)?;
		let pin_deadline = deadline_after_positive_wait(
			self.config.control_pin_wait_timeout,
			NativeUpgradeParticipantsError::InvalidControlPinWaitTimeout,
		)?;
		let interruption = tx.statement_interruption_marker();
		let (source_phase, mut physical) =
			tokio::time::timeout_at(tokio_instant(pin_deadline), async {
				let phase = self.config.installation_binding.pin_source(tx).await?;
				let physical = self
					.config
					.control_storage
					.lock_exclusive_for_participant_work(tx, self.config.conflict_attempts)
					.await?;
				Ok::<_, NativeUpgradeParticipantsError>((phase, physical))
			})
			.await
			.map_err(|_| {
				interruption.mark_interrupted();
				NativeUpgradeParticipantsError::ControlPinWaitElapsed
			})??;
		let (decoded, route, parsed_backend) =
			self.decode_control_with_registration(physical.snapshot(), source_phase)?;
		let origin = match route {
			InstallationStorageRoute::NativeFresh => UpgradeSourceOrigin::Fresh,
			InstallationStorageRoute::SourceCoexistence
			| InstallationStorageRoute::SourceRelocation
			| InstallationStorageRoute::NativeImported => UpgradeSourceOrigin::Imported,
		};
		let current = decoded.into_state();
		let candidate = request.resolved.candidate();
		candidate.require_control_target(
			&current,
			&self.config.administration.declaration(),
			self.config.administration.participant_schema(),
			self.config.administration.control_component(),
		)?;
		self.config
			.installation_binding
			.check_scope_policies(current.scope_policies())?;
		if let Some(scopes) = candidate.scope_replacements() {
			self.config
				.installation_binding
				.check_scope_policies(scopes)?;
		}
		let mut work = UpgradeWork::decode_json(
			current
				.pending_upgrade()
				.ok_or(NativeUpgradeParticipantsError::WorkAbsent)?,
		)?;
		if !work.target().matches_target(request.invocation.target())
			|| !work.target().matches_target(request.resolved.target())
		{
			return Err(UpgradeWorkError::WorkNotCurrent.into());
		}
		let expected = work.checkpoint_context();
		work.require_checkpoint_context(&current, candidate, origin, &expected)?;
		if matches!(
			work.status(),
			PgUpgradeStatus::Completed | PgUpgradeStatus::Cancelled
		) {
			return Err(UpgradeWorkError::TerminalWork.into());
		}
		require_invariants(&work, &current, current.scope_policies())?;
		self.require_control_writer(&current)?;
		let target = work
			.target()
			.request()
			.scope_replacements()
			.and_then(|scopes| scopes.lookup(request.scope).ok())
			.ok_or_else(|| ScopeAdditionError::ScopeNotTargeted {
				scope: request.scope.clone(),
			})?;
		let code = request
			.resolved
			.code()
			.scopes()
			.lookup(request.scope)
			.map_err(|error| ScopeAdditionError::Code {
				scope: request.scope.clone(),
				source: Box::new(error),
			})?;
		let initial = match (code, target.owner()) {
			(ManagedScopeRegistration::Document(_), ScopeOwner::Document(document)) => {
				let document = DocumentScopePolicy::new(
					document.schema().clone(),
					document.maintenance_indexes().cloned().collect(),
				)
				.map_err(|source| ScopeAdditionError::Scope {
					scope: request.scope.clone(),
					source,
				})?;
				ScopePolicy::new(request.scope.clone(), ScopeOwner::Document(document))
			}
			// A new primitive scope holds no records, so it starts at the target policy, which
			// is the policy that opening installs when it creates an installation.
			(ManagedScopeRegistration::Raw(_), ScopeOwner::Raw(_))
			| (ManagedScopeRegistration::Primitive(_), ScopeOwner::Primitive(_)) => target.clone(),
			_ => {
				return Err(ScopeAdditionError::UnsupportedOwner {
					scope: request.scope.clone(),
				}
				.into());
			}
		};
		physical
			.authenticate_upgrade(&current, &request.invocation)
			.await?;
		require_before_deadline(deadline, ScopeAdditionError::InstallationWaitElapsed)?;
		if let Ok(installed) = current.scope_policies().lookup(request.scope) {
			if !installed.matches_policy(&initial) && !installed.matches_policy(target) {
				return Err(ScopeAdditionError::ExistingScopeRequiresProcedure {
					scope: request.scope.clone(),
				}
				.into());
			}
			physical.keep().await?;
			require_before_deadline(deadline, ScopeAdditionError::InstallationWaitElapsed)?;
			return Ok(ScopeAdditionResult::AlreadyInstalled {
				work,
				observed: NativeUpgradeObservedControl::new(NativeUpgradeObservedControlFields {
					control: current,
					route,
					backend: parsed_backend,
				}),
			});
		}
		let additions =
			ScopePolicySet::new(vec![initial]).map_err(|source| ScopeAdditionError::Scope {
				scope: request.scope.clone(),
				source,
			})?;
		let scopes = current
			.scope_policies()
			.with_replacements(&additions)
			.map_err(|source| ScopeAdditionError::Scope {
				scope: request.scope.clone(),
				source,
			})?;
		self.config
			.administration
			.native_bindings()
			.require_scope_policy_associations(&parsed_backend, &scopes)
			.map_err(|error| ScopeAdditionError::Backend(Box::new(error)))?;
		require_invariants(&work, &current, &scopes)?;
		let storage = self
			.config
			.installation_binding
			.app_storage(request.scope)?;
		let maximum = physical
			.observe_scope_namespace(&storage, request.scope)
			.await
			.map_err(|source| ScopeAdditionError::Namespace {
				scope: request.scope.clone(),
				source,
			})?;
		require_before_deadline(deadline, ScopeAdditionError::InstallationWaitElapsed)?;
		if let RecordMaximumValuedKey::Key(key) = maximum.maximum() {
			return Err(ScopeAdditionError::PreexistingData {
				scope: request.scope.clone(),
				identity: RecordIdentity::new(request.scope.namespace().clone(), key.clone()),
			}
			.into());
		}
		let declaration = self.config.administration.declaration();
		let assessment = ParticipantPolicyAssessment::new(ParticipantPolicyAssessmentFields {
			administration: &declaration,
			installation_id: current.installation_id(),
			components: current.component_policies(),
			scopes: &scopes,
			participant: self.config.administration.participant_schema(),
			control: self.config.administration.control_component(),
		})?;
		let inspection = physical
			.inspect_participants(ParticipantInspectionFields {
				inspector: self,
				current: &current,
				proposed: &assessment,
				budget: request.participant_budget,
			})
			.await?;
		require_before_deadline(deadline, ScopeAdditionError::InstallationWaitElapsed)?;
		if inspection.coverage != ParticipantInspectionCoverage::Complete
			|| !inspection.deferred.is_empty()
		{
			return Err(ScopeAdditionError::ParticipantInspection {
				scope: request.scope.clone(),
				summary: Box::new(inspection),
			}
			.into());
		}
		let generations = work.advance_effective_build(&current, candidate, origin, &expected)?;
		let mut replacement = EffectiveControlReplacement::new(current);
		let fields = replacement.fields_mut();
		fields.scope_policies = scopes;
		fields.generation = generations.next;
		fields.pending_upgrade = Some(work.encode_json()?);
		let updated = replacement
			.finish()
			.map_err(UpgradeCompatibilityError::from)?;
		require_before_deadline(deadline, ScopeAdditionError::InstallationWaitElapsed)?;
		let now = physical.observe_time().await?;
		let encoded = self.encode_current_control(&updated, now)?;
		require_before_deadline(deadline, ScopeAdditionError::InstallationWaitElapsed)?;
		physical.write(encoded).await?;
		require_before_deadline(deadline, ScopeAdditionError::InstallationWaitElapsed)?;
		Ok(ScopeAdditionResult::Installed {
			work,
			observed: NativeUpgradeObservedControl::new(NativeUpgradeObservedControlFields {
				control: updated,
				route,
				backend: parsed_backend,
			}),
		})
	}
}

pub(crate) struct DeclaredScopeAdditionRequest<'a> {
	/// The starting runtime's scope declarations.
	pub(crate) registry: &'a ManagedScopeRegistry,
	pub(crate) participant_budget: ParticipantInspectionBudget,
	/// Bounds protected work after BEGIN; transaction ending retains its own lifetime.
	pub(crate) installation_wait_timeout: Duration,
}

/// Every outcome requires a known successful COMMIT.
#[derive(Debug)]
pub(crate) enum DeclaredScopeAdditionResult {
	/// Every declared scope with a JSON writer was installed, so nothing changed.
	NoAdditions,
	/// The installation lacks a declared scope and has an unfinished upgrade, so nothing
	/// changed. The upgrade's own steps own every change of the scope policies until it ends.
	UpgradePending,
	/// Every absent declared scope with a JSON writer is installed.
	Installed,
}

impl PgNativeUpgradeParticipants {
	/// Installs each absent scope with a JSON writer that the starting runtime declares, in one
	/// control transaction, with the policy that the upgrade step that adds a scope installs:
	/// the target policy, and for a document scope the target's schema with every declared
	/// index maintained and none ready. Each such scope must hold no record, and every running
	/// participant must stay compatible with the installation's scopes and the added ones,
	/// which the participant inspection checks under the exclusive control lock, as it does for an
	/// upgrade step. A running participant uses only the scopes that it registered with, which
	/// exclude every absent scope, so it continues unchanged. The control generation advances,
	/// and a finished upgrade that the control state records moves to the new generation, which
	/// means that the next upgrade and the inspection of the finished one accept it.
	pub(crate) async fn install_absent_declared_scopes(
		&self,
		request: DeclaredScopeAdditionRequest<'_>,
	) -> Result<DeclaredScopeAdditionResult, NativeUpgradeParticipantsError> {
		require_valid_wait(
			request.installation_wait_timeout,
			ScopeAdditionError::InvalidInstallationWaitTimeout,
		)?;
		// The exclusive control lock blocks every ordinary operation while it is held, so a
		// shared read first decides whether the addition can change anything.
		if let Some(result) = self.addition_result_without_change(&request).await? {
			return Ok(result);
		}
		let mut tx = self
			.config
			.control_pool
			.begin_labeled_transaction("db.native_upgrade_participants.install_declared_scopes")
			.await
			.map_err(NativeUpgradeParticipantsError::Begin)?;
		let outcome = async {
			let mut completion = tx.track_state_mutation();
			let deadline = deadline_after_positive_wait(
				request.installation_wait_timeout,
				ScopeAdditionError::InvalidInstallationWaitTimeout,
			)?;
			let interruption = tx.statement_interruption_marker();
			let outcome = tokio::time::timeout_at(
				tokio_instant(deadline),
				self.install_absent_declared_scopes_in_transaction(&mut tx, &request, deadline),
			)
			.await
			.map_err(|_| {
				interruption.mark_interrupted();
				ScopeAdditionError::InstallationWaitElapsed
			})??;
			require_before_deadline(deadline, ScopeAdditionError::InstallationWaitElapsed)?;
			completion.complete();
			Ok(outcome)
		}
		.await;
		crate::db::finish_pool_owned_write_transaction_and_preserve_rollback_error(
			"db.native_upgrade_participants.install_declared_scopes",
			tx,
			outcome,
			NativeUpgradeParticipantsError::Commit,
			|_, operation, rollback| NativeUpgradeParticipantsError::RollbackFailed {
				operation_error: Box::new(operation),
				rollback_error: rollback,
			},
		)
		.await
	}

	/// The result of the addition when the shared control state shows that it changes nothing:
	/// every declared scope with a JSON writer is installed, or an unfinished upgrade owns the scope policies.
	async fn addition_result_without_change(
		&self,
		request: &DeclaredScopeAdditionRequest<'_>,
	) -> Result<Option<DeclaredScopeAdditionResult>, NativeUpgradeParticipantsError> {
		let mut tx = self
			.config
			.control_pool
			.begin_labeled_transaction("db.native_upgrade_participants.find_absent_declared_scopes")
			.await
			.map_err(NativeUpgradeParticipantsError::Begin)?;
		let outcome = async {
			let source_phase = self.config.installation_binding.pin_source(&mut tx).await?;
			let snapshot = self
				.config
				.control_storage
				.read_pinned_shared(&mut tx)
				.await?;
			let (decoded, _route) = self.decode_control(&snapshot, source_phase)?;
			let current = decoded.into_state();
			let targets = request
				.registry
				.absent_scope_targets(current.scope_policies())
				.map_err(|error| ScopeAdditionError::DeclaredScopeSelection(Box::new(error)))?;
			if targets.is_empty() {
				return Ok(Some(DeclaredScopeAdditionResult::NoAdditions));
			}
			Ok(match recorded_upgrade(&current)? {
				RecordedUpgrade::Unfinished => Some(DeclaredScopeAdditionResult::UpgradePending),
				RecordedUpgrade::None | RecordedUpgrade::Finished(_) => None,
			})
		}
		.await;
		crate::db::finish_pool_owned_write_transaction_and_preserve_rollback_error(
			"db.native_upgrade_participants.find_absent_declared_scopes",
			tx,
			outcome,
			NativeUpgradeParticipantsError::Commit,
			|_, operation, rollback| NativeUpgradeParticipantsError::RollbackFailed {
				operation_error: Box::new(operation),
				rollback_error: rollback,
			},
		)
		.await
	}

	async fn install_absent_declared_scopes_in_transaction(
		&self,
		tx: &mut WriteTx<'_>,
		request: &DeclaredScopeAdditionRequest<'_>,
		deadline: Deadline,
	) -> Result<DeclaredScopeAdditionResult, NativeUpgradeParticipantsError> {
		let pin_deadline = deadline_after_positive_wait(
			self.config.control_pin_wait_timeout,
			NativeUpgradeParticipantsError::InvalidControlPinWaitTimeout,
		)?;
		let interruption = tx.statement_interruption_marker();
		let (source_phase, mut physical) =
			tokio::time::timeout_at(tokio_instant(pin_deadline), async {
				let phase = self.config.installation_binding.pin_source(tx).await?;
				let physical = self
					.config
					.control_storage
					.lock_exclusive_for_participant_work(tx, self.config.conflict_attempts)
					.await?;
				Ok::<_, NativeUpgradeParticipantsError>((phase, physical))
			})
			.await
			.map_err(|_| {
				interruption.mark_interrupted();
				NativeUpgradeParticipantsError::ControlPinWaitElapsed
			})??;
		let (decoded, _route, parsed_backend) =
			self.decode_control_with_registration(physical.snapshot(), source_phase)?;
		let current = decoded.into_state();
		self.config
			.installation_binding
			.check_scope_policies(current.scope_policies())?;
		let targets = request
			.registry
			.absent_scope_targets(current.scope_policies())
			.map_err(|error| ScopeAdditionError::DeclaredScopeSelection(Box::new(error)))?;
		if targets.is_empty() {
			physical.keep().await?;
			return Ok(DeclaredScopeAdditionResult::NoAdditions);
		}
		let finished_upgrade = match recorded_upgrade(&current)? {
			RecordedUpgrade::Unfinished => {
				physical.keep().await?;
				return Ok(DeclaredScopeAdditionResult::UpgradePending);
			}
			RecordedUpgrade::None => None,
			RecordedUpgrade::Finished(work) => Some(work),
		};
		self.require_control_writer(&current)?;
		let mut additions = Vec::with_capacity(targets.len());
		for target in targets {
			let initial = match target.owner() {
				ScopeOwner::Document(document) => {
					let document = DocumentScopePolicy::new(
						document.schema().clone(),
						document.maintenance_indexes().cloned().collect(),
					)
					.map_err(|source| ScopeAdditionError::Scope {
						scope: target.identity().clone(),
						source,
					})?;
					ScopePolicy::new(target.identity().clone(), ScopeOwner::Document(document))
				}
				ScopeOwner::Raw(_) | ScopeOwner::Primitive(_) => target,
				_ => {
					return Err(ScopeAdditionError::UnsupportedOwner {
						scope: target.identity().clone(),
					}
					.into());
				}
			};
			additions.push(initial);
		}
		let added = additions
			.iter()
			.map(|policy| policy.identity().clone())
			.collect::<Vec<_>>();
		let first = added[0].clone();
		let additions =
			ScopePolicySet::new(additions).map_err(|source| ScopeAdditionError::Scope {
				scope: first.clone(),
				source,
			})?;
		let scopes = current
			.scope_policies()
			.with_replacements(&additions)
			.map_err(|source| ScopeAdditionError::Scope {
				scope: first.clone(),
				source,
			})?;
		self.config
			.administration
			.native_bindings()
			.require_scope_policy_associations(&parsed_backend, &scopes)
			.map_err(|error| ScopeAdditionError::Backend(Box::new(error)))?;
		self.config
			.installation_binding
			.check_scope_policies(&scopes)?;
		for scope in &added {
			let storage = self.config.installation_binding.app_storage(scope)?;
			let maximum = physical
				.observe_scope_namespace(&storage, scope)
				.await
				.map_err(|source| ScopeAdditionError::Namespace {
					scope: scope.clone(),
					source,
				})?;
			require_before_deadline(deadline, ScopeAdditionError::InstallationWaitElapsed)?;
			if let RecordMaximumValuedKey::Key(key) = maximum.maximum() {
				return Err(ScopeAdditionError::PreexistingData {
					scope: scope.clone(),
					identity: RecordIdentity::new(scope.namespace().clone(), key.clone()),
				}
				.into());
			}
		}
		let declaration = self.config.administration.declaration();
		let assessment = ParticipantPolicyAssessment::new(ParticipantPolicyAssessmentFields {
			administration: &declaration,
			installation_id: current.installation_id(),
			components: current.component_policies(),
			scopes: &scopes,
			participant: self.config.administration.participant_schema(),
			control: self.config.administration.control_component(),
		})?;
		let inspection = physical
			.inspect_participants(ParticipantInspectionFields {
				inspector: self,
				current: &current,
				proposed: &assessment,
				budget: request.participant_budget,
			})
			.await?;
		require_before_deadline(deadline, ScopeAdditionError::InstallationWaitElapsed)?;
		if inspection.coverage != ParticipantInspectionCoverage::Complete
			|| !inspection.deferred.is_empty()
		{
			return Err(ScopeAdditionError::ParticipantInspection {
				scope: first,
				summary: Box::new(inspection),
			}
			.into());
		}
		let generation = current
			.generation()
			.checked_add(1)
			.ok_or(ScopeAdditionError::GenerationOverflow)?;
		let mut replacement = EffectiveControlReplacement::new(current);
		let fields = replacement.fields_mut();
		fields.scope_policies = scopes;
		fields.generation = generation;
		if let Some(mut work) = finished_upgrade {
			work.follow_generation_after_finish(generation)?;
			fields.pending_upgrade = Some(work.encode_json()?);
		}
		let updated = replacement
			.finish()
			.map_err(UpgradeCompatibilityError::from)?;
		let now = physical.observe_time().await?;
		let encoded = self.encode_current_control(&updated, now)?;
		require_before_deadline(deadline, ScopeAdditionError::InstallationWaitElapsed)?;
		physical.write(encoded).await?;
		require_before_deadline(deadline, ScopeAdditionError::InstallationWaitElapsed)?;
		Ok(DeclaredScopeAdditionResult::Installed)
	}
}

/// The upgrade that a control state records.
enum RecordedUpgrade {
	None,
	/// Work that is queued or building.
	Unfinished,
	/// Work that completed or was cancelled.
	Finished(Box<UpgradeWork>),
}

fn recorded_upgrade(
	control: &InstallationControlState,
) -> Result<RecordedUpgrade, UpgradeWorkError> {
	let Some(metadata) = control.pending_upgrade() else {
		return Ok(RecordedUpgrade::None);
	};
	let work = UpgradeWork::decode_json(metadata)?;
	if work.is_unfinished() {
		Ok(RecordedUpgrade::Unfinished)
	} else {
		Ok(RecordedUpgrade::Finished(Box::new(work)))
	}
}

fn require_invariants(
	work: &UpgradeWork,
	control: &InstallationControlState,
	scopes: &ScopePolicySet,
) -> Result<(), UpgradeWorkError> {
	work.require_source_checkpoint_invariants(control.component_policies(), scopes)?;
	work.require_document_checkpoint_invariants(scopes)?;
	work.require_document_verification_invariants(scopes)?;
	work.require_payload_checkpoint_invariants(scopes)
}
