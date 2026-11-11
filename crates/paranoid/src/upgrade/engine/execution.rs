//! One closed fixed-operation dispatch using actual registered ending and observation owners.
//!
//! This module adds no transaction, post-step read, retry, policy reconstruction or outer timeout.

use std::sync::Arc;

use crate::db::transaction_ending::TransactionEndingReceipt;
use crate::runtime::admission::upgrade::{NativeUpgradeInvocation, NativeUpgradeInvocationFields};
use crate::runtime::local_admission::upgrade_work::RuntimeUpgradeWorkPermit;
use crate::runtime::targets::ResolvedNativeRuntimeTarget;
use crate::upgrade::observation::NativeUpgradeObservedControl;
use crate::upgrade::participants::document_index::{
	DocumentIndexRegistrationRequest, DocumentIndexRegistrationRequestFields,
	DocumentIndexRegistrationResult,
};
use crate::upgrade::participants::document_readiness::{
	DocumentReadyRequest, DocumentReadyRequestFields, DocumentReadyResult,
};
use crate::upgrade::participants::queue_job::{
	QueueJobPolicyChangeKind, QueueJobPolicyChangeRequest,
};
use crate::upgrade::participants::scope_addition::{ScopeAdditionRequest, ScopeAdditionResult};
use crate::upgrade::participants::scope_reader::ScopeFinalizationRequest;
use crate::upgrade::participants::scope_writer::{ScopeWriterKind, ScopeWriterPromotionRequest};
use crate::upgrade::participants::target_completion::{
	TargetCompletionRequest, TargetCompletionResult,
};
use crate::upgrade::participants::v10::source_data::{
	SourceDataRetirementRequest, SourceDataRetirementResult,
};
use crate::upgrade::participants::v10::source_writer::{
	SourceWriterRetirementRequest, SourceWriterRetirementResult,
};
use crate::upgrade::requests::batch::{NativeUpgradeBatchOperation, NativeUpgradeBatchRequest};
use crate::upgrade::requests::work::observation::{
	NativeUpgradeControllerObservation, NativeUpgradeControllerObservationFields,
};
use crate::upgrade::work::UpgradeWork;

use super::NativeUpgradeExecutionServices;
use super::selection::PgUpgradeStep;

pub(crate) mod error;
pub(crate) mod failure;
pub(in crate::upgrade) mod index_placements;
pub(crate) mod limits;

use error::{NativeUpgradeStepExecutionCauseError, NativeUpgradeStepExecutionError};
use index_placements::{NativeUpgradeIndexResolutionFields, resolve_index_name};
use limits::NativeUpgradeExecutionLimits;

pub(in crate::upgrade) struct NativeUpgradeStepExecutionFields<'a> {
	pub(crate) services: &'a NativeUpgradeExecutionServices,
	pub(crate) observation: &'a NativeUpgradeControllerObservation,
	pub(crate) resolved: &'a ResolvedNativeRuntimeTarget,
	pub(crate) owner: &'a RuntimeUpgradeWorkPermit,
	pub(crate) ending: &'a mut Option<TransactionEndingReceipt>,
	pub(crate) step: &'a PgUpgradeStep,
	pub(crate) limits: &'a NativeUpgradeExecutionLimits,
}

/// The caller retains the owner and actual receipt through ending settlement and any recovery.
pub(in crate::upgrade) async fn execute_upgrade_step(
	fields: NativeUpgradeStepExecutionFields<'_>,
) -> Result<NativeUpgradeControllerObservation, NativeUpgradeStepExecutionError> {
	let step = fields.step.clone();
	execute(fields)
		.await
		.map_err(|error| NativeUpgradeStepExecutionError::new(step, error))
}

