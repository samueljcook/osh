//! An app scope is one namespace in the installation's app table. [`ScopeIdentity`] names a
//! scope, the typed scopes such as [`DocumentScope`] bind a scope to its code, and
//! [`AppDeclarations`] declares every scope that the app's code uses, with the roles that the
//! code supports in each scope.

// Parsed policies and code-derived support establish no installation, physical association,
// participant authority, query readiness, or transaction admission.

use std::fmt;

use crate::installation::contract::role_binding::{BoundSchemaReader, bind_producer};
use crate::installation::contract::{
	BoundComponentContract, BoundComponentReader, ComponentDeclaration, ComponentPolicy,
	ContractStateReadError, SchemaPolicy, StateContractError, StateStorageEnvelope,
};
use crate::kv::document::schema::{
	BoundDocumentSchema, DocumentBindingError, DocumentDeclarationError, DocumentProjectionError,
	DocumentSchema, IndexContract, IndexContractIdentity, IndexFieldDef, SelectedDocumentIndexes,
};
use crate::records::state_schema::StateSchemaError;
use crate::records::{
	EncodedMetadataUpdate, EncodedRecord, RecordMetadata, RecordNamespace, StoredRecord,
};

pub(crate) mod declarations;
pub(crate) mod managed;
pub(crate) mod payload;
pub(crate) mod payload_policy;
pub(crate) mod primitive;
pub(crate) use payload_policy::{
	PrimitivePayloadPolicy, PrimitiveScopePolicy, PrimitiveScopePolicyFields,
};
pub(crate) mod raw;
pub(crate) mod selection;
mod support;
pub(crate) mod typed;
pub use declarations::{
	AppCacheDeclarationFields, AppDeclarationError, AppDeclarationErrorKind, AppDeclarations,
	AppDeclarationsFields, AppDocumentDeclaration, AppDocumentDeclarationFields,
	AppPrimitiveDeclaration, AppPrimitiveDeclarationFields, AppRawDeclaration,
	AppRawDeclarationFields, AppTopicDeclarationFields, DocumentUseIntent, InitialUse,
	PayloadUseIntent, PrimitiveUseIntent, QueryUseIntent, RawUseIntent, RoleUse,
};
pub(crate) use primitive::{PrimitiveRole, PrimitiveScopeSupport, PrimitiveScopeSupportSet};
pub(crate) use raw::{RawRole, RawRoleSupport, RawRoleSupportSet, RawScopeDeclaration};
pub use selection::{ScopeSelectionError, ScopeSelectionErrorKind};
pub use typed::{CacheScope, DocumentScope, DocumentScopeFields, PayloadScopeFields, TopicScope};
pub(crate) mod wire;
pub(crate) use support::{DocumentRole, DocumentRoleSupport, DocumentRoleSupportSet};

/// The storage part of a scope's stored identity. Every app scope has the same storage part;
/// a scope is identified by its namespace.
#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct StorageIdentity(Vec<u8>);

/// A stored storage identity cannot be empty.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum StorageIdentityError {
	#[error("logical storage identity must not be empty")]
	Empty,
}

/// The storage part of every app scope's identity.
const APP_STORAGE_IDENTITY: &[u8] = b"paranoid.app";

impl StorageIdentity {
	pub(crate) fn new(bytes: Vec<u8>) -> Result<Self, StorageIdentityError> {
		if bytes.is_empty() {
			return Err(StorageIdentityError::Empty);
		}
		Ok(Self(bytes))
	}
	pub(crate) fn app() -> Self {
		Self(APP_STORAGE_IDENTITY.to_vec())
	}
	pub(crate) fn as_bytes(&self) -> &[u8] {
		&self.0
	}
}

impl fmt::Debug for StorageIdentity {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("StorageIdentity")
			.field("byte_len", &self.0.len())
			.finish()
	}
}

/// The identity of one app scope: its namespace in the installation's app table.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ScopeIdentity {
	storage: StorageIdentity,
	namespace: RecordNamespace,
}

/// The inputs of a scope identity.
pub struct ScopeIdentityFields {
	/// The scope's namespace, which every other scope's namespace must differ from.
	pub namespace: RecordNamespace,
}

