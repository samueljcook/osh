//! Document indexes: their declared fields and the app functions that compute a document's
//! index values.

use std::{fmt, num::NonZeroU32};

pub(crate) mod managed;
pub(crate) mod owned;

use crate::installation::contract::role_binding::BoundSchemaReader;
use crate::installation::contract::{
	BoundStateSchema, ContractStateReadError, StateStorageEnvelope,
};
use crate::records::state_schema::{
	StateRepresentationCapability, StateRepresentationFormat, StateSchema, StateSchemaError,
};
use crate::records::{
	EncodedMetadataUpdate, EncodedRecord, IndexFields, IndexScalar, RecordMetadata, StoredRecord,
};

/// The identity of a document index: an identifier and a version. Paranoid names the
/// database index itself.
#[derive(Clone, Eq, PartialEq)]
pub struct IndexContractIdentity {
	identifier: String,
	version: NonZeroU32,
}

impl IndexContractIdentity {
	/// Checks that the identifier is not empty and has no NUL character and that the version is
	/// positive.
	pub fn new(identifier: String, version: u32) -> Result<Self, DocumentDeclarationError> {
		if identifier.is_empty() || identifier.contains('\0') {
			return Err(DocumentDeclarationError::InvalidIndexIdentifier);
		}
		let version =
			NonZeroU32::new(version).ok_or(DocumentDeclarationError::IndexVersionIsZero)?;
		Ok(Self {
			identifier,
			version,
		})
	}

	/// The index identifier.
	pub fn identifier(&self) -> &str {
		&self.identifier
	}

	/// The index version.
	pub fn version(&self) -> NonZeroU32 {
		self.version
	}
}

impl fmt::Debug for IndexContractIdentity {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("IndexContractIdentity")
			.field("identifier_byte_len", &self.identifier.len())
			.field("version", &self.version)
			.finish()
	}
}

/// The type of one index field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndexScalarType {
	/// Boolean values.
	Boolean,
	/// Signed 64-bit integer values.
	SignedInteger,
	/// Signed Unix microsecond timestamp values.
	Timestamp,
	/// Text compared by its UTF-8 bytes.
	BytewiseText,
	/// Opaque bytes compared bytewise.
	Bytes,
}

impl IndexScalarType {
	pub(crate) fn matches(self, value: &IndexScalar) -> bool {
		matches!(
			(self, value),
			(Self::Boolean, IndexScalar::Boolean(_))
				| (Self::SignedInteger, IndexScalar::SignedInteger(_))
				| (Self::Timestamp, IndexScalar::Timestamp(_))
				| (Self::BytewiseText, IndexScalar::BytewiseText(_))
				| (Self::Bytes, IndexScalar::Bytes(_))
		)
	}
}

/// One index field: its name and its type.
pub struct IndexFieldDef {
	name: String,
	scalar_type: IndexScalarType,
}

impl IndexFieldDef {
	/// Checks that the name is not empty and has no NUL character.
	pub fn new(
		name: String,
		scalar_type: IndexScalarType,
	) -> Result<Self, DocumentDeclarationError> {
		if name.is_empty() || name.contains('\0') {
			return Err(DocumentDeclarationError::InvalidFieldName);
		}
		Ok(Self { name, scalar_type })
	}

	/// The field name.
	pub fn name(&self) -> &str {
		&self.name
	}
	/// The field type.
	pub fn scalar_type(&self) -> IndexScalarType {
		self.scalar_type
	}
}

impl fmt::Debug for IndexFieldDef {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("IndexFieldDef")
			.field("name_byte_len", &self.name.len())
			.field("scalar_type", &self.scalar_type)
			.finish()
	}
}

/// A document index: its identity and its ordered fields. An index without fields records only
/// which documents it includes.
pub struct IndexContract {
	identity: IndexContractIdentity,
	fields: Vec<IndexFieldDef>,
}

impl IndexContract {
	/// Checks that the field names are unique.
	pub fn new(
		identity: IndexContractIdentity,
		fields: Vec<IndexFieldDef>,
	) -> Result<Self, DocumentDeclarationError> {
		for (field_index, field) in fields.iter().enumerate() {
			if fields[..field_index]
				.iter()
				.any(|prior| prior.name == field.name)
			{
				return Err(DocumentDeclarationError::DuplicateField { field_index });
			}
		}
		Ok(Self { identity, fields })
	}

	/// The index's identity.
	pub fn identity(&self) -> &IndexContractIdentity {
		&self.identity
	}
	/// The index's fields, in the order in which a projector returns their values.
	pub fn fields(&self) -> &[IndexFieldDef] {
		&self.fields
	}

