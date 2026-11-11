//! Bounded Cookie parsing and ordinary Set-Cookie validation and identity.

use cookie::{Cookie, SameSite};
use http::{HeaderMap, HeaderValue, header};
use std::fmt;
mod parsing;
pub use parsing::RequestCookies;

/// Maximum complete field-value size emitted by a managed cookie definition.
pub const MAX_SET_COOKIE_SIZE: usize = 4096;
/// Maximum aggregate request-cookie input size, including one byte per field.
pub const MAX_COOKIE_HEADER_SIZE: usize = 16 * 1024;
/// Browsers ignore a Path or Domain attribute value longer than this byte count.
const MAX_ATTRIBUTE_VALUE_SIZE: usize = 1024;

/// A cookie failure. Error messages and Debug never include cookie values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CookieError {
	/// A client-readable cookie cannot be HttpOnly.
	InvalidHttpOnly,
	/// A name is empty, contains a non-token character, or starts with a managed
	/// prefix in any ASCII case.
	InvalidName,
	/// A value contains a character outside the cookie value grammar.
	InvalidValue,
	/// A path is invalid, is longer than 1024 bytes, or contains an attribute
	/// delimiter or control character.
	InvalidPath,
	/// A domain is invalid, is longer than 1024 bytes, or contains an attribute
	/// delimiter or control character.
	InvalidDomain,
	/// A name with the `__Host-` or `__Secure-` prefix, in any ASCII case, lacks
	/// the attributes that browsers require for that prefix. Both prefixes
	/// require Secure, and `__Host-` also requires `Path=/` and no Domain.
	InvalidPrefixAttributes,
	/// Max-Age is zero or exceeds the signed whole-second range.
	InvalidMaxAge,
	/// SameSite=None requires Secure.
	SameSiteRequiresSecure,
	/// Serialized output exceeds the managed cookie's size bound.
	OutputTooLarge,
	/// A cookie value supplied for decoding exceeds the decoder's size bound.
	ValueTooLarge,
	/// Aggregate request input exceeds the parser's size bound.
	HeaderTooLarge,
	/// The Cookie fields contain the requested cookie with a value outside the
	/// cookie value grammar.
	MalformedHeader,
	/// More than one cookie has the requested name.
	DuplicateCookie,
	/// The cookie cannot be represented as an HTTP field value.
	InvalidHeaderValue,
}
impl fmt::Display for CookieError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::InvalidHttpOnly => "client-readable cookies cannot be HttpOnly",
			Self::InvalidName => {
				"cookie name is not a valid HTTP token or includes a managed prefix"
			}
			Self::InvalidValue => {
				"cookie value contains a character outside RFC 6265's cookie characters"
			}
			Self::InvalidPath => {
				"the Path is invalid, exceeds 1024 bytes, or contains an attribute delimiter or control character"
			}
			Self::InvalidDomain => {
				"the Domain is invalid, exceeds 1024 bytes, or contains an attribute delimiter or control character"
			}
			Self::InvalidPrefixAttributes => {
				"cookie attributes do not meet the requirements of the name's __Host- or __Secure- prefix"
			}
			Self::InvalidMaxAge => "cookie max-age must be a positive signed whole-second count",
			Self::SameSiteRequiresSecure => "SameSite=None requires Secure",
			Self::OutputTooLarge => "serialized cookie exceeds the size limit",
			Self::ValueTooLarge => "cookie value exceeds the size limit for decoding",
			Self::HeaderTooLarge => "request cookies exceed the parser size limit",
			Self::MalformedHeader => "the requested cookie has a malformed value",
			Self::DuplicateCookie => "duplicate requested cookie",
			Self::InvalidHeaderValue => "invalid Set-Cookie field value",
		})
	}
}
impl std::error::Error for CookieError {}