impl ScopeIdentity {
	/// Builds a scope identity without database work.
	pub fn new(fields: ScopeIdentityFields) -> Self {
		Self {
			storage: StorageIdentity::app(),
			namespace: fields.namespace,
		}
	}
	/// Rebuilds a stored identity.
	pub(crate) fn from_stored(storage: StorageIdentity, namespace: RecordNamespace) -> Self {
		Self { storage, namespace }
	}
	pub(crate) fn storage(&self) -> &StorageIdentity {
		&self.storage
	}
	/// Borrows the scope's namespace.
	pub fn namespace(&self) -> &RecordNamespace {
		&self.namespace
	}
}

/// Structural shape of an actual index contract, with no projection function or readiness claim.
pub(crate) struct IndexContractDescriptor(IndexContract);

impl IndexContractDescriptor {
	pub(crate) fn from_declaration(contract: &IndexContract) -> Self {
		Self(
			IndexContract::new(
				contract.identity().clone(),
				contract
					.fields()
					.iter()
					.map(|field| {
						IndexFieldDef::new(field.name().to_owned(), field.scalar_type())
							.expect("actual index field declaration is parsed")
					})
					.collect(),
			)
			.expect("actual index contract has unique fields"),
		)
	}
	pub(crate) fn identity(&self) -> &IndexContractIdentity {
		self.0.identity()
	}
	pub(crate) fn fields(&self) -> &[IndexFieldDef] {
		self.0.fields()
	}
	pub(crate) fn coverage_field_key(&self) -> String {
		self.0.coverage_field_key()
	}
	pub(crate) fn field_key(&self, field_index: usize) -> Result<String, DocumentDeclarationError> {
		self.0.field_key(field_index)
	}
	pub(crate) fn matches_descriptor(&self, other: &Self) -> bool {
		self.matches_declaration(&other.0)
	}
	pub(crate) fn matches_declaration(&self, actual: &IndexContract) -> bool {
		self.identity() == actual.identity()
			&& self.fields().len() == actual.fields().len()
			&& self
				.fields()
				.iter()
				.zip(actual.fields())
				.all(|(a, b)| a.name() == b.name() && a.scalar_type() == b.scalar_type())
	}
}

impl Clone for IndexContractDescriptor {
	fn clone(&self) -> Self {
		Self::from_declaration(&self.0)
	}
}

impl fmt::Debug for IndexContractDescriptor {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		self.0.fmt(f)
	}
}

#[derive(Clone)]
pub(crate) struct DocumentScopePolicy {
	schema: SchemaPolicy,
	indexes: Vec<DocumentIndexPolicy>,
}

#[derive(Clone)]
struct DocumentIndexPolicy {
	descriptor: IndexContractDescriptor,
	readiness: DocumentIndexReadiness,
}

#[derive(Clone, Copy)]
enum DocumentIndexReadiness {
	Maintained,
	Ready { position: usize },
}

