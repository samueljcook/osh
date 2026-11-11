//! Code-bound application choices for initial installation and requested upgrade targets.

use std::fmt;

use crate::fleet::primitive_scope::PrimitiveScopeDeclaration;
use crate::installation::contract::{RepresentationPolicy, SchemaPolicy, StateStorageEnvelope};
use crate::kv::document::schema::IndexContract;
use crate::records::state_schema::{StateRepresentationFormat, StateSchemaCapabilities};
use crate::upgrade::policy::scope::{
	CandidateDocumentScopePolicy, CandidateScopePolicy, ScopeUpgradeCandidatePolicies,
};

use super::{
	ComponentScopeDeclaration, ComponentScopeKind, DocumentScopeDeclaration, DocumentScopePolicy,
	IndexContractDescriptor, PrimitivePayloadPolicy, PrimitiveScopePolicy,
	PrimitiveScopePolicyFields, RawScopeDeclaration, ScopeError, ScopeIdentity, ScopeOwner,
	ScopePolicy, ScopePolicySet,
};

mod error;
pub use error::{ScopeSelectionError, ScopeSelectionErrorKind};
use error::{ScopeSelectionInnerError, SelectionFailureFields};

/// The requested final meaning of one logical index.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DocumentIndexRequirement {
	/// Requires stored projections without permitting queries through this index.
	Maintained,
	/// Requests query readiness, which an upgrade establishes, or opening when it creates the
	/// installation.
	Ready,
}

/// An exact logical index declaration and its requested final meaning.
pub(crate) struct DocumentIndexSelection<'a> {
	/// The actual contract registered in the supplied document declaration.
	pub(crate) contract: &'a IndexContract,
	/// The desired maintained or query-ready target; this is not a readiness proof.
	pub(crate) requirement: DocumentIndexRequirement,
}

/// Explicit version and index choices checked against actual document code.
pub(crate) struct DocumentScopeSelectionFields<'view, 'scope, 'document, T, C, P> {
	/// The actual borrowed scope and document code declaration.
	pub(crate) declaration: &'view DocumentScopeDeclaration<'scope, 'document, T, C, P>,
	/// The explicitly selected representation version with a registered writer.
	pub(crate) writer_version: u32,
	/// Required readable versions, including the writer explicitly.
	pub(crate) retained_reader_versions: Vec<u32>,
	/// Exact logical contracts to maintain or make query-ready.
	pub(crate) indexes: Vec<DocumentIndexSelection<'view>>,
}

/// The actual schema-free Raw code and scope to select.
pub(crate) struct RawScopeSelectionFields<'a> {
	/// The application's actual Raw scope declaration.
	pub(crate) declaration: &'a RawScopeDeclaration,
}

/// A requested scope policy checked against actual code, without installed authority.
pub(crate) struct ScopeSelection {
	candidate: CandidateScopePolicy,
}

impl ScopeSelection {
	/// Checks explicit document choices without invoking a codec or projector.
	///
	/// Every requested index is maintained. A Ready request selects no physical index: an
	/// upgrade, or the opening that creates the installation, creates and records it.
	pub(crate) fn document<T, C, P>(
		fields: DocumentScopeSelectionFields<'_, '_, '_, T, C, P>,
	) -> Result<Self, ScopeSelectionError> {
		let declaration = fields.declaration;
		let scope = declaration.identity();
		let document = declaration.document();
		let capabilities = document.schema().capabilities();
		let fail = |source, version, index_position| {
			ScopeSelectionError::new(SelectionFailureFields {
				scope: Some(scope),
				schema_identifier: Some(capabilities.identifier()),
				version,
				index_position,
				source,
			})
		};
		let schema = selected_schema(SelectedSchemaFields {
			scope,
			capabilities,
			writer_version: fields.writer_version,
			retained_reader_versions: fields.retained_reader_versions,
		})?;
		let mut maintained = Vec::with_capacity(fields.indexes.len());
		let mut ready = Vec::new();
		for (position, index) in fields.indexes.into_iter().enumerate() {
			document
				.require_index_declaration(index.contract)
				.map_err(|error| {
					fail(ScopeSelectionInnerError::Index(error), None, Some(position))
				})?;
			maintained.push(IndexContractDescriptor::from_declaration(index.contract));
			if index.requirement == DocumentIndexRequirement::Ready {
				ready.push(index.contract.identity().clone());
			}
		}
		let selected = DocumentScopePolicy::new_with_ready_indexes(schema, maintained, ready)
			.map_err(|error| fail(ScopeSelectionInnerError::Scope(error), None, None))?;
		let candidate = CandidateDocumentScopePolicy::new(
			declaration,
			ScopePolicy::new(scope.clone(), ScopeOwner::Document(selected)),
		)
		.map_err(|error| fail(ScopeSelectionInnerError::Candidate(error), None, None))?;
		Ok(Self {
			candidate: CandidateScopePolicy::from_document(candidate),
		})
	}

