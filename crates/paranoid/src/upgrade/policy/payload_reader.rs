//! One actual-code final payload scope overlay, without effective-state or coverage authority.

use crate::scope::{ScopeError, ScopePolicySet};

use super::payload_stage::PrimitivePayloadWriterStage;

pub(in crate::upgrade) struct PrimitivePayloadReaderCompletionFields<'a, 'code> {
	pub(crate) writer_stage: &'a PrimitivePayloadWriterStage<'code>,
	pub(crate) current_scopes: &'a ScopePolicySet,
}

/// The executor must bind this prepared policy to actual current Work and a fresh inspection.
pub(in crate::upgrade) struct PrimitivePayloadReaderCompletion<'a, 'code> {
	writer_stage: &'a PrimitivePayloadWriterStage<'code>,
	current_scopes: &'a ScopePolicySet,
	scopes: ScopePolicySet,
}
impl<'a, 'code> PrimitivePayloadReaderCompletion<'a, 'code> {
	pub(crate) fn new(
		fields: PrimitivePayloadReaderCompletionFields<'a, 'code>,
	) -> Result<Self, BoxedPrimitivePayloadReaderCompletionError> {
		let stage = fields.writer_stage;
		let build = || {
			let current = fields.current_scopes.lookup(stage.policy().identity())?;
			if !stage.matches_policy(current) {
				return Err(PrimitivePayloadReaderCompletionError::CurrentStageMismatch);
			}
			Ok(Self {
				writer_stage: stage,
				current_scopes: fields.current_scopes,
				scopes: fields
					.current_scopes
					.with_replaced_scope(stage.final_policy())?,
			})
		};
		build().map_err(|error| BoxedPrimitivePayloadReaderCompletionError {
			source: Box::new(error),
		})
	}
	pub(crate) fn writer_stage(&self) -> &PrimitivePayloadWriterStage<'code> {
		self.writer_stage
	}
	pub(in crate::upgrade) fn current_scopes(&self) -> &ScopePolicySet {
		self.current_scopes
	}
	pub(crate) fn scopes(&self) -> &ScopePolicySet {
		&self.scopes
	}
	pub(in crate::upgrade) fn into_scopes(self) -> ScopePolicySet {
		self.scopes
	}
}
impl std::fmt::Debug for PrimitivePayloadReaderCompletion<'_, '_> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("PrimitivePayloadReaderCompletion")
			.finish_non_exhaustive()
	}
}

#[derive(Debug, thiserror::Error)]
pub(in crate::upgrade) enum PrimitivePayloadReaderCompletionError {
	#[error("payload reader completion current scope differs from the writer stage")]
	CurrentStageMismatch,
	#[error("payload reader completion scope overlay is invalid")]
	Scope(#[from] ScopeError),
}
pub(crate) struct BoxedPrimitivePayloadReaderCompletionError {
	source: Box<PrimitivePayloadReaderCompletionError>,
}
impl std::fmt::Debug for BoxedPrimitivePayloadReaderCompletionError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("BoxedPrimitivePayloadReaderCompletionError")
			.field("source", &self.source)
			.finish_non_exhaustive()
	}
}
impl std::fmt::Display for BoxedPrimitivePayloadReaderCompletionError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("actual current payload stage rejected final reader preparation")
	}
}
impl std::error::Error for BoxedPrimitivePayloadReaderCompletionError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(self.source.as_ref())
	}
}