impl DocumentScopePolicy {
	pub(crate) fn new(
		schema: SchemaPolicy,
		maintenance_indexes: Vec<IndexContractDescriptor>,
	) -> Result<Self, ScopeError> {
		Self::new_with_ready_indexes(schema, maintenance_indexes, Vec::new())
	}
	pub(crate) fn new_with_ready_indexes(
		schema: SchemaPolicy,
		maintenance_indexes: Vec<IndexContractDescriptor>,
		ready_indexes: Vec<IndexContractIdentity>,
	) -> Result<Self, ScopeError> {
		require_identifier(schema.identifier())?;
		if schema.writer().envelope != StateStorageEnvelope::NativeJson {
			return Err(ScopeError::NativeWriterRequired);
		}
		check_indexes(&maintenance_indexes)?;
		for (index, ready) in ready_indexes.iter().enumerate() {
			if ready_indexes[..index].contains(ready) {
				return Err(ScopeError::DuplicateReadyIndex { index });
			}
			if !maintenance_indexes
				.iter()
				.any(|maintained| maintained.identity() == ready)
			{
				return Err(ScopeError::ReadyIndexNotMaintained { index });
			}
		}
		let indexes = maintenance_indexes
			.into_iter()
			.map(|descriptor| {
				let readiness = match ready_indexes
					.iter()
					.position(|ready| ready == descriptor.identity())
				{
					Some(position) => DocumentIndexReadiness::Ready { position },
					None => DocumentIndexReadiness::Maintained,
				};
				DocumentIndexPolicy {
					descriptor,
					readiness,
				}
			})
			.collect();
		Ok(Self { schema, indexes })
	}
	pub(crate) fn schema(&self) -> &SchemaPolicy {
		&self.schema
	}
	pub(crate) fn maintenance_indexes(
		&self,
	) -> impl ExactSizeIterator<Item = &IndexContractDescriptor> {
		self.indexes.iter().map(|index| &index.descriptor)
	}
	pub(crate) fn ready_indexes(&self) -> impl ExactSizeIterator<Item = &IndexContractIdentity> {
		let mut ready: Vec<_> = self
			.indexes
			.iter()
			.filter_map(|index| match index.readiness {
				DocumentIndexReadiness::Maintained => None,
				DocumentIndexReadiness::Ready { position } => {
					Some((position, index.descriptor.identity()))
				}
			})
			.collect();
		// Stored policy bytes retain readiness order independently of maintenance order.
		ready.sort_unstable_by_key(|(position, _)| *position);
		ready.into_iter().map(|(_, identity)| identity)
	}
	pub(crate) fn is_index_ready(&self, identity: &IndexContractIdentity) -> bool {
		self.indexes.iter().any(|index| {
			index.descriptor.identity() == identity
				&& matches!(index.readiness, DocumentIndexReadiness::Ready { .. })
		})
	}
	pub(crate) fn with_ready_index(&self, actual: &IndexContract) -> Result<Self, ScopeError> {
		let mut policy = self.clone();
		let position = self
			.indexes
			.iter()
			.filter(|index| matches!(index.readiness, DocumentIndexReadiness::Ready { .. }))
			.count();
		let index = policy
			.indexes
			.iter_mut()
			.find(|index| index.descriptor.identity() == actual.identity())
			.ok_or(ScopeError::QueryIndexNotMaintained)?;
		if !index.descriptor.matches_declaration(actual) {
			return Err(ScopeError::QueryIndexShapeMismatch);
		}
		match index.readiness {
			DocumentIndexReadiness::Maintained => {
				index.readiness = DocumentIndexReadiness::Ready { position }
			}
			DocumentIndexReadiness::Ready { .. } => {}
		}
		Ok(policy)
	}
	/// Exact maintained descriptor and Ready meaning, independent from schema reader selection.
	pub(crate) fn matches_index_policy(&self, other: &Self) -> bool {
		self.indexes.len() == other.indexes.len()
			&& self.indexes.iter().all(|index| {
				other.indexes.iter().any(|other| {
					index.descriptor.matches_descriptor(&other.descriptor)
						&& matches!(
							(index.readiness, other.readiness),
							(
								DocumentIndexReadiness::Maintained,
								DocumentIndexReadiness::Maintained
							) | (
								DocumentIndexReadiness::Ready { .. },
								DocumentIndexReadiness::Ready { .. }
							)
						)
				})
			})
	}
	pub(crate) fn require_ready_index(&self, actual: &IndexContract) -> Result<(), ScopeError> {
		let index = self
			.indexes
			.iter()
			.find(|index| index.descriptor.identity() == actual.identity())
			.ok_or(ScopeError::QueryIndexNotMaintained)?;
		if !index.descriptor.matches_declaration(actual) {
			return Err(ScopeError::QueryIndexShapeMismatch);
		}
		match index.readiness {
			DocumentIndexReadiness::Maintained => Err(ScopeError::QueryIndexNotReady),
			DocumentIndexReadiness::Ready { .. } => Ok(()),
		}
	}
}

impl fmt::Debug for DocumentScopePolicy {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("DocumentScopePolicy")
			.field("schema", &self.schema)
			.field("index_count", &self.indexes.len())
			.field(
				"ready_index_count",
				&self
					.indexes
					.iter()
					.filter(|index| matches!(index.readiness, DocumentIndexReadiness::Ready { .. }))
					.count(),
			)
			.finish()
	}
}

#[derive(Clone, Debug)]
pub(crate) enum ScopeOwner {
	Raw(ComponentPolicy),
	Document(DocumentScopePolicy),
	Primitive(PrimitiveScopePolicy),
	Closed,
}

