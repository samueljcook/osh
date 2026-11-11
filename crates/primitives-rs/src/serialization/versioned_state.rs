//! Versioned state encoding with explicit reader and writer registration.
//!
//! Each version's decoder converts encoded bytes into one working value type.
//! Writers require an explicit version; registration order selects no writer.
//! Byte representations carry no envelope. JSON documents record their schema,
//! version, and state without converting the payload through a JSON value tree.
//!
//! This facility performs no I/O, migration scheduling, encryption, or runtime
//! compatibility negotiation. A caller selects the writer that its storage or
//! transport contract permits. Encoders determine their payload bytes; this
//! facility does not promise canonical JSON or stable content hashes.
//! Callers bound input before parsing and own their codec's resource use.
//!
//! ```
//! use primitives_rs::serialization::versioned_state::{
//!     StateRepresentationFormat, StateSchema, StateSchemaFields, StateVersion,
//! };
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let schema = StateSchema::new(StateSchemaFields {
//!     identifier: "example.counter",
//!     versions: vec![StateVersion {
//!         version: 1,
//!         format: StateRepresentationFormat::Json,
//!         decoder: |bytes| serde_json::from_slice::<u64>(bytes),
//!         encoder: Some(serde_json::to_vec::<u64>),
//!     }],
//! })?;
//! let bytes = schema.encode_document(1, &42)?;
//! let (version, value) = schema.decode_document(&bytes)?;
//! assert_eq!((version, value), (1, 42));
//! # Ok(())
//! # }
//! ```

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::borrow::Cow;

mod document;
pub use document::{EncodedStateDocument, StateDocumentError};

/// A function that decodes one stored format into the value type.
pub type StateDecoder<T, E> = fn(&[u8]) -> Result<T, E>;

/// A function that encodes the value type into one stored format.
pub type StateEncoder<T, E> = fn(&T) -> Result<Vec<u8>, E>;

/// The kind of a stored format.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateRepresentationFormat {
	/// Arbitrary bytes. JSON document operations cannot use this kind.
	Bytes,
	/// JSON payloads that document operations wrap with the schema and version.
	Json,
}

/// One version of a schema's stored format.
pub struct StateVersion<T, E> {
	/// The positive version number, unique within the schema.
	pub version: u32,
	/// The kind of the stored format.
	pub format: StateRepresentationFormat,
	/// Decodes the stored format into the value type.
	pub decoder: StateDecoder<T, E>,
	/// Encodes the value type into the stored format, or `None` for a version that the code
	/// can only read.
	pub encoder: Option<StateEncoder<T, E>>,
}

/// Capability information derived from one actual reader/writer registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StateRepresentationCapability {
	/// The representation encoding.
	pub format: StateRepresentationFormat,
	/// Whether this declaration contains an encoder for the representation.
	pub writable: bool,
}

/// An immutable, type-erased view of an actual schema declaration.
/// Only `StateSchema::capabilities` constructs this view; it owns no copied version list.
#[derive(Clone, Copy)]
pub struct StateSchemaCapabilities<'a> {
	declaration: &'a dyn RegisteredSchemaCapabilities,
}

trait RegisteredSchemaCapabilities: Send + Sync {
	fn identifier(&self) -> &'static str;
	fn representation(&self, version: u32) -> Option<StateRepresentationCapability>;
	fn visit_representations(&self, visit_fn: &mut dyn FnMut(u32, StateRepresentationCapability));
}

impl StateSchemaCapabilities<'_> {
	/// Returns the schema identifier from the borrowed declaration.
	pub fn identifier(&self) -> &'static str {
		self.declaration.identifier()
	}
	/// Returns the registered format and writer availability for a version.
	pub fn representation(&self, version: u32) -> Option<StateRepresentationCapability> {
		self.declaration.representation(version)
	}
	/// Visits every registered version in declaration order without copying the registry.
	pub fn for_each_representation(
		&self,
		mut visit_fn: impl FnMut(u32, StateRepresentationCapability),
	) {
		self.declaration.visit_representations(&mut visit_fn);
	}
	/// Tests whether both views borrow the same schema instance.
	///
	/// Equal identifiers and version lists do not make separate declarations the
	/// same instance. Moving a schema requires ending every borrowed view first.
	pub fn is_same_declaration(&self, other: StateSchemaCapabilities<'_>) -> bool {
		std::ptr::eq(
			std::ptr::from_ref(self.declaration).cast::<()>(),
			std::ptr::from_ref(other.declaration).cast::<()>(),
		)
	}
}