	/// Selects the actual Raw primitive's fixed opaque-byte contract.
	pub(crate) fn raw(fields: RawScopeSelectionFields<'_>) -> Result<Self, ScopeSelectionError> {
		let scope = fields.declaration.identity();
		let fail = |source| {
			ScopeSelectionError::new(SelectionFailureFields {
				scope: Some(scope),
				schema_identifier: None,
				version: None,
				index_position: None,
				source,
			})
		};
		let component = fields.declaration.component();
		let policy = component
			.initial_policy()
			.map_err(|error| fail(ScopeSelectionInnerError::Contract(error)))?;
		let declaration = component.declaration();
		let scoped = ComponentScopeDeclaration::new(scope, &declaration, ComponentScopeKind::Raw);
		let candidate = CandidateScopePolicy::from_component(
			&scoped,
			ScopePolicy::new(scope.clone(), ScopeOwner::Raw(policy)),
		)
		.map_err(|error| fail(ScopeSelectionInnerError::Candidate(error)))?;
		Ok(Self { candidate })
	}

	pub(crate) fn from_candidate(candidate: CandidateScopePolicy) -> Self {
		Self { candidate }
	}

	/// The initial selection of a document scope: the newest representation that the code
	/// writes as native JSON is the writer and the only readable version, an index is
	/// query-ready when `queries` names it, and every other declared index is maintained.
	pub(crate) fn initial_document<T, C, P>(
		declaration: &DocumentScopeDeclaration<'_, '_, T, C, P>,
		queries: &[crate::kv::document::schema::IndexContractIdentity],
	) -> Result<Self, ScopeSelectionError> {
		let document = declaration.document();
		let capabilities = document.schema().capabilities();
		let writer = newest_json_writer_version(capabilities).ok_or_else(|| {
			ScopeSelectionError::new(SelectionFailureFields {
				scope: Some(declaration.identity()),
				schema_identifier: Some(capabilities.identifier()),
				version: None,
				index_position: None,
				source: ScopeSelectionInnerError::NoJsonWriter,
			})
		})?;
		Self::document(DocumentScopeSelectionFields {
			declaration,
			writer_version: writer,
			retained_reader_versions: vec![writer],
			indexes: document
				.indexes()
				.iter()
				.map(|contract| DocumentIndexSelection {
					contract,
					requirement: if queries.contains(contract.identity()) {
						DocumentIndexRequirement::Ready
					} else {
						DocumentIndexRequirement::Maintained
					},
				})
				.collect(),
		})
	}