#[derive(Clone, Debug)]
pub(crate) struct ScopePolicy {
	identity: ScopeIdentity,
	owner: ScopeOwner,
}

impl ScopePolicy {
	pub(crate) fn new(identity: ScopeIdentity, owner: ScopeOwner) -> Self {
		Self { identity, owner }
	}
	pub(crate) fn identity(&self) -> &ScopeIdentity {
		&self.identity
	}
	pub(crate) fn owner(&self) -> &ScopeOwner {
		&self.owner
	}
	/// Full parsed meaning, including owner kind, settings and ordered index fields.
	pub(crate) fn matches_policy(&self, other: &Self) -> bool {
		if self.identity != other.identity {
			return false;
		}
		match (&self.owner, &other.owner) {
			(ScopeOwner::Raw(policy), ScopeOwner::Raw(other)) => policy.matches_policy(other),
			(ScopeOwner::Primitive(policy), ScopeOwner::Primitive(other)) => {
				policy.matches_policy(other)
			}
			(ScopeOwner::Closed, ScopeOwner::Closed) => true,
			(ScopeOwner::Document(document), ScopeOwner::Document(other)) => {
				document.schema.matches_policy(&other.schema)
					&& document.matches_index_policy(other)
			}
			_ => false,
		}
	}
}

#[derive(Clone)]
pub(crate) struct ScopePolicySet {
	policies: Vec<ScopePolicy>,
}

impl ScopePolicySet {
	pub(crate) fn new(policies: Vec<ScopePolicy>) -> Result<Self, ScopeError> {
		for (index, policy) in policies.iter().enumerate() {
			if policies[..index]
				.iter()
				.any(|prior| prior.identity == policy.identity)
			{
				return Err(ScopeError::DuplicateScope { index });
			}
		}
		Ok(Self { policies })
	}
	pub(crate) fn policies(&self) -> &[ScopePolicy] {
		&self.policies
	}
	pub(crate) fn matches_policies(&self, other: &Self) -> bool {
		self.policies.len() == other.policies.len()
			&& self.policies.iter().all(|policy| {
				other
					.policies
					.iter()
					.any(|other| policy.matches_policy(other))
			})
	}
	/// Applies explicit nonempty replacements; unlisted scopes retain their complete policy.
	pub(crate) fn with_replacements(&self, replacements: &Self) -> Result<Self, ScopeError> {
		if replacements.policies.is_empty() {
			return Err(ScopeError::EmptyScopeReplacement);
		}
		let mut policies: Vec<_> = self
			.policies
			.iter()
			.map(|current| {
				replacements
					.policies
					.iter()
					.find(|replacement| replacement.identity() == current.identity())
					.unwrap_or(current)
					.clone()
			})
			.collect();
		policies.extend(
			replacements
				.policies
				.iter()
				.filter(|replacement| {
					!self
						.policies
						.iter()
						.any(|current| current.identity() == replacement.identity())
				})
				.cloned(),
		);
		Self::new(policies)
	}
	pub(crate) fn lookup(&self, identity: &ScopeIdentity) -> Result<&ScopePolicy, ScopeError> {
		self.policies
			.iter()
			.find(|policy| policy.identity == *identity)
			.ok_or(ScopeError::ScopeNotInstalled)
	}
	/// Replaces one existing parsed scope while preserving every other scope and its order.
	pub(crate) fn with_replaced_scope(
		&self,
		replacement: &ScopePolicy,
	) -> Result<Self, ScopeError> {
		self.lookup(replacement.identity())?;
		Self::new(
			self.policies
				.iter()
				.map(|current| {
					if current.identity() == replacement.identity() {
						replacement.clone()
					} else {
						current.clone()
					}
				})
				.collect(),
		)
	}
}

impl fmt::Debug for ScopePolicySet {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("ScopePolicySet")
			.field("scope_count", &self.policies.len())
			.finish()
	}
}

/// One document scope's identity and parsed document code.
pub(crate) struct DocumentScopeDeclaration<'a, 'd, T, C, P> {
	identity: &'a ScopeIdentity,
	document: DocumentSchema<'d, T, C, P>,
}

