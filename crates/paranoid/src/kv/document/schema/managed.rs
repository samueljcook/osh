//! Synchronous fixed preparations using runtime-owned heterogeneous actual document code.
//!
//! These values grant no work, record, transaction, or readiness authority. A fixed executor
//! must bind current installation context and hold actual record authority before applying them.

use std::{any::Any, fmt, sync::Arc};

use crate::installation::contract::{
	ContractStateReadError, RepresentationPolicy, StateContractError,
};
use crate::records::state_schema::StateSchemaError;
use crate::records::{EncodedMetadataUpdate, EncodedRecord, RecordIdentity, StoredRecord};
use crate::scope::{
	DocumentRole, DocumentRoleSupport, DocumentScopeDeclaration, DocumentScopeDeclarationFields,
	ScopeError, ScopeIdentity, ScopeOwner, ScopePolicy,
};
use crate::upgrade::policy::scope::{
	CandidateDocumentScopePolicy, ScopedUpgradeCompatibilityError,
};

use super::owned::DocumentCode;
use super::{DocumentProjectionError, IndexContract, IndexContractIdentity};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::kv) enum ManagedDocumentOperation {
	DeclareRole(DocumentRole),
	DeclareQuerySupport,
	BindFinalCandidate,
	CheckWriterStage,
	SelectedWriterConversion,
	SelectedWriterProjection,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::kv) enum ManagedDocumentFailureStage {
	RecordIdentity,
	Binding,
	RetainedDecode,
	RetainedProjection,
	EncodingAndProjection,
}

enum ManagedDocumentCause {
	Scope(ScopeError),
	Candidate(ScopedUpgradeCompatibilityError),
	App(ManagedAppCause),
}

/// A failure of the typed document code, kept as its own typed value. The library's wrapper
/// types are generic in the app's codec and projector error types, which have no `Error`
/// bound, so `locate_fn` finds the part that the app can name by its own types.
struct ManagedAppCause {
	source: Box<dyn Any + Send + Sync>,
	locate_fn: fn(&(dyn Any + Send + Sync)) -> Option<ManagedAppPart<'_>>,
}

/// The part of a typed document code failure that the app's own types name.
enum ManagedAppPart<'a> {
	/// A `StateSchemaError` of the app's codec error type: the stored or encoded format failed.
	Format(&'a (dyn Any + Send + Sync)),
	/// A value of the app's projector error type.
	Projector(&'a (dyn Any + Send + Sync)),
}

impl ManagedAppCause {
	fn read<C: Send + Sync + 'static>(source: ContractStateReadError<C>) -> Self {
		Self {
			source: Box::new(source),
			locate_fn: |error| match error.downcast_ref::<ContractStateReadError<C>>()? {
				ContractStateReadError::Representation(format) => {
					Some(ManagedAppPart::Format(format))
				}
				ContractStateReadError::Policy(_) => None,
			},
		}
	}
	fn projection<C: Send + Sync + 'static, P: Send + Sync + 'static>(
		source: DocumentProjectionError<C, P>,
	) -> Self {
		Self {
			source: Box::new(source),
			locate_fn: |error| match error.downcast_ref::<DocumentProjectionError<C, P>>()? {
				DocumentProjectionError::Codec(format)
				| DocumentProjectionError::Read(ContractStateReadError::Representation(format)) => {
					Some(ManagedAppPart::Format(format))
				}
				DocumentProjectionError::Projector { error, .. } => {
					Some(ManagedAppPart::Projector(error))
				}
				_ => None,
			},
		}
	}
	fn part(&self) -> Option<ManagedAppPart<'_>> {
		(self.locate_fn)(self.source.as_ref())
	}
}

/// Exact typed application causes remain inspectable without formatting their contents.
pub(crate) struct ManagedDocumentError {
	context: Box<ManagedDocumentFailureContext>,
	source: ManagedDocumentCause,
}

struct ManagedDocumentFailureContext {
	operation: ManagedDocumentOperation,
	stage: ManagedDocumentFailureStage,
	selected_writer: Option<RepresentationPolicy>,
	record_identity: Option<RecordIdentity>,
	stored_version: Option<u32>,
}

