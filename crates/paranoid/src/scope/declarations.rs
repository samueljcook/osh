//! Code-derived application advertisements and explicit immutable initial use intent.
//!
//! Parsed declarations grant no installed ownership, active participation, or background work.
//! Managed code remains owned independently from whether its roles are initially active.

use std::{fmt, sync::Arc};

use crate::kv::document::schema::IndexContractIdentity;
use crate::kv::document::schema::managed::{
	ManagedDocumentError, ManagedDocumentRegistry, ManagedDocumentRegistryError,
};
use crate::runtime::participation::state::catalog::ParticipantUseReference;
use crate::scope::managed::{
	ManagedScopeRegistry, ManagedScopeRegistryError, ManagedScopeRegistryFields,
};
use crate::scope::payload::managed::ManagedPayloadError;
use crate::scope::payload::{PayloadRoleSupport, PayloadRoleSupportSet};
use crate::scope::{
	DocumentRoleSupport, DocumentRoleSupportSet, PrimitiveScopeSupport, PrimitiveScopeSupportSet,
	RawRole, RawRoleSupport, RawRoleSupportSet, RawScopeDeclaration, ScopeError, ScopeIdentity,
};

mod document;
mod primitive;
mod public_error;
use crate::scope::DocumentScope;
pub use document::AppDocumentDeclaration;
pub use primitive::{
	AppCacheDeclarationFields, AppPrimitiveDeclaration, AppPrimitiveDeclarationFields,
	AppTopicDeclarationFields,
};
pub use public_error::AppDeclarationErrorKind;

/// Whether a runtime uses a role that its code supports when the runtime starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitialUse {
	/// The runtime uses the role when it starts.
	Active,
	/// The runtime does not use the role when it starts.
	Dormant,
}

/// Whether this deployment's code supports a role, and whether its runtime uses the role when
/// it starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoleUse {
	/// The code does not support the role.
	Unsupported,
	/// The code supports the role, and the runtime does not use it when it starts.
	Dormant,
	/// The code supports the role, and the runtime uses it when it starts.
	Active,
}

impl RoleUse {
	pub(crate) fn is_supported(self) -> bool {
		self != Self::Unsupported
	}
	pub(crate) fn initial_use(self) -> Option<InitialUse> {
		match self {
			Self::Unsupported => None,
			Self::Dormant => Some(InitialUse::Dormant),
			Self::Active => Some(InitialUse::Active),
		}
	}
}

/// One index that a scope's code queries.
///
/// When opening creates an installation or imports v10, every index that the declarations
/// query, `Active` or `Dormant`, is ready for queries, so the first release can declare its
/// queries [`InitialUse::Active`]. On an existing installation, an index becomes ready for
/// queries only through [`PgRuntime::start_upgrade`](crate::runtime::PgRuntime::start_upgrade),
/// also an index of a scope that a starting runtime added, and a runtime that starts with an
/// `Active` query of an index that is not ready fails to start. A release that adds a query to an existing installation therefore declares it
/// [`InitialUse::Dormant`] and runs the upgrade, and a later release declares it `Active`.
pub struct QueryUseIntent {
	/// The queried index, which the scope's document code declares.
	pub index: IndexContractIdentity,
	/// Whether the runtime queries the index when it starts.
	pub initial_use: InitialUse,
}

/// The document roles that a scope's code supports, and whether the runtime uses each one
/// when it starts.
///
/// A query use does not make the runtime a point reader, and it does not make the index
/// ready for queries.
pub struct DocumentUseIntent {
	/// Reading records by key.
	pub point_reader: RoleUse,
	/// Writing records.
	pub producer: RoleUse,
	/// Maintaining records, such as deleting expired records.
	pub maintenance: RoleUse,
	/// The indexes that the code queries; each index appears at most once.
	pub queries: Vec<QueryUseIntent>,
}

/// The inputs of a document scope declaration.
pub struct AppDocumentDeclarationFields<T, C, P> {
	/// The scope and its document code.
	pub scope: DocumentScope<T, C, P>,
	/// The document roles that the code supports, and the queries that it runs.
	pub intent: DocumentUseIntent,
}

