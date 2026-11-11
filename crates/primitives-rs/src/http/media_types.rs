//! Media types (RFC 9110 section 8.3.1), such as the value of a
//! `Content-Type` field: parsing, the essence, parameters, and the media type
//! of a file name's extension.
//!
//! Enable the `media-types` Cargo feature for parsing. Enable
//! `media-types-by-extension` for `find_media_type_by_extension`.
//!
//! [`MediaType::parse`] checks the complete grammar, parameters included.
//! [`MediaTypeEssence::parse`] reads only the type and the subtype, for a
//! check that does not read the parameters, such as whether a request body is
//! JSON.
//!
//! ```
//! use primitives_rs::http::media_types::{MediaType, MediaTypeEssence};
//!
//! let essence = MediaTypeEssence::parse("Application/Problem+JSON; charset=utf-8")?;
//! assert_eq!(essence.as_str(), "application/problem+json");
//! assert!(essence.is_json());
//!
//! let media_type = MediaType::parse("multipart/form-data; Boundary=\"a b\"")?;
//! assert_eq!(media_type.essence().as_str(), "multipart/form-data");
//! assert_eq!(media_type.params()[0].name(), "boundary");
//! assert_eq!(media_type.params()[0].value(), "a b");
//! # Ok::<(), primitives_rs::http::media_types::MediaTypeError>(())
//! ```
//!
//! The parsers read text. A field value with bytes outside visible ASCII, which
//! `http::HeaderValue::to_str` refuses, must be decoded by the caller first.

use std::fmt;

/// A media type's type and subtype in ASCII lowercase, without parameters,
/// such as `text/html`. The WHATWG MIME Sniffing Standard calls this the
/// essence of a MIME type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaTypeEssence {
	/// `type/subtype` in ASCII lowercase.
	text: String,
}

impl MediaTypeEssence {
	/// Parses the type and the subtype at the start of a media type and does
	/// not read the text from the first `;` on.
	///
	/// Spaces and tabs before the type and after the subtype are ignored. The
	/// type and the subtype must each be an RFC 9110 token, and the essence
	/// stores them in ASCII lowercase because they are case-insensitive. The
	/// parameters are not checked, so a value with malformed parameters still
	/// has an essence; [`MediaType::parse`] checks them.
	///
	/// # Errors
	///
	/// [`MediaTypeError::InvalidEssence`] when the text before the first `;`
	/// is not a type and a subtype separated by `/`.
	pub fn parse(text: &str) -> Result<Self, MediaTypeError> {
		let essence = trim_ows(text.split(';').next().unwrap_or_default());
		let (kind, subtype) = essence
			.split_once('/')
			.ok_or(MediaTypeError::InvalidEssence)?;
		if !is_token(kind) || !is_token(subtype) {
			return Err(MediaTypeError::InvalidEssence);
		}
		Ok(Self {
			text: essence.to_ascii_lowercase(),
		})
	}

	/// The essence as `type/subtype` in ASCII lowercase.
	pub fn as_str(&self) -> &str {
		&self.text
	}

	/// Whether the media type is JSON: `application/json`, or a subtype with a
	/// non-empty name and the `+json` structured syntax suffix of RFC 6839,
	/// such as `application/merge-patch+json`. `text/json`, which IANA does
	/// not register, is not JSON.
	pub fn is_json(&self) -> bool {
		self.text == "application/json"
			|| self
				.text
				.split_once('/')
				.and_then(|(_, subtype)| subtype.strip_suffix("+json"))
				.is_some_and(|name| !name.is_empty())
	}
}

/// A media type with its parameters, as [`MediaType::parse`] reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaType {
	essence: MediaTypeEssence,
	params: Vec<MediaTypeParam>,
}

/// One `name=value` parameter of a [`MediaType`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaTypeParam {
	name: String,
	value: String,
}

impl MediaTypeParam {
	/// The parameter's name in ASCII lowercase because parameter names are
	/// case-insensitive.
	pub fn name(&self) -> &str {
		&self.name
	}

	/// The parameter's value in its own case. A quoted value is returned
	/// without its quotes and with each backslash escape replaced by the
	/// character that it escapes.
	pub fn value(&self) -> &str {
		&self.value
	}
}