	pub(crate) fn coverage_field_key(&self) -> String {
		format!("{}coverage", self.key_prefix())
	}

	pub(crate) fn field_key(&self, field_index: usize) -> Result<String, DocumentDeclarationError> {
		let field = self
			.fields
			.get(field_index)
			.ok_or(DocumentDeclarationError::UnknownField { field_index })?;
		Ok(format!("{}field:{}", self.key_prefix(), field.name))
	}

	fn key_prefix(&self) -> String {
		// The byte length distinguishes identifiers that contain the key separators.
		format!(
			"document-index:{}:{}:{}:",
			self.identity.identifier.len(),
			self.identity.identifier,
			self.identity.version
		)
	}
}

impl fmt::Debug for IndexContract {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("IndexContract")
			.field("identity", &self.identity)
			.field("field_count", &self.fields.len())
			.finish()
	}
}

/// A document's entry in one index, which an index projector returns.
pub enum IndexProjection {
	/// The index includes the document, with one value for each index field in the field order.
	Applicable(Vec<IndexScalar>),
	/// The index does not include the document.
	NotApplicable,
}

impl fmt::Debug for IndexProjection {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Applicable(values) => formatter
				.debug_struct("Applicable")
				.field("field_count", &values.len())
				.finish(),
			Self::NotApplicable => formatter.write_str("NotApplicable"),
		}
	}
}

/// An app function that computes a document's index entry from the document and its metadata.
pub type IndexProjector<T, E> = fn(&T, RecordMetadata) -> Result<IndexProjection, E>;

/// Actual implementation of one exact contract instance for one representation.
pub struct RegisteredIndexProjector<'d, T, E> {
	/// The actual logical contract that this function implements.
	pub contract: &'d IndexContract,
	/// The representation's code-owned projection function.
	pub project_fn: IndexProjector<T, E>,
}

/// Actual index implementations for one registered document representation.
pub struct RepresentationIndexes<'d, T, E> {
	/// The registered JSON representation version.
	pub representation_version: u32,
	/// Its actual projection functions and contract references.
	pub projectors: Vec<RegisteredIndexProjector<'d, T, E>>,
}

/// An invalid index or document code. The error reports positions and versions, never
/// identifiers or values.
#[derive(Debug, thiserror::Error)]
pub enum DocumentDeclarationError {
	/// The index identifier is empty or has a NUL character.
	#[error("document index identifier must be nonempty and contain no NUL")]
	InvalidIndexIdentifier,
	/// The index version is zero.
	#[error("document index version must be positive")]
	IndexVersionIsZero,
	/// A field name is empty or has a NUL character.
	#[error("document field name must be nonempty and contain no NUL")]
	InvalidFieldName,
	/// An index has two fields with the same name.
	#[error("document field {field_index} is declared more than once")]
	DuplicateField {
		/// The repeated field's position.
		field_index: usize,
	},
	/// A field position is outside the index.
	#[error("document index has no field {field_index}")]
	UnknownField {
		/// The requested field's position.
		field_index: usize,
	},
	/// Two indexes have the same identity.
	#[error("document index {index} is declared more than once")]
	DuplicateIndex {
		/// The repeated index's position.
		index: usize,
	},
	/// Projectors are given for a stored format version that the schema does not have.
	#[error("document representation {version} has no registered codec")]
	UnknownRepresentation {
		/// The unknown version.
		version: u32,
	},
	/// Projectors are given for a stored format version of the kind `Bytes`.
	#[error("document representation {version} is not native JSON")]
	NotJsonRepresentation {
		/// The version.
		version: u32,
	},
	/// Projectors are given twice for one stored format version.
	#[error("document representation {version} has multiple projection registrations")]
	DuplicateRepresentation {
		/// The version.
		version: u32,
	},
	/// A projector names an index that the document code does not declare.
	#[error("representation {version} projector {projector_index} names an undeclared index")]
	UndeclaredIndex {
		/// The stored format version of the projector.
		version: u32,
		/// The projector's position among that version's projectors.
		projector_index: usize,
	},
	/// A stored format version has two projectors for one index.
	#[error("representation {version} projector {projector_index} repeats an index")]
	DuplicateProjector {
		/// The stored format version.
		version: u32,
		/// The repeated projector's position.
		projector_index: usize,
	},
}

