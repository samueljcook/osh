//! Code-owned semantic contracts and immutable component/schema policy binding.
//!
//! Codec registrations establish representation capabilities, not supported semantics.
//! Primitive transitions supply explicit semantic definition entries. This module binds
//! immutable declarations and policy without admitting a transaction or selecting a generation.

use std::fmt;

pub(crate) mod policy_wire;
pub(crate) mod role_binding;
pub(crate) mod settings;
use role_binding::BoundSchemaReader;
pub(crate) use settings::{
	ComponentSettings, ComponentSettingsBindingError, ComponentSettingsDeclaration,
	ComponentSettingsRequirement, ComponentSettingsSchema,
};

use crate::records::state_schema::{
	ParsedStateDocument, StateRepresentationFormat, StateSchema, StateSchemaCapabilities,
	StateSchemaError,
};

/// Physical envelope selected by policy, independently for each reader and the writer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StateStorageEnvelope {
	SourceBytes,
	NativeJson,
}

impl StateStorageEnvelope {
	pub(crate) fn representation_format(self) -> StateRepresentationFormat {
		match self {
			Self::SourceBytes => StateRepresentationFormat::Bytes,
			Self::NativeJson => StateRepresentationFormat::Json,
		}
	}
}

/// One explicitly selected representation and its physical envelope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RepresentationPolicy {
	pub(crate) version: u32,
	pub(crate) envelope: StateStorageEnvelope,
}

/// A parsed per-schema writer and retained-reader requirement.
#[derive(Clone)]
pub(crate) struct SchemaPolicy {
	identifier: String,
	writer: RepresentationPolicy,
	required_readers: Vec<RepresentationPolicy>,
}

impl SchemaPolicy {
	pub(crate) fn new(
		identifier: String,
		writer: RepresentationPolicy,
		required_readers: Vec<RepresentationPolicy>,
	) -> Result<Self, StateContractError> {
		if identifier.is_empty() {
			return Err(StateContractError::EmptyPolicyIdentifier {
				field: PolicyIdentifierField::Schema,
			});
		}
		if writer.version == 0 {
			return Err(StateContractError::RepresentationVersionIsZero);
		}
		for (index, reader) in required_readers.iter().enumerate() {
			if reader.version == 0 {
				return Err(StateContractError::RepresentationVersionIsZero);
			}
			if required_readers[..index]
				.iter()
				.any(|previous| previous.version == reader.version)
			{
				return Err(StateContractError::DuplicateRequiredReader {
					version: reader.version,
				});
			}
		}
		if !required_readers.contains(&writer) {
			return Err(StateContractError::WriterNotRetained {
				version: writer.version,
			});
		}
		Ok(Self {
			identifier,
			writer,
			required_readers,
		})
	}
	pub(crate) fn identifier(&self) -> &str {
		&self.identifier
	}
	pub(crate) fn writer(&self) -> RepresentationPolicy {
		self.writer
	}
	pub(crate) fn required_readers(&self) -> &[RepresentationPolicy] {
		&self.required_readers
	}
	pub(crate) fn matches_policy(&self, other: &Self) -> bool {
		self.identifier == other.identifier
			&& self.writer == other.writer
			&& self.required_readers.len() == other.required_readers.len()
			&& self
				.required_readers
				.iter()
				.all(|reader| other.required_readers.contains(reader))
	}
}

impl fmt::Debug for SchemaPolicy {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("SchemaPolicy")
			.field("writer", &self.writer)
			.field("required_readers", &self.required_readers)
			.finish_non_exhaustive()
	}
}

/// Component and semantic contract selected explicitly by code-owned policy.
pub(crate) struct ComponentPolicyFields {
	pub(crate) component_identifier: String,
	pub(crate) semantic_contract: String,
	pub(crate) schemas: Vec<SchemaPolicy>,
	pub(crate) settings: ComponentSettings,
}

/// Parsed immutable policy; construction chooses no identity or representation implicitly.
#[derive(Clone)]
pub(crate) struct ComponentPolicy {
	component_identifier: String,
	semantic_contract: String,
	schemas: Vec<SchemaPolicy>,
	settings: ComponentSettings,
}