pub(crate) struct DocumentScopeDeclarationFields<'a, 'd, T, C, P> {
	pub(crate) identity: &'a ScopeIdentity,
	pub(crate) document: DocumentSchema<'d, T, C, P>,
}

impl<'a, 'd, T, C, P> DocumentScopeDeclaration<'a, 'd, T, C, P> {
	pub(crate) fn new(fields: DocumentScopeDeclarationFields<'a, 'd, T, C, P>) -> Self {
		Self {
			identity: fields.identity,
			document: fields.document,
		}
	}
	pub(crate) fn identity(&self) -> &ScopeIdentity {
		self.identity
	}
	pub(crate) fn document(&self) -> &DocumentSchema<'d, T, C, P> {
		&self.document
	}
	fn policy<'b>(&self, policy: &'b ScopePolicy) -> Result<&'b DocumentScopePolicy, ScopeError> {
		if *self.identity != policy.identity {
			return Err(ScopeError::ScopeMismatch);
		}
		match &policy.owner {
			ScopeOwner::Document(document) => Ok(document),
			_ => Err(ScopeError::WrongOwner),
		}
	}
	pub(crate) fn bind_reader<'b>(
		&'b self,
		policy: &'b ScopePolicy,
	) -> Result<BoundDocumentReader<'b, 'a, 'd, T, C, P>, ScopeError> {
		let policy = self.policy(policy)?;
		let schema = self.document.schema();
		let reader = BoundSchemaReader::bind(schema.capabilities(), schema, policy.schema())?;
		Ok(BoundDocumentReader {
			declaration: self,
			reader,
		})
	}
	fn required_index_identities(
		&self,
		policy: &DocumentScopePolicy,
	) -> Result<Vec<IndexContractIdentity>, ScopeError> {
		for (index, required) in policy.maintenance_indexes().enumerate() {
			let actual = self
				.document
				.indexes()
				.iter()
				.find(|actual| actual.identity() == required.identity())
				.ok_or(ScopeError::MissingIndex { index })?;
			if !required.matches_declaration(actual) {
				return Err(ScopeError::IndexShapeMismatch { index });
			}
		}
		Ok(policy
			.maintenance_indexes()
			.map(|index| index.identity().clone())
			.collect())
	}
	pub(crate) fn bind_producer<'b>(
		&'b self,
		policy: &'b ScopePolicy,
	) -> Result<BoundDocumentProducer<'b, 'a, 'd, T, C, P>, ScopeError> {
		let policy = self.policy(policy)?;
		let identities = self.required_index_identities(policy)?;
		let schema = self.document.schema();
		let bound = bind_producer(schema.capabilities(), schema, policy.schema())?;
		let producer = self.document.bind(bound, &identities)?;
		Ok(BoundDocumentProducer {
			declaration: self,
			producer,
		})
	}
	pub(crate) fn bind_maintenance<'b>(
		&'b self,
		policy: &'b ScopePolicy,
	) -> Result<BoundDocumentMaintenance<'b, 'd, T, C, P>, ScopeError> {
		let policy = self.policy(policy)?;
		let schema = self.document.schema();
		let reader = BoundSchemaReader::bind(schema.capabilities(), schema, policy.schema())?;
		let identities = self.required_index_identities(policy)?;
		let indexes = self.document.select_indexes(&identities)?;
		for representation in policy.schema.required_readers() {
			if representation.envelope != StateStorageEnvelope::NativeJson {
				return Err(ScopeError::NativeMaintenanceRequired);
			}
			indexes.require_implementations(representation.version)?;
		}
		Ok(BoundDocumentMaintenance { reader, indexes })
	}

	/// Readiness verification requires selected-version projectors and no encoder.
	pub(crate) fn bind_selected_writer_projection<'b>(
		&'b self,
		policy: &'b ScopePolicy,
	) -> Result<BoundDocumentMaintenance<'b, 'd, T, C, P>, ScopeError> {
		let policy = self.policy(policy)?;
		if policy.schema().writer().envelope != StateStorageEnvelope::NativeJson {
			return Err(ScopeError::NativeMaintenanceRequired);
		}
		let schema = self.document.schema();
		let reader = BoundSchemaReader::bind(schema.capabilities(), schema, policy.schema())?;
		let identities = self.required_index_identities(policy)?;
		let indexes = self.document.select_indexes(&identities)?;
		indexes.require_implementations(policy.schema().writer().version)?;
		Ok(BoundDocumentMaintenance { reader, indexes })
	}
	pub(crate) fn support(&self, role: DocumentRole) -> Result<DocumentRoleSupport, ScopeError> {
		DocumentRoleSupport::from_declaration(self, role)
	}
	pub(crate) fn support_reader_queries(
		&self,
		indexes: &[&IndexContract],
	) -> Result<DocumentRoleSupport, ScopeError> {
		let mut support = self.support(DocumentRole::Reader)?;
		for actual in indexes {
			self.document.require_index_declaration(actual)?;
			support
				.query_indexes
				.push(IndexContractDescriptor::from_declaration(actual));
		}
		check_indexes(&support.query_indexes)?;
		Ok(support)
	}
	pub(crate) fn bind_query_reader<'b>(
		&'b self,
		policy: &'b ScopePolicy,
		index: &'b IndexContract,
	) -> Result<BoundDocumentQueryReader<'b, 'a, 'd, T, C, P>, ScopeError> {
		self.document.require_index_declaration(index)?;
		self.policy(policy)?.require_ready_index(index)?;
		let point_reader = self.bind_reader(policy)?;
		Ok(BoundDocumentQueryReader {
			reader: point_reader.reader,
			declaration: self,
			index,
		})
	}
}