/// Explicit selection and binding failures contain no requested identity strings.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DocumentBindingError {
	#[error("document binding uses another index declaration")]
	DifferentIndexDeclaration,
	#[error("required document index {index} has no declaration")]
	UnknownIndex { index: usize },
	#[error("required document index {index} is selected more than once")]
	DuplicateRequiredIndex { index: usize },
	#[error("document binding uses a different schema declaration")]
	DifferentSchemaDeclaration,
	#[error("document writer must use the native JSON envelope")]
	NativeWriterRequired,
	#[error("document representation {version} has no implementation for required index {index}")]
	MissingProjector { version: u32, index: usize },
}

/// Typed causes remain available without printing their contents through this wrapper.
#[derive(thiserror::Error)]
pub(crate) enum DocumentProjectionError<C, P> {
	#[error("document codec rejected selected writer state")]
	Codec(#[source] StateSchemaError<C>),
	#[error("document reader rejected state under its bound policy")]
	Read(#[source] ContractStateReadError<C>),
	#[error("document index selection failed")]
	Binding(#[source] DocumentBindingError),
	#[error("document projector rejected required index {index}")]
	Projector {
		index: usize,
		#[source]
		error: P,
	},
	#[error("document index {index} produced the wrong number of fields")]
	FieldCountMismatch { index: usize },
	#[error("document index {index} field {field_index} has the wrong scalar type")]
	ScalarTypeMismatch { index: usize, field_index: usize },
	#[error("document encoded a representation different from its selected writer")]
	WrittenRepresentationMismatch,
}

impl<C, P> DocumentProjectionError<C, P> {
	pub(crate) fn codec_error(&self) -> Option<&StateSchemaError<C>> {
		match self {
			Self::Codec(source) | Self::Read(ContractStateReadError::Representation(source)) => {
				Some(source)
			}
			_ => None,
		}
	}
	pub(crate) fn projector_error(&self) -> Option<&P> {
		match self {
			Self::Projector { error, .. } => Some(error),
			_ => None,
		}
	}
	pub(crate) fn index_position(&self) -> Option<usize> {
		match self {
			Self::Projector { index, .. }
			| Self::FieldCountMismatch { index }
			| Self::ScalarTypeMismatch { index, .. } => Some(*index),
			Self::Binding(
				DocumentBindingError::UnknownIndex { index }
				| DocumentBindingError::DuplicateRequiredIndex { index }
				| DocumentBindingError::MissingProjector { index, .. },
			) => Some(*index),
			_ => None,
		}
	}
	pub(in crate::kv) fn field_position(&self) -> Option<usize> {
		match self {
			Self::ScalarTypeMismatch { field_index, .. } => Some(*field_index),
			_ => None,
		}
	}
}

impl<C, P> fmt::Debug for DocumentProjectionError<C, P> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Codec(_) => formatter.write_str("Codec"),
			Self::Read(_) => formatter.write_str("Read"),
			Self::Binding(error) => formatter.debug_tuple("Binding").field(error).finish(),
			Self::Projector { index, .. } => formatter
				.debug_struct("Projector")
				.field("index", index)
				.finish(),
			Self::FieldCountMismatch { index } => formatter
				.debug_struct("FieldCountMismatch")
				.field("index", index)
				.finish(),
			Self::ScalarTypeMismatch { index, field_index } => formatter
				.debug_struct("ScalarTypeMismatch")
				.field("index", index)
				.field("field_index", field_index)
				.finish(),
			Self::WrittenRepresentationMismatch => {
				formatter.write_str("WrittenRepresentationMismatch")
			}
		}
	}
}

/// Composition references actual declarations; it owns no copied capability matrix.
pub struct DocumentSchema<'d, T, C, P> {
	schema: &'d StateSchema<T, C>,
	indexes: Vec<&'d IndexContract>,
	representations: Vec<RepresentationIndexes<'d, T, P>>,
}

impl<'d, T, C, P> DocumentSchema<'d, T, C, P> {
	/// Parses actual codec, logical index and per-representation projector registrations.
	pub fn new(
		schema: &'d StateSchema<T, C>,
		indexes: Vec<&'d IndexContract>,
		representations: Vec<RepresentationIndexes<'d, T, P>>,
	) -> Result<Self, DocumentDeclarationError> {
		for (index, contract) in indexes.iter().enumerate() {
			if indexes[..index]
				.iter()
				.any(|prior| prior.identity == contract.identity)
			{
				return Err(DocumentDeclarationError::DuplicateIndex { index });
			}
		}
		for (position, representation) in representations.iter().enumerate() {
			let version = representation.representation_version;
			let capability = schema
				.capabilities()
				.representation(version)
				.ok_or(DocumentDeclarationError::UnknownRepresentation { version })?;
			if capability.format != StateRepresentationFormat::Json {
				return Err(DocumentDeclarationError::NotJsonRepresentation { version });
			}
			if representations[..position]
				.iter()
				.any(|prior| prior.representation_version == version)
			{
				return Err(DocumentDeclarationError::DuplicateRepresentation { version });
			}
			for (projector_index, projector) in representation.projectors.iter().enumerate() {
				if !indexes
					.iter()
					.any(|declared| std::ptr::eq(*declared, projector.contract))
				{
					return Err(DocumentDeclarationError::UndeclaredIndex {
						version,
						projector_index,
					});
				}
				if representation.projectors[..projector_index]
					.iter()
					.any(|prior| std::ptr::eq(prior.contract, projector.contract))
				{
					return Err(DocumentDeclarationError::DuplicateProjector {
						version,
						projector_index,
					});
				}
			}
		}
		Ok(Self {
			schema,
			indexes,
			representations,
		})
	}

