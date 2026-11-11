//! Owned JSON state documents with parsed schema and version metadata.

use super::StateDocument;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::fmt;

/// An exact encoded JSON state document with parsed schema and version metadata.
///
/// Structural parsing does not select a registered codec or authenticate the payload.
#[derive(Clone)]
pub struct EncodedStateDocument {
	encoded: Box<RawValue>,
	schema_identifier: String,
	version: u32,
}

impl EncodedStateDocument {
	/// Parses an owned JSON value without interpreting its state payload.
	pub fn from_raw(encoded: Box<RawValue>) -> Result<Self, StateDocumentError> {
		let document: StateDocument<'_> =
			serde_json::from_str(encoded.get()).map_err(json_error)?;
		if document.schema.is_empty() {
			return Err(StateDocumentError::EmptySchema);
		}
		if document.version == 0 {
			return Err(StateDocumentError::VersionIsZero);
		}
		let schema = document.schema.into_owned();
		let version = document.version;
		Ok(Self {
			encoded,
			schema_identifier: schema,
			version,
		})
	}

	/// Parses UTF-8 JSON bytes into owned storage without interpreting the state.
	pub fn from_encoded(encoded: Vec<u8>) -> Result<Self, StateDocumentError> {
		let encoded = String::from_utf8(encoded).map_err(|_| StateDocumentError::NonUtf8)?;
		let encoded = RawValue::from_string(encoded).map_err(json_error)?;
		Self::from_raw(encoded)
	}

	/// Returns the nonempty schema identifier.
	pub fn schema_identifier(&self) -> &str {
		&self.schema_identifier
	}
	/// Returns the positive encoded version.
	pub fn version(&self) -> u32 {
		self.version
	}
	/// Returns the stored JSON bytes, preserving payload number precision.
	pub fn as_bytes(&self) -> &[u8] {
		self.encoded.get().as_bytes()
	}
	/// Exact document equality preserves wide integers and never ignores a value change.
	pub fn matches_document(&self, other: &Self) -> bool {
		self.encoded.get() == other.encoded.get()
	}
}

impl fmt::Debug for EncodedStateDocument {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("EncodedStateDocument")
			.field("version", &self.version)
			.finish_non_exhaustive()
	}
}

impl Serialize for EncodedStateDocument {
	fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		self.encoded.serialize(serializer)
	}
}

impl<'de> Deserialize<'de> for EncodedStateDocument {
	fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
		Self::from_raw(Box::<RawValue>::deserialize(deserializer)?)
			.map_err(serde::de::Error::custom)
	}
}

/// Structural envelope rejection without printing identifiers or encoded contents.
#[derive(Debug, thiserror::Error)]
pub enum StateDocumentError {
	#[error("state document schema must not be empty")]
	/// The schema identifier was empty.
	EmptySchema,
	#[error("state document version must be positive")]
	/// The version was zero.
	VersionIsZero,
	#[error("state document encoding is not UTF-8")]
	/// The input bytes were not UTF-8.
	NonUtf8,
	#[error("state document JSON is invalid: {category:?} at line {line}, column {column}")]
	/// JSON parsing rejected the document structure.
	Json {
		/// The JSON parser error category.
		category: serde_json::error::Category,
		/// The one-based line number.
		line: usize,
		/// The parser column number.
		column: usize,
	},
}

fn json_error(error: serde_json::Error) -> StateDocumentError {
	StateDocumentError::Json {
		category: error.classify(),
		line: error.line(),
		column: error.column(),
	}
}