/// An exact query decoder/index binding grants no point-reader, encoder, or projector access.
pub(crate) struct BoundDocumentQueryReader<'b, 'a, 'd, T, C, P> {
	declaration: &'b DocumentScopeDeclaration<'a, 'd, T, C, P>,
	index: &'b IndexContract,
	reader: BoundSchemaReader<'b, T, C>,
}
impl<T, C, P> BoundDocumentQueryReader<'_, '_, '_, T, C, P> {
	pub(crate) fn require_query(
		&self,
		query: &crate::kv::document::query::ScopedDocumentIndexQuery<'_, '_, '_, T, C, P>,
	) -> Result<(), ScopeError> {
		require_document_declaration(self.declaration, query.declaration())?;
		if !std::ptr::eq(self.index, query.index()) {
			return Err(ScopeError::IndexBinding(
				DocumentBindingError::DifferentIndexDeclaration,
			));
		}
		Ok(())
	}
	pub(crate) fn decode_document(&self, bytes: &[u8]) -> Result<(u32, T), ScopeReadError<C>> {
		self.reader.decode_document(bytes).map_err(ScopeReadError)
	}
}

pub(crate) struct BoundDocumentReader<'b, 'a, 'd, T, C, P> {
	declaration: &'b DocumentScopeDeclaration<'a, 'd, T, C, P>,
	reader: BoundSchemaReader<'b, T, C>,
}

impl<T, C, P> BoundDocumentReader<'_, '_, '_, T, C, P> {
	pub(crate) fn require_matching_producer(
		&self,
		producer: &BoundDocumentProducer<'_, '_, '_, T, C, P>,
	) -> Result<(), ScopeError> {
		require_document_declaration(self.declaration, producer.declaration)
	}

	pub(crate) fn decode_document(&self, bytes: &[u8]) -> Result<(u32, T), ScopeReadError<C>> {
		self.reader.decode_document(bytes).map_err(ScopeReadError)
	}
}

pub(crate) struct BoundDocumentProducer<'b, 'a, 'd, T, C, P> {
	declaration: &'b DocumentScopeDeclaration<'a, 'd, T, C, P>,
	producer: BoundDocumentSchema<'b, 'd, T, C, P>,
}

impl<T, C, P> BoundDocumentProducer<'_, '_, '_, T, C, P> {
	pub(crate) fn encode_record(
		&self,
		value: &T,
		metadata: RecordMetadata,
	) -> Result<EncodedRecord, DocumentProjectionError<C, P>> {
		self.producer.encode_record(value, metadata)
	}
}

pub(crate) struct BoundDocumentMaintenance<'b, 'd, T, C, P> {
	reader: BoundSchemaReader<'b, T, C>,
	indexes: SelectedDocumentIndexes<'b, 'd, T, C, P>,
}

