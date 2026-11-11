//! Ordered form fields, bounded query decoding, and Serde conversion.
//!
//! Enable `form-data` for URL-encoded query strings and bodies. Enable
//! `form-data-multipart` for async body decoding and disk-backed uploads on Tokio.
//! No request method, routing rule, reserved name, or HTTP response policy is
//! imposed. Applications choose which source to parse and how to report errors.
//!
//! ```
//! use primitives_rs::http::form_data::{FormData, FormLimits};
//! use serde::Deserialize;
//!
//! #[derive(Deserialize)]
//! struct Search { term: String, #[serde(default)] tag: Vec<String> }
//! let form = FormData::parse_url_encoded(b"term=rust+forms&tag=a&tag=b", FormLimits {
//!     max_size: 4096, max_parts: Some(20), max_text_size: Some(1024),
//!     max_file_size: None,
//! })?;
//! let search: Search = form.into_deserialized()?;
//! assert_eq!(search.tag, ["a", "b"]);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod content_type;
mod deserialize;
#[cfg(feature = "form-data-multipart")]
mod multipart;

pub use content_type::{FormContentType, FormContentTypeError};
pub use deserialize::FormDeserializeError;
#[cfg(feature = "form-data-multipart")]
pub use multipart::{FormDecodeConfig, FormDecodeError, UploadedFile, decode_form_data};

/// Resource limits for decoding one form or query string.
///
/// Sizes are byte counts. `max_size` always bounds the encoded input, including
/// multipart headers, boundaries, and epilogue. Optional limits add narrower
/// restrictions; `None` leaves that restriction to the total input bound.
#[derive(Clone, Copy, Debug)]
pub struct FormLimits {
	/// Maximum encoded input size.
	pub max_size: usize,
	/// Maximum number of parts, including repeated names and files. Each
	/// multipart file part creates one temporary file, so this limit also bounds
	/// the number of temporary files that one form creates. Without it, only
	/// `max_size` bounds that number, as the documentation of `decode_form_data`
	/// describes.
	pub max_parts: Option<usize>,
	/// Maximum size of each decoded UTF-8 text value and its raw multipart content.
	/// URL-encoded values are checked after decoding. Encoded names contribute
	/// to `max_size`; this limit applies to values.
	pub max_text_size: Option<usize>,
	/// Maximum size of each multipart file's contents.
	pub max_file_size: Option<usize>,
}

/// The resource that exceeded a form limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormLimit {
	/// The complete encoded input size.
	InputSize,
	/// The number of submitted parts.
	PartCount,
	/// One text value's size.
	TextSize,
	/// One uploaded file's size.
	FileSize,
}

/// A form exceeded an explicit resource limit. Submitted data is not retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FormLimitExceeded {
	/// The resource that exceeded its limit.
	pub resource: FormLimit,
	/// The configured maximum count or size.
	pub max: usize,
}
impl std::fmt::Display for FormLimitExceeded {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(
			f,
			"form {:?} exceeds its limit of {}",
			self.resource, self.max
		)
	}
}
impl std::error::Error for FormLimitExceeded {}

pub(super) fn check_limit(
	size: usize,
	max: Option<usize>,
	resource: FormLimit,
) -> Result<(), FormLimitExceeded> {
	if let Some(max) = max
		&& size > max
	{
		return Err(FormLimitExceeded { resource, max });
	}
	Ok(())
}

/// One named form value. Empty names and repeated names are preserved.
///
/// `Debug` shows the name, the value's kind, and a text value's byte length,
/// but no submitted value.
pub struct FormEntry {
	/// The submitted field name, without interpreting dots or brackets.
	pub name: String,
	/// The submitted text or file.
	pub value: FormValue,
}

/// A text value or one owned uploaded file.
///
/// The `File` variant exists only with the `form-data-multipart` feature, so a
/// `match` outside this crate needs a wildcard arm. `Debug` shows the kind and
/// a text value's byte length, but no submitted value.
#[non_exhaustive]
pub enum FormValue {
	/// Decoded text.
	Text(String),
	/// A file part, identified by a multipart `filename` or `filename*` parameter.
	#[cfg(feature = "form-data-multipart")]
	File(UploadedFile),
}

/// Form values in submission order, including repeated and interleaved names.
///
/// The entries own their files. Dropping an entry deletes its temporary file;
/// moving an entry or deserializing it transfers that ownership. The original
/// encoded body is not retained. Parsing enforces limits; constructing or
/// modifying entries directly does not enforce parsing limits. `Debug` shows
/// names, kinds, and text byte lengths, but no submitted value.
pub struct FormData {
	/// The ordered submitted entries.
	pub entries: Vec<FormEntry>,
}

impl std::fmt::Debug for FormData {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("FormData")
			.field("entries", &self.entries)
			.finish()
	}
}

impl std::fmt::Debug for FormEntry {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("FormEntry")
			.field("name", &self.name)
			.field("value", &self.value)
			.finish()
	}
}

impl std::fmt::Debug for FormValue {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Text(text) => f.debug_struct("Text").field("len", &text.len()).finish(),
			#[cfg(feature = "form-data-multipart")]
			Self::File(_) => f.debug_struct("File").finish_non_exhaustive(),
		}
	}
}

impl FormData {
	/// Parses a query string without its leading `?`, or a URL-encoded body.
	///
	/// This follows the URL Standard: `+` means space, percent escapes decode
	/// bytes, malformed escapes remain literal, invalid UTF-8 becomes U+FFFD,
	/// empty segments are ignored, and a missing `=` means an empty value.
	/// A field named `__proto__`, a dotted name, and an empty name are ordinary
	/// names. No application-specific query protocol is inferred.
	pub fn parse_url_encoded(input: &[u8], limits: FormLimits) -> Result<Self, FormLimitExceeded> {
		check_limit(input.len(), Some(limits.max_size), FormLimit::InputSize)?;
		let mut entries = Vec::new();
		for (name, value) in form_urlencoded::parse(input) {
			check_limit(entries.len() + 1, limits.max_parts, FormLimit::PartCount)?;
			check_limit(value.len(), limits.max_text_size, FormLimit::TextSize)?;
			entries.push(FormEntry {
				name: name.into_owned(),
				value: FormValue::Text(value.into_owned()),
			});
		}
		Ok(Self { entries })
	}

	/// Encodes all text entries as a query string or URL-encoded body in entry
	/// order. Files cannot be represented and cause an error. Spaces become `+`.
	pub fn to_url_encoded(&self) -> Result<String, FormEncodeError> {
		let mut encoded = form_urlencoded::Serializer::new(String::new());
		for entry in &self.entries {
			match &entry.value {
				FormValue::Text(text) => {
					encoded.append_pair(&entry.name, text);
				}
				#[cfg(feature = "form-data-multipart")]
				FormValue::File(_) => return Err(FormEncodeError::FileRequiresMultipart),
			}
		}
		Ok(encoded.finish())
	}
}

/// Why form entries cannot be URL-encoded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormEncodeError {
	/// A file needs multipart encoding.
	FileRequiresMultipart,
}
impl std::fmt::Display for FormEncodeError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("a file cannot be URL-encoded")
	}
}
impl std::error::Error for FormEncodeError {}