/// A schema-matched JSON document's version and borrowed state, before reader dispatch.
pub struct ParsedStateDocument<'a> {
	version: u32,
	state: &'a [u8],
}

impl ParsedStateDocument<'_> {
	/// Returns the encoded version without asserting that a reader supports it.
	pub fn version(&self) -> u32 {
		self.version
	}
	/// Returns the exact JSON payload bytes without decoding them.
	pub fn state(&self) -> &[u8] {
		self.state
	}
}

/// An invalid schema, or a failure to decode or encode a value.
#[derive(Debug, thiserror::Error)]
pub enum StateSchemaError<E> {
	/// The schema identifier is empty.
	#[error("state schema identifier must not be empty")]
	EmptyIdentifier,
	/// The schema has no versions.
	#[error("state schema must register at least one representation")]
	NoRepresentations,
	/// A version number is zero.
	#[error("state representation versions must be positive integers")]
	VersionIsZero,
	/// Two versions have the same number.
	#[error("state representation version {version} is registered more than once")]
	DuplicateVersion {
		/// The repeated version.
		version: u32,
	},
	/// The schema has no version with the stored version number.
	#[error("stored state representation version {version} is not supported")]
	UnsupportedReadVersion {
		/// The unsupported stored version.
		version: u32,
	},
	/// The explicitly selected writer version has no encoder.
	#[error("state representation version {version} has no supported writer")]
	UnsupportedWriteVersion {
		/// The selected version.
		version: u32,
	},
	/// A document operation selected a version of the kind `Bytes`.
	#[error("state representation version {version} is not a JSON state representation")]
	NotJsonRepresentation {
		/// The selected version.
		version: u32,
	},
	/// The stored document records another schema.
	#[error("stored document does not belong to schema {expected}")]
	WrongSchema {
		/// The identifier of this schema.
		expected: &'static str,
	},
	/// The stored document, or the JSON that an encoder produced, is not valid JSON of the
	/// expected form.
	#[error("state document JSON is invalid: {category:?} error at line {line}, column {column}")]
	InvalidDocument {
		/// The JSON parser's error category.
		category: serde_json::error::Category,
		/// The parser's one-based line number.
		line: usize,
		/// The parser's column number.
		column: usize,
	},
	/// The app's decoder or encoder returned an error.
	#[error(transparent)]
	Codec(E),
}

/// The stored formats of one app value type: an identifier and its versions.
pub struct StateSchema<T, E> {
	identifier: &'static str,
	versions: Vec<StateVersion<T, E>>,
}

impl<T, E> RegisteredSchemaCapabilities for StateSchema<T, E> {
	fn identifier(&self) -> &'static str {
		self.identifier
	}
	fn representation(&self, version: u32) -> Option<StateRepresentationCapability> {
		self.versions
			.iter()
			.find(|registered| registered.version == version)
			.map(|registered| StateRepresentationCapability {
				format: registered.format,
				writable: registered.encoder.is_some(),
			})
	}
	fn visit_representations(&self, visit_fn: &mut dyn FnMut(u32, StateRepresentationCapability)) {
		for registered in &self.versions {
			visit_fn(
				registered.version,
				StateRepresentationCapability {
					format: registered.format,
					writable: registered.encoder.is_some(),
				},
			);
		}
	}
}

/// The JSON envelope. The selected schema reader interprets its state.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StateDocument<'a> {
	#[serde(borrow)]
	schema: Cow<'a, str>,
	version: u32,
	#[serde(borrow)]
	state: &'a RawValue,
}

/// The inputs of a state schema.
pub struct StateSchemaFields<T, E> {
	/// The schema's identifier, which JSON document operations include in the envelope.
	pub identifier: &'static str,
	/// The stored formats that the code reads, each with its version and its optional encoder.
	pub versions: Vec<StateVersion<T, E>>,
}

