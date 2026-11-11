//! Serde JSON with explicit byte and container-depth limits.
//!
//! Limits are checked while serializing and before deserializing. No intermediate
//! `serde_json::Value` is required, so integer precision, borrowed values, custom
//! Serde validation, and representation attributes retain their ordinary behavior.
//! Pretty output accepts explicit spaces or tabs. File paths, permissions, and
//! atomic file replacement belong to the caller's filesystem operations.
//!
//! ```
//! use primitives_rs::serialization::json::{self, JsonFormat, JsonLimits, SerializeOptions};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let limits = JsonLimits { max_size: 1024, max_depth: 8 };
//! let text = json::serialize_to_string(&vec![1, 2], SerializeOptions {
//!     limits, format: JsonFormat::Pretty { indent: "\t" },
//! })?;
//! assert_eq!(text, "[\n\t1,\n\t2\n]");
//! let values: Vec<u32> = json::deserialize_slice(text.as_bytes(), limits)?;
//! assert_eq!(values, [1, 2]);
//! # Ok(())
//! # }
//! ```

use serde::{Deserialize, Serialize};
use std::io::Write;
mod buffer;
mod error;
mod finite_floats;
mod limits;
mod safe_integer;
pub use error::{JsonDecodeFailureSummary, JsonError, JsonFailureCategory};
pub use finite_floats::FiniteFloats;
use limits::{DepthScanner, LimitedJsonWriter};
pub use safe_integer::{JsSafeI64, JsSafeIntegerError, JsSafeU64};

/// Bounds on one JSON document, including whitespace and delimiters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JsonLimits {
	/// Maximum encoded byte length.
	pub max_size: usize,
	/// Maximum nested arrays or objects along a path. Scalars have depth zero.
	/// Choose a value appropriate for the deserializer's stack and value types.
	pub max_depth: usize,
}

/// Whitespace formatting for JSON serialization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonFormat<'a> {
	/// Emits no optional whitespace.
	Compact,
	/// Emits line breaks and repeats `indent` for each nesting level.
	Pretty {
		/// Spaces and tabs only. Empty indentation still emits line breaks.
		indent: &'a str,
	},
}

/// Explicit serialization limits and formatting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SerializeOptions<'a> {
	/// Byte and container-depth limits.
	pub limits: JsonLimits,
	/// Compact or explicitly indented output.
	pub format: JsonFormat<'a>,
}

/// Serializes a value once into a writer while enforcing the supplied limits.
///
/// An error may leave partial JSON in the writer. This function does not flush
/// the writer. Serde callbacks execute application code; the limits constrain
/// their JSON output, not arbitrary work inside those callbacks.
pub fn serialize_to_writer<W: Write + ?Sized, T: Serialize + ?Sized>(
	writer: &mut W,
	value: &T,
	options: SerializeOptions<'_>,
) -> Result<(), JsonError> {
	if let JsonFormat::Pretty { indent } = options.format
		&& !indent.bytes().all(|byte| matches!(byte, b' ' | b'\t'))
	{
		return Err(JsonError::InvalidIndentation);
	}
	let mut writer = LimitedJsonWriter::new(writer, options.limits);
	let result = match options.format {
		JsonFormat::Compact => value.serialize(&mut serde_json::Serializer::new(&mut writer)),
		JsonFormat::Pretty { indent } => {
			value.serialize(&mut serde_json::Serializer::with_formatter(
				&mut writer,
				serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes()),
			))
		}
	};
	writer.finish(result)
}

/// Serializes a complete document into an owned byte vector.
pub fn serialize_to_vec<T: Serialize + ?Sized>(
	value: &T,
	options: SerializeOptions<'_>,
) -> Result<Vec<u8>, JsonError> {
	buffer::serialize(value, options, buffer::BufferZeroization::Disabled)
}

/// Serializes a complete document into an owned UTF-8 string.
pub fn serialize_to_string<T: Serialize + ?Sized>(
	value: &T,
	options: SerializeOptions<'_>,
) -> Result<String, JsonError> {
	Ok(String::from_utf8(serialize_to_vec(value, options)?).expect("serde_json writes UTF-8"))
}

/// Serializes into a secret buffer, clearing failed and replaced allocations.
///
/// The input value remains the caller's responsibility. Error formatting omits
/// custom Serde messages, which can contain plaintext.
#[cfg(feature = "json-secret")]
pub fn serialize_to_secret_bytes<T: Serialize + ?Sized>(
	value: &T,
	options: SerializeOptions<'_>,
) -> Result<crate::secret_bytes::SecretBytes, JsonError> {
	buffer::serialize(value, options, buffer::BufferZeroization::Enabled)
		.map(crate::secret_bytes::SecretBytes::from_vec)
}

/// Parses exactly one complete document, preserving borrowed deserialization.
///
/// Size and depth are checked before Serde runs. The explicit depth limit
/// replaces Serde JSON's fixed recursion limit. The caller must choose a limit
/// appropriate for its stack, custom deserializers, and recursive value types.
/// Trailing whitespace is accepted and counts toward the byte limit; trailing
/// non-whitespace input is rejected.
pub fn deserialize_slice<'de, T: Deserialize<'de>>(
	bytes: &'de [u8],
	limits: JsonLimits,
) -> Result<T, JsonError> {
	if bytes.len() > limits.max_size {
		return Err(JsonError::TooLarge {
			actual_minimum: bytes.len(),
			max: limits.max_size,
		});
	}
	DepthScanner::new(limits.max_depth).check(bytes)?;
	let mut deserializer = serde_json::Deserializer::from_slice(bytes);
	deserializer.disable_recursion_limit();
	let value = T::deserialize(&mut deserializer).map_err(JsonError::Deserialize)?;
	deserializer.end().map_err(JsonError::Deserialize)?;
	Ok(value)
}

/// Deserializes an explicit JSON `null` or value into `Option<T>` when a field must be present.
///
/// Use `#[serde(deserialize_with = "deserialize_required_option")]` without `#[serde(default)]`
/// so that Serde rejects an omitted object property while accepting `"field": null` as `Ok(None)`.
pub fn deserialize_required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
	D: serde::Deserializer<'de>,
	T: Deserialize<'de>,
{
	Option::<T>::deserialize(deserializer)
}

/// Returns whether lexical JSON scanning of `bytes` encounters nested arrays or objects deeper
/// than `max_depth`.
///
/// Brackets inside JSON string literals are ignored.
pub fn exceeds_max_nesting_depth(bytes: &[u8], max_depth: usize) -> bool {
	DepthScanner::new(max_depth).check(bytes).is_err()
}
