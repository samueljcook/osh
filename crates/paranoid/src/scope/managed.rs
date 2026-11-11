//! Retained actual document, raw-byte, and primitive owners for one logical scope domain.
//!
//! Code retention and pure candidate binding provide no admission, physical association,
//! adoption, data conversion, readiness, or lifecycle authority.

use std::{fmt, sync::Arc};

use super::{
	ComponentScopeDeclaration, ComponentScopeKind, RawScopeDeclaration, ScopeIdentity, ScopeOwner,
	ScopePolicy,
};
use crate::fleet::payload::scope::retained::RetainedPayloadError;
use crate::fleet::primitive_scope::ManagedPrimitiveRegistration;
use crate::kv::document::schema::managed::{
	ManagedDocumentError, ManagedDocumentRegistration, ManagedDocumentRegistry,
};
use crate::upgrade::policy::scope::{CandidateScopePolicy, ScopedUpgradeCompatibilityError};

pub(crate) struct ManagedScopeRegistryFields {
	pub(crate) document_registry: Arc<ManagedDocumentRegistry>,
	/// Each document scope's queried indexes.
	pub(crate) document_queries: Vec<(
		ScopeIdentity,
		Vec<crate::kv::document::schema::IndexContractIdentity>,
	)>,
	pub(crate) raw: Vec<Arc<RawScopeDeclaration>>,
	pub(crate) primitives: Vec<ManagedPrimitiveRegistration>,
}

/// One exact retained owner per scope; installed policies remain owned by installation control.
pub(crate) struct ManagedScopeRegistry {
	fields: ManagedScopeRegistryFields,
}

