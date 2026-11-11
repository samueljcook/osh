use std::error::Error as StdError;
use std::fmt;

/// Errors returned by the CSRF module.
#[derive(Debug)]
pub enum Error {
	/// A cookie operation failed.
	Cookie(crate::http::cookies::manager::CookieError),
	/// Secure random generation of a token nonce failed.
	Crypto(crate::random::secure_bytes::RandomError),
	/// Encrypted cookie processing failed.
	EncryptedCookie(crate::http::cookies::manager::EncryptedCookieError),
	/// Output allocation failed.
	AllocationFailed,
	/// System time was before the Unix epoch.
	ClockBeforeUnixEpoch,
	/// A CSRF token expiration timestamp could not be represented.
	ExpirationOverflow,
	/// A CSRF origin or referer header could not be parsed.
	OriginParse {
		/// Header label.
		label: &'static str,
		/// Underlying URL parse error.
		source: url::ParseError,
	},
	/// A configured or request CSRF origin value exceeded the supported size.
	OriginTooLarge {
		/// Header or configuration label.
		label: &'static str,
		/// Origin byte length.
		actual: usize,
		/// Maximum supported byte length.
		max: usize,
	},
	/// A CSRF origin allowlist contained too many entries.
	AllowedOriginCountTooLarge {
		/// Number of configured entries.
		actual: usize,
		/// Maximum supported entry count.
		max: usize,
	},
	/// A CSRF origin or referer header did not contain a host.
	OriginMissingHost {
		/// Header label.
		label: &'static str,
	},
	/// A CSRF origin or referer used a scheme other than `http` or `https`.
	OriginUnsupportedScheme {
		/// Header label.
		label: &'static str,
		/// Scheme that was rejected.
		scheme: String,
	},
	/// A configured CSRF origin or request `Origin` header contained URL parts
	/// that are not part of an origin.
	OriginContainsNonOriginParts {
		/// Header label.
		label: &'static str,
	},
	/// Development-mode web helpers only accept loopback request authorities.
	DevModeNonLocalhostHost {
		/// Host header or URI authority that was rejected.
		host: String,
	},
	/// A development-mode request contained more than one `Host` field.
	DevModeDuplicateHostHeader,
	/// A development-mode `Host` field was not valid visible header text.
	DevModeHostHeaderDecode(http::header::ToStrError),
	/// A development-mode request had neither a `Host` field nor URI authority.
	DevModeRequestAuthorityMissing,
	/// A development-mode request authority was syntactically invalid.
	DevModeInvalidRequestAuthority {
		/// Authority source.
		label: &'static str,
	},
	/// A development-mode `Host` field and absolute URI named different authorities.
	DevModeRequestAuthorityConflict {
		/// Parsed `Host` field value.
		host_header: String,
		/// Parsed URI authority.
		uri_authority: String,
	},
	/// A CSRF header was not valid visible header text.
	HeaderDecode {
		/// Header label.
		label: &'static str,
		/// Underlying header decode error.
		source: http::header::ToStrError,
	},
	/// A present CSRF decision header was empty.
	EmptyHeader {
		/// Header label.
		label: &'static str,
	},
	/// A single-valued CSRF decision header was present more than once.
	DuplicateHeader {
		/// Header label.
		label: &'static str,
	},
	/// A CSRF origin or referer header was not in the allowlist.
	OriginNotAllowed {
		/// Normalized origin.
		origin: String,
	},
	/// A CSRF origin allowlist was configured, but the request had no Origin or Referer header.
	OriginAndRefererMissing,
	/// CSRF binding bytes must not be empty.
	EmptyBinding,
	/// CSRF binding bytes exceeded the supported size.
	BindingTooLarge {
		/// Requested CSRF binding byte length.
		actual: usize,
		/// Maximum supported CSRF binding byte length.
		max: usize,
	},
	/// A submitted CSRF token header was not valid visible header text.
	SubmittedTokenHeaderDecode(http::header::ToStrError),
	/// A submitted CSRF token header exceeded the supported size.
	SubmittedTokenTooLarge {
		/// Submitted token byte length.
		actual: usize,
		/// Maximum supported byte length.
		max: usize,
	},
	/// A CSRF request did not include a submitted token.
	TokenMissing,
	/// A CSRF cookie token and submitted token did not match.
	TokenMismatch,
	/// A CSRF token payload was invalid or expired.
	TokenInvalidOrExpired,
	/// A CSRF token was bound to different request identity bytes.
	BindingMismatch,
}

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Cookie(err) => write!(f, "{err}"),
			Self::EncryptedCookie(err) => write!(f, "{err}"),
			Self::Crypto(err) => write!(f, "{err}"),
			Self::AllocationFailed => write!(f, "primitives-rs: output allocation failed"),
			Self::ClockBeforeUnixEpoch => {
				write!(f, "primitives-rs: clock is before Unix epoch")
			}
			Self::ExpirationOverflow => {
				write!(f, "primitives-rs: csrf expiration overflow")
			}
			Self::OriginParse { label, source } => {
				write!(f, "primitives-rs: csrf {label} parse: {source}")
			}
			Self::OriginTooLarge { label, actual, max } => {
				write!(f, "primitives-rs: csrf {label} length {actual}, max {max}")
			}
			Self::AllowedOriginCountTooLarge { actual, max } => {
				write!(
					f,
					"primitives-rs: csrf allowed-origin count {actual}, max {max}"
				)
			}
			Self::OriginMissingHost { label } => {
				write!(f, "primitives-rs: csrf {label} is missing host")
			}
			Self::OriginUnsupportedScheme { label, scheme } => {
				write!(
					f,
					"primitives-rs: csrf {label} scheme must be http or https, got {scheme}"
				)
			}
			Self::OriginContainsNonOriginParts { label } => {
				write!(
					f,
					"primitives-rs: csrf {label} must be an origin without path, query, fragment, username, or password"
				)
			}
			Self::DevModeNonLocalhostHost { host } => {
				write!(
					f,
					"primitives-rs: development mode only accepts loopback request authorities, got {host}"
				)
			}
			Self::DevModeDuplicateHostHeader => {
				write!(
					f,
					"primitives-rs: development request has duplicate Host headers"
				)
			}
			Self::DevModeHostHeaderDecode(source) => {
				write!(f, "primitives-rs: development Host header decode: {source}")
			}
			Self::DevModeRequestAuthorityMissing => {
				write!(f, "primitives-rs: development request authority is missing")
			}
			Self::DevModeInvalidRequestAuthority { label } => {
				write!(f, "primitives-rs: development {label} authority is invalid")
			}
			Self::DevModeRequestAuthorityConflict {
				host_header,
				uri_authority,
			} => {
				write!(
					f,
					"primitives-rs: development Host header {host_header} conflicts with request URI authority {uri_authority}"
				)
			}
			Self::HeaderDecode { label, source } => {
				write!(f, "primitives-rs: csrf {label} header decode: {source}")
			}
			Self::EmptyHeader { label } => {
				write!(f, "primitives-rs: csrf {label} header is empty")
			}
			Self::DuplicateHeader { label } => {
				write!(
					f,
					"primitives-rs: csrf {label} header appeared more than once"
				)
			}
			Self::OriginNotAllowed { origin } => {
				write!(f, "primitives-rs: csrf origin not allowed: {origin}")
			}
			Self::OriginAndRefererMissing => {
				write!(f, "primitives-rs: csrf origin and referer are missing")
			}
			Self::EmptyBinding => write!(f, "primitives-rs: csrf binding is empty"),
			Self::BindingTooLarge { actual, max } => {
				write!(f, "primitives-rs: csrf binding length {actual}, max {max}")
			}
			Self::SubmittedTokenHeaderDecode(err) => {
				write!(
					f,
					"primitives-rs: csrf submitted token header decode: {err}"
				)
			}
			Self::SubmittedTokenTooLarge { actual, max } => {
				write!(
					f,
					"primitives-rs: csrf submitted token length {actual}, max {max}"
				)
			}
			Self::TokenMissing => write!(f, "primitives-rs: csrf token missing"),
			Self::TokenMismatch => write!(f, "primitives-rs: csrf token mismatch"),
			Self::TokenInvalidOrExpired => {
				write!(f, "primitives-rs: csrf token invalid or expired")
			}
			Self::BindingMismatch => write!(f, "primitives-rs: csrf binding mismatch"),
		}
	}
}

impl From<crate::http::cookies::manager::CookieError> for Error {
	fn from(value: crate::http::cookies::manager::CookieError) -> Self {
		Self::Cookie(value)
	}
}

impl From<crate::random::secure_bytes::RandomError> for Error {
	fn from(value: crate::random::secure_bytes::RandomError) -> Self {
		Self::Crypto(value)
	}
}

impl From<crate::http::cookies::manager::EncryptedCookieError> for Error {
	fn from(value: crate::http::cookies::manager::EncryptedCookieError) -> Self {
		Self::EncryptedCookie(value)
	}
}

impl StdError for Error {
	fn source(&self) -> Option<&(dyn StdError + 'static)> {
		match self {
			Self::Cookie(err) => Some(err),
			Self::EncryptedCookie(err) => Some(err),
			Self::Crypto(err) => Some(err),
			Self::HeaderDecode { source, .. } => Some(source),
			Self::SubmittedTokenHeaderDecode(err) => Some(err),
			Self::DevModeHostHeaderDecode(err) => Some(err),
			Self::OriginParse { source, .. } => Some(source),
			_ => None,
		}
	}
}