impl ManagedDocumentError {
	pub(crate) fn record_identity(&self) -> Option<&RecordIdentity> {
		self.context.record_identity.as_ref()
	}
	fn at_record(mut self, record: ManagedDocumentRecord<'_>, version: Option<u32>) -> Self {
		self.context.record_identity = Some(record.identity.clone());
		self.context.stored_version = version;
		self
	}
	/// The format failure of the app's codec error type `C`, when the app's decoder or
	/// encoder, or the stored format, failed.
	pub(crate) fn format_error<C: 'static>(&self) -> Option<&StateSchemaError<C>> {
		match &self.source {
			ManagedDocumentCause::App(source) => match source.part()? {
				ManagedAppPart::Format(format) => format.downcast_ref(),
				ManagedAppPart::Projector(_) => None,
			},
			ManagedDocumentCause::Scope(_) | ManagedDocumentCause::Candidate(_) => None,
		}
	}
	/// The error of the app's projector error type `P`, when the app's projector failed.
	pub(crate) fn projector_error<P: 'static>(&self) -> Option<&P> {
		match &self.source {
			ManagedDocumentCause::App(source) => match source.part()? {
				ManagedAppPart::Projector(error) => error.downcast_ref(),
				ManagedAppPart::Format(_) => None,
			},
			ManagedDocumentCause::Scope(_) | ManagedDocumentCause::Candidate(_) => None,
		}
	}
}

impl fmt::Debug for ManagedDocumentError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("ManagedDocumentError")
			.field("operation", &self.context.operation)
			.field("stage", &self.context.stage)
			.field("selected_writer", &self.context.selected_writer)
			.field("stored_version", &self.context.stored_version)
			.finish_non_exhaustive()
	}
}
impl fmt::Display for ManagedDocumentError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(match self.context.stage {
			ManagedDocumentFailureStage::RecordIdentity => {
				"a stored document does not belong to the document scope"
			}
			ManagedDocumentFailureStage::Binding => {
				"the document code cannot serve the installed policy"
			}
			ManagedDocumentFailureStage::RetainedDecode => {
				"the stored format or the app's decoder rejected a stored document"
			}
			ManagedDocumentFailureStage::RetainedProjection => {
				"the stored format or the app's projector rejected a stored document"
			}
			ManagedDocumentFailureStage::EncodingAndProjection => {
				"the app's encoder or projector rejected a converted document"
			}
		})
	}
}
impl std::error::Error for ManagedDocumentError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match &self.source {
			ManagedDocumentCause::Scope(error) => Some(error),
			ManagedDocumentCause::Candidate(error) => Some(error),
			// Any retains non-Error application types too; typed access exposes the original wrapper.
			ManagedDocumentCause::App(_) => None,
		}
	}
}

/// The library owns this erased interface; applications provide only actual codec functions.
trait ErasedManagedDocument: Send + Sync {
	fn as_any(&self) -> &dyn Any;
	fn scope(&self) -> &ScopeIdentity;
	fn initial_selection(
		&self,
		queries: &[IndexContractIdentity],
	) -> Result<crate::scope::selection::ScopeSelection, crate::scope::selection::ScopeSelectionError>;
	fn target_selection(
		&self,
		installed: Option<&ScopePolicy>,
		queries: &[IndexContractIdentity],
	) -> Result<crate::scope::selection::ScopeSelection, crate::scope::selection::ScopeSelectionError>;
	fn schema_identifier(&self) -> &'static str;
	fn index_contract(&self, identity: &IndexContractIdentity) -> Option<&IndexContract>;
	fn support(&self, role: DocumentRole) -> Result<DocumentRoleSupport, ManagedDocumentError>;
	fn support_reader_queries(
		&self,
		indexes: &[IndexContractIdentity],
	) -> Result<DocumentRoleSupport, ManagedDocumentError>;
	fn bind_final_candidate(
		&self,
		policy: ScopePolicy,
	) -> Result<CandidateDocumentScopePolicy, ManagedDocumentError>;
	fn check_writer_stage(&self, policy: &ScopePolicy) -> Result<(), ManagedDocumentError>;
	fn prepare_selected_writer_projection(
		&self,
		current: &ScopePolicy,
		record: ManagedDocumentRecord<'_>,
	) -> Result<EncodedMetadataUpdate, ManagedDocumentError>;
	fn prepare_selected_writer_conversion(
		&self,
		current: &ScopePolicy,
		record: ManagedDocumentRecord<'_>,
	) -> Result<EncodedRecord, ManagedDocumentError>;
}