impl ComponentPolicy {
	pub(crate) fn new(fields: ComponentPolicyFields) -> Result<Self, StateContractError> {
		let ComponentPolicyFields {
			component_identifier: component,
			semantic_contract,
			schemas,
			settings,
		} = fields;
		if component.is_empty() {
			return Err(StateContractError::EmptyPolicyIdentifier {
				field: PolicyIdentifierField::Component,
			});
		}
		if semantic_contract.is_empty() {
			return Err(StateContractError::EmptyPolicyIdentifier {
				field: PolicyIdentifierField::SemanticContract,
			});
		}
		for (index, schema) in schemas.iter().enumerate() {
			if schemas[..index]
				.iter()
				.any(|previous| previous.identifier == schema.identifier)
			{
				return Err(StateContractError::DuplicatePolicySchema {
					policy_index: index,
				});
			}
		}
		settings
			.check_shape()
			.map_err(|error| StateContractError::Settings(Box::new(error)))?;
		Ok(Self {
			component_identifier: component,
			semantic_contract,
			schemas,
			settings,
		})
	}
	pub(crate) fn component_identifier(&self) -> &str {
		&self.component_identifier
	}
	pub(crate) fn semantic_contract(&self) -> &str {
		&self.semantic_contract
	}
	pub(crate) fn schemas(&self) -> &[SchemaPolicy] {
		&self.schemas
	}
	pub(crate) fn settings(&self) -> &ComponentSettings {
		&self.settings
	}
	pub(crate) fn matches_policy(&self, other: &Self) -> bool {
		self.component_identifier == other.component_identifier
			&& self.semantic_contract == other.semantic_contract
			&& self.schemas.len() == other.schemas.len()
			&& self.schemas.iter().all(|schema| {
				other
					.schemas
					.iter()
					.any(|other| schema.matches_policy(other))
			}) && self.settings.matches_settings(&other.settings)
	}
}

impl fmt::Debug for ComponentPolicy {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("ComponentPolicy")
			.field("schemas", &self.schemas)
			.finish_non_exhaustive()
	}
}

/// Primitive-owned transition implementation for one explicit semantic contract identifier.
/// Several definition entries may borrow the same implementation when semantics permit it.
pub(crate) struct SemanticContractDef<'a, Transitions> {
	pub(crate) identifier: &'a str,
	pub(crate) transitions: &'a Transitions,
	pub(crate) settings: ComponentSettingsRequirement,
}

/// Actual code declarations supplied with explicit settings and fixed-schema ownership.
pub(crate) struct ComponentDeclarationFields<'a, Transitions> {
	pub(crate) identifier: &'static str,
	pub(crate) fixed_schemas: Vec<StateSchemaCapabilities<'a>>,
	pub(crate) settings: ComponentSettingsDeclaration<'a>,
	pub(crate) semantics: Vec<SemanticContractDef<'a, Transitions>>,
}

/// Actual schema declarations and explicit semantic implementations of one component.
/// Schema capabilities are borrowed from their registries, never restated as version lists.
pub(crate) struct ComponentDeclaration<'a, Transitions> {
	identifier: &'static str,
	schemas: Vec<StateSchemaCapabilities<'a>>,
	semantics: Vec<SemanticContractDef<'a, Transitions>>,
	settings: ComponentSettingsDeclaration<'a>,
}