/// The Semaphore, Throttler, cache, or Topic roles that a scope's code supports, and whether
/// the runtime uses each one when it starts.
pub struct PrimitiveUseIntent {
	/// Reading the scope's state.
	pub reader: RoleUse,
	/// Changing the scope's state.
	pub writer: RoleUse,
}

/// The cache value or Topic payload roles that a scope's code supports, and whether the
/// runtime uses each one when it starts.
pub struct PayloadUseIntent {
	/// Decoding the scope's values.
	pub reader: RoleUse,
	/// Encoding new values.
	pub producer: RoleUse,
}

/// The Raw roles that a scope's code supports, and whether the runtime uses each one when it
/// starts.
pub struct RawUseIntent {
	/// Reading the scope's bytes.
	pub reader: RoleUse,
	/// Writing the scope's bytes.
	pub writer: RoleUse,
}

/// The inputs of a Raw scope declaration.
pub struct AppRawDeclarationFields {
	/// The scope's identity.
	pub scope: ScopeIdentity,
	/// The roles that the code supports.
	pub intent: RawUseIntent,
}

/// One Raw scope that the app declares.
pub struct AppRawDeclaration {
	scope: ScopeIdentity,
	support: Vec<RawRoleSupport>,
	initial_uses: Vec<ParticipantUseReference>,
	managed: Arc<RawScopeDeclaration>,
}
impl AppRawDeclaration {
	/// Declares a Raw scope.
	pub fn new(fields: AppRawDeclarationFields) -> Result<Self, AppDeclarationError> {
		let declaration = RawScopeDeclaration::new(fields.scope);
		let (support, initial_uses) = derive_raw_support(&declaration, fields.intent)?;
		Ok(Self {
			scope: declaration.identity().clone(),
			support,
			initial_uses,
			managed: Arc::new(declaration),
		})
	}
}
fn derive_raw_support(
	declaration: &RawScopeDeclaration,
	intent: RawUseIntent,
) -> Result<(Vec<RawRoleSupport>, Vec<ParticipantUseReference>), AppDeclarationError> {
	let mut support = Vec::new();
	let mut initial_uses = Vec::new();
	for (role, requested) in [
		(RawRole::Reader, intent.reader.initial_use()),
		(RawRole::Writer, intent.writer.initial_use()),
	] {
		if let Some(initial) = requested {
			support.push(declaration.support(role).map_err(|error| {
				AppDeclarationError::at_scope(
					declaration.identity(),
					AppDeclarationInnerError::Scope(error),
				)
			})?);
			if initial == InitialUse::Active {
				initial_uses.push(ParticipantUseReference::Raw {
					scope: declaration.identity().clone(),
					role,
				});
			}
		}
	}
	Ok((support, initial_uses))
}

/// The inputs of an app's declarations. Each list may be empty.
pub struct AppDeclarationsFields {
	/// The document scopes.
	pub documents: Vec<AppDocumentDeclaration>,
	/// The Raw scopes.
	pub raw: Vec<AppRawDeclaration>,
	/// The Semaphore, Throttler, cache, and Topic scopes.
	pub primitives: Vec<AppPrimitiveDeclaration>,
}

/// Every scope that an app's code declares, with the roles that the code supports in each
/// scope. Opening an installation takes the declarations, and every runtime of the
/// installation uses them.
pub struct AppDeclarations {
	document_roles: DocumentRoleSupportSet,
	raw_roles: RawRoleSupportSet,
	primitive_scopes: PrimitiveScopeSupportSet,
	payload_roles: PayloadRoleSupportSet,
	initial_optional_uses: Vec<ParticipantUseReference>,
	managed_scopes: Arc<ManagedScopeRegistry>,
}