async fn execute(
	fields: NativeUpgradeStepExecutionFields<'_>,
) -> Result<NativeUpgradeControllerObservation, NativeUpgradeStepExecutionCauseError> {
	let NativeUpgradeStepExecutionFields {
		services,
		observation,
		resolved,
		owner,
		ending,
		step,
		limits,
	} = fields;
	let work = observation
		.work()
		.ok_or(NativeUpgradeStepExecutionCauseError::WorkAbsent)?;
	if !work.target().matches_target(resolved.target())
		|| observation.control().installation_id()
			!= resolved
				.code()
				.administration()
				.control_component()
				.installation_id()
	{
		return Err(NativeUpgradeStepExecutionCauseError::ContextMismatch);
	}
	let index_name = match step {
		PgUpgradeStep::RegisterDocumentIndex { scope, index } => Some(
			resolve_index_name(NativeUpgradeIndexResolutionFields {
				observation,
				resolved,
				scope,
				index,
			})
			.map_err(|error| {
				NativeUpgradeStepExecutionCauseError::IndexPlacement(Box::new(error))
			})?,
		),
		_ => None,
	};
	let invocation = NativeUpgradeInvocation::new(NativeUpgradeInvocationFields {
		admission: &services.admission,
		owner,
		target: resolved.target(),
		ending,
	})
	.map_err(|error| NativeUpgradeStepExecutionCauseError::Admission(Box::new(error)))?;
	let participant_budget = limits.participant_budget();
	let protected_work_wait_timeout = limits.protected_work_wait_timeout();
	match step {
		PgUpgradeStep::PromoteQueueJobWriter | PgUpgradeStep::FinalizeQueueJobReaders => {
			let result = services
				.participants
				.change_queue_job_policy(QueueJobPolicyChangeRequest {
					invocation,
					resolved,
					participant_budget,
					kind: if matches!(step, PgUpgradeStep::PromoteQueueJobWriter) {
						QueueJobPolicyChangeKind::PromoteWriter
					} else {
						QueueJobPolicyChangeKind::FinalizeReaders
					},
				})
				.await
				.map_err(|error| {
					NativeUpgradeStepExecutionCauseError::Participants(Box::new(error))
				})?;
			Ok(next_observation(result.observed, result.work))
		}
		PgUpgradeStep::RetireV10Writers => {
			let result = services
				.participants
				.retire_source_writers(SourceWriterRetirementRequest {
					invocation,
					final_candidate: resolved.candidate(),
					source_lock_wait_timeout: limits.source_lock_wait_timeout(),
					participant_budget,
				})
				.await
				.map_err(|error| {
					NativeUpgradeStepExecutionCauseError::SourceWriter(Box::new(error))
				})?;
			let (observed, work) = match result {
				SourceWriterRetirementResult::Retired { observed, work, .. }
				| SourceWriterRetirementResult::AlreadyRetired { observed, work, .. } => (observed, work),
			};
			Ok(next_observation(observed, work))
		}
		PgUpgradeStep::MoveV10Records(_)
		| PgUpgradeStep::ConvertQueueJobs
		| PgUpgradeStep::ConvertDocument { .. }
		| PgUpgradeStep::VerifyDocument { .. }
		| PgUpgradeStep::ConvertPayload { .. } => {
			let operation = match step {
				PgUpgradeStep::ConvertQueueJobs => NativeUpgradeBatchOperation::ConvertQueueJobs,
				PgUpgradeStep::MoveV10Records(procedure) => {
					NativeUpgradeBatchOperation::Source(*procedure)
				}
				PgUpgradeStep::ConvertDocument { scope } => {
					NativeUpgradeBatchOperation::ConvertDocument { scope }
				}
				PgUpgradeStep::VerifyDocument { scope, index } => {
					NativeUpgradeBatchOperation::VerifyDocument { scope, index }
				}
				PgUpgradeStep::ConvertPayload { scope } => {
					NativeUpgradeBatchOperation::ConvertPayload { scope }
				}
				_ => unreachable!("the batch branch selects only batch operations"),
			};
			let result = services
				.requests
				.execute_batch(NativeUpgradeBatchRequest {
					invocation,
					resolved,
					operation,
					batch_wait_timeout: protected_work_wait_timeout,
					limit: limits.limit(),
				})
				.await
				.map_err(|error| NativeUpgradeStepExecutionCauseError::Batch(Box::new(error)))?;
			Ok(next_observation(result.observed, result.work))
		}
		PgUpgradeStep::RetireV10Data => {
			let result = services
				.participants
				.retire_source_data(SourceDataRetirementRequest {
					invocation,
					resolved,
					source_lock_wait_timeout: limits.source_lock_wait_timeout(),
					retirement_wait_timeout: protected_work_wait_timeout,
					participant_budget,
				})
				.await
				.map_err(|error| {
					NativeUpgradeStepExecutionCauseError::SourceData(Box::new(error))
				})?;
			let (observed, work) = match result {
				SourceDataRetirementResult::Retired { observed, work, .. }
				| SourceDataRetirementResult::AlreadyRetired { observed, work } => (observed, work),
			};
			Ok(next_observation(observed, work))
		}
		PgUpgradeStep::InstallAdditiveScope { scope } => {
			let result = services
				.participants
				.install_additive_scope(ScopeAdditionRequest {
					invocation,
					resolved,
					scope,
					participant_budget,
					installation_wait_timeout: protected_work_wait_timeout,
				})
				.await
				.map_err(|error| {
					NativeUpgradeStepExecutionCauseError::Participants(Box::new(error))
				})?;
			let (observed, work) = match result {
				ScopeAdditionResult::Installed { observed, work, .. }
				| ScopeAdditionResult::AlreadyInstalled { observed, work, .. } => (observed, work),
			};
			Ok(next_observation(observed, work))
		}
		PgUpgradeStep::PromoteDocumentWriter { scope } => {
			let result = services
				.participants
				.promote_scope_writer(ScopeWriterPromotionRequest {
					invocation,
					resolved,
					kind: ScopeWriterKind::Document,
					scope,
					participant_budget,
				})
				.await
				.map_err(|error| {
					NativeUpgradeStepExecutionCauseError::Participants(Box::new(error))
				})?;
			Ok(next_observation(result.observed, result.work))
		}
		PgUpgradeStep::RegisterDocumentIndex { scope, index } => {
			let request =
				DocumentIndexRegistrationRequest::new(DocumentIndexRegistrationRequestFields {
					invocation,
					final_candidate: resolved.candidate(),
					registry: resolved.code().document_registry(),
					scope,
					index,
					index_name: index_name.expect("registration name resolved before enrollment"),
					participant_budget,
					registration_wait_timeout: protected_work_wait_timeout,
				})
				.map_err(|error| {
					NativeUpgradeStepExecutionCauseError::IndexRequest(Box::new(error))
				})?;
			let result = services
				.participants
				.register_document_index(request)
				.await
				.map_err(|error| {
					NativeUpgradeStepExecutionCauseError::Participants(Box::new(error))
				})?;
			let (observed, work) = match result {
				DocumentIndexRegistrationResult::Registered { observed, work, .. }
				| DocumentIndexRegistrationResult::AlreadyRegistered { observed, work, .. } => (observed, work),
			};
			Ok(next_observation(observed, work))
		}
		PgUpgradeStep::PublishDocumentReady { scope, index } => {
			let request = DocumentReadyRequest::new(DocumentReadyRequestFields {
				invocation,
				resolved,
				scope,
				index,
				participant_budget,
				publication_wait_timeout: protected_work_wait_timeout,
			})
			.map_err(|error| NativeUpgradeStepExecutionCauseError::ReadyRequest(Box::new(error)))?;
			let result = services
				.participants
				.publish_document_ready(request)
				.await
				.map_err(|error| {
					NativeUpgradeStepExecutionCauseError::Participants(Box::new(error))
				})?;
			let (observed, work) = match result {
				DocumentReadyResult::Published { observed, work, .. }
				| DocumentReadyResult::AlreadyReady { observed, work, .. } => (observed, work),
			};
			Ok(next_observation(observed, work))
		}
		PgUpgradeStep::PromotePayloadWriter { scope } => {
			let result = services
				.participants
				.promote_scope_writer(ScopeWriterPromotionRequest {
					invocation,
					resolved,
					kind: ScopeWriterKind::Payload,
					scope,
					participant_budget,
				})
				.await
				.map_err(|error| {
					NativeUpgradeStepExecutionCauseError::Participants(Box::new(error))
				})?;
			Ok(next_observation(result.observed, result.work))
		}
		PgUpgradeStep::FinalizeScope { scope } => {
			let result = services
				.participants
				.finalize_scope(ScopeFinalizationRequest {
					invocation,
					resolved,
					scope,
					participant_budget,
					retirement_wait_timeout: protected_work_wait_timeout,
				})
				.await
				.map_err(|error| {
					NativeUpgradeStepExecutionCauseError::Participants(Box::new(error))
				})?;
			Ok(next_observation(result.observed, result.work))
		}
		PgUpgradeStep::CompleteTarget => {
			let result = services
				.participants
				.complete_target(TargetCompletionRequest {
					invocation,
					resolved,
					participant_budget,
					completion_wait_timeout: protected_work_wait_timeout,
				})
				.await
				.map_err(|error| {
					NativeUpgradeStepExecutionCauseError::Participants(Box::new(error))
				})?;
			let (observed, work) = match result {
				TargetCompletionResult::Completed { observed, work, .. }
				| TargetCompletionResult::AlreadyCompleted { observed, work } => (observed, work),
			};
			Ok(next_observation(observed, work))
		}
	}
}

fn next_observation(
	observed: NativeUpgradeObservedControl,
	work: UpgradeWork,
) -> NativeUpgradeControllerObservation {
	NativeUpgradeControllerObservation::new(NativeUpgradeControllerObservationFields {
		observed,
		work: Some(Arc::new(work)),
	})
}
