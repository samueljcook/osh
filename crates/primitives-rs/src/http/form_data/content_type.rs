use http::{HeaderMap, header::CONTENT_TYPE};

use crate::http::media_types::{MediaType, MediaTypeEssence};

/// A validated URL-encoded or multipart form content type.
#[derive(Clone, Debug)]
pub struct FormContentType(pub(super) FormEncoding);

#[derive(Clone, Debug)]
pub(super) enum FormEncoding {
	UrlEncoded,
	Multipart { boundary: String },
}

/// Why a request's headers do not identify one supported form encoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormContentTypeError {
	/// No content type is present, or the content type does not name a form.
	Unsupported,
	/// A form content type or multipart boundary is malformed or ambiguous.
	Invalid,
}
impl std::fmt::Display for FormContentTypeError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(match self {
			Self::Unsupported => "the request does not have a form content type",
			Self::Invalid => "the form content type is malformed or ambiguous",
		})
	}
}
impl std::error::Error for FormContentTypeError {}

impl FormContentType {
	/// Parses exactly one `Content-Type` header. Duplicate headers are invalid.
	/// A value with a type and subtype that do not name a form is unsupported. A
	/// form content type must be a media type with valid parameters, as
	/// [`MediaType::parse`] checks it. A multipart boundary must be unique
	/// and satisfy RFC 2046's 1-to-70-byte boundary grammar. URL-encoded text
	/// uses UTF-8 regardless of a supplied charset parameter.
	///
	/// A multipart value that contains a backslash anywhere is invalid. RFC
	/// 9110 reads a backslash in a quoted value as an escape and removes it,
	/// but other readers of the same header keep it, such as the `multer`
	/// crate's `parse_boundary` function, which axum's multipart extractor
	/// uses, and the default `compat32` policy of Python's `email` package. A
	/// component in front of the application that reads the header that way
	/// finds a different boundary, or finds `boundary=` text inside a value
	/// that this function reads as escaped, and so splits the body into
	/// different parts. A boundary never needs an escape because the RFC 2046
	/// boundary grammar has no backslash.
	pub fn from_headers(headers: &HeaderMap) -> Result<Self, FormContentTypeError> {
		let mut values = headers.get_all(CONTENT_TYPE).iter();
		let Some(value) = values.next() else {
			return Err(FormContentTypeError::Unsupported);
		};
		if values.next().is_some() {
			return Err(FormContentTypeError::Invalid);
		}
		let text = value.to_str().map_err(|_| FormContentTypeError::Invalid)?;
		let essence =
			MediaTypeEssence::parse(text).map_err(|_| FormContentTypeError::Unsupported)?;
		let is_url_encoded = essence.as_str() == "application/x-www-form-urlencoded";
		if !is_url_encoded && essence.as_str() != "multipart/form-data" {
			return Err(FormContentTypeError::Unsupported);
		}
		if !is_url_encoded && text.contains('\\') {
			return Err(FormContentTypeError::Invalid);
		}
		let media_type = MediaType::parse(text).map_err(|_| FormContentTypeError::Invalid)?;
		if is_url_encoded {
			return Ok(Self(FormEncoding::UrlEncoded));
		}
		let mut boundaries = media_type
			.params()
			.iter()
			.filter(|param| param.name() == "boundary");
		let boundary = boundaries
			.next()
			.ok_or(FormContentTypeError::Invalid)?
			.value();
		if boundaries.next().is_some()
			|| boundary.is_empty()
			|| boundary.len() > 70
			|| boundary.ends_with(' ')
			|| !boundary
				.bytes()
				.all(|b| b.is_ascii_alphanumeric() || b"'()+_,-./:=? ".contains(&b))
		{
			return Err(FormContentTypeError::Invalid);
		}
		Ok(Self(FormEncoding::Multipart {
			boundary: boundary.to_owned(),
		}))
	}

	/// Whether decoding requires multipart support.
	pub fn is_multipart(&self) -> bool {
		matches!(self.0, FormEncoding::Multipart { .. })
	}

	/// The validated multipart boundary, or `None` for URL-encoded data.
	pub fn boundary(&self) -> Option<&str> {
		match &self.0 {
			FormEncoding::Multipart { boundary } => Some(boundary),
			FormEncoding::UrlEncoded => None,
		}
	}
}