	/// Borrows the actual schema declaration.
	pub fn schema(&self) -> &StateSchema<T, C> {
		self.schema
	}
	/// Borrows the actual logical index contracts.
	pub fn indexes(&self) -> &[&IndexContract] {
		&self.indexes
	}
	pub(crate) fn require_index_declaration(
		&self,
		index: &IndexContract,
	) -> Result<(), DocumentBindingError> {
		if !self
			.indexes
			.iter()
			.any(|actual| std::ptr::eq(*actual, index))
		{
			return Err(DocumentBindingError::DifferentIndexDeclaration);
		}
		Ok(())
	}

	/// Visits only actual projector registrations and their actual codec capabilities.
	pub(crate) fn for_each_index_implementation(
		&self,
		mut visit_fn: impl FnMut(u32, StateRepresentationCapability, &IndexContract),
	) {
		for representation in &self.representations {
			let version = representation.representation_version;
			let capability = self
				.schema
				.capabilities()
				.representation(version)
				.expect("document construction checked the immutable codec declaration");
			for projector in &representation.projectors {
				visit_fn(version, capability, projector.contract);
			}
		}
	}

	/// Resolves explicit installed identities to this declaration's actual instances.
	pub(crate) fn select_indexes<'a>(
		&'a self,
		required: &[IndexContractIdentity],
	) -> Result<SelectedDocumentIndexes<'a, 'd, T, C, P>, DocumentBindingError> {
		let mut indexes = Vec::with_capacity(required.len());
		for (index, identity) in required.iter().enumerate() {
			if required[..index].contains(identity) {
				return Err(DocumentBindingError::DuplicateRequiredIndex { index });
			}
			let contract = self
				.indexes
				.iter()
				.find(|contract| contract.identity == *identity)
				.ok_or(DocumentBindingError::UnknownIndex { index })?;
			indexes.push(*contract);
		}
		Ok(SelectedDocumentIndexes {
			document: self,
			indexes,
		})
	}

	pub(crate) fn bind<'a>(
		&'a self,
		schema: BoundStateSchema<'a, T, C>,
		required: &[IndexContractIdentity],
	) -> Result<BoundDocumentSchema<'a, 'd, T, C, P>, DocumentBindingError> {
		if !schema.matches_schema_declaration(self.schema) {
			return Err(DocumentBindingError::DifferentSchemaDeclaration);
		}
		if schema.policy().writer().envelope != StateStorageEnvelope::NativeJson {
			return Err(DocumentBindingError::NativeWriterRequired);
		}
		let selected = self.select_indexes(required)?;
		selected.require_implementations(schema.policy().writer().version)?;
		Ok(BoundDocumentSchema { schema, selected })
	}
}

/// Explicit requirements bound to one actual document declaration, without admission.
pub(crate) struct SelectedDocumentIndexes<'a, 'd, T, C, P> {
	document: &'a DocumentSchema<'d, T, C, P>,
	indexes: Vec<&'d IndexContract>,
}