impl MediaType {
	/// Parses a media type with the grammar of RFC 9110 sections 5.6 and
	/// 8.3.1: a type and a subtype separated by `/`, and parameters, each
	/// after a `;`.
	///
	/// The type, the subtype, and each parameter's name must be tokens. A
	/// parameter's value must be a token or a quoted string; a quoted string
	/// may contain backslash escapes and characters outside ASCII. Spaces and
	/// tabs are allowed before and after the media type and around each `;`.
	/// An empty parameter between two `;` is ignored, and so is an empty
	/// parameter after a trailing `;`, so `text/plain;` and `text/plain;;`
	/// have no parameters. No space is allowed around a parameter's `=`. A
	/// parameter name that repeats is kept each time, in the order of the text,
	/// and the caller decides what repetition means.
	///
	/// # Errors
	///
	/// [`MediaTypeError::InvalidEssence`] when the text before the first `;`
	/// is not a type and a subtype, and [`MediaTypeError::InvalidParams`] when
	/// a parameter is malformed or other text follows a parameter.
	pub fn parse(text: &str) -> Result<Self, MediaTypeError> {
		let text = trim_ows(text);
		let params_start = text.find(';').unwrap_or(text.len());
		let essence = MediaTypeEssence::parse(&text[..params_start])?;
		Ok(Self {
			essence,
			params: parse_params(&text[params_start..])?,
		})
	}

	/// The media type's type and subtype.
	pub fn essence(&self) -> &MediaTypeEssence {
		&self.essence
	}

	/// The parameters in the order of the text, with repeated names.
	pub fn params(&self) -> &[MediaTypeParam] {
		&self.params
	}
}

/// Why a text is not a media type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaTypeError {
	/// The text before the first `;` is not a type and a subtype, each a
	/// token, separated by `/`.
	InvalidEssence,
	/// A parameter is not a token name, `=`, and a token or quoted-string
	/// value, or other text follows a parameter before the next `;`.
	InvalidParams,
}

impl fmt::Display for MediaTypeError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::InvalidEssence => {
				"the media type does not start with a type and a subtype separated by `/`"
			}
			Self::InvalidParams => "a parameter of the media type is malformed",
		})
	}
}

impl std::error::Error for MediaTypeError {}

/// Parses the parameters of a media type: empty text, or text that starts
/// with `;`.
fn parse_params(text: &str) -> Result<Vec<MediaTypeParam>, MediaTypeError> {
	let mut params = Vec::new();
	let mut rest = text;
	while let Some(after_separator) = rest.strip_prefix(';') {
		let param_text = trim_leading_ows(after_separator);
		if param_text.is_empty() || param_text.starts_with(';') {
			rest = param_text;
			continue;
		}
		let (param, after_param) = parse_param(param_text)?;
		params.push(param);
		rest = trim_leading_ows(after_param);
	}
	if rest.is_empty() {
		Ok(params)
	} else {
		Err(MediaTypeError::InvalidParams)
	}
}

/// Parses one `name=value` parameter at the start of `text` and returns it
/// with the text that follows it.
fn parse_param(text: &str) -> Result<(MediaTypeParam, &str), MediaTypeError> {
	let name_end = text
		.find(|character: char| !is_token_char(character))
		.unwrap_or(text.len());
	let (name, after_name) = text.split_at(name_end);
	let value_text = after_name
		.strip_prefix('=')
		.filter(|_| !name.is_empty())
		.ok_or(MediaTypeError::InvalidParams)?;
	let (value, after_value) = match value_text.strip_prefix('"') {
		Some(quoted) => parse_quoted_string(quoted)?,
		None => {
			let value_end = value_text
				.find(|character: char| !is_token_char(character))
				.unwrap_or(value_text.len());
			if value_end == 0 {
				return Err(MediaTypeError::InvalidParams);
			}
			let (value, after_value) = value_text.split_at(value_end);
			(value.to_owned(), after_value)
		}
	};
	Ok((
		MediaTypeParam {
			name: name.to_ascii_lowercase(),
			value,
		},
		after_value,
	))
}