impl<T, E> StateSchema<T, E> {
	/// Checks that the identifier is not empty, that there is at least one version, and that
	/// the versions are positive and unique.
	pub fn new(fields: StateSchemaFields<T, E>) -> Result<Self, StateSchemaError<E>> {
		let StateSchemaFields {
			identifier,
			versions,
		} = fields;
		if identifier.is_empty() {
			return Err(StateSchemaError::EmptyIdentifier);
		}
		if versions.is_empty() {
			return Err(StateSchemaError::NoRepresentations);
		}
		for (index, registered) in versions.iter().enumerate() {
			if registered.version == 0 {
				return Err(StateSchemaError::VersionIsZero);
			}
			if versions[..index]
				.iter()
				.any(|previous| previous.version == registered.version)
			{
				return Err(StateSchemaError::DuplicateVersion {
					version: registered.version,
				});
			}
		}
		Ok(Self {
			identifier,
			versions,
		})
	}

	/// The schema's identifier.
	pub fn identifier(&self) -> &'static str {
		self.identifier
	}

	/// Borrows a type-erased view of the actual reader and writer registrations.
	pub fn capabilities(&self) -> StateSchemaCapabilities<'_> {
		StateSchemaCapabilities { declaration: self }
	}

	/// Invokes the selected decoder once to parse and transform the representation.
	pub fn decode(&self, version: u32, bytes: &[u8]) -> Result<T, StateSchemaError<E>> {
		let registered = self.reader(version)?;
		(registered.decoder)(bytes).map_err(StateSchemaError::Codec)
	}

	/// Writes the caller's selected version without substituting another representation.
	pub fn encode(&self, version: u32, value: &T) -> Result<Vec<u8>, StateSchemaError<E>> {
		let (encode, _) = self.writer(version)?;
		encode(value).map_err(StateSchemaError::Codec)
	}

	/// Parses the strict envelope and exact schema identity without invoking a state reader.
	pub fn parse_document<'a>(
		&self,
		bytes: &'a [u8],
	) -> Result<ParsedStateDocument<'a>, StateSchemaError<E>> {
		let document: StateDocument<'_> = serde_json::from_slice(bytes).map_err(document_error)?;
		if document.schema.as_ref() != self.identifier {
			return Err(StateSchemaError::WrongSchema {
				expected: self.identifier,
			});
		}
		Ok(ParsedStateDocument {
			version: document.version,
			state: document.state.get().as_bytes(),
		})
	}

	/// Parses a JSON envelope and returns its representation version and decoded value.
	pub fn decode_document(&self, bytes: &[u8]) -> Result<(u32, T), StateSchemaError<E>> {
		let document = self.parse_document(bytes)?;
		let registered = self.reader(document.version())?;
		if registered.format != StateRepresentationFormat::Json {
			return Err(StateSchemaError::NotJsonRepresentation {
				version: document.version(),
			});
		}
		let value = (registered.decoder)(document.state()).map_err(StateSchemaError::Codec)?;
		Ok((document.version(), value))
	}

	/// Encodes the selected writer's JSON state inside the schema/version envelope.
	pub fn encode_document(&self, version: u32, value: &T) -> Result<Vec<u8>, StateSchemaError<E>> {
		let (encode, format) = self.writer(version)?;
		if format != StateRepresentationFormat::Json {
			return Err(StateSchemaError::NotJsonRepresentation { version });
		}
		let state = encode(value).map_err(StateSchemaError::Codec)?;
		let state: &RawValue = serde_json::from_slice(&state).map_err(document_error)?;
		serde_json::to_vec(&StateDocument {
			schema: Cow::Borrowed(self.identifier),
			version,
			state,
		})
		.map_err(document_error)
	}

	fn reader(&self, version: u32) -> Result<&StateVersion<T, E>, StateSchemaError<E>> {
		self.versions
			.iter()
			.find(|registered| registered.version == version)
			.ok_or(StateSchemaError::UnsupportedReadVersion { version })
	}

	fn writer(
		&self,
		version: u32,
	) -> Result<(StateEncoder<T, E>, StateRepresentationFormat), StateSchemaError<E>> {
		self.versions
			.iter()
			.find(|registered| registered.version == version)
			.and_then(|registered| {
				registered
					.encoder
					.map(|encoder| (encoder, registered.format))
			})
			.ok_or(StateSchemaError::UnsupportedWriteVersion { version })
	}
}

fn document_error<E>(error: serde_json::Error) -> StateSchemaError<E> {
	StateSchemaError::InvalidDocument {
		category: error.classify(),
		line: error.line(),
		column: error.column(),
	}
}