/// The fixed executor supplies its actual lookup identity and held physical snapshot together.
/// This data describes a record; it grants no record authority.
#[derive(Clone, Copy)]
pub(crate) struct ManagedDocumentRecord<'a> {
	pub(crate) identity: &'a RecordIdentity,
	pub(crate) record: &'a StoredRecord,
}

/// Actual immutable code and scope are retained before any background ownership is erased.
pub(crate) struct TypedManagedDocument<T, C, P> {
	scope: ScopeIdentity,
	code: Arc<DocumentCode<T, C, P>>,
}

/// Actual scope and code ownership for library-managed document maintenance.
pub(crate) struct TypedManagedDocumentFields<T, C, P> {
	/// The application scope associated with this retained code.
	pub(crate) scope: ScopeIdentity,
	/// The actual immutable codec, index and projector owners.
	pub(crate) code: Arc<DocumentCode<T, C, P>>,
}

impl<T, C, P> TypedManagedDocument<T, C, P> {
	/// Retains actual code and identity without activating a role or background task.
	pub(crate) fn new(fields: TypedManagedDocumentFields<T, C, P>) -> Self {
		Self {
			scope: fields.scope,
			code: fields.code,
		}
	}

	/// Erases retained code for library-owned background maintenance.
	///
	/// This ownership boundary requires shareable retained code and application causes.
	/// Ordinary borrowed declarations and operations do not require these bounds.
	pub(crate) fn into_registration(self) -> ManagedDocumentRegistration
	where
		Self: Send + Sync + 'static,
		T: 'static,
		C: Send + Sync + 'static,
		P: Send + Sync + 'static,
	{
		ManagedDocumentRegistration {
			code: Arc::new(self),
		}
	}

	fn failure_context(
		&self,
		operation: ManagedDocumentOperation,
		stage: ManagedDocumentFailureStage,
		current: Option<&ScopePolicy>,
	) -> ManagedDocumentFailureContext {
		let selected_writer = current.and_then(|policy| match policy.owner() {
			ScopeOwner::Document(document) => Some(document.schema().writer()),
			_ => None,
		});
		ManagedDocumentFailureContext {
			operation,
			stage,
			selected_writer,
			record_identity: None,
			stored_version: None,
		}
	}
	fn failure(
		&self,
		operation: ManagedDocumentOperation,
		stage: ManagedDocumentFailureStage,
		current: Option<&ScopePolicy>,
		source: ManagedDocumentCause,
	) -> ManagedDocumentError {
		ManagedDocumentError {
			context: Box::new(self.failure_context(operation, stage, current)),
			source,
		}
	}
	fn require_record_identity(
		&self,
		current: &ScopePolicy,
		record: ManagedDocumentRecord<'_>,
		operation: ManagedDocumentOperation,
	) -> Result<(), ManagedDocumentError> {
		if record.identity.namespace() != self.scope.namespace() {
			return Err(self
				.failure(
					operation,
					ManagedDocumentFailureStage::RecordIdentity,
					Some(current),
					ManagedDocumentCause::Scope(ScopeError::ScopeMismatch),
				)
				.at_record(record, None));
		}
		Ok(())
	}
}

