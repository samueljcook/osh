use std::{fmt, io};

/// A JSON limit, formatting, serialization, deserialization, or output failure.
///
/// Display and Debug omit Serde and I/O messages because custom errors can
/// contain input values. The typed variants retain those errors for explicit
/// inspection. Error sources are also omitted to preserve this redaction.
pub enum JsonError {
	/// The document exceeded the byte limit.
	TooLarge {
		/// The byte count reached when the operation stopped, not the full document size.
		actual_minimum: usize,
		/// The configured maximum encoded size.
		max: usize,
	},
	/// An array or object opened beyond the configured container depth.
	TooDeep {
		/// The configured maximum container depth.
		max: usize,
	},
	/// Pretty indentation contained a byte other than a space or tab.
	InvalidIndentation,
	/// The output could not reserve memory.
	AllocationFailed,
	/// Serde serialization failed. Its message may contain an input value.
	Serialize(serde_json::Error),
	/// Serde deserialization failed. Its message may contain an input value.
	Deserialize(serde_json::Error),
	/// The output writer failed. Its message may contain an input value.
	Io(io::Error),
}
impl JsonError {
	/// Converts a failure for consumers with a Serde JSON error contract.
	///
	/// Limit errors contain no document contents. Existing Serde errors retain
	/// their original messages, which may contain input values.
	pub fn into_serde_json_error(self) -> serde_json::Error {
		match self {
			Self::Serialize(error) | Self::Deserialize(error) => error,
			Self::Io(error) => serde_json::Error::io(error),
			error => <serde_json::Error as serde::ser::Error>::custom(error),
		}
	}
}
impl fmt::Display for JsonError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::TooLarge { max, .. } => write!(f, "JSON is longer than {max} bytes"),
			Self::TooDeep { max } => write!(
				f,
				"JSON nests more than {max} arrays or objects along a path"
			),
			Self::InvalidIndentation => {
				f.write_str("JSON indentation must contain only spaces and tabs")
			}
			Self::AllocationFailed => f.write_str("JSON buffer allocation failed"),
			Self::Serialize(_) => f.write_str("JSON serialization failed"),
			Self::Deserialize(_) => f.write_str("JSON deserialization failed"),
			Self::Io(_) => f.write_str("JSON output failed"),
		}
	}
}
impl fmt::Debug for JsonError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		fmt::Display::fmt(self, f)
	}
}
impl std::error::Error for JsonError {}

/// Coarse failure classification of a `serde_json::Error` without input text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonFailureCategory {
	/// Underlying I/O read or write failure.
	Io,
	/// Malformed JSON syntax.
	Syntax,
	/// Valid JSON syntax with types or values that did not match the target schema.
	Data,
	/// Unexpected end of input before a complete JSON value finished.
	Eof,
}

impl From<serde_json::error::Category> for JsonFailureCategory {
	fn from(category: serde_json::error::Category) -> Self {
		match category {
			serde_json::error::Category::Io => Self::Io,
			serde_json::error::Category::Syntax => Self::Syntax,
			serde_json::error::Category::Data => Self::Data,
			serde_json::error::Category::Eof => Self::Eof,
		}
	}
}

/// Non-echoing summary of a `serde_json::Error` that records only its category and source position.
///
/// Its `Display` implementation writes `"{category:?} error at line {line}, column {column}"` and
/// omits the custom `serde_json` message so that stored or secret input values never appear in
/// diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JsonDecodeFailureSummary {
	category: JsonFailureCategory,
	line: usize,
	column: usize,
}

impl JsonDecodeFailureSummary {
	/// Summarizes `error` by its failure category and the line and column numbers that
	/// `serde_json` reports (`0` when `serde_json` reports no source position).
	pub fn from_serde_error(error: &serde_json::Error) -> Self {
		Self {
			category: JsonFailureCategory::from(error.classify()),
			line: error.line(),
			column: error.column(),
		}
	}

	/// Returns the coarse failure category.
	pub const fn category(&self) -> JsonFailureCategory {
		self.category
	}

	/// Returns the 1-based line number that `serde_json` reports, or `0` when `serde_json`
	/// reports no line position.
	pub const fn line(&self) -> usize {
		self.line
	}

	/// Returns the column number that `serde_json` reports, or `0` when `serde_json` reports no
	/// column position or stops at a newline boundary.
	pub const fn column(&self) -> usize {
		self.column
	}
}

impl From<&serde_json::Error> for JsonDecodeFailureSummary {
	fn from(error: &serde_json::Error) -> Self {
		Self::from_serde_error(error)
	}
}

impl fmt::Display for JsonDecodeFailureSummary {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(
			f,
			"{:?} error at line {}, column {}",
			self.category, self.line, self.column
		)
	}
}
