//! Canonical URL-path wire identity shared by server dispatch and browser matching.
//!
//! Pattern declarations and the values of params and splats are logical Unicode strings; HTTP and
//! browser matchers operate on one canonical percent-encoded path spelling. The
//! conversion operates on individual segments, so an encoded slash remains data
//! in one segment instead of becoming a routing separator. Param and splat
//! values are decoded only after a pattern has been selected.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

use std::fmt;

const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// Prefix of a dynamic segment in Vorma's patterns, as in `/users/:user_id`.
pub const DYNAMIC_PARAM_PREFIX: char = ':';

/// A complete splat segment in Vorma's patterns, as in `/files/*`.
pub const SPLAT_SEGMENT: char = '*';

/// A complete final segment that identifies a view pattern's index, as in `/_index`.
pub const INDEX_SEGMENT: &str = "_index";

/// Convert an absolute request pathname into Vorma's canonical matcher wire space.
///
/// The function splits on literal `/` before decoding. Every segment is then
/// strictly percent-decoded, checked as UTF-8, and re-encoded with uppercase
/// escapes. Consequently `%2F` remains data inside one segment. Empty segments
/// remain empty; path matching applies its own normalization rules.
pub fn canonicalize_request_path(path: &str) -> Result<String, PathWireError> {
	if !path.starts_with('/') {
		return Err(PathWireError::PathMustBeAbsolute);
	}

	let mut encoded = String::with_capacity(path.len());
	for (index, segment) in path.split('/').enumerate() {
		if index > 0 {
			encoded.push('/');
		}
		if segment.is_empty() {
			continue;
		}
		let decoded = decode_percent_segment(segment)?;
		encode_bytes_into(decoded.as_bytes(), &mut encoded);
	}
	Ok(encoded)
}

/// Convert one logical pattern declaration into canonical matcher wire text.
///
/// `:name` and `*` segments remain matcher grammar. Static segments are
/// encoded as logical text, so a literal `%` in a declaration means a literal
/// percent sign rather than an alternate spelling of some other pattern. Static
/// `.` and `..` segments are rejected because browsers erase them during URL
/// resolution. Param and splat values have the same restriction.
pub fn canonicalize_pattern(pattern: &str) -> Result<String, PathWireError> {
	if !pattern.starts_with('/') {
		return Err(PathWireError::PathMustBeAbsolute);
	}

	let mut encoded = String::with_capacity(pattern.len());
	for (index, segment) in pattern.split('/').enumerate() {
		if index > 0 {
			encoded.push('/');
		}
		if segment.is_empty()
			|| segment.strip_prefix(SPLAT_SEGMENT) == Some("")
			|| segment.starts_with(DYNAMIC_PARAM_PREFIX)
		{
			encoded.push_str(segment);
			continue;
		}
		if segment == "." || segment == ".." {
			return Err(PathWireError::StaticDotSegment {
				segment: segment.to_owned(),
			});
		}
		encode_bytes_into(segment.as_bytes(), &mut encoded);
	}
	Ok(encoded)
}

/// Encode one non-empty logical dynamic or splat value as a single wire segment.
///
/// A slash in `value` becomes `%2F`; it never creates an additional path
/// segment. Exact `.` and `..` values are rejected because URL
/// parsing treats them as navigation segments.
pub fn encode_path_segment(value: &str) -> Result<String, PathWireError> {
	if value.is_empty() {
		return Err(PathWireError::EmptySegment);
	}
	match value {
		"." | ".." => Err(PathWireError::DotSegment {
			value: value.to_owned(),
		}),
		_ => {
			let mut encoded = String::with_capacity(value.len());
			encode_bytes_into(value.as_bytes(), &mut encoded);
			Ok(encoded)
		}
	}
}

/// Decode one matched canonical wire segment back into its logical value.
pub fn decode_path_segment(segment: &str) -> Result<String, PathWireError> {
	if segment.is_empty() {
		return Err(PathWireError::EmptySegment);
	}
	if segment.contains('/') {
		return Err(PathWireError::SegmentContainsLiteralSlash);
	}
	let value = decode_percent_segment(segment)?;
	if value == "." || value == ".." {
		return Err(PathWireError::DotSegment { value });
	}
	Ok(value)
}