impl<'d, Transitions> ComponentDeclaration<'d, Transitions> {
	pub(crate) fn new(
		fields: ComponentDeclarationFields<'d, Transitions>,
	) -> Result<Self, StateContractError> {
		let ComponentDeclarationFields {
			identifier,
			fixed_schemas: schemas,
			settings,
			semantics,
		} = fields;
		if identifier.is_empty() {
			return Err(StateContractError::EmptyComponentIdentifier);
		}
		if semantics.is_empty() {
			return Err(StateContractError::NoSemanticDefs);
		}
		for (index, schema) in schemas.iter().enumerate() {
			if schemas[..index]
				.iter()
				.any(|previous| previous.identifier() == schema.identifier())
			{
				return Err(StateContractError::DuplicateDeclaredSchema {
					declaration_index: index,
				});
			}
		}
		for (index, semantic) in semantics.iter().enumerate() {
			if semantic.settings == ComponentSettingsRequirement::Required
				&& matches!(settings, ComponentSettingsDeclaration::Parameterless)
			{
				return Err(StateContractError::Settings(Box::new(
					ComponentSettingsBindingError::MissingDeclaration,
				)));
			}
			if semantic.identifier.is_empty() {
				return Err(StateContractError::EmptySemanticDef { def_index: index });
			}
			if semantics[..index]
				.iter()
				.any(|previous| previous.identifier == semantic.identifier)
			{
				return Err(StateContractError::DuplicateSemanticDef { def_index: index });
			}
		}
		if let ComponentSettingsDeclaration::Schema(schema) = &settings
			&& schemas
				.iter()
				.any(|fixed| fixed.identifier() == schema.capabilities().identifier())
		{
			return Err(StateContractError::Settings(Box::new(
				ComponentSettingsBindingError::ConflictingDeclaration,
			)));
		}
		Ok(Self {
			identifier,
			schemas,
			semantics,
			settings,
		})
	}
	pub(crate) fn identifier(&self) -> &'static str {
		self.identifier
	}
	pub(crate) fn schemas(&self) -> &[StateSchemaCapabilities<'d>] {
		&self.schemas
	}
	/// Enumerates only identifiers with an actual declared transition implementation.
	pub(crate) fn semantic_contracts(&self) -> impl Iterator<Item = &'d str> + '_ {
		self.semantics.iter().map(|semantic| semantic.identifier)
	}
	pub(crate) fn semantic_defs(&self) -> &[SemanticContractDef<'d, Transitions>] {
		&self.semantics
	}
	pub(crate) fn settings_schema(&self) -> Option<StateSchemaCapabilities<'d>> {
		match &self.settings {
			ComponentSettingsDeclaration::Parameterless => None,
			ComponentSettingsDeclaration::Schema(schema) => Some(schema.capabilities()),
		}
	}

	/// Checks all schemas and semantics before returning an operation binding.
	pub(crate) fn bind<'a>(
		&'a self,
		policy: &'a ComponentPolicy,
	) -> Result<BoundComponentContract<'a, 'd, Transitions>, StateContractError> {
		Ok(BoundComponentContract {
			binding: self.bind_for_access(policy, ComponentBindingAccess::Complete)?,
		})
	}

	/// Binds exact semantics, retained state readers and decoded settings without encoders.
	pub(crate) fn bind_reader<'a>(
		&'a self,
		policy: &'a ComponentPolicy,
	) -> Result<BoundComponentReader<'a, 'd, Transitions>, StateContractError> {
		Ok(BoundComponentReader {
			binding: self.bind_for_access(policy, ComponentBindingAccess::Reader)?,
		})
	}

	fn bind_for_access<'a>(
		&'a self,
		policy: &'a ComponentPolicy,
		access: ComponentBindingAccess,
	) -> Result<ComponentBinding<'a, 'd, Transitions>, StateContractError> {
		if policy.component_identifier != self.identifier {
			return Err(StateContractError::WrongComponent);
		}
		let semantic = self
			.semantics
			.iter()
			.find(|semantic| semantic.identifier == policy.semantic_contract)
			.ok_or(StateContractError::UnsupportedSemanticContract)?;
		for (policy_index, required) in policy.schemas.iter().enumerate() {
			let schema = self
				.schemas
				.iter()
				.find(|schema| schema.identifier() == required.identifier)
				.ok_or(StateContractError::UnknownPolicySchema { policy_index })?;
			if access == ComponentBindingAccess::Complete {
				check_representation(schema, required.writer, policy_index, true)?;
			}
			for reader in &required.required_readers {
				check_representation(schema, *reader, policy_index, false)?;
			}
		}
		for (declaration_index, declared) in self.schemas.iter().enumerate() {
			if !policy
				.schemas
				.iter()
				.any(|schema| schema.identifier == declared.identifier())
			{
				return Err(StateContractError::MissingPolicySchema { declaration_index });
			}
		}
		let settings = match (semantic.settings, policy.settings(), &self.settings) {
			(ComponentSettingsRequirement::Parameterless, ComponentSettings::Parameterless, _) => {
				None
			}
			(ComponentSettingsRequirement::Parameterless, _, _) => {
				return Err(StateContractError::Settings(Box::new(
					ComponentSettingsBindingError::ParameterlessRequired,
				)));
			}
			(ComponentSettingsRequirement::Required, ComponentSettings::Parameterless, _) => {
				return Err(StateContractError::Settings(Box::new(
					ComponentSettingsBindingError::SettingsRequired,
				)));
			}
			(
				ComponentSettingsRequirement::Required,
				_,
				ComponentSettingsDeclaration::Parameterless,
			) => {
				return Err(StateContractError::Settings(Box::new(
					ComponentSettingsBindingError::MissingDeclaration,
				)));
			}
			(
				ComponentSettingsRequirement::Required,
				ComponentSettings::Document {
					representation_policy,
					document,
				},
				ComponentSettingsDeclaration::Schema(schema),
			) => Some(settings::BoundComponentSettings {
				schema: schema.capabilities(),
				decoded: match access {
					ComponentBindingAccess::Complete => {
						schema.bind_document(representation_policy, document)
					}
					ComponentBindingAccess::Reader => {
						schema.bind_reader_document(representation_policy, document)
					}
				}
				.map_err(|error| StateContractError::Settings(Box::new(error)))?,
			}),
		};
		Ok(ComponentBinding {
			declaration: self,
			policy,
			semantic,
			settings,
		})
	}
}

