//! Serializable scoped support derived from actual codec and projector registrations.

use std::fmt;

use crate::installation::contract::{RepresentationPolicy, StateStorageEnvelope};
use crate::records::state_schema::StateRepresentationFormat;

use super::{
	DocumentScopeDeclaration, DocumentScopePolicy, IndexContractDescriptor, ScopeError,
	ScopeIdentity, ScopeOwner, ScopePolicy, check_indexes, require_identifier,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DocumentRole {
	Reader,
	Producer,
	Maintenance,
}

pub(super) struct SupportedRepresentation {
	pub(super) representation: RepresentationPolicy,
	pub(super) indexes: Vec<IndexContractDescriptor>,
}

/// Stored support is capability data, not admission or evidence of query readiness.
pub(crate) struct DocumentRoleSupport {
	pub(super) scope: ScopeIdentity,
	pub(super) schema_identifier: String,
	pub(super) role: DocumentRole,
	pub(super) representations: Vec<SupportedRepresentation>,
	pub(super) query_indexes: Vec<IndexContractDescriptor>,
}

impl DocumentRoleSupport {
	pub(super) fn from_declaration<T, C, P>(
		declaration: &DocumentScopeDeclaration<'_, '_, T, C, P>,
		role: DocumentRole,
	) -> Result<Self, ScopeError> {
		let document = declaration.document();
		let representations = Self::representations_from_declaration(declaration, role);
		Self::parse_parts(
			declaration.identity().clone(),
			document.schema().identifier().to_owned(),
			role,
			representations,
			Vec::new(),
		)
	}

	fn representations_from_declaration<T, C, P>(
		declaration: &DocumentScopeDeclaration<'_, '_, T, C, P>,
		role: DocumentRole,
	) -> Vec<SupportedRepresentation> {
		let document = declaration.document();
		let mut representations = Vec::new();
		document
			.schema()
			.capabilities()
			.for_each_representation(|version, capability| {
				if (role == DocumentRole::Producer
					&& (!capability.writable
						|| capability.format != StateRepresentationFormat::Json))
					|| (role == DocumentRole::Maintenance
						&& capability.format != StateRepresentationFormat::Json)
				{
					return;
				}
				let mut indexes = Vec::new();
				if role != DocumentRole::Reader {
					document.for_each_index_implementation(|projected_version, _, contract| {
						if projected_version == version {
							indexes.push(IndexContractDescriptor::from_declaration(contract));
						}
					});
				}
				representations.push(SupportedRepresentation {
					representation: RepresentationPolicy {
						version,
						envelope: match capability.format {
							StateRepresentationFormat::Bytes => StateStorageEnvelope::SourceBytes,
							StateRepresentationFormat::Json => StateStorageEnvelope::NativeJson,
						},
					},
					indexes,
				});
			});
		representations
	}

	pub(super) fn parse_parts(
		scope: ScopeIdentity,
		schema: String,
		role: DocumentRole,
		representations: Vec<SupportedRepresentation>,
		query_indexes: Vec<IndexContractDescriptor>,
	) -> Result<Self, ScopeError> {
		require_identifier(&schema)?;
		check_indexes(&query_indexes)?;
		if role != DocumentRole::Reader && !query_indexes.is_empty() {
			return Err(ScopeError::InvalidRoleSupport);
		}
		if representations.is_empty() {
			return Err(ScopeError::InvalidRoleSupport);
		}
		for (index, supported) in representations.iter().enumerate() {
			let version = supported.representation.version;
			if version == 0 {
				return Err(ScopeError::RepresentationVersionIsZero);
			}
			if representations[..index]
				.iter()
				.any(|prior| prior.representation.version == version)
			{
				return Err(ScopeError::DuplicateRepresentation { version });
			}
			check_indexes(&supported.indexes)?;
			if (role == DocumentRole::Reader && !supported.indexes.is_empty())
				|| (role != DocumentRole::Reader
					&& supported.representation.envelope != StateStorageEnvelope::NativeJson)
			{
				return Err(ScopeError::InvalidRoleSupport);
			}
		}
		Ok(Self {
			scope,
			schema_identifier: schema,
			role,
			representations,
			query_indexes,
		})
	}

	pub(crate) fn identity(&self) -> &ScopeIdentity {
		&self.scope
	}
	pub(crate) fn role(&self) -> DocumentRole {
		self.role
	}
	pub(crate) fn query_indexes(&self) -> &[IndexContractDescriptor] {
		&self.query_indexes
	}
	pub(crate) fn check_query_policy(
		&self,
		policy: &ScopePolicy,
		index: &crate::kv::document::schema::IndexContractIdentity,
	) -> Result<(), ScopeError> {
		if self.role != DocumentRole::Reader {
			return Err(ScopeError::InvalidRoleSupport);
		}
		self.check_policy(policy)?;
		let declared = self
			.query_indexes
			.iter()
			.find(|declared| declared.identity() == index)
			.ok_or(ScopeError::QueryUseNotRegistered)?;
		let ScopeOwner::Document(document) = policy.owner() else {
			return Err(ScopeError::WrongOwner);
		};
		document.require_ready_index(&declared.0)
	}

	/// Matches parsed cross-process structure. A local operation must separately bind actual code.
	pub(crate) fn check_policy(&self, policy: &ScopePolicy) -> Result<(), ScopeError> {
		let document = self.document_policy(policy)?;
		match self.role {
			DocumentRole::Reader => {
				for representation in document.schema().required_readers() {
					self.require_representation(*representation)?;
				}
			}
			DocumentRole::Producer => {
				self.require_indexes(document.schema().writer(), document)?;
			}
			DocumentRole::Maintenance => {
				for representation in document.schema().required_readers() {
					if representation.envelope != StateStorageEnvelope::NativeJson {
						return Err(ScopeError::NativeMaintenanceRequired);
					}
					self.require_indexes(*representation, document)?;
				}
			}
		}
		Ok(())
	}

	fn document_policy<'a>(
		&self,
		policy: &'a ScopePolicy,
	) -> Result<&'a DocumentScopePolicy, ScopeError> {
		if self.scope != *policy.identity() {
			return Err(ScopeError::ScopeMismatch);
		}
		let ScopeOwner::Document(document) = policy.owner() else {
			return Err(ScopeError::WrongOwner);
		};
		if self.schema_identifier != document.schema().identifier() {
			return Err(ScopeError::Contract(
				crate::installation::contract::StateContractError::SchemaNotBound,
			));
		}
		Ok(document)
	}

	fn check_selected_writer_projection(&self, policy: &ScopePolicy) -> Result<(), ScopeError> {
		if self.role != DocumentRole::Maintenance {
			return Err(ScopeError::InvalidRoleSupport);
		}
		let document = self.document_policy(policy)?;
		self.require_indexes(document.schema().writer(), document)
	}

	fn require_representation(
		&self,
		representation: RepresentationPolicy,
	) -> Result<&SupportedRepresentation, ScopeError> {
		self.representations
			.iter()
			.find(|supported| supported.representation == representation)
			.ok_or(ScopeError::UnsupportedRepresentation {
				version: representation.version,
			})
	}
	fn require_indexes(
		&self,
		representation: RepresentationPolicy,
		policy: &DocumentScopePolicy,
	) -> Result<(), ScopeError> {
		let supported = self.require_representation(representation)?;
		for (index, required) in policy.maintenance_indexes().enumerate() {
			let actual = supported
				.indexes
				.iter()
				.find(|actual| actual.identity() == required.identity())
				.ok_or(ScopeError::MissingIndex { index })?;
			if !required.matches_declaration(&actual.0) {
				return Err(ScopeError::IndexShapeMismatch { index });
			}
		}
		Ok(())
	}
}

