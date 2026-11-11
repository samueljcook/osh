//! Scoped document roles bound only after private participation and control admission.
//!
//! These pure checks add no IO or physical routing proof. A read-modify-write operation
//! must bind both reader and producer before protected work; neither role implies the other.

use crate::scope::{
	BoundDocumentMaintenance, BoundDocumentProducer, BoundDocumentQueryReader, BoundDocumentReader,
	DocumentRole, DocumentScopeDeclaration, ScopeIdentity, ScopePolicy,
};

use super::{NativeAdmissionError, NativeAdmittedContracts};

impl NativeAdmittedContracts {
	pub(crate) fn bind_document_query_reader<'a, 's, 'd, T, C, P>(
		&'a self,
		query: &'a crate::kv::document::query::ScopedDocumentIndexQuery<'_, 's, 'd, T, C, P>,
	) -> Result<BoundDocumentQueryReader<'a, 's, 'd, T, C, P>, NativeAdmissionError> {
		let declaration = query.declaration();
		let policy = self
			.observation
			.control
			.state()
			.scope_policies()
			.lookup(declaration.identity())?;
		let support = self
			.observation
			.participant()?
			.capabilities()
			.document_queries()
			.lookup(declaration.identity(), query.index().identity())?;
		support.check_policy(policy)?;
		if !support.index().matches_declaration(query.index()) {
			return Err(crate::scope::ScopeError::QueryIndexShapeMismatch.into());
		}
		Ok(declaration.bind_query_reader(policy, query.index())?)
	}
	pub(crate) fn bind_document_reader<'a, 's, 'd, T, C, P>(
		&'a self,
		declaration: &'a DocumentScopeDeclaration<'s, 'd, T, C, P>,
	) -> Result<BoundDocumentReader<'a, 's, 'd, T, C, P>, NativeAdmissionError> {
		let policy = self.document_policy(declaration.identity(), DocumentRole::Reader)?;
		Ok(declaration.bind_reader(policy)?)
	}

	pub(crate) fn bind_document_producer<'a, 's, 'd, T, C, P>(
		&'a self,
		declaration: &'a DocumentScopeDeclaration<'s, 'd, T, C, P>,
	) -> Result<BoundDocumentProducer<'a, 's, 'd, T, C, P>, NativeAdmissionError> {
		let policy = self.document_policy(declaration.identity(), DocumentRole::Producer)?;
		Ok(declaration.bind_producer(policy)?)
	}

	pub(crate) fn bind_document_maintenance<'a, 's, 'd, T, C, P>(
		&'a self,
		declaration: &'a DocumentScopeDeclaration<'s, 'd, T, C, P>,
	) -> Result<BoundDocumentMaintenance<'a, 'd, T, C, P>, NativeAdmissionError> {
		let policy = self.document_policy(declaration.identity(), DocumentRole::Maintenance)?;
		Ok(declaration.bind_maintenance(policy)?)
	}

	fn document_policy(
		&self,
		scope: &ScopeIdentity,
		role: DocumentRole,
	) -> Result<&ScopePolicy, NativeAdmissionError> {
		let policy = self
			.observation
			.control
			.state()
			.scope_policies()
			.lookup(scope)?;
		let support = self
			.observation
			.participant()?
			.capabilities()
			.document_roles()
			.lookup(scope, role)?;
		support.check_policy(policy)?;
		Ok(policy)
	}
}