/// Owned parts move once into registration and the runtime; they contain no admission authority.
pub(crate) struct AppDeclarationParts {
	pub(crate) document_roles: Vec<DocumentRoleSupport>,
	pub(crate) raw_roles: Vec<RawRoleSupport>,
	pub(crate) primitive_scopes: Vec<PrimitiveScopeSupport>,
	pub(crate) payload_roles: Vec<PayloadRoleSupport>,
	pub(crate) initial_optional_uses: Vec<ParticipantUseReference>,
	pub(crate) managed_scopes: Arc<ManagedScopeRegistry>,
}

impl AppDeclarations {
	/// Checks the declarations together without database work. It fails when two declarations
	/// name one scope.
	pub fn new(fields: AppDeclarationsFields) -> Result<Self, AppDeclarationError> {
		for declaration in &fields.raw {
			if fields
				.documents
				.iter()
				.any(|document| document.scope == declaration.scope)
				|| fields
					.primitives
					.iter()
					.any(|primitive| primitive.scope == declaration.scope)
			{
				return Err(AppDeclarationError::at_scope(
					&declaration.scope,
					AppDeclarationInnerError::ConflictingScopeOwner,
				));
			}
		}
		for (index, declaration) in fields.primitives.iter().enumerate() {
			if fields
				.documents
				.iter()
				.any(|document| document.scope == declaration.scope)
			{
				return Err(AppDeclarationError::at_scope(
					&declaration.scope,
					AppDeclarationInnerError::ConflictingScopeOwner,
				));
			}
			if fields.primitives[..index]
				.iter()
				.any(|prior| prior.scope == declaration.scope)
			{
				return Err(AppDeclarationError::at_scope(
					&declaration.scope,
					AppDeclarationInnerError::Scope(ScopeError::DuplicateScope { index }),
				));
			}
		}
		for (index, declaration) in fields.documents.iter().enumerate() {
			if fields.documents[..index].iter().any(|prior| {
				prior.scope == declaration.scope
					&& prior.schema_identifier != declaration.schema_identifier
			}) {
				return Err(AppDeclarationError::at_scope(
					&declaration.scope,
					AppDeclarationInnerError::ConflictingSchemaFamily { index },
				));
			}
		}
		let mut document_support = Vec::new();
		let mut raw_support = Vec::new();
		let mut initial_optional_uses = Vec::new();
		let mut managed = Vec::new();
		let mut document_queries = Vec::new();
		let mut managed_raw = Vec::new();
		let mut managed_primitives = Vec::new();
		let mut primitive_support = Vec::new();
		let mut payload_support = Vec::new();
		for declaration in fields.documents {
			document_support.extend(declaration.support);
			initial_optional_uses.extend(declaration.initial_uses);
			document_queries.push((declaration.scope.clone(), declaration.query_indexes));
			managed.push(declaration.managed);
		}
		for declaration in fields.raw {
			raw_support.extend(declaration.support);
			initial_optional_uses.extend(declaration.initial_uses);
			managed_raw.push(declaration.managed);
		}
		for declaration in fields.primitives {
			primitive_support.extend(declaration.support);
			payload_support.extend(declaration.payload_support);
			initial_optional_uses.extend(declaration.initial_uses);
			managed_primitives.push(declaration.managed);
		}
		let document_roles =
			DocumentRoleSupportSet::new(document_support).map_err(AppDeclarationError::unscoped)?;
		let raw_roles =
			RawRoleSupportSet::new(raw_support).map_err(AppDeclarationError::unscoped)?;
		let managed_documents =
			Arc::new(ManagedDocumentRegistry::new(managed).map_err(|error| {
				AppDeclarationError {
					scope: None,
					source: Box::new(AppDeclarationInnerError::DocumentRegistry(error)),
				}
			})?);
		let primitive_scopes = PrimitiveScopeSupportSet::new(primitive_support)
			.map_err(AppDeclarationError::unscoped)?;
		let payload_roles =
			PayloadRoleSupportSet::new(payload_support).map_err(AppDeclarationError::unscoped)?;
		let managed_scopes = Arc::new(
			ManagedScopeRegistry::new(ManagedScopeRegistryFields {
				document_registry: managed_documents,
				document_queries,
				raw: managed_raw,
				primitives: managed_primitives,
			})
			.map_err(|error| AppDeclarationError {
				scope: Some(error.scope().clone()),
				source: Box::new(AppDeclarationInnerError::ScopeRegistry(error)),
			})?,
		);
		Ok(Self {
			document_roles,
			raw_roles,
			primitive_scopes,
			payload_roles,
			initial_optional_uses,
			managed_scopes,
		})
	}
	/// The document code of every declared document scope.
	pub(crate) fn document_registry(
		&self,
	) -> &crate::kv::document::schema::managed::ManagedDocumentRegistry {
		self.managed_scopes.document_registry()
	}

