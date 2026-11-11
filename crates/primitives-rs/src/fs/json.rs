use serde::{Serialize, de::DeserializeOwned};

use super::{Dir, RelativePath, WriteError, WriteOptions, WriteOutcome};
use crate::serialization::json::{JsonError, JsonLimits, SerializeOptions};

/// JSON encoding and atomic file publication options.
#[derive(Clone, Copy, Debug)]
pub struct WriteJsonOptions<'a> {
	/// JSON size, depth, and formatting options.
	pub json: SerializeOptions<'a>,
	/// File publication, permissions, and durability options.
	pub file: WriteOptions,
}

/// A bounded JSON file operation failed.
#[derive(Debug)]
pub enum JsonFileError {
	/// Opening or bounded reading failed.
	Read(crate::io::bounded_reads::ReadError),
	/// JSON parsing or encoding failed.
	Json(JsonError),
	/// File publication failed. The contained error identifies publication status.
	Write(WriteError),
}

impl std::fmt::Display for JsonFileError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Read(error) => error.fmt(f),
			Self::Json(error) => error.fmt(f),
			Self::Write(error) => error.fmt(f),
		}
	}
}

impl std::error::Error for JsonFileError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(match self {
			Self::Read(error) => error,
			Self::Json(error) => error,
			Self::Write(error) => error,
		})
	}
}

impl Dir {
	/// Reads and parses one bounded JSON document with ordinary Serde semantics.
	pub fn read_json<T: DeserializeOwned>(
		&self,
		path: &RelativePath,
		limits: JsonLimits,
	) -> Result<T, JsonFileError> {
		let bytes = self
			.read_file(
				path,
				crate::io::bounded_reads::ReadOptions {
					max_size: limits.max_size,
				},
			)
			.map_err(JsonFileError::Read)?;
		crate::serialization::json::deserialize_slice(&bytes, limits).map_err(JsonFileError::Json)
	}

	/// Encodes bounded JSON before publishing a file. Encoding errors leave the
	/// destination unchanged. Use a secret JSON buffer and [`Self::write_file_atomically`]
	/// for plaintext secrets that need zeroizing memory.
	pub fn write_json_atomically<T: Serialize + ?Sized>(
		&self,
		path: &RelativePath,
		value: &T,
		options: WriteJsonOptions<'_>,
	) -> Result<WriteOutcome, JsonFileError> {
		let bytes = crate::serialization::json::serialize_to_vec(value, options.json)
			.map_err(JsonFileError::Json)?;
		self.write_file_atomically(path, &bytes, options.file)
			.map_err(JsonFileError::Write)
	}
}