	/// The initial policy of a Primitive scope. A schema payload's writer is the newest
	/// representation that the code writes as native JSON and the only readable version.
	pub(crate) fn initial_primitive_policy(
		declaration: &PrimitiveScopeDeclaration<'_, '_>,
		payload: Option<&crate::fleet::payload::scope::retained::RetainedPayloadCode>,
	) -> Result<ScopePolicy, ScopeSelectionError> {
		use crate::fleet::payload::scope::retained::RetainedPayloadCode;
		let scope = declaration.identity();
		let fail = |source, schema| {
			ScopeSelectionError::new(SelectionFailureFields {
				scope: Some(scope),
				schema_identifier: schema,
				version: None,
				index_position: None,
				source,
			})
		};
		let intrinsic = declaration
			.initial_intrinsic_policy()
			.map_err(|error| fail(ScopeSelectionInnerError::Primitive(error), None))?;
		let payload = match payload {
			None => PrimitivePayloadPolicy::None,

			Some(RetainedPayloadCode::Schema(schema)) => {
				let capabilities = schema.capabilities();
				let writer = newest_json_writer_version(capabilities).ok_or_else(|| {
					fail(
						ScopeSelectionInnerError::NoJsonWriter,
						Some(capabilities.identifier()),
					)
				})?;
				PrimitivePayloadPolicy::Schema(selected_schema(SelectedSchemaFields {
					scope,
					capabilities,
					writer_version: writer,
					retained_reader_versions: vec![writer],
				})?)
			}
		};
		let primitive =
			PrimitiveScopePolicy::new(PrimitiveScopePolicyFields { intrinsic, payload })
				.map_err(|error| fail(ScopeSelectionInnerError::Scope(error), None))?;
		Ok(ScopePolicy::new(
			scope.clone(),
			ScopeOwner::Primitive(primitive),
		))
	}

	/// The upgrade target of a document scope: the newest representation that the code writes
	/// as native JSON is the writer; the readable versions are the writer and every installed
	/// readable version that the code still reads; an index is query-ready when `queries`
	/// names it or the installation already has it ready, and every other declared index is
	/// maintained.
	pub(crate) fn target_document<T, C, P>(
		declaration: &DocumentScopeDeclaration<'_, '_, T, C, P>,
		installed: Option<&ScopePolicy>,
		queries: &[crate::kv::document::schema::IndexContractIdentity],
	) -> Result<Self, ScopeSelectionError> {
		let document = declaration.document();
		let capabilities = document.schema().capabilities();
		let writer = newest_json_writer_version(capabilities).ok_or_else(|| {
			ScopeSelectionError::new(SelectionFailureFields {
				scope: Some(declaration.identity()),
				schema_identifier: Some(capabilities.identifier()),
				version: None,
				index_position: None,
				source: ScopeSelectionInnerError::NoJsonWriter,
			})
		})?;
		let installed = installed.and_then(|policy| match policy.owner() {
			ScopeOwner::Document(document) => Some(document),
			_ => None,
		});
		require_writer_not_older(
			declaration.identity(),
			capabilities.identifier(),
			writer,
			installed.map(DocumentScopePolicy::schema),
		)?;
		Self::document(DocumentScopeSelectionFields {
			declaration,
			writer_version: writer,
			// The upgrade converts every record stored in another version to the writer
			// version and then stops requiring that version.
			retained_reader_versions: vec![writer],
			indexes: document
				.indexes()
				.iter()
				.map(|contract| DocumentIndexSelection {
					contract,
					requirement: if queries.contains(contract.identity())
						|| installed
							.is_some_and(|installed| installed.is_index_ready(contract.identity()))
					{
						DocumentIndexRequirement::Ready
					} else {
						DocumentIndexRequirement::Maintained
					},
				})
				.collect(),
		})
	}

