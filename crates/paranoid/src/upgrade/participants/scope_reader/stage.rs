//! Closed actual-code dispatch for the fixed final reader-retirement operation.

use crate::installation::control_state::InstallationControlState;
use crate::runtime::targets::NativeRuntimeTargetCode;
use crate::scope::{ScopeIdentity, ScopeOwner, ScopePolicy, ScopePolicySet};
use crate::upgrade::participants::NativeUpgradeParticipantsError;
use crate::upgrade::policy::InstallationUpgradeCandidate;
use crate::upgrade::policy::document_reader::{
	DocumentReaderCompletion, DocumentReaderCompletionFields,
};
use crate::upgrade::policy::document_stage::{DocumentWriterStage, RetainedDocumentScopeCandidate};
use crate::upgrade::policy::payload_reader::{
	PrimitivePayloadReaderCompletion, PrimitivePayloadReaderCompletionFields,
};
use crate::upgrade::policy::payload_stage::{
	PrimitivePayloadWriterStage, PrimitivePayloadWriterStageFields,
};
use crate::upgrade::work::document_checkpoint::DocumentCheckpointContext;
use crate::upgrade::work::document_completion::DocumentReaderRetirementWorkFields;
use crate::upgrade::work::payload_checkpoint::PayloadCheckpointContext;
use crate::upgrade::work::payload_completion::PayloadReaderRetirementWorkFields;
use crate::upgrade::work::{
	UpgradeSourceOrigin, UpgradeWork, UpgradeWorkCheckpointContext, UpgradeWorkError,
	UpgradeWorkGenerationChange,
};

use super::ScopeFinalizationError;

pub(super) struct ScopeReaderStageFields<'a> {
	pub(super) code: &'a NativeRuntimeTargetCode,
	pub(super) current: &'a ScopePolicy,
	pub(super) target: &'a ScopePolicy,
}
pub(super) enum ScopeReaderStage<'code> {
	Document(Box<DocumentWriterStage>),
	Payload(Box<PrimitivePayloadWriterStage<'code>>),
}
impl<'code> ScopeReaderStage<'code> {
	pub(super) fn new(
		fields: ScopeReaderStageFields<'code>,
	) -> Result<Self, NativeUpgradeParticipantsError> {
		match fields.target.owner() {
			ScopeOwner::Document(_) => {
				let target = RetainedDocumentScopeCandidate::bind(
					fields.code.document_registry(),
					fields.target.clone(),
				)
				.map_err(|error| NativeUpgradeParticipantsError::DocumentStage(Box::new(error)))?;
				let stage = DocumentWriterStage::new(&target, fields.current).map_err(|error| {
					NativeUpgradeParticipantsError::DocumentStage(Box::new(error))
				})?;
				Ok(Self::Document(Box::new(stage)))
			}
			ScopeOwner::Primitive(_) => {
				let stage = PrimitivePayloadWriterStage::new(PrimitivePayloadWriterStageFields {
					registry: fields.code.scopes(),
					current: fields.current,
					target: fields.target,
				})
				.map_err(|error| NativeUpgradeParticipantsError::PayloadStage(Box::new(error)))?;
				Ok(Self::Payload(Box::new(stage)))
			}
			_ => Err(ScopeFinalizationError::UnsupportedScope {
				scope: fields.target.identity().clone(),
			}
			.into()),
		}
	}
	pub(super) fn prepare<'stage>(
		&'stage self,
		current_scopes: &'stage ScopePolicySet,
	) -> Result<ScopeReaderCompletion<'stage, 'code>, NativeUpgradeParticipantsError> {
		match self {
			Self::Document(writer_stage) => {
				DocumentReaderCompletion::new(DocumentReaderCompletionFields {
					writer_stage,
					current_scopes,
				})
				.map(ScopeReaderCompletion::Document)
				.map_err(|error| ScopeFinalizationError::Document(Box::new(error)).into())
			}
			Self::Payload(writer_stage) => {
				PrimitivePayloadReaderCompletion::new(PrimitivePayloadReaderCompletionFields {
					writer_stage,
					current_scopes,
				})
				.map(ScopeReaderCompletion::Payload)
				.map_err(|error| ScopeFinalizationError::Payload(Box::new(error)).into())
			}
		}
	}
	pub(super) fn has_progress(&self, work: &UpgradeWork, scope: &ScopeIdentity) -> bool {
		match self {
			Self::Document(_) => {
				work.document_conversion_checkpoint(scope).is_some()
					|| work.document_verification(scope).is_some()
			}
			Self::Payload(_) => work.payload_checkpoint(scope).is_some(),
		}
	}
}

pub(super) enum ScopeReaderCompletion<'stage, 'code> {
	Document(DocumentReaderCompletion<'stage>),
	Payload(PrimitivePayloadReaderCompletion<'stage, 'code>),
}
impl ScopeReaderCompletion<'_, '_> {
	pub(super) fn scopes(&self) -> &ScopePolicySet {
		match self {
			Self::Document(prepared) => prepared.scopes(),
			Self::Payload(prepared) => prepared.scopes(),
		}
	}
	pub(super) fn into_scopes(self) -> ScopePolicySet {
		match self {
			Self::Document(prepared) => prepared.into_scopes(),
			Self::Payload(prepared) => prepared.into_scopes(),
		}
	}
	pub(super) fn complete_work(
		&self,
		work: &mut UpgradeWork,
		control: &InstallationControlState,
		candidate: &InstallationUpgradeCandidate,
		origin: UpgradeSourceOrigin,
		expected: &UpgradeWorkCheckpointContext,
	) -> Result<UpgradeWorkGenerationChange, UpgradeWorkError> {
		match self {
			Self::Document(completion) => {
				work.complete_document_reader_retirement(DocumentReaderRetirementWorkFields {
					context: DocumentCheckpointContext {
						control,
						candidate,
						origin,
						expected,
					},
					completion,
				})
			}
			Self::Payload(completion) => {
				work.complete_payload_reader_retirement(PayloadReaderRetirementWorkFields {
					context: PayloadCheckpointContext {
						control,
						candidate,
						origin,
						expected,
					},
					completion,
				})
			}
		}
	}
}
