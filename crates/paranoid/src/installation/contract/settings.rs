//! Core-owned settings codecs bound to actual semantic and representation policy.

use super::{
	BoundStateSchema, ComponentBindingAccess, ContractStateReadError, SchemaPolicy,
	StateContractError, StateStorageEnvelope, check_representation,
};
use crate::records::state_schema::{
	NativeDocumentError, NativeStateDocument, StateSchema, StateSchemaCapabilities,
	StateSchemaError,
};
use std::{any::Any, error::Error, fmt};

/// Explicit policy meaning; a document never substitutes for parameterless semantics.
#[derive(Clone)]
pub(crate) enum ComponentSettings {
	Parameterless,
	Document {
		representation_policy: SchemaPolicy,
		document: NativeStateDocument,
	},
}

impl ComponentSettings {
	pub(super) fn check_shape(&self) -> Result<(), ComponentSettingsBindingError> {
		if let Self::Document {
			representation_policy,
			document,
		} = self
		{
			check_native_policy(representation_policy)?;
			if representation_policy.identifier() != document.schema_identifier() {
				return Err(ComponentSettingsBindingError::WrongSchema);
			}
			if !representation_policy
				.required_readers()
				.iter()
				.any(|reader| {
					reader.version == document.version()
						&& reader.envelope == StateStorageEnvelope::NativeJson
				}) {
				return Err(ComponentSettingsBindingError::StoredReaderNotRetained);
			}
		}
		Ok(())
	}
	pub(crate) fn matches_settings(&self, other: &Self) -> bool {
		match (self, other) {
			(Self::Parameterless, Self::Parameterless) => true,
			(
				Self::Document {
					representation_policy,
					document,
				},
				Self::Document {
					representation_policy: other_policy,
					document: other_document,
				},
			) => {
				representation_policy.matches_policy(other_policy)
					&& document.matches_document(other_document)
			}
			_ => false,
		}
	}
}
impl fmt::Debug for ComponentSettings {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Parameterless => formatter.write_str("Parameterless"),
			Self::Document {
				representation_policy,
				document,
			} => formatter
				.debug_struct("SettingsDocument")
				.field("representation_policy", representation_policy)
				.field("document", document)
				.finish(),
		}
	}
}

/// One actual optional settings registry, independent from mandatory state schemas.
pub(crate) enum ComponentSettingsDeclaration<'a> {
	Parameterless,
	Schema(&'a dyn RegisteredComponentSettingsSchema),
}

/// Requirement belongs to the actual selected semantic implementation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ComponentSettingsRequirement {
	Parameterless,
	Required,
}

mod sealed {
	pub trait Sealed {}
}

/// Only the typed library adapter constructs a settings codec/validation declaration.
pub(crate) trait RegisteredComponentSettingsSchema: sealed::Sealed + Send + Sync {
	fn capabilities(&self) -> StateSchemaCapabilities<'_>;
	fn bind_document(
		&self,
		policy: &SchemaPolicy,
		document: &NativeStateDocument,
	) -> Result<DecodedComponentSettings, ComponentSettingsBindingError>;
	/// Decodes installed settings through actual retained readers without requiring an encoder.
	fn bind_reader_document(
		&self,
		policy: &SchemaPolicy,
		document: &NativeStateDocument,
	) -> Result<DecodedComponentSettings, ComponentSettingsBindingError>;
}

pub(crate) struct DecodedComponentSettings {
	pub(super) value: Box<dyn Any + Send + Sync>,
}

pub(super) struct BoundComponentSettings<'a> {
	pub(super) schema: StateSchemaCapabilities<'a>,
	pub(super) decoded: DecodedComponentSettings,
}

/// Owns the actual registry and sealed typed decoder used by component binding.
pub(crate) struct ComponentSettingsSchema<T, E> {
	schema: StateSchema<T, E>,
}

impl<T, E> ComponentSettingsSchema<T, E> {
	pub(crate) fn new(schema: StateSchema<T, E>) -> Self {
		Self { schema }
	}
	pub(crate) fn schema(&self) -> &StateSchema<T, E> {
		&self.schema
	}
}

impl<T, E> ComponentSettingsSchema<T, E>
where
	T: Send + Sync + 'static,
	E: Error + Send + Sync + 'static,
{
	/// The explicit writer creates the document; its exact encoded state is checked once.
	pub(crate) fn encode_settings(
		&self,
		representation_policy: SchemaPolicy,
		value: &T,
	) -> Result<ComponentSettings, ComponentSettingsBindingError> {
		let bound = bind_schema(
			&self.schema,
			&representation_policy,
			ComponentBindingAccess::Complete,
		)?;
		let encoded = bound.encode(value).map_err(codec_error)?;
		let document = NativeStateDocument::from_encoded(encoded)
			.map_err(ComponentSettingsBindingError::Document)?;
		bound
			.decode_document(document.as_bytes())
			.map_err(read_error)?;
		Ok(ComponentSettings::Document {
			representation_policy,
			document,
		})
	}
}