	/// The upgrade target of a Primitive scope; a schema payload follows the document rule for
	/// its writer and readable versions.
	pub(crate) fn target_primitive_policy(
		declaration: &PrimitiveScopeDeclaration<'_, '_>,
		payload: Option<&crate::fleet::payload::scope::retained::RetainedPayloadCode>,
		installed: Option<&ScopePolicy>,
	) -> Result<ScopePolicy, ScopeSelectionError> {
		use crate::fleet::payload::scope::retained::RetainedPayloadCode;
		let scope = declaration.identity();
		let fail = |source, schema| {
			ScopeSelectionError::new(SelectionFailureFields {
				scope: Some(scope),
				schema_identifier: schema,
				version: None,
				index_position: None,
				source,
			})
		};
		let intrinsic = declaration
			.initial_intrinsic_policy()
			.map_err(|error| fail(ScopeSelectionInnerError::Primitive(error), None))?;
		let payload = match payload {
			None => PrimitivePayloadPolicy::None,

			Some(RetainedPayloadCode::Schema(schema)) => {
				let capabilities = schema.capabilities();
				let writer = newest_json_writer_version(capabilities).ok_or_else(|| {
					fail(
						ScopeSelectionInnerError::NoJsonWriter,
						Some(capabilities.identifier()),
					)
				})?;
				let installed_payload = installed.and_then(|policy| match policy.owner() {
					ScopeOwner::Primitive(primitive) => match primitive.payload() {
						PrimitivePayloadPolicy::Schema(schema) => Some(schema),
						PrimitivePayloadPolicy::None | PrimitivePayloadPolicy::Opaque => None,
					},
					_ => None,
				});
				require_writer_not_older(
					scope,
					capabilities.identifier(),
					writer,
					installed_payload,
				)?;
				PrimitivePayloadPolicy::Schema(selected_schema(SelectedSchemaFields {
					scope,
					capabilities,
					writer_version: writer,
					retained_reader_versions: vec![writer],
				})?)
			}
		};
		let primitive =
			PrimitiveScopePolicy::new(PrimitiveScopePolicyFields { intrinsic, payload })
				.map_err(|error| fail(ScopeSelectionInnerError::Scope(error), None))?;
		Ok(ScopePolicy::new(
			scope.clone(),
			ScopeOwner::Primitive(primitive),
		))
	}

	pub(crate) fn payload_failure(
		scope: &ScopeIdentity,
		source: crate::fleet::payload::scope::retained::RetainedPayloadError,
	) -> ScopeSelectionError {
		ScopeSelectionError::new(SelectionFailureFields {
			scope: Some(scope),
			schema_identifier: None,
			version: None,
			index_position: None,
			source: ScopeSelectionInnerError::Payload(source),
		})
	}

	/// Borrows the exact logical identity of this requested scope.
	pub(crate) fn identity(&self) -> &ScopeIdentity {
		self.candidate.policy().identity()
	}

	/// The selected policy.
	pub(crate) fn into_policy(self) -> ScopePolicy {
		self.candidate.into_policy()
	}
}

struct SelectedSchemaFields<'a, 'code> {
	scope: &'a ScopeIdentity,
	capabilities: StateSchemaCapabilities<'code>,
	writer_version: u32,
	retained_reader_versions: Vec<u32>,
}

/// Rejects a target writer older than the installed writer of the same schema family.
fn require_writer_not_older(
	scope: &ScopeIdentity,
	schema: &str,
	writer_version: u32,
	installed: Option<&SchemaPolicy>,
) -> Result<(), ScopeSelectionError> {
	match installed {
		Some(installed)
			if installed.identifier() == schema && installed.writer().version > writer_version =>
		{
			Err(ScopeSelectionError::new(SelectionFailureFields {
				scope: Some(scope),
				schema_identifier: Some(schema),
				version: Some(writer_version),
				index_position: None,
				source: ScopeSelectionInnerError::WriterOlderThanInstalled {
					installed: installed.writer().version,
				},
			}))
		}
		_ => Ok(()),
	}
}

/// The newest representation version that `capabilities` writes as native JSON.
pub(crate) fn newest_json_writer_version(capabilities: StateSchemaCapabilities<'_>) -> Option<u32> {
	let mut newest: Option<u32> = None;
	capabilities.for_each_representation(|version, capability| {
		if capability.writable
			&& capability.format == StateRepresentationFormat::Json
			&& newest.is_none_or(|prior| version > prior)
		{
			newest = Some(version);
		}
	});
	newest
}

