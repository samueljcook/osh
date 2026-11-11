//! One actual-code final Document overlay with no new Ready references.
//!
//! This prepared metadata supplies no route, coverage, inspection, physical index or commit authority.

use crate::kv::document::schema::IndexContractIdentity;
use crate::scope::{ScopeError, ScopeOwner, ScopePolicySet};

use super::document_stage::DocumentWriterStage;

pub(in crate::upgrade) struct DocumentReaderCompletionFields<'a> {
	pub(crate) writer_stage: &'a DocumentWriterStage,
	pub(crate) current_scopes: &'a ScopePolicySet,
}
pub(in crate::upgrade) struct DocumentReaderCompletion<'a> {
	writer_stage: &'a DocumentWriterStage,
	current_scopes: &'a ScopePolicySet,
	scopes: ScopePolicySet,
	schema_policy_unchanged: bool,
}
impl<'a> DocumentReaderCompletion<'a> {
	pub(crate) fn new(
		fields: DocumentReaderCompletionFields<'a>,
	) -> Result<Self, BoxedDocumentReaderCompletionError> {
		let stage = fields.writer_stage;
		let build = || {
			let current = fields.current_scopes.lookup(stage.policy().identity())?;
			if !stage.matches_policy(current) {
				return Err(DocumentReaderCompletionError::CurrentStageMismatch);
			}
			let (ScopeOwner::Document(current), ScopeOwner::Document(target)) =
				(current.owner(), stage.final_policy().owner())
			else {
				unreachable!("actual Document writer stage contains only Document policies");
			};
			for index in target.ready_indexes() {
				if !current.is_index_ready(index) {
					return Err(DocumentReaderCompletionError::ReadyAdditionRequired {
						index: index.clone(),
					});
				}
			}
			Ok(Self {
				writer_stage: stage,
				current_scopes: fields.current_scopes,
				scopes: fields
					.current_scopes
					.with_replaced_scope(stage.final_policy())?,
				schema_policy_unchanged: current.schema().matches_policy(target.schema()),
			})
		};
		build().map_err(|error| BoxedDocumentReaderCompletionError {
			source: Box::new(error),
		})
	}
	pub(crate) fn writer_stage(&self) -> &DocumentWriterStage {
		self.writer_stage
	}
	pub(in crate::upgrade) fn current_scopes(&self) -> &ScopePolicySet {
		self.current_scopes
	}
	pub(in crate::upgrade) fn schema_policy_unchanged(&self) -> bool {
		self.schema_policy_unchanged
	}
	pub(crate) fn scopes(&self) -> &ScopePolicySet {
		&self.scopes
	}
	pub(in crate::upgrade) fn into_scopes(self) -> ScopePolicySet {
		self.scopes
	}
}
impl std::fmt::Debug for DocumentReaderCompletion<'_> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("DocumentReaderCompletion")
			.finish_non_exhaustive()
	}
}

#[derive(Debug, thiserror::Error)]
pub(in crate::upgrade) enum DocumentReaderCompletionError {
	#[error("Document reader completion current scope differs from the writer stage")]
	CurrentStageMismatch,
	#[error("Document reader completion requires a requested Ready addition first")]
	ReadyAdditionRequired { index: IndexContractIdentity },
	#[error("Document reader completion scope overlay is invalid")]
	Scope(#[from] ScopeError),
}
pub(crate) struct BoxedDocumentReaderCompletionError {
	source: Box<DocumentReaderCompletionError>,
}
impl std::fmt::Debug for BoxedDocumentReaderCompletionError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("BoxedDocumentReaderCompletionError")
			.field("source", &self.source)
			.finish_non_exhaustive()
	}
}
impl std::fmt::Display for BoxedDocumentReaderCompletionError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("actual current Document stage rejected final reader preparation")
	}
}
impl std::error::Error for BoxedDocumentReaderCompletionError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(self.source.as_ref())
	}
}