/// The access requirement is internal; callers choose the distinct binding method.
#[derive(Clone, Copy, Eq, PartialEq)]
enum ComponentBindingAccess {
	Complete,
	Reader,
}

struct ComponentBinding<'a, 'd, Transitions> {
	declaration: &'a ComponentDeclaration<'d, Transitions>,
	policy: &'a ComponentPolicy,
	semantic: &'a SemanticContractDef<'d, Transitions>,
	settings: Option<settings::BoundComponentSettings<'d>>,
}

impl<Transitions> ComponentBinding<'_, '_, Transitions> {
	fn policy(&self) -> &ComponentPolicy {
		self.policy
	}
	fn transitions(&self) -> &Transitions {
		self.semantic.transitions
	}
	fn settings<'a, T: 'static, E>(
		&'a self,
		actual: &ComponentSettingsSchema<T, E>,
	) -> Result<&'a T, ComponentSettingsBindingError> {
		let settings = self
			.settings
			.as_ref()
			.ok_or(ComponentSettingsBindingError::SettingsRequired)?;
		if !settings
			.schema
			.is_same_declaration(actual.schema().capabilities())
		{
			return Err(ComponentSettingsBindingError::DifferentDeclaration);
		}
		settings
			.decoded
			.value
			.downcast_ref()
			.ok_or(ComponentSettingsBindingError::DifferentWorkingType)
	}

	/// Binds only the actual immutable schema instance that component binding checked.
	fn bind_schema<'a, T, E>(
		&'a self,
		schema: &'a StateSchema<T, E>,
	) -> Result<BoundStateSchema<'a, T, E>, StateContractError> {
		let (policy_index, policy) = self
			.policy
			.schemas
			.iter()
			.enumerate()
			.find(|(_, required)| required.identifier == schema.identifier())
			.ok_or(StateContractError::SchemaNotBound)?;
		let declared = self
			.declaration
			.schemas
			.iter()
			.find(|declared| declared.identifier() == schema.identifier())
			.ok_or(StateContractError::SchemaNotBound)?;
		if !declared.is_same_declaration(schema.capabilities()) {
			return Err(StateContractError::DifferentSchemaDeclaration { policy_index });
		}
		Ok(BoundStateSchema {
			schema,
			policy,
			policy_index,
		})
	}
}

/// Complete immutable semantic/schema policy, including selected state/settings writers.
/// This binding provides no transaction admission or generation proof.
pub(crate) struct BoundComponentContract<'a, 'd, Transitions> {
	binding: ComponentBinding<'a, 'd, Transitions>,
}

impl<Transitions> BoundComponentContract<'_, '_, Transitions> {
	pub(crate) fn policy(&self) -> &ComponentPolicy {
		self.binding.policy()
	}
	pub(crate) fn transitions(&self) -> &Transitions {
		self.binding.transitions()
	}
	pub(crate) fn settings<'a, T: 'static, E>(
		&'a self,
		actual: &ComponentSettingsSchema<T, E>,
	) -> Result<&'a T, ComponentSettingsBindingError> {
		self.binding.settings(actual)
	}
	pub(crate) fn bind_schema<'a, T, E>(
		&'a self,
		schema: &'a StateSchema<T, E>,
	) -> Result<BoundStateSchema<'a, T, E>, StateContractError> {
		self.binding.bind_schema(schema)
	}
}