fn selected_schema(
	fields: SelectedSchemaFields<'_, '_>,
) -> Result<SchemaPolicy, ScopeSelectionError> {
	let fail = |source, version| {
		ScopeSelectionError::new(SelectionFailureFields {
			scope: Some(fields.scope),
			schema_identifier: Some(fields.capabilities.identifier()),
			version,
			index_position: None,
			source,
		})
	};
	let writer = representation(fields.capabilities, fields.writer_version)
		.map_err(|error| fail(error, Some(fields.writer_version)))?;
	let readers = fields
		.retained_reader_versions
		.into_iter()
		.map(|version| {
			representation(fields.capabilities, version).map_err(|error| fail(error, Some(version)))
		})
		.collect::<Result<Vec<_>, _>>()?;
	SchemaPolicy::new(fields.capabilities.identifier().to_owned(), writer, readers)
		.map_err(|error| fail(ScopeSelectionInnerError::Contract(error), None))
}

fn representation(
	capabilities: StateSchemaCapabilities<'_>,
	version: u32,
) -> Result<RepresentationPolicy, ScopeSelectionInnerError> {
	let registered = capabilities
		.representation(version)
		.ok_or(ScopeSelectionInnerError::UnsupportedRepresentation { version })?;
	Ok(RepresentationPolicy {
		version,
		envelope: match registered.format {
			StateRepresentationFormat::Bytes => StateStorageEnvelope::SourceBytes,
			StateRepresentationFormat::Json => StateStorageEnvelope::NativeJson,
		},
	})
}

/// Explicit scope choices for an initial installation or requested replacements.
pub(crate) struct ScopeSelectionsFields {
	/// Checked Document, Raw, and Primitive selections; an initial installation may supply an empty
	/// vector.
	pub(crate) scopes: Vec<ScopeSelection>,
}

/// A parsed set of distinct logical scopes; selection alone installs nothing.
pub(crate) struct ScopeSelections {
	scopes: Vec<ScopeSelection>,
}

impl ScopeSelections {
	/// Rejects repeated logical identities without creating an installation or upgrade.
	pub(crate) fn new(fields: ScopeSelectionsFields) -> Result<Self, ScopeSelectionError> {
		for (index, scope) in fields.scopes.iter().enumerate() {
			if fields.scopes[..index]
				.iter()
				.any(|prior| prior.identity() == scope.identity())
			{
				return Err(ScopeSelectionError::new(SelectionFailureFields {
					scope: Some(scope.identity()),
					schema_identifier: None,
					version: None,
					index_position: None,
					source: ScopeSelectionInnerError::Scope(ScopeError::DuplicateScope { index }),
				}));
			}
		}
		Ok(Self {
			scopes: fields.scopes,
		})
	}

	/// Returns the number of explicitly selected scopes.
	pub(crate) fn len(&self) -> usize {
		self.scopes.len()
	}

	/// Reports whether no scopes were selected.
	pub(crate) fn is_empty(&self) -> bool {
		self.scopes.is_empty()
	}

	/// The policies that opening records when it creates an installation. Opening creates
	/// and records the physical index of each query-ready index in the same transaction.
	pub(crate) fn into_initial_policies(self) -> Result<ScopePolicySet, ScopeSelectionError> {
		ScopePolicySet::new(
			self.scopes
				.into_iter()
				.map(|scope| scope.candidate.into_policy())
				.collect(),
		)
		.map_err(ScopeSelectionError::from_scope)
	}

	pub(crate) fn into_upgrade_candidate_policies(
		self,
	) -> Result<ScopeUpgradeCandidatePolicies, ScopeSelectionError> {
		ScopeUpgradeCandidatePolicies::new(
			self.scopes
				.into_iter()
				.map(|scope| scope.candidate)
				.collect(),
		)
		.map_err(ScopeSelectionError::from_candidate)
	}
}

impl fmt::Debug for ScopeSelection {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("ScopeSelection")
			.field("identity", self.identity())
			.finish_non_exhaustive()
	}
}
impl fmt::Debug for ScopeSelections {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("ScopeSelections")
			.field("scope_count", &self.len())
			.finish()
	}
}