impl<T, C, P> BoundDocumentMaintenance<'_, '_, T, C, P> {
	pub(crate) fn prepare_selected_writer_projection(
		&self,
		record: &StoredRecord,
	) -> Result<EncodedMetadataUpdate, DocumentProjectionError<C, P>> {
		self.indexes
			.prepare_selected_writer_projection(&self.reader, record)
	}
	pub(crate) fn prepare_metadata_update(
		&self,
		record: &StoredRecord,
		metadata: RecordMetadata,
	) -> Result<EncodedMetadataUpdate, DocumentProjectionError<C, P>> {
		self.indexes
			.prepare_metadata_update(&self.reader, record, metadata)
	}
}

fn require_document_declaration<T, C, P>(
	declared: &DocumentScopeDeclaration<'_, '_, T, C, P>,
	actual: &DocumentScopeDeclaration<'_, '_, T, C, P>,
) -> Result<(), ScopeError> {
	if !std::ptr::eq(declared, actual) {
		return Err(ScopeError::DifferentDocumentDeclaration);
	}
	Ok(())
}

/// Raw and primitive ownership bind actual component policy, without arbitrary settings DTOs.
pub(crate) struct ComponentScopeDeclaration<'a, 'd, T> {
	identity: &'a ScopeIdentity,
	component: &'a ComponentDeclaration<'d, T>,
	kind: ComponentScopeKind,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ComponentScopeKind {
	Raw,
	Primitive,
}
impl<'a, 'd, T> ComponentScopeDeclaration<'a, 'd, T> {
	pub(crate) fn new(
		identity: &'a ScopeIdentity,
		component: &'a ComponentDeclaration<'d, T>,
		kind: ComponentScopeKind,
	) -> Self {
		Self {
			identity,
			component,
			kind,
		}
	}
	pub(crate) fn identity(&self) -> &ScopeIdentity {
		self.identity
	}
	pub(crate) fn kind(&self) -> ComponentScopeKind {
		self.kind
	}
	pub(crate) fn bind<'b>(
		&self,
		actual_scope: &ScopeIdentity,
		policy: &'b ScopePolicy,
	) -> Result<BoundComponentContract<'b, 'd, T>, ScopeError>
	where
		'a: 'b,
	{
		Ok(self
			.component
			.bind(self.component_policy(actual_scope, policy)?)?)
	}
	/// Checks exact scope/owner and binds actual semantics, readers and settings without encoders.
	pub(crate) fn bind_reader<'b>(
		&self,
		actual_scope: &ScopeIdentity,
		policy: &'b ScopePolicy,
	) -> Result<BoundComponentReader<'b, 'd, T>, ScopeError>
	where
		'a: 'b,
	{
		Ok(self
			.component
			.bind_reader(self.component_policy(actual_scope, policy)?)?)
	}
	fn component_policy<'b>(
		&self,
		actual_scope: &ScopeIdentity,
		policy: &'b ScopePolicy,
	) -> Result<&'b ComponentPolicy, ScopeError> {
		if !std::ptr::eq(self.identity, actual_scope) {
			return Err(ScopeError::DifferentScopeDeclaration);
		}
		if policy.identity != *self.identity {
			return Err(ScopeError::ScopeMismatch);
		}
		match (&policy.owner, self.kind) {
			(ScopeOwner::Raw(component), ComponentScopeKind::Raw) => Ok(component),
			(ScopeOwner::Primitive(primitive), ComponentScopeKind::Primitive) => {
				Ok(primitive.intrinsic())
			}
			_ => Err(ScopeError::WrongOwner),
		}
	}
}