/// Exact immutable semantics and retained-reader policy with decoded actual settings.
/// Encoders are not checked or exposed, and this is not an admission or mutation proof.
pub(crate) struct BoundComponentReader<'a, 'd, Transitions> {
	binding: ComponentBinding<'a, 'd, Transitions>,
}

impl<Transitions> BoundComponentReader<'_, '_, Transitions> {
	pub(crate) fn bind_schema<'a, T, E>(
		&'a self,
		schema: &'a StateSchema<T, E>,
	) -> Result<BoundSchemaReader<'a, T, E>, StateContractError> {
		Ok(BoundSchemaReader::from_checked_binding(
			self.binding.bind_schema(schema)?,
		))
	}
}

/// Typed schema operations fixed to the exact bound writer and retained-reader policy.
pub(crate) struct BoundStateSchema<'a, T, E> {
	schema: &'a StateSchema<T, E>,
	policy: &'a SchemaPolicy,
	policy_index: usize,
}

impl<T, E> BoundStateSchema<'_, T, E> {
	/// Composition must retain the actual codec registry already checked by this binding.
	pub(crate) fn matches_schema_declaration(&self, schema: &StateSchema<T, E>) -> bool {
		self.schema
			.capabilities()
			.is_same_declaration(schema.capabilities())
	}

	pub(crate) fn policy(&self) -> &SchemaPolicy {
		self.policy
	}

	/// Uses the selected raw source encoder or native document encoder explicitly.
	pub(crate) fn encode(&self, value: &T) -> Result<Vec<u8>, StateSchemaError<E>> {
		let writer = self.policy.writer;
		match writer.envelope {
			StateStorageEnvelope::SourceBytes => self.schema.encode(writer.version, value),
			StateStorageEnvelope::NativeJson => self.schema.encode_document(writer.version, value),
		}
	}

	/// Source metadata must identify its representation; parse success never selects it.
	pub(crate) fn decode_source(
		&self,
		version: u32,
		bytes: &[u8],
	) -> Result<T, ContractStateReadError<E>> {
		self.require_reader(version, StateStorageEnvelope::SourceBytes)?;
		self.schema
			.decode(version, bytes)
			.map_err(ContractStateReadError::Representation)
	}

	/// Parses the strict native envelope once, then checks the bound policy before reader dispatch.
	pub(crate) fn decode_document(
		&self,
		bytes: &[u8],
	) -> Result<(u32, T), ContractStateReadError<E>> {
		let document = self
			.schema
			.parse_document(bytes)
			.map_err(ContractStateReadError::Representation)?;
		self.decode_parsed_document(document)
	}

	/// Rejects a retained older body before application decoding or projection.
	pub(crate) fn decode_selected_writer_document(
		&self,
		bytes: &[u8],
	) -> Result<(u32, T), ContractStateReadError<E>> {
		let document = self
			.schema
			.parse_document(bytes)
			.map_err(ContractStateReadError::Representation)?;
		let stored = RepresentationPolicy {
			version: document.version(),
			envelope: StateStorageEnvelope::NativeJson,
		};
		if stored != self.policy.writer() {
			return Err(ContractStateReadError::Policy(
				StateContractError::StoredRepresentationNotSelectedWriter {
					stored,
					selected: self.policy.writer(),
				},
			));
		}
		self.decode_parsed_document(document)
	}

	fn decode_parsed_document(
		&self,
		document: ParsedStateDocument<'_>,
	) -> Result<(u32, T), ContractStateReadError<E>> {
		self.require_reader(document.version(), StateStorageEnvelope::NativeJson)?;
		let value = self
			.schema
			.decode(document.version(), document.state())
			.map_err(ContractStateReadError::Representation)?;
		Ok((document.version(), value))
	}

	/// Checks a provisionally decoded framework control version without invoking its parser again.
	pub(crate) fn require_reader(
		&self,
		version: u32,
		envelope: StateStorageEnvelope,
	) -> Result<(), ContractStateReadError<E>> {
		if !self
			.policy
			.required_readers
			.contains(&RepresentationPolicy { version, envelope })
		{
			return Err(ContractStateReadError::Policy(
				StateContractError::ReaderNotPermitted {
					policy_index: self.policy_index,
					version,
					envelope,
				},
			));
		}
		Ok(())
	}
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum PolicyIdentifierField {
	Component,
	SemanticContract,
	Schema,
}

