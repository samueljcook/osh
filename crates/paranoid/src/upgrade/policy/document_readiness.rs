//! Pure exact Ready addition bound to retained actual document code.
//!
//! The stage supplies no catalog, coverage, participant or transaction proof.

use crate::kv::document::schema::IndexContractIdentity;
use crate::kv::document::schema::managed::ManagedDocumentIndexLookupError;
use crate::scope::{ScopeError, ScopeOwner, ScopePolicy};

use super::document_stage::{
	BoxedDocumentStageError, DocumentWriterStage, RetainedDocumentScopeCandidate,
};

pub(in crate::upgrade) struct DocumentReadinessStage {
	writer_stage: DocumentWriterStage,
	policy: ScopePolicy,
	index: IndexContractIdentity,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum DocumentReadinessStageError {
	#[error("document readiness writer stage failed")]
	WriterStage(#[from] BoxedDocumentStageError),
	#[error("document readiness requires the installed writer stage")]
	WriterStageNotInstalled,
	#[error("document readiness index is not requested Ready in the final target")]
	IndexNotRequested,
	#[error("document readiness index is already Ready in current policy")]
	AlreadyReady,
	#[error("document readiness index code is absent")]
	IndexCode(#[from] ManagedDocumentIndexLookupError),
	#[error("document readiness scope policy failed")]
	Scope(#[from] ScopeError),
}

impl DocumentReadinessStage {
	/// Adds one requested Ready reference and preserves the complete installed maintenance union.
	pub(crate) fn new(
		target: &RetainedDocumentScopeCandidate,
		current: &ScopePolicy,
		index: &IndexContractIdentity,
	) -> Result<Self, DocumentReadinessStageError> {
		let writer_stage = DocumentWriterStage::new(target, current)?;
		if !writer_stage.matches_policy(current) {
			return Err(DocumentReadinessStageError::WriterStageNotInstalled);
		}
		let ScopeOwner::Document(target_document) = target.policy().owner() else {
			unreachable!("actual retained document target binds only Document ownership");
		};
		if !target_document.is_index_ready(index) {
			return Err(DocumentReadinessStageError::IndexNotRequested);
		}
		let ScopeOwner::Document(current_document) = current.owner() else {
			unreachable!("actual writer stage requires current Document ownership");
		};
		if current_document.is_index_ready(index) {
			return Err(DocumentReadinessStageError::AlreadyReady);
		}
		let actual_index = writer_stage.registration().index_contract(index)?;
		let document = current_document.with_ready_index(actual_index)?;
		Ok(Self {
			writer_stage,
			policy: ScopePolicy::new(current.identity().clone(), ScopeOwner::Document(document)),
			index: index.clone(),
		})
	}

	pub(crate) fn writer_stage(&self) -> &DocumentWriterStage {
		&self.writer_stage
	}
	pub(crate) fn policy(&self) -> &ScopePolicy {
		&self.policy
	}
	pub(crate) fn index(&self) -> &IndexContractIdentity {
		&self.index
	}
}
impl std::fmt::Debug for DocumentReadinessStage {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("DocumentReadinessStage { .. }")
	}
}