/// Reads the content of a quoted string that starts after its opening quote,
/// and returns the content without escapes and the text after the closing
/// quote.
fn parse_quoted_string(text: &str) -> Result<(String, &str), MediaTypeError> {
	let mut value = String::new();
	let mut characters = text.char_indices();
	while let Some((index, character)) = characters.next() {
		match character {
			'"' => return Ok((value, &text[index + 1..])),
			'\\' => match characters.next() {
				Some((_, escaped)) if is_quoted_pair_char(escaped) => value.push(escaped),
				_ => return Err(MediaTypeError::InvalidParams),
			},
			character if is_qdtext_char(character) => value.push(character),
			_ => return Err(MediaTypeError::InvalidParams),
		}
	}
	Err(MediaTypeError::InvalidParams)
}

/// Whether `text` is an RFC 9110 token: one or more token characters.
fn is_token(text: &str) -> bool {
	!text.is_empty() && text.chars().all(is_token_char)
}

fn is_token_char(character: char) -> bool {
	character.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(character)
}

/// Whether a character can appear unescaped in a quoted string: a tab, a
/// space, visible ASCII other than `"` and `\`, or a character outside ASCII,
/// which RFC 9110 calls obs-text.
fn is_qdtext_char(character: char) -> bool {
	matches!(character, '\t' | ' ' | '!' | '#'..='[' | ']'..='~') || !character.is_ascii()
}

/// Whether a character can follow `\` in a quoted string.
fn is_quoted_pair_char(character: char) -> bool {
	matches!(character, '\t' | ' '..='~') || !character.is_ascii()
}

/// `text` without the spaces and tabs at its start and end.
fn trim_ows(text: &str) -> &str {
	text.trim_matches([' ', '\t'])
}

fn trim_leading_ows(text: &str) -> &str {
	text.trim_start_matches([' ', '\t'])
}

/// The media types that the extension table holds where they differ from the
/// `mime_guess` crate's. Each is the type that IANA registers for the format.
/// Where IANA registers none, it is the type that browsers and servers
/// commonly use.
#[cfg(feature = "media-types-by-extension")]
const MEDIA_TYPES_BY_EXTENSION: [(&str, &str); 17] = [
	// IANA registers `text/javascript`; `application/javascript` is obsolete.
	("cjs", "text/javascript"),
	("mjs", "text/javascript"),
	// IANA registers `font/otf`, `font/woff`, and `font/collection`.
	("otc", "font/collection"),
	("otf", "font/otf"),
	("woff", "font/woff"),
	// IANA registers `image/vnd.microsoft.icon`.
	("ico", "image/vnd.microsoft.icon"),
	// IANA registers `application/xml` and `application/xslt+xml`.
	("xml", "application/xml"),
	("xsl", "application/xslt+xml"),
	("xslt", "application/xslt+xml"),
	// IANA registers `application/yaml` and `application/toml`.
	("toml", "application/toml"),
	("yaml", "application/yaml"),
	("yml", "application/yaml"),
	// IANA registers `video/matroska`.
	("mkv", "video/matroska"),
	// No IANA type exists for source maps and JSX; these are the common ones.
	("jsx", "text/jsx"),
	("map", "application/json"),
	// `.ts` and `.mts` name MPEG transport stream segments (`video/mp2t`), as
	// HLS uses them, given that a browser does not run TypeScript source.
	("mts", "video/mp2t"),
	("ts", "video/mp2t"),
];

/// The media type for the extension of a path's file name, or `None` when the
/// file name has no extension or the table has no entry for it.
///
/// The extension is the text after the last `.` of the file name, as
/// [`std::path::Path::extension`] defines it, compared without regard to ASCII
/// case. For a fixed set of extensions, the result is the type that IANA
/// registers for the format, or the type that browsers and servers commonly
/// use where IANA registers none, such as `text/javascript` for `.mjs` and
/// `video/mp2t` for `.ts`. For every other extension, the result is the first
/// type that the `mime_guess` crate lists, in ASCII lowercase. The function reads no file, so the result names
/// the format that the extension claims, not the format of the file's bytes.
/// A caller that sends a file decides what to send for `None`, such as
/// `application/octet-stream`.
#[cfg(feature = "media-types-by-extension")]
pub fn find_media_type_by_extension(path: &std::path::Path) -> Option<MediaTypeEssence> {
	let extension = path.extension()?.to_str()?.to_ascii_lowercase();
	let text = MEDIA_TYPES_BY_EXTENSION
		.iter()
		.find(|(listed, _)| *listed == extension)
		.map(|(_, media_type)| *media_type)
		.or_else(|| mime_guess::from_ext(&extension).first_raw())?;
	MediaTypeEssence::parse(text).ok()
}