/// Binding failures print only typed categories, policy positions, and version/format values.
/// Unknown identifiers and any encoded state are excluded from both Display and Debug.
#[derive(Debug, thiserror::Error)]
pub(crate) enum StateContractError {
	#[error("stored representation {stored:?} differs from selected writer {selected:?}")]
	StoredRepresentationNotSelectedWriter {
		stored: RepresentationPolicy,
		selected: RepresentationPolicy,
	},
	#[error("component settings binding failed")]
	Settings(#[source] Box<ComponentSettingsBindingError>),
	#[error("component declaration identifier must not be empty")]
	EmptyComponentIdentifier,
	#[error("component must declare semantic transition definitions")]
	NoSemanticDefs,
	#[error("semantic definition {def_index} identifier must not be empty")]
	EmptySemanticDef { def_index: usize },
	#[error("semantic definition {def_index} repeats an identifier")]
	DuplicateSemanticDef { def_index: usize },
	#[error("declared schema {declaration_index} repeats an identifier")]
	DuplicateDeclaredSchema { declaration_index: usize },
	#[error("policy identifier {field:?} must not be empty")]
	EmptyPolicyIdentifier { field: PolicyIdentifierField },
	#[error("representation policy versions must be positive")]
	RepresentationVersionIsZero,
	#[error("required reader version {version} is repeated")]
	DuplicateRequiredReader { version: u32 },
	#[error("selected writer version {version} must be a reader with the same envelope")]
	WriterNotRetained { version: u32 },
	#[error("schema policy {policy_index} repeats an identifier")]
	DuplicatePolicySchema { policy_index: usize },
	#[error("policy belongs to another component")]
	WrongComponent,
	#[error("selected semantic contract has no declared transition implementation")]
	UnsupportedSemanticContract,
	#[error("schema policy {policy_index} has no declared schema")]
	UnknownPolicySchema { policy_index: usize },
	#[error("declared schema {declaration_index} has no policy")]
	MissingPolicySchema { declaration_index: usize },
	#[error("schema policy {policy_index} requires unsupported reader version {version}")]
	UnsupportedReader { policy_index: usize, version: u32 },
	#[error("schema policy {policy_index} selects unsupported writer version {version}")]
	UnsupportedWriter { policy_index: usize, version: u32 },
	#[error(
		"schema policy {policy_index} version {version} uses {registered:?}, incompatible with {envelope:?}"
	)]
	IncompatibleEnvelope {
		policy_index: usize,
		version: u32,
		registered: StateRepresentationFormat,
		envelope: StateStorageEnvelope,
	},
	#[error("schema is not bound by this component")]
	SchemaNotBound,
	#[error("schema policy {policy_index} was checked against a different declaration instance")]
	DifferentSchemaDeclaration { policy_index: usize },
	#[error(
		"schema policy {policy_index} does not permit reader version {version} with {envelope:?}"
	)]
	ReaderNotPermitted {
		policy_index: usize,
		version: u32,
		envelope: StateStorageEnvelope,
	},
}

/// Policy failure is distinguished from a schema's own parse/transformation failure.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ContractStateReadError<E> {
	#[error("state read is not permitted by the bound policy")]
	Policy(#[source] StateContractError),
	#[error("state representation parsing failed")]
	Representation(#[source] StateSchemaError<E>),
}

fn check_representation(
	schema: &StateSchemaCapabilities<'_>,
	selected: RepresentationPolicy,
	policy_index: usize,
	is_writer: bool,
) -> Result<(), StateContractError> {
	let capability = schema.representation(selected.version).ok_or({
		if is_writer {
			StateContractError::UnsupportedWriter {
				policy_index,
				version: selected.version,
			}
		} else {
			StateContractError::UnsupportedReader {
				policy_index,
				version: selected.version,
			}
		}
	})?;
	if is_writer && !capability.writable {
		return Err(StateContractError::UnsupportedWriter {
			policy_index,
			version: selected.version,
		});
	}
	if capability.format != selected.envelope.representation_format() {
		return Err(StateContractError::IncompatibleEnvelope {
			policy_index,
			version: selected.version,
			registered: capability.format,
			envelope: selected.envelope,
		});
	}
	Ok(())
}