impl<T, C, P> ErasedManagedDocument for TypedManagedDocument<T, C, P>
where
	Self: Send + Sync,
	T: 'static,
	C: Send + Sync + 'static,
	P: Send + Sync + 'static,
{
	fn as_any(&self) -> &dyn Any {
		self
	}
	fn scope(&self) -> &ScopeIdentity {
		&self.scope
	}
	fn initial_selection(
		&self,
		queries: &[IndexContractIdentity],
	) -> Result<crate::scope::selection::ScopeSelection, crate::scope::selection::ScopeSelectionError>
	{
		let document = self.code.document_schema();
		crate::scope::selection::ScopeSelection::initial_document(
			&DocumentScopeDeclaration::new(DocumentScopeDeclarationFields {
				identity: &self.scope,
				document,
			}),
			queries,
		)
	}
	fn target_selection(
		&self,
		installed: Option<&ScopePolicy>,
		queries: &[IndexContractIdentity],
	) -> Result<crate::scope::selection::ScopeSelection, crate::scope::selection::ScopeSelectionError>
	{
		let document = self.code.document_schema();
		crate::scope::selection::ScopeSelection::target_document(
			&DocumentScopeDeclaration::new(DocumentScopeDeclarationFields {
				identity: &self.scope,
				document,
			}),
			installed,
			queries,
		)
	}
	fn schema_identifier(&self) -> &'static str {
		self.code.schema().identifier()
	}
	fn index_contract(&self, identity: &IndexContractIdentity) -> Option<&IndexContract> {
		self.code.index_contract(identity)
	}
	fn support(&self, role: DocumentRole) -> Result<DocumentRoleSupport, ManagedDocumentError> {
		let document = self.code.document_schema();
		DocumentScopeDeclaration::new(DocumentScopeDeclarationFields {
			identity: &self.scope,
			document,
		})
		.support(role)
		.map_err(|error| {
			self.failure(
				ManagedDocumentOperation::DeclareRole(role),
				ManagedDocumentFailureStage::Binding,
				None,
				ManagedDocumentCause::Scope(error),
			)
		})
	}
	fn support_reader_queries(
		&self,
		indexes: &[IndexContractIdentity],
	) -> Result<DocumentRoleSupport, ManagedDocumentError> {
		let operation = ManagedDocumentOperation::DeclareQuerySupport;
		let document = self.code.document_schema();
		let selected = indexes
			.iter()
			.enumerate()
			.map(|(index, identity)| {
				document
					.indexes()
					.iter()
					.find(|actual| actual.identity() == identity)
					.copied()
					.ok_or_else(|| {
						self.failure(
							operation,
							ManagedDocumentFailureStage::Binding,
							None,
							ManagedDocumentCause::Scope(ScopeError::MissingIndex { index }),
						)
					})
			})
			.collect::<Result<Vec<_>, _>>()?;
		DocumentScopeDeclaration::new(DocumentScopeDeclarationFields {
			identity: &self.scope,
			document: self.code.document_schema(),
		})
		.support_reader_queries(&selected)
		.map_err(|error| {
			self.failure(
				operation,
				ManagedDocumentFailureStage::Binding,
				None,
				ManagedDocumentCause::Scope(error),
			)
		})
	}

	fn bind_final_candidate(
		&self,
		policy: ScopePolicy,
	) -> Result<CandidateDocumentScopePolicy, ManagedDocumentError> {
		let context = self.failure_context(
			ManagedDocumentOperation::BindFinalCandidate,
			ManagedDocumentFailureStage::Binding,
			Some(&policy),
		);
		let document = self.code.document_schema();
		let declaration = DocumentScopeDeclaration::new(DocumentScopeDeclarationFields {
			identity: &self.scope,
			document,
		});
		CandidateDocumentScopePolicy::new(&declaration, policy).map_err(|error| {
			ManagedDocumentError {
				context: Box::new(context),
				source: ManagedDocumentCause::Candidate(error),
			}
		})
	}
	fn check_writer_stage(&self, policy: &ScopePolicy) -> Result<(), ManagedDocumentError> {
		let document = self.code.document_schema();
		let declaration = DocumentScopeDeclaration::new(DocumentScopeDeclarationFields {
			identity: &self.scope,
			document,
		});
		declaration
			.bind_reader(policy)
			.and_then(|_| declaration.bind_producer(policy))
			.map(|_| ())
			.map_err(|error| {
				self.failure(
					ManagedDocumentOperation::CheckWriterStage,
					ManagedDocumentFailureStage::Binding,
					Some(policy),
					ManagedDocumentCause::Scope(error),
				)
			})
	}

	fn prepare_selected_writer_conversion(
		&self,
		current: &ScopePolicy,
		record: ManagedDocumentRecord<'_>,
	) -> Result<EncodedRecord, ManagedDocumentError> {
		let operation = ManagedDocumentOperation::SelectedWriterConversion;
		self.require_record_identity(current, record, operation)?;
		let document = self.code.document_schema();
		let declaration = DocumentScopeDeclaration::new(DocumentScopeDeclarationFields {
			identity: &self.scope,
			document,
		});
		// Resolve both exact current roles before invoking any application decoder or encoder.
		let reader = declaration.bind_reader(current).map_err(|error| {
			self.failure(
				operation,
				ManagedDocumentFailureStage::Binding,
				Some(current),
				ManagedDocumentCause::Scope(error),
			)
			.at_record(record, None)
		})?;
		let producer = declaration.bind_producer(current).map_err(|error| {
			self.failure(
				operation,
				ManagedDocumentFailureStage::Binding,
				Some(current),
				ManagedDocumentCause::Scope(error),
			)
			.at_record(record, None)
		})?;
		let (version, value) = reader
			.decode_document(record.record.body())
			.map_err(|error| {
				self.failure(
					operation,
					ManagedDocumentFailureStage::RetainedDecode,
					Some(current),
					ManagedDocumentCause::App(ManagedAppCause::read(error.0)),
				)
				.at_record(record, None)
			})?;
		producer
			.encode_record(&value, record.record.metadata())
			.map_err(|error| {
				self.failure(
					operation,
					ManagedDocumentFailureStage::EncodingAndProjection,
					Some(current),
					ManagedDocumentCause::App(ManagedAppCause::projection(error)),
				)
				.at_record(record, Some(version))
			})
	}

	fn prepare_selected_writer_projection(
		&self,
		current: &ScopePolicy,
		record: ManagedDocumentRecord<'_>,
	) -> Result<EncodedMetadataUpdate, ManagedDocumentError> {
		let operation = ManagedDocumentOperation::SelectedWriterProjection;
		self.require_record_identity(current, record, operation)?;
		let document = self.code.document_schema();
		let declaration = DocumentScopeDeclaration::new(DocumentScopeDeclarationFields {
			identity: &self.scope,
			document,
		});
		let maintenance = declaration
			.bind_selected_writer_projection(current)
			.map_err(|error| {
				self.failure(
					operation,
					ManagedDocumentFailureStage::Binding,
					Some(current),
					ManagedDocumentCause::Scope(error),
				)
				.at_record(record, None)
			})?;
		maintenance
			.prepare_selected_writer_projection(record.record)
			.map_err(|error| {
				let version = match &error {
					DocumentProjectionError::Read(ContractStateReadError::Policy(
						StateContractError::StoredRepresentationNotSelectedWriter {
							stored, ..
						},
					)) => Some(stored.version),
					_ => None,
				};
				self.failure(
					operation,
					ManagedDocumentFailureStage::RetainedProjection,
					Some(current),
					ManagedDocumentCause::App(ManagedAppCause::projection(error)),
				)
				.at_record(record, version)
			})
	}
}