pub(crate) struct ScopeReadError<E>(pub(crate) ContractStateReadError<E>);
impl<E> ScopeReadError<E> {
	pub(crate) fn representation_error(&self) -> Option<&StateSchemaError<E>> {
		match &self.0 {
			ContractStateReadError::Representation(source) => Some(source),
			_ => None,
		}
	}
}
impl<E> fmt::Debug for ScopeReadError<E> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("ScopeReadRejected")
	}
}
impl<E> fmt::Display for ScopeReadError<E> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("scoped document reader rejected its stored representation")
	}
}
impl<E: std::error::Error + 'static> std::error::Error for ScopeReadError<E> {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(&self.0)
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ScopeError {
	#[error("scope replacements must contain an explicitly selected policy")]
	EmptyScopeReplacement,
	#[error("ready index {index} is repeated")]
	DuplicateReadyIndex { index: usize },
	#[error("ready index {index} is not maintained")]
	ReadyIndexNotMaintained { index: usize },
	#[error("query index is not maintained")]
	QueryIndexNotMaintained,
	#[error("query index has another structural declaration")]
	QueryIndexShapeMismatch,
	#[error("query index is not ready")]
	QueryIndexNotReady,
	#[error("query index use is not registered")]
	QueryUseNotRegistered,
	#[error("logical storage identity must not be empty")]
	EmptyStorageIdentity,
	#[error("scope is not installed")]
	ScopeNotInstalled,
	#[error("scoped role is not registered")]
	RoleNotRegistered,
	#[error("scoped role use {index} is repeated")]
	DuplicateRoleSupport { index: usize },
	#[error("scope policy belongs to another logical scope")]
	ScopeMismatch,
	#[error("scope identity uses a different declaration")]
	DifferentScopeDeclaration,
	#[error("document role uses a different declaration")]
	DifferentDocumentDeclaration,
	#[error("raw role uses a different primitive declaration")]
	DifferentRawDeclaration,
	#[error("scope {index} is repeated")]
	DuplicateScope { index: usize },
	#[error("scope owner does not permit this role")]
	WrongOwner,
	#[error("this primitive family does not allow an embedded payload policy")]
	PrimitivePayloadNotAllowed,
	#[error("this Primitive family requires explicit opaque or structured payload ownership")]
	PrimitivePayloadRequired,
	#[error("this payload role requires explicit structured schema ownership")]
	PayloadSchemaRequired,
	#[error("payload policy belongs to another schema family")]
	PayloadSchemaMismatch,
	#[error("payload role uses another scope or schema declaration")]
	DifferentPayloadDeclaration,
	#[error("payload binding belongs to another full current policy")]
	DifferentPayloadPolicy,
	#[error("scoped identifier must be nonempty and contain no NUL")]
	InvalidIdentifier,
	#[error("typed writers require a native JSON writer")]
	NativeWriterRequired,
	#[error("projection-only maintenance requires native JSON records")]
	NativeMaintenanceRequired,
	#[error("required index {index} is repeated")]
	DuplicateIndex { index: usize },
	#[error("required index {index} has no declaration")]
	MissingIndex { index: usize },
	#[error("required index {index} has a different structural declaration")]
	IndexShapeMismatch { index: usize },
	#[error("scope schema policy was rejected")]
	Contract(#[from] StateContractError),
	#[error("scope index declaration was rejected")]
	IndexDeclaration(#[from] DocumentDeclarationError),
	#[error("scope index selection was rejected")]
	IndexBinding(#[from] DocumentBindingError),
	#[error("scope support representation {version} is repeated")]
	DuplicateRepresentation { version: u32 },
	#[error("scope support representation version must be positive")]
	RepresentationVersionIsZero,
	#[error("scope role support violates its representation domain")]
	InvalidRoleSupport,
	#[error("primitive reader/writer contract is unsupported")]
	PrimitiveContractUnsupported {
		#[source]
		source: Box<crate::runtime::participation::state::ParticipantStateError>,
	},
	#[error("primitive Writer role requires a registered encoder")]
	PrimitiveWriterUnavailable,
	#[error("scoped raw policy does not select a supported schema-free opaque-byte contract")]
	RawContractUnsupported,
	#[error("scoped role does not support required representation {version}")]
	UnsupportedRepresentation { version: u32 },
}

fn require_identifier(identifier: &str) -> Result<(), ScopeError> {
	if identifier.is_empty() || identifier.contains('\0') {
		return Err(ScopeError::InvalidIdentifier);
	}
	Ok(())
}
fn check_indexes(indexes: &[IndexContractDescriptor]) -> Result<(), ScopeError> {
	for (index, current) in indexes.iter().enumerate() {
		if indexes[..index]
			.iter()
			.any(|prior| prior.identity() == current.identity())
		{
			return Err(ScopeError::DuplicateIndex { index });
		}
	}
	Ok(())
}