/// Checks raw cookie syntax and the attribute rules that browsers enforce,
/// without changing attributes, encoding, or size policy. Frameworks can accept
/// ordinary ecosystem cookies through this operation.
///
/// The name must be an HTTP token and the value must satisfy the cookie value
/// grammar. Path and Domain must contain no `;` or control character and must
/// have at most 1024 bytes. A name that starts
/// with `__Secure-` or `__Host-`, compared without regard to ASCII case,
/// requires Secure, and `__Host-` also requires `Path=/` and no Domain.
/// `SameSite=None` requires Secure. A cookie has Secure when the `cookie` crate
/// writes the Secure attribute for it: when Secure is set, when Partitioned is
/// set, or when SameSite is None and Secure is unset.
pub fn validate_set_cookie(cookie: &Cookie<'_>) -> Result<(), CookieError> {
	validate_name(cookie.name())?;
	if !cookie.value().bytes().all(is_value_byte) {
		return Err(CookieError::InvalidValue);
	}
	for (attribute, error) in [
		(cookie.path(), CookieError::InvalidPath),
		(cookie.domain(), CookieError::InvalidDomain),
	] {
		if attribute.is_some_and(|text| {
			text.len() > MAX_ATTRIBUTE_VALUE_SIZE
				|| text.chars().any(|c| c == ';' || c.is_control())
		}) {
			return Err(error);
		}
	}
	let secure = cookie.secure() == Some(true)
		|| cookie.partitioned() == Some(true)
		|| cookie.secure().is_none() && cookie.same_site() == Some(SameSite::None);
	let name = cookie.name();
	if (has_prefix_ignoring_ascii_case(name, "__Secure-") && !secure)
		|| (has_prefix_ignoring_ascii_case(name, "__Host-")
			&& !(secure && cookie.path() == Some("/") && cookie.domain().is_none()))
	{
		return Err(CookieError::InvalidPrefixAttributes);
	}
	if cookie.same_site() == Some(SameSite::None) && !secure {
		return Err(CookieError::SameSiteRequiresSecure);
	}
	HeaderValue::from_str(&cookie.to_string()).map_err(|_| CookieError::InvalidHeaderValue)?;
	Ok(())
}

/// Renders a raw cookie as a sensitive Set-Cookie value after syntax validation.
/// This operation does not apply a manager's policy or size bound.
pub fn to_set_cookie_header(cookie: &Cookie<'_>) -> Result<HeaderValue, CookieError> {
	validate_set_cookie(cookie)?;
	let mut value =
		HeaderValue::from_str(&cookie.to_string()).map_err(|_| CookieError::InvalidHeaderValue)?;
	value.set_sensitive(true);
	Ok(value)
}

/// Compares name, explicit path, normalized domain, and partitioned storage mode.
/// Missing path/domain attributes remain distinct from explicitly supplied ones.
/// Within one response, partitioned cookies share its browser-selected partition.
pub fn same_cookie_identity(first: &Cookie<'_>, second: &Cookie<'_>) -> bool {
	first.name() == second.name()
		&& first.path() == second.path()
		&& (first.partitioned() == Some(true)) == (second.partitioned() == Some(true))
		&& match (first.domain(), second.domain()) {
			(Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
			(None, None) => true,
			_ => false,
		}
}

pub(crate) fn validate_name(name: &str) -> Result<(), CookieError> {
	if name.is_empty()
		|| !name
			.bytes()
			.all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
	{
		return Err(CookieError::InvalidName);
	}
	Ok(())
}
/// Compares a cookie name prefix as browsers do, without regard to ASCII case.
pub(crate) fn has_prefix_ignoring_ascii_case(name: &str, prefix: &str) -> bool {
	name.get(..prefix.len())
		.is_some_and(|start| start.eq_ignore_ascii_case(prefix))
}
pub(crate) fn is_value_byte(byte: u8) -> bool {
	matches!(byte, 0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e)
}
#[cfg(feature = "cookies")]
pub(crate) fn is_attribute_byte(byte: u8) -> bool {
	matches!(byte, 0x20..=0x7e) && byte != b';'
}
