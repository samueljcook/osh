//! Domain policy preparation for the common writer-promotion transaction.

use crate::installation::control_state::InstallationControlState;
use crate::runtime::targets::NativeRuntimeTargetCode;
use crate::scope::{ScopeError, ScopeIdentity, ScopePolicy, ScopePolicySet};
use crate::upgrade::participants::{NativeUpgradeParticipantsError, ParticipantInspectionSummary};
use crate::upgrade::policy::document_stage::{DocumentWriterStage, ResolvedDocumentTargets};
use crate::upgrade::policy::payload_stage::{
	PrimitivePayloadWriterStage, PrimitivePayloadWriterStageFields,
};
use crate::upgrade::work::{UpgradeWork, UpgradeWorkError};

#[derive(Clone, Copy)]
pub(in crate::upgrade) enum ScopeWriterKind {
	Document,
	Payload,
}

impl ScopeWriterKind {
	pub(super) fn transaction_label(self) -> &'static str {
		match self {
			Self::Document => "db.native_upgrade_participants.promote_document_writer",
			Self::Payload => "db.native_upgrade_participants.promote_payload_writer",
		}
	}

	/// None means that the current policy already satisfies the writer stage.
	pub(super) fn prepare_policy(
		self,
		code: &NativeRuntimeTargetCode,
		current: &InstallationControlState,
		work: &UpgradeWork,
		scope: &ScopeIdentity,
	) -> Result<Option<ScopePolicy>, NativeUpgradeParticipantsError> {
		match self {
			Self::Document => {
				let targets =
					ResolvedDocumentTargets::resolve(code.document_registry(), work.target())
						.map_err(|error| {
							NativeUpgradeParticipantsError::DocumentStage(Box::new(error))
						})?;
				let target = targets
					.targets()
					.and_then(|targets| {
						targets
							.iter()
							.find(|target| target.policy().identity() == scope)
					})
					.ok_or_else(
						|| NativeUpgradeParticipantsError::DocumentScopeNotTargeted {
							scope: scope.clone(),
						},
					)?;
				let current_scope = current.scope_policies().lookup(scope).map_err(|source| {
					NativeUpgradeParticipantsError::DocumentScope {
						scope: scope.clone(),
						source,
					}
				})?;
				let stage = DocumentWriterStage::new(target, current_scope).map_err(|error| {
					NativeUpgradeParticipantsError::DocumentStage(Box::new(error))
				})?;
				let already_effective = stage.matches_policy(current_scope);
				self.require_invariants(work, current.scope_policies())?;
				if let Some(existing) = work.document_conversion_checkpoint(scope)
					&& (!stage.matches_policy(existing.invariant()) || !already_effective)
				{
					return Err(NativeUpgradeParticipantsError::DocumentCheckpointConflict {
						scope: scope.clone(),
					});
				}
				Ok((!already_effective).then(|| stage.policy().clone()))
			}
			Self::Payload => {
				let target = work
					.target()
					.request()
					.scope_replacements()
					.and_then(|scopes| scopes.lookup(scope).ok())
					.ok_or_else(|| NativeUpgradeParticipantsError::PayloadScopeNotTargeted {
						scope: scope.clone(),
					})?
					.clone();
				let current_scope = current.scope_policies().lookup(scope).map_err(|source| {
					NativeUpgradeParticipantsError::PayloadScope {
						scope: scope.clone(),
						source,
					}
				})?;
				let stage = PrimitivePayloadWriterStage::new(PrimitivePayloadWriterStageFields {
					registry: code.scopes(),
					current: current_scope,
					target: &target,
				})
				.map_err(|error| NativeUpgradeParticipantsError::PayloadStage(Box::new(error)))?;
				let already_effective = stage.matches_policy(current_scope);
				self.require_invariants(work, current.scope_policies())?;
				if let Some(existing) = work.payload_checkpoint(scope)
					&& (!stage.matches_policy(existing.invariant()) || !already_effective)
				{
					return Err(NativeUpgradeParticipantsError::PayloadCheckpointConflict {
						scope: scope.clone(),
					});
				}
				Ok((!already_effective).then(|| stage.policy().clone()))
			}
		}
	}

	pub(super) fn require_invariants(
		self,
		work: &UpgradeWork,
		scopes: &ScopePolicySet,
	) -> Result<(), UpgradeWorkError> {
		if matches!(self, Self::Payload) {
			work.require_document_checkpoint_invariants(scopes)?;
		}
		work.require_document_verification_invariants(scopes)?;
		if matches!(self, Self::Payload) {
			work.require_payload_checkpoint_invariants(scopes)?;
		}
		Ok(())
	}

	pub(super) fn scope_error(
		self,
		scope: &ScopeIdentity,
		source: ScopeError,
	) -> NativeUpgradeParticipantsError {
		match self {
			Self::Document => NativeUpgradeParticipantsError::DocumentScope {
				scope: scope.clone(),
				source,
			},
			Self::Payload => NativeUpgradeParticipantsError::PayloadScope {
				scope: scope.clone(),
				source,
			},
		}
	}

	pub(super) fn inspection_error(
		self,
		scope: &ScopeIdentity,
		summary: ParticipantInspectionSummary,
	) -> NativeUpgradeParticipantsError {
		match self {
			Self::Document => NativeUpgradeParticipantsError::DocumentWriterInspection {
				scope: scope.clone(),
				summary: Box::new(summary),
			},
			Self::Payload => NativeUpgradeParticipantsError::PayloadWriterInspection {
				scope: scope.clone(),
				summary: Box::new(summary),
			},
		}
	}
}