	/// The initial selection of every declared scope.
	pub(crate) fn initial_selections(
		&self,
	) -> Result<
		crate::scope::selection::ScopeSelections,
		crate::scope::selection::ScopeSelectionError,
	> {
		self.managed_scopes.initial_selections()
	}

	pub(crate) fn into_parts(self) -> AppDeclarationParts {
		AppDeclarationParts {
			document_roles: self.document_roles.into_entries(),
			raw_roles: self.raw_roles.into_entries(),
			primitive_scopes: self.primitive_scopes.into_entries(),
			payload_roles: self.payload_roles.into_entries(),
			initial_optional_uses: self.initial_optional_uses,
			managed_scopes: self.managed_scopes,
		}
	}
}
impl fmt::Debug for AppDeclarations {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("AppDeclarations")
			.field("document_role_count", &self.document_roles.entries().len())
			.field("raw_role_count", &self.raw_roles.entries().len())
			.field(
				"primitive_scope_count",
				&self.primitive_scopes.entries().len(),
			)
			.field("payload_role_count", &self.payload_roles.entries().len())
			.field("initial_use_count", &self.initial_optional_uses.len())
			.field(
				"managed_document_scope_count",
				&self
					.managed_scopes
					.document_registry()
					.registrations()
					.len(),
			)
			.finish()
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AppDeclarationInnerError {
	#[error("payload code cannot describe the requested initial role")]
	Payload(#[source] Box<ManagedPayloadError>),
	#[error("scope declarations have conflicting ownership kinds")]
	ConflictingScopeOwner,
	#[error("scope registry rejects competing scope owners")]
	ScopeRegistry(#[source] ManagedScopeRegistryError),
	#[error("document query intent {index} repeats an index")]
	DuplicateQuery { index: usize },
	#[error("document declaration {index} conflicts with this scope's schema family")]
	ConflictingSchemaFamily { index: usize },
	#[error("scope support cannot describe the requested initial declaration")]
	Scope(#[source] ScopeError),
	#[error("managed document code cannot describe the requested initial declaration")]
	DocumentDeclaration(#[source] ManagedDocumentError),
	#[error("managed document registry rejects competing scope registrations")]
	DocumentRegistry(#[source] ManagedDocumentRegistryError),
}

/// A declaration that does not match its code or conflicts with another declaration. Its
/// formatting shows no stored values.
#[derive(Debug)]
pub struct AppDeclarationError {
	scope: Option<ScopeIdentity>,
	source: Box<AppDeclarationInnerError>,
}
impl AppDeclarationError {
	fn at_scope(scope: &ScopeIdentity, source: AppDeclarationInnerError) -> Self {
		Self {
			scope: Some(scope.clone()),
			source: Box::new(source),
		}
	}
	fn unscoped(source: ScopeError) -> Self {
		Self {
			scope: None,
			source: Box::new(AppDeclarationInnerError::Scope(source)),
		}
	}
	/// The scope of the failed declaration, when the failure concerns one scope.
	pub fn scope(&self) -> Option<&ScopeIdentity> {
		self.scope.as_ref()
	}
}
impl fmt::Display for AppDeclarationError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("the app declarations do not match their code")
	}
}
impl std::error::Error for AppDeclarationError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(self.source.as_ref())
	}
}