pub(crate) enum ManagedScopeRegistration<'a> {
	Document(&'a ManagedDocumentRegistration),
	Raw(&'a RawScopeDeclaration),
	Primitive(&'a ManagedPrimitiveRegistration),
}

impl ManagedScopeRegistry {
	pub(crate) fn new(
		fields: ManagedScopeRegistryFields,
	) -> Result<Self, ManagedScopeRegistryError> {
		for (index, entry) in fields.raw.iter().enumerate() {
			if fields
				.document_registry
				.registrations()
				.iter()
				.any(|document| document.scope() == entry.identity())
				|| fields.raw[..index]
					.iter()
					.any(|prior| prior.identity() == entry.identity())
			{
				return Err(ManagedScopeRegistryError::duplicate(
					entry.identity(),
					index,
				));
			}
		}
		for (index, entry) in fields.primitives.iter().enumerate() {
			if fields
				.document_registry
				.registrations()
				.iter()
				.any(|document| document.scope() == entry.identity())
				|| fields
					.raw
					.iter()
					.any(|raw| raw.identity() == entry.identity())
				|| fields.primitives[..index]
					.iter()
					.any(|prior| prior.identity() == entry.identity())
			{
				return Err(ManagedScopeRegistryError::duplicate(
					entry.identity(),
					index,
				));
			}
		}
		Ok(Self { fields })
	}
	pub(crate) fn document_registry(&self) -> &Arc<ManagedDocumentRegistry> {
		&self.fields.document_registry
	}
	/// The indexes that the code queries in `scope`, whether it declares each query `Active`
	/// or `Dormant`.
	fn queries(
		&self,
		scope: &ScopeIdentity,
	) -> &[crate::kv::document::schema::IndexContractIdentity] {
		self.fields
			.document_queries
			.iter()
			.find(|(queried, _)| queried == scope)
			.map(|(_, indexes)| indexes.as_slice())
			.unwrap_or_default()
	}
	/// The upgrade target of every registered scope, given the installed policies.
	pub(crate) fn target_selections(
		&self,
		installed: &super::ScopePolicySet,
	) -> Result<super::selection::ScopeSelections, super::selection::ScopeSelectionError> {
		use super::selection::{RawScopeSelectionFields, ScopeSelection, ScopeSelectionsFields};
		let installed_policy = |scope: &ScopeIdentity| {
			installed
				.policies()
				.iter()
				.find(|policy| policy.identity() == scope)
		};
		let mut scopes = Vec::new();
		for document in self.fields.document_registry.registrations() {
			scopes.push(document.target_selection(
				installed_policy(document.scope()),
				self.queries(document.scope()),
			)?);
		}
		for raw in &self.fields.raw {
			scopes.push(ScopeSelection::raw(RawScopeSelectionFields {
				declaration: raw,
			})?);
		}
		for primitive in &self.fields.primitives {
			let policy = ScopeSelection::target_primitive_policy(
				&primitive.declaration(),
				primitive.payload_code(),
				installed_policy(primitive.identity()),
			)?;
			let candidate = primitive
				.bind_final_candidate(policy)
				.map_err(|error| ScopeSelection::payload_failure(primitive.identity(), error))?;
			scopes.push(ScopeSelection::from_candidate(candidate));
		}
		super::selection::ScopeSelections::new(ScopeSelectionsFields { scopes })
	}

	/// The upgrade target of every registered scope that `installed` does not have. Such a
	/// scope holds no records, so a starting runtime installs it as the upgrade that adds a
	/// scope does. A scope with no JSON writer stays absent; participant registration
	/// still requires every active role to have an installed scope.
	pub(crate) fn absent_scope_targets(
		&self,
		installed: &super::ScopePolicySet,
	) -> Result<Vec<super::ScopePolicy>, super::selection::ScopeSelectionError> {
		use super::selection::{RawScopeSelectionFields, ScopeSelection, ScopeSelectionErrorKind};
		let absent = |scope: &ScopeIdentity| installed.lookup(scope).is_err();
		let mut targets = Vec::new();
		for document in self.fields.document_registry.registrations() {
			if absent(document.scope()) {
				match document.target_selection(None, self.queries(document.scope())) {
					Ok(selection) => targets.push(selection.into_policy()),
					Err(error) if error.kind() == ScopeSelectionErrorKind::NoJsonWriter => {}
					Err(error) => return Err(error),
				}
			}
		}
		for raw in &self.fields.raw {
			if absent(raw.identity()) {
				targets.push(
					ScopeSelection::raw(RawScopeSelectionFields { declaration: raw })?
						.into_policy(),
				);
			}
		}
		for primitive in &self.fields.primitives {
			if absent(primitive.identity()) {
				let policy = match ScopeSelection::target_primitive_policy(
					&primitive.declaration(),
					primitive.payload_code(),
					None,
				) {
					Ok(policy) => policy,
					Err(error) if error.kind() == ScopeSelectionErrorKind::NoJsonWriter => continue,
					Err(error) => return Err(error),
				};
				let candidate = primitive.bind_final_candidate(policy).map_err(|error| {
					ScopeSelection::payload_failure(primitive.identity(), error)
				})?;
				targets.push(ScopeSelection::from_candidate(candidate).into_policy());
			}
		}
		Ok(targets)
	}

	/// The initial selection of every registered scope, which opening records when it creates
	/// an installation or imports v10. An index that the code queries is ready for queries,
	/// and every other declared index is maintained.
	pub(crate) fn initial_selections(
		&self,
	) -> Result<super::selection::ScopeSelections, super::selection::ScopeSelectionError> {
		use super::selection::{RawScopeSelectionFields, ScopeSelection, ScopeSelectionsFields};
		let mut scopes = Vec::new();
		for document in self.fields.document_registry.registrations() {
			scopes.push(document.initial_selection(self.queries(document.scope()))?);
		}
		for raw in &self.fields.raw {
			scopes.push(ScopeSelection::raw(RawScopeSelectionFields {
				declaration: raw,
			})?);
		}
		for primitive in &self.fields.primitives {
			let policy = ScopeSelection::initial_primitive_policy(
				&primitive.declaration(),
				primitive.payload_code(),
			)?;
			let candidate = primitive
				.bind_final_candidate(policy)
				.map_err(|error| ScopeSelection::payload_failure(primitive.identity(), error))?;
			scopes.push(ScopeSelection::from_candidate(candidate));
		}
		super::selection::ScopeSelections::new(ScopeSelectionsFields { scopes })
	}
	pub(crate) fn lookup(
		&self,
		scope: &ScopeIdentity,
	) -> Result<ManagedScopeRegistration<'_>, ManagedScopeRegistryError> {
		if let Some(document) = self
			.fields
			.document_registry
			.registrations()
			.iter()
			.find(|entry| entry.scope() == scope)
		{
			return Ok(ManagedScopeRegistration::Document(document));
		}
		if let Some(raw) = self
			.fields
			.raw
			.iter()
			.find(|entry| entry.identity() == scope)
		{
			return Ok(ManagedScopeRegistration::Raw(raw));
		}
		if let Some(primitive) = self
			.fields
			.primitives
			.iter()
			.find(|entry| entry.identity() == scope)
		{
			return Ok(ManagedScopeRegistration::Primitive(primitive));
		}
		Err(ManagedScopeRegistryError {
			scope: scope.clone(),
			source: ManagedScopeRegistryInnerError::MissingOwner,
		})
	}
	pub(crate) fn bind_final_candidate(
		&self,
		policy: ScopePolicy,
	) -> Result<CandidateScopePolicy, ManagedScopeRegistryError> {
		let scope = policy.identity().clone();
		let fail = |source| ManagedScopeRegistryError {
			scope: scope.clone(),
			source,
		};
		if matches!(policy.owner(), ScopeOwner::Closed) {
			return CandidateScopePolicy::closed(policy)
				.map_err(|error| fail(ManagedScopeRegistryInnerError::Candidate(error)));
		}
		match self.lookup(&scope)? {
			ManagedScopeRegistration::Document(document) => document
				.bind_final_candidate(policy)
				.map(CandidateScopePolicy::from_document)
				.map_err(|error| fail(ManagedScopeRegistryInnerError::Document(error))),
			ManagedScopeRegistration::Raw(raw) => {
				let component = raw.declaration();
				let declaration = ComponentScopeDeclaration::new(
					raw.identity(),
					&component,
					ComponentScopeKind::Raw,
				);
				CandidateScopePolicy::from_component(&declaration, policy)
					.map_err(|error| fail(ManagedScopeRegistryInnerError::Candidate(error)))
			}
			ManagedScopeRegistration::Primitive(primitive) => primitive
				.bind_final_candidate(policy)
				.map_err(|error| fail(ManagedScopeRegistryInnerError::Payload(Box::new(error)))),
		}
	}
}

impl fmt::Debug for ManagedScopeRegistry {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("ManagedScopeRegistry")
			.field(
				"document_count",
				&self.fields.document_registry.registrations().len(),
			)
			.field("raw_count", &self.fields.raw.len())
			.field("primitive_count", &self.fields.primitives.len())
			.finish()
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ManagedScopeRegistryInnerError {
	#[error("scope target does not bind its intrinsic and payload code")]
	Payload(#[source] Box<RetainedPayloadError>),
	#[error("scope repeats an owner at registration {index}")]
	DuplicateOwner { index: usize },
	#[error("scope has no owner")]
	MissingOwner,
	#[error("scope target does not bind its component code")]
	Candidate(#[source] ScopedUpgradeCompatibilityError),
	#[error("scope target does not bind its document code")]
	Document(#[source] ManagedDocumentError),
}

pub(crate) struct ManagedScopeRegistryError {
	scope: ScopeIdentity,
	source: ManagedScopeRegistryInnerError,
}
impl ManagedScopeRegistryError {
	fn duplicate(scope: &ScopeIdentity, index: usize) -> Self {
		Self {
			scope: scope.clone(),
			source: ManagedScopeRegistryInnerError::DuplicateOwner { index },
		}
	}
	pub(crate) fn scope(&self) -> &ScopeIdentity {
		&self.scope
	}
	pub(crate) fn inner_error(&self) -> &ManagedScopeRegistryInnerError {
		&self.source
	}
}
impl fmt::Debug for ManagedScopeRegistryError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("ManagedScopeRegistryError")
			.field("source", &self.source)
			.finish_non_exhaustive()
	}
}
impl fmt::Display for ManagedScopeRegistryError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(
			"the scope's registration is invalid, or its declared code does not support the \
			 requested target",
		)
	}
}
impl std::error::Error for ManagedScopeRegistryError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(&self.source)
	}
}