impl fmt::Debug for DocumentRoleSupport {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("DocumentRoleSupport")
			.field("role", &self.role)
			.field("representation_count", &self.representations.len())
			.finish()
	}
}

/// Immutable unique scoped roles. The same scope may have distinct consuming/producing roles.
pub(crate) struct DocumentRoleSupportSet {
	entries: Vec<DocumentRoleSupport>,
}

impl DocumentRoleSupportSet {
	/// Fixed verification uses advertised retained decoders and only selected-writer projectors.
	/// This comparison activates no role and leaves ordinary Maintenance requirements unchanged.
	pub(crate) fn check_selected_writer_verification(
		&self,
		policy: &ScopePolicy,
	) -> Result<(), ScopeError> {
		self.lookup(policy.identity(), DocumentRole::Reader)?
			.check_policy(policy)?;
		self.lookup(policy.identity(), DocumentRole::Maintenance)?
			.check_selected_writer_projection(policy)
	}
	pub(crate) fn into_entries(self) -> Vec<DocumentRoleSupport> {
		self.entries
	}
	pub(crate) fn new(entries: Vec<DocumentRoleSupport>) -> Result<Self, ScopeError> {
		for (index, current) in entries.iter().enumerate() {
			if entries[..index].iter().any(|prior| {
				prior.identity() == current.identity() && prior.role() == current.role()
			}) {
				return Err(ScopeError::DuplicateRoleSupport { index });
			}
		}
		Ok(Self { entries })
	}
	pub(crate) fn entries(&self) -> &[DocumentRoleSupport] {
		&self.entries
	}
	pub(crate) fn lookup(
		&self,
		scope: &ScopeIdentity,
		role: DocumentRole,
	) -> Result<&DocumentRoleSupport, ScopeError> {
		self.entries
			.iter()
			.find(|declared| declared.identity() == scope && declared.role() == role)
			.ok_or(ScopeError::RoleNotRegistered)
	}
}
impl fmt::Debug for DocumentRoleSupportSet {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("DocumentRoleSupportSet")
			.field("use_count", &self.entries.len())
			.finish()
	}
}