/// A pathname, a param, or a splat value could not be represented in the canonical wire space.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PathWireError {
	/// A request path or pattern did not begin with `/`.
	PathMustBeAbsolute,
	/// A percent sign was not followed by two hexadecimal digits.
	MalformedPercentEncoding {
		/// Segment containing the malformed escape.
		segment: String,
		/// Byte offset of the malformed percent sign within the segment.
		byte_offset: usize,
	},
	/// Percent decoding did not produce valid UTF-8.
	InvalidUtf8 {
		/// Segment with decoded bytes that are not valid UTF-8.
		segment: String,
	},
	/// Params and individual splat values cannot be empty.
	EmptySegment,
	/// A param or splat value is a URL navigation segment that URL parsing would erase.
	DotSegment {
		/// Rejected logical value, either `.` or `..`.
		value: String,
	},
	/// A function expecting one wire segment received a literal `/` separator.
	SegmentContainsLiteralSlash,
	/// A static `.` or `..` declaration would be erased by URL normalization.
	StaticDotSegment {
		/// Rejected static segment.
		segment: String,
	},
}

impl fmt::Display for PathWireError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::PathMustBeAbsolute => f.write_str("path must begin with '/'"),
			Self::MalformedPercentEncoding {
				segment,
				byte_offset,
			} => write!(
				f,
				"malformed percent encoding at byte {byte_offset} of path segment {segment:?}"
			),
			Self::InvalidUtf8 { segment } => {
				write!(f, "percent-decoded path segment is not UTF-8: {segment:?}")
			}
			Self::EmptySegment => f.write_str("path segment cannot be empty"),
			Self::DotSegment { value } => write!(
				f,
				"path segment {value:?} would be erased by URL normalization"
			),
			Self::SegmentContainsLiteralSlash => {
				f.write_str("one path wire segment cannot contain a literal '/' separator")
			}
			Self::StaticDotSegment { segment } => write!(
				f,
				"static path segment {segment:?} is erased by URL dot-segment normalization"
			),
		}
	}
}

impl std::error::Error for PathWireError {}

fn decode_percent_segment(segment: &str) -> Result<String, PathWireError> {
	let bytes = segment.as_bytes();
	let mut decoded = Vec::with_capacity(bytes.len());
	let mut index = 0;
	while index < bytes.len() {
		if bytes[index] != b'%' {
			decoded.push(bytes[index]);
			index += 1;
			continue;
		}
		let Some(high) = bytes
			.get(index + 1)
			.and_then(|byte| decode_hex_digit(*byte))
		else {
			return Err(PathWireError::MalformedPercentEncoding {
				segment: segment.to_owned(),
				byte_offset: index,
			});
		};
		let Some(low) = bytes
			.get(index + 2)
			.and_then(|byte| decode_hex_digit(*byte))
		else {
			return Err(PathWireError::MalformedPercentEncoding {
				segment: segment.to_owned(),
				byte_offset: index,
			});
		};
		decoded.push((high << 4) | low);
		index += 3;
	}
	String::from_utf8(decoded).map_err(|_| PathWireError::InvalidUtf8 {
		segment: segment.to_owned(),
	})
}

fn decode_hex_digit(byte: u8) -> Option<u8> {
	match byte {
		b'0'..=b'9' => Some(byte - b'0'),
		b'a'..=b'f' => Some(byte - b'a' + 10),
		b'A'..=b'F' => Some(byte - b'A' + 10),
		_ => None,
	}
}

fn encode_bytes_into(bytes: &[u8], encoded: &mut String) {
	for &byte in bytes {
		if is_wire_safe(byte) {
			encoded.push(char::from(byte));
		} else {
			encoded.push('%');
			encoded.push(char::from(HEX[(byte >> 4) as usize]));
			encoded.push(char::from(HEX[(byte & 0x0f) as usize]));
		}
	}
}

fn is_wire_safe(byte: u8) -> bool {
	byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+')
}