impl<T, E> sealed::Sealed for ComponentSettingsSchema<T, E> {}
impl<T, E> RegisteredComponentSettingsSchema for ComponentSettingsSchema<T, E>
where
	T: Send + Sync + 'static,
	E: Error + Send + Sync + 'static,
{
	fn capabilities(&self) -> StateSchemaCapabilities<'_> {
		self.schema.capabilities()
	}
	fn bind_document(
		&self,
		policy: &SchemaPolicy,
		document: &NativeStateDocument,
	) -> Result<DecodedComponentSettings, ComponentSettingsBindingError> {
		decode_document(
			&self.schema,
			policy,
			document,
			ComponentBindingAccess::Complete,
		)
	}
	fn bind_reader_document(
		&self,
		policy: &SchemaPolicy,
		document: &NativeStateDocument,
	) -> Result<DecodedComponentSettings, ComponentSettingsBindingError> {
		decode_document(
			&self.schema,
			policy,
			document,
			ComponentBindingAccess::Reader,
		)
	}
}

fn decode_document<T, E>(
	schema: &StateSchema<T, E>,
	policy: &SchemaPolicy,
	document: &NativeStateDocument,
	access: ComponentBindingAccess,
) -> Result<DecodedComponentSettings, ComponentSettingsBindingError>
where
	T: Send + Sync + 'static,
	E: Error + Send + Sync + 'static,
{
	let bound = bind_schema(schema, policy, access)?;
	let (_representation_version, value) = bound
		.decode_document(document.as_bytes())
		.map_err(read_error)?;
	Ok(DecodedComponentSettings {
		value: Box::new(value),
	})
}

fn bind_schema<'a, T, E>(
	schema: &'a StateSchema<T, E>,
	policy: &'a SchemaPolicy,
	access: ComponentBindingAccess,
) -> Result<BoundStateSchema<'a, T, E>, ComponentSettingsBindingError> {
	check_native_policy(policy)?;
	if schema.identifier() != policy.identifier() {
		return Err(ComponentSettingsBindingError::WrongSchema);
	}
	let capabilities = schema.capabilities();
	if access == ComponentBindingAccess::Complete {
		check_representation(&capabilities, policy.writer(), 0, true)
			.map_err(ComponentSettingsBindingError::Policy)?;
	}
	for reader in policy.required_readers() {
		check_representation(&capabilities, *reader, 0, false)
			.map_err(ComponentSettingsBindingError::Policy)?;
	}
	Ok(BoundStateSchema {
		schema,
		policy,
		policy_index: 0,
	})
}

fn check_native_policy(policy: &SchemaPolicy) -> Result<(), ComponentSettingsBindingError> {
	if policy.writer().envelope != StateStorageEnvelope::NativeJson
		|| policy
			.required_readers()
			.iter()
			.any(|reader| reader.envelope != StateStorageEnvelope::NativeJson)
	{
		return Err(ComponentSettingsBindingError::NativeJsonRequired);
	}
	Ok(())
}

fn codec_error<E: Error + Send + Sync + 'static>(
	error: StateSchemaError<E>,
) -> ComponentSettingsBindingError {
	ComponentSettingsBindingError::Codec {
		source: Box::new(error),
	}
}
fn read_error<E: Error + Send + Sync + 'static>(
	error: ContractStateReadError<E>,
) -> ComponentSettingsBindingError {
	match error {
		ContractStateReadError::Policy(error) => ComponentSettingsBindingError::Policy(error),
		ContractStateReadError::Representation(error) => codec_error(error),
	}
}

/// Typed causes remain inspectable while formatting excludes arbitrary codec contents.
pub(crate) enum ComponentSettingsBindingError {
	ParameterlessRequired,
	SettingsRequired,
	MissingDeclaration,
	ConflictingDeclaration,
	DifferentDeclaration,
	DifferentWorkingType,
	WrongSchema,
	NativeJsonRequired,
	StoredReaderNotRetained,
	Policy(StateContractError),
	Document(NativeDocumentError),
	Codec {
		source: Box<dyn Error + Send + Sync>,
	},
}
impl fmt::Debug for ComponentSettingsBindingError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		fmt::Display::fmt(self, formatter)
	}
}
impl fmt::Display for ComponentSettingsBindingError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(match self {
			Self::ParameterlessRequired => {
				"selected component semantics require parameterless policy"
			}
			Self::SettingsRequired => "selected component semantics require settings",
			Self::MissingDeclaration => "required settings have no actual codec declaration",
			Self::ConflictingDeclaration => "settings and fixed state schema identities conflict",
			Self::DifferentDeclaration => {
				"settings binding uses a different actual codec declaration"
			}
			Self::DifferentWorkingType => "settings binding has a different working type",
			Self::WrongSchema => "settings policy and document have a different schema",
			Self::NativeJsonRequired => "component settings require native JSON representations",
			Self::StoredReaderNotRetained { .. } => {
				"stored settings version is not retained by policy"
			}
			Self::Policy(_) => "component settings representation policy rejected",
			Self::Document(_) => "component settings native envelope rejected",
			Self::Codec { .. } => "component settings codec rejected the representation",
		})
	}
}
impl Error for ComponentSettingsBindingError {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		match self {
			Self::Policy(error) => Some(error),
			Self::Document(error) => Some(error),
			Self::Codec { source } => Some(source.as_ref()),
			_ => None,
		}
	}
}