impl<T, C, P> SelectedDocumentIndexes<'_, '_, T, C, P> {
	fn projector(
		&self,
		version: u32,
		index: usize,
	) -> Result<&RegisteredIndexProjector<'_, T, P>, DocumentBindingError> {
		self.document
			.representations
			.iter()
			.find(|representation| representation.representation_version == version)
			.and_then(|representation| {
				representation
					.projectors
					.iter()
					.find(|projector| std::ptr::eq(projector.contract, self.indexes[index]))
			})
			.ok_or(DocumentBindingError::MissingProjector { version, index })
	}

	pub(crate) fn require_implementations(&self, version: u32) -> Result<(), DocumentBindingError> {
		for index in 0..self.indexes.len() {
			self.projector(version, index)?;
		}
		Ok(())
	}

	fn derive_fields(
		&self,
		version: u32,
		value: &T,
		metadata: RecordMetadata,
	) -> Result<IndexFields, DocumentProjectionError<C, P>> {
		// Resolve the whole selection before invoking application-owned pure projectors.
		let projectors = (0..self.indexes.len())
			.map(|index| self.projector(version, index))
			.collect::<Result<Vec<_>, _>>()
			.map_err(DocumentProjectionError::Binding)?;
		let mut fields = IndexFields::new();
		for (index, (contract, projector)) in self.indexes.iter().zip(projectors).enumerate() {
			let projection = (projector.project_fn)(value, metadata)
				.map_err(|error| DocumentProjectionError::Projector { index, error })?;
			match projection {
				IndexProjection::NotApplicable => {
					fields.insert(contract.coverage_field_key(), IndexScalar::Boolean(false));
				}
				IndexProjection::Applicable(values) => {
					if values.len() != contract.fields.len() {
						return Err(DocumentProjectionError::FieldCountMismatch { index });
					}
					for (field_index, (def, value)) in
						contract.fields.iter().zip(values).enumerate()
					{
						if !def.scalar_type.matches(&value) {
							return Err(DocumentProjectionError::ScalarTypeMismatch {
								index,
								field_index,
							});
						}
						fields.insert(
							contract.field_key(field_index).expect(
								"iterated field ordinals belong to their actual index declaration",
							),
							value,
						);
					}
					fields.insert(contract.coverage_field_key(), IndexScalar::Boolean(true));
				}
			}
		}
		Ok(fields)
	}

	/// Preserves the exact stored body while deriving all maintained fields under new metadata.
	pub(crate) fn prepare_metadata_update(
		&self,
		schema: &BoundSchemaReader<'_, T, C>,
		record: &StoredRecord,
		metadata: RecordMetadata,
	) -> Result<EncodedMetadataUpdate, DocumentProjectionError<C, P>> {
		self.prepare_with_reader(
			schema.matches_schema_declaration(self.document.schema),
			|body| schema.decode_document(body),
			record,
			metadata,
		)
	}

	/// Derives the complete maintained map only from the actual selected stored writer.
	pub(crate) fn prepare_selected_writer_projection(
		&self,
		schema: &BoundSchemaReader<'_, T, C>,
		record: &StoredRecord,
	) -> Result<EncodedMetadataUpdate, DocumentProjectionError<C, P>> {
		self.prepare_with_reader(
			schema.matches_schema_declaration(self.document.schema),
			|body| schema.decode_selected_writer_document(body),
			record,
			record.metadata(),
		)
	}

	fn prepare_with_reader(
		&self,
		matches_actual_schema: bool,
		decode_fn: impl FnOnce(&[u8]) -> Result<(u32, T), ContractStateReadError<C>>,
		record: &StoredRecord,
		metadata: RecordMetadata,
	) -> Result<EncodedMetadataUpdate, DocumentProjectionError<C, P>> {
		if !matches_actual_schema {
			return Err(DocumentProjectionError::Binding(
				DocumentBindingError::DifferentSchemaDeclaration,
			));
		}
		EncodedMetadataUpdate::derive(record.body(), metadata, |body, metadata| {
			let (version, value) = decode_fn(body).map_err(DocumentProjectionError::Read)?;
			self.derive_fields(version, &value, metadata)
		})
	}
}

/// Selected schema writer and explicit document maintenance requirements.
pub(crate) struct BoundDocumentSchema<'a, 'd, T, C, P> {
	schema: BoundStateSchema<'a, T, C>,
	selected: SelectedDocumentIndexes<'a, 'd, T, C, P>,
}

impl<T, C, P> BoundDocumentSchema<'_, '_, T, C, P> {
	/// Encodes the bound writer and decodes its exact bytes before deriving any index field.
	pub(crate) fn encode_record(
		&self,
		value: &T,
		metadata: RecordMetadata,
	) -> Result<EncodedRecord, DocumentProjectionError<C, P>> {
		let body = self
			.schema
			.encode(value)
			.map_err(DocumentProjectionError::Codec)?;
		EncodedRecord::derive(body, metadata, |encoded, metadata| {
			let (version, value) = self
				.selected
				.document
				.schema
				.decode_document(encoded)
				.map_err(DocumentProjectionError::Codec)?;
			if version != self.schema.policy().writer().version {
				return Err(DocumentProjectionError::WrittenRepresentationMismatch);
			}
			self.selected.derive_fields(version, &value, metadata)
		})
	}
}