/// Retained heterogeneous document code for library-managed maintenance.
///
/// Clones share the actual code instance without another support inventory.
#[derive(Clone)]
pub(crate) struct ManagedDocumentRegistration {
	code: Arc<dyn ErasedManagedDocument>,
}

/// Lookup diagnostics identify the logical association and retain no projection function.
#[derive(Debug, thiserror::Error)]
#[error("managed document has no index declaration")]
pub(crate) struct ManagedDocumentIndexLookupError {
	scope: ScopeIdentity,
	schema_identifier: &'static str,
	index: IndexContractIdentity,
}
impl ManagedDocumentRegistration {
	pub(crate) fn prepare_selected_writer_projection(
		&self,
		current: &ScopePolicy,
		record: ManagedDocumentRecord<'_>,
	) -> Result<EncodedMetadataUpdate, ManagedDocumentError> {
		self.code
			.prepare_selected_writer_projection(current, record)
	}
	/// Borrows the actual retained scope identity.
	pub(crate) fn scope(&self) -> &ScopeIdentity {
		self.code.scope()
	}
	/// The typed code, when this registration holds documents of type `T` with codec failures
	/// `C` and projector failures `P`.
	pub(crate) fn typed_code<T: 'static, C: 'static, P: 'static>(
		&self,
	) -> Option<&Arc<crate::kv::document::schema::owned::DocumentCode<T, C, P>>> {
		self.code
			.as_any()
			.downcast_ref::<TypedManagedDocument<T, C, P>>()
			.map(|code| &code.code)
	}
	pub(crate) fn initial_selection(
		&self,
		queries: &[IndexContractIdentity],
	) -> Result<crate::scope::selection::ScopeSelection, crate::scope::selection::ScopeSelectionError>
	{
		self.code.initial_selection(queries)
	}
	pub(crate) fn target_selection(
		&self,
		installed: Option<&ScopePolicy>,
		queries: &[IndexContractIdentity],
	) -> Result<crate::scope::selection::ScopeSelection, crate::scope::selection::ScopeSelectionError>
	{
		self.code.target_selection(installed, queries)
	}
	/// Returns the actual retained schema family identifier.
	pub(crate) fn schema_identifier(&self) -> &'static str {
		self.code.schema_identifier()
	}
	pub(crate) fn index_contract(
		&self,
		identity: &IndexContractIdentity,
	) -> Result<&IndexContract, ManagedDocumentIndexLookupError> {
		self.code
			.index_contract(identity)
			.ok_or_else(|| ManagedDocumentIndexLookupError {
				scope: self.scope().clone(),
				schema_identifier: self.schema_identifier(),
				index: identity.clone(),
			})
	}
	pub(crate) fn support(
		&self,
		role: DocumentRole,
	) -> Result<DocumentRoleSupport, ManagedDocumentError> {
		self.code.support(role)
	}
	pub(crate) fn support_reader_queries(
		&self,
		indexes: &[IndexContractIdentity],
	) -> Result<DocumentRoleSupport, ManagedDocumentError> {
		self.code.support_reader_queries(indexes)
	}
	pub(crate) fn bind_final_candidate(
		&self,
		policy: ScopePolicy,
	) -> Result<CandidateDocumentScopePolicy, ManagedDocumentError> {
		self.code.bind_final_candidate(policy)
	}
	pub(crate) fn check_writer_stage(
		&self,
		policy: &ScopePolicy,
	) -> Result<(), ManagedDocumentError> {
		self.code.check_writer_stage(policy)
	}
	pub(crate) fn prepare_selected_writer_conversion(
		&self,
		current: &ScopePolicy,
		record: ManagedDocumentRecord<'_>,
	) -> Result<EncodedRecord, ManagedDocumentError> {
		self.code
			.prepare_selected_writer_conversion(current, record)
	}
}
impl fmt::Debug for ManagedDocumentRegistration {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("ManagedDocumentRegistration { .. }")
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ManagedDocumentRegistryError {
	#[error("managed document scope is repeated at registration {index}")]
	DuplicateScope { index: usize },
	#[error("managed document scope has no code")]
	ScopeNotRegistered,
}

/// Immutable heterogeneous actual code; snapshots/targets alone cannot construct its entries.
pub(crate) struct ManagedDocumentRegistry {
	entries: Vec<ManagedDocumentRegistration>,
}
impl ManagedDocumentRegistry {
	pub(crate) fn new(
		entries: Vec<ManagedDocumentRegistration>,
	) -> Result<Self, ManagedDocumentRegistryError> {
		for (index, entry) in entries.iter().enumerate() {
			if entries[..index]
				.iter()
				.any(|prior| prior.scope() == entry.scope())
			{
				return Err(ManagedDocumentRegistryError::DuplicateScope { index });
			}
		}
		Ok(Self { entries })
	}
	pub(crate) fn registrations(&self) -> &[ManagedDocumentRegistration] {
		&self.entries
	}
	pub(crate) fn lookup(
		&self,
		scope: &ScopeIdentity,
	) -> Result<&ManagedDocumentRegistration, ManagedDocumentRegistryError> {
		self.entries
			.iter()
			.find(|entry| entry.scope() == scope)
			.ok_or(ManagedDocumentRegistryError::ScopeNotRegistered)
	}
}
impl fmt::Debug for ManagedDocumentRegistry {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("ManagedDocumentRegistry")
			.field("scope_count", &self.entries.len())
			.finish()
	}
}
