//! Managed plaintext and encrypted cookies with shared policy and live keys.
//!
//! The manager applies per-cookie overrides, host-only or explicit scope, and
//! matching creation/deletion attributes. HttpOnly controls browser access
//! independently of encryption; CSRF cookies contain encrypted values that
//! browser code submits without decrypting them.
//!
//! Encrypted values use the shared encryption envelope with one fixed child
//! keyset of the manager's keyset, so every encrypted-cookie definition of
//! every manager with the same keyset uses the same child keyset. The
//! envelope's associated data binds a fixed protocol label, the final cookie
//! name including any managed prefix, the path, the normalized domain (empty
//! without a Domain attribute), whether the name has a managed host-only
//! prefix, and whether the cookie is partitioned. It does not bind the request
//! host, the Secure flag, SameSite, HttpOnly, Max-Age, or the value's Rust
//! type. A host-only cookie therefore has the same associated data on every
//! host, and a value that one host issued decrypts on another host that uses
//! the same keyset and defines a cookie with the same name and partition mode.
//! An application keyset must not be shared between applications or hosts that
//! define cookies with the same name.
//!
//! ```
//! use primitives_rs::http::cookies::manager::*;
//! use primitives_rs::crypto::{Key32, keysets::derive_keyset_from_latest_first_keys};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let keys = derive_keyset_from_latest_first_keys([Key32::generate()?], "app.cookies")?;
//! let manager = CookieManager::new(CookieManagerConfig {
//!     keys: CookieKeysetSource::from_keyset(keys), security: CookieSecurity::Https,
//!     default_same_site: SameSite::Lax, default_http_only: true, default_partitioned: false,
//! })?;
//! let cookie = manager.define_encrypted_cookie::<String>(CookieConfig {
//!     name: "preferences".into(), scope: CookieScope::HostOnly, max_age: None,
//!     same_site: None, http_only: None, partitioned: None,
//! })?;
//! let emitted = cookie.create_cookie(&"dark".to_owned())?;
//! assert_eq!(cookie.decode_value(emitted.value())?, "dark");
//! assert_eq!(cookie.definition().deletion_cookie().path(), emitted.path());
//! # Ok(())
//! # }
//! ```

pub use crate::http::cookies::headers::{
	CookieError, MAX_COOKIE_HEADER_SIZE, MAX_SET_COOKIE_SIZE, RequestCookies,
};
use crate::http::cookies::headers::{
	has_prefix_ignoring_ascii_case, is_attribute_byte, is_value_byte, validate_name,
	validate_set_cookie,
};
use cookie::Cookie;
pub use cookie::SameSite;
use http::HeaderMap;
use std::num::NonZeroU64;
mod encrypted;
pub use encrypted::{CookieKeysetSource, EncryptedCookieDefinition, EncryptedCookieError};
mod plaintext;
pub use plaintext::PlaintextCookieDefinition;

/// A positive persistent cookie lifetime, expressed in whole seconds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CookieMaxAge(NonZeroU64);
impl CookieMaxAge {
	/// Parses a positive duration that fits the cookie crate's signed seconds.
	pub fn from_secs(secs: u64) -> Result<Self, CookieError> {
		let value = NonZeroU64::new(secs)
			.filter(|n| n.get() <= i64::MAX as u64)
			.ok_or(CookieError::InvalidMaxAge)?;
		Ok(Self(value))
	}
	/// Returns the whole-second lifetime.
	pub fn as_secs(self) -> u64 {
		self.0.get()
	}
}

/// Transport policy for managed cookies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CookieSecurity {
	/// Emits Secure cookies and the standard host-only prefix.
	Https,
	/// Emits development cookies for loopback HTTP. The manager adapts security
	/// attributes for this transport: it emits no Secure or Partitioned
	/// attribute, even when a definition or the manager's default setting
	/// configures partitioned storage, and it uses the `__Dev-` prefix in place
	/// of `__Host-`.
	LoopbackHttp,
}

/// Shared policy. Every application-level default is explicit.
#[derive(Clone, Debug)]
pub struct CookieManagerConfig {
	/// Live application-purpose keys shared by encrypted cookie definitions.
	pub keys: CookieKeysetSource,
	/// The transport policy for this manager's lifetime.
	pub security: CookieSecurity,
	/// The SameSite attribute for definitions without an override.
	pub default_same_site: SameSite,
	/// Whether definitions without an override forbid JavaScript access.
	pub default_http_only: bool,
	/// Whether definitions without an override use partitioned storage. Under
	/// [`CookieSecurity::LoopbackHttp`], the manager emits no Partitioned
	/// attribute regardless of this setting.
	pub default_partitioned: bool,
}

/// Immutable shared cookie policy.
#[derive(Clone, Debug)]
pub struct CookieManager {
	config: CookieManagerConfig,
}
impl CookieManager {
	/// Validates shared policy without reading keys or starting runtime work.
	pub fn new(config: CookieManagerConfig) -> Result<Self, CookieError> {
		if config.security == CookieSecurity::LoopbackHttp
			&& config.default_same_site == SameSite::None
		{
			return Err(CookieError::SameSiteRequiresSecure);
		}
		Ok(Self { config })
	}
	/// Returns the manager's transport policy.
	pub fn security(&self) -> CookieSecurity {
		self.config.security
	}
	/// Defines an encrypted cookie with this manager's policy and live keys.
	pub fn define_encrypted_cookie<T>(
		&self,
		config: CookieConfig,
	) -> Result<EncryptedCookieDefinition<T>, CookieError> {
		Ok(EncryptedCookieDefinition::new(
			self.define_cookie(config)?,
			self.config.keys.clone(),
		))
	}
	/// Defines a plaintext cookie that browser JavaScript can read.
	/// An explicit HttpOnly=true setting is rejected.
	pub fn define_client_readable_cookie<T>(
		&self,
		mut config: CookieConfig,
	) -> Result<PlaintextCookieDefinition<T>, CookieError> {
		if config.http_only == Some(true) {
			return Err(CookieError::InvalidHttpOnly);
		}
		config.http_only = Some(false);
		Ok(PlaintextCookieDefinition::new(self.define_cookie(config)?))
	}
	/// Resolves policy and validates a cookie's complete creation/deletion scope.
	/// Values use the caller's encoding without additional encryption. Use this operation
	/// for protocol values that are already protected, or use `define_encrypted_cookie`
	/// when the manager must encrypt typed values.
	pub fn define_cookie(&self, config: CookieConfig) -> Result<CookieDefinition, CookieError> {
		validate_name(&config.name)?;
		if ["__Host-", "__Dev-", "__Secure-"]
			.iter()
			.any(|prefix| has_prefix_ignoring_ascii_case(&config.name, prefix))
		{
			return Err(CookieError::InvalidName);
		}
		let secure = self.config.security == CookieSecurity::Https;
		let same_site = config.same_site.unwrap_or(self.config.default_same_site);
		if !secure && same_site == SameSite::None {
			return Err(CookieError::SameSiteRequiresSecure);
		}
		let (name, path, domain) = match config.scope {
			CookieScope::HostOnly => (
				format!(
					"{}{}",
					if secure { "__Host-" } else { "__Dev-" },
					config.name
				),
				"/".to_owned(),
				None,
			),
			CookieScope::Explicit { path, domain } => {
				if !path.starts_with('/') || !path.bytes().all(is_attribute_byte) {
					return Err(CookieError::InvalidPath);
				}
				let domain = domain
					.map(|domain| {
						let domain = domain
							.strip_prefix('.')
							.unwrap_or(&domain)
							.to_ascii_lowercase();
						if domain.is_empty()
							|| domain.len() > 253 || domain.split('.').any(|label| {
							label.is_empty()
								|| label.len() > 63 || label.starts_with('-')
								|| label.ends_with('-') || !label
								.bytes()
								.all(|b| b.is_ascii_alphanumeric() || b == b'-')
						}) {
							return Err(CookieError::InvalidDomain);
						}
						Ok(domain)
					})
					.transpose()?;
				(config.name, path, domain)
			}
		};
		let definition = CookieDefinition {
			name,
			path,
			domain,
			secure,
			same_site,
			http_only: config.http_only.unwrap_or(self.config.default_http_only),
			partitioned: secure
				&& config
					.partitioned
					.unwrap_or(self.config.default_partitioned),
			max_age: config.max_age,
		};
		definition.check_size(&definition.deletion_cookie())?;
		definition.check_size(&definition.build_cookie(String::new()))?;
		Ok(definition)
	}
}

/// Browser scope for a managed cookie.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CookieScope {
	/// Uses a managed host-only prefix, Path=/, and no Domain.
	HostOnly,
	/// Uses an unprefixed name and explicit path/domain scope.
	Explicit {
		/// An absolute cookie path. Empty paths are rejected.
		path: String,
		/// An ASCII domain, or no Domain attribute. Empty domains are rejected.
		domain: Option<String>,
	},
}

/// A cookie's identity, lifetime, and overrides of shared policy.
#[derive(Clone, Debug)]
pub struct CookieConfig {
	/// An HTTP token that does not start with `__Host-`, `__Secure-`, or
	/// `__Dev-` in any ASCII case.
	pub name: String,
	/// Host-only protection or explicit browser scope.
	pub scope: CookieScope,
	/// Persistent lifetime, or a browser-session cookie when absent.
	pub max_age: Option<CookieMaxAge>,
	/// Overrides the manager's SameSite attribute when present.
	pub same_site: Option<SameSite>,
	/// Overrides the manager's JavaScript access policy when present.
	pub http_only: Option<bool>,
	/// Overrides the manager's partition policy when present. Under
	/// [`CookieSecurity::LoopbackHttp`], the manager emits no Partitioned
	/// attribute, even when this setting is `Some(true)` because it adapts
	/// security attributes for loopback development.
	pub partitioned: Option<bool>,
}

/// Validated, immutable attributes shared by creation, reading, and deletion.
#[derive(Clone, Debug)]
pub struct CookieDefinition {
	name: String,
	path: String,
	domain: Option<String>,
	secure: bool,
	same_site: SameSite,
	http_only: bool,
	partitioned: bool,
	max_age: Option<CookieMaxAge>,
}
impl CookieDefinition {
	/// Returns the final cookie name, including any managed prefix.
	pub fn name(&self) -> &str {
		&self.name
	}
	/// Creates an ordinary cookie without percent encoding or value conversion.
	///
	/// The returned cookie's `Debug` output includes its value. Do not log it.
	pub fn create_cookie(&self, value: impl Into<String>) -> Result<Cookie<'static>, CookieError> {
		let value = value.into();
		if !value.bytes().all(is_value_byte) {
			return Err(CookieError::InvalidValue);
		}
		let cookie = self.build_cookie(value);
		self.check_size(&cookie)?;
		Ok(cookie)
	}
	/// Returns Max-Age=0 with the same name, path, domain, and security attributes.
	pub fn deletion_cookie(&self) -> Cookie<'static> {
		let mut cookie = self.build_cookie(String::new());
		cookie.set_max_age(cookie::time::Duration::ZERO);
		cookie
	}
	/// Reads the value of the only request cookie with this definition's name.
	/// Returns `Ok(None)` when the cookie is absent and preserves empty values as
	/// `Ok(Some(""))`.
	///
	/// Cookies with other names never cause an error. Returns
	/// [`CookieError::DuplicateCookie`] when two or more cookies have this name
	/// and [`CookieError::MalformedHeader`] when the cookie's value is outside
	/// the cookie value grammar. See [`RequestCookies`] for the parsing rules.
	pub fn read_value<'a>(&self, headers: &'a HeaderMap) -> Result<Option<&'a str>, CookieError> {
		RequestCookies::parse(headers)?.value(&self.name)
	}
	/// Returns Max-Age=0 cookies for the unpartitioned and the partitioned
	/// variant of this cookie, which share its other attributes.
	///
	/// A browser can hold both variants after an application changes the
	/// cookie's partition setting, and it then sends two cookies with this name,
	/// which [`Self::read_value`] reports as [`CookieError::DuplicateCookie`]. A
	/// response that sets both deletion cookies removes both variants.
	pub fn deletion_cookies_for_both_partition_modes(&self) -> [Cookie<'static>; 2] {
		[false, true].map(|partitioned| {
			let mut cookie = self.deletion_cookie();
			cookie.set_partitioned(partitioned);
			cookie
		})
	}
	fn build_cookie(&self, value: String) -> Cookie<'static> {
		let mut cookie = Cookie::new(self.name.clone(), value);
		cookie.set_path(self.path.clone());
		if let Some(domain) = &self.domain {
			cookie.set_domain(domain.clone());
		}
		cookie.set_secure(self.secure);
		cookie.set_same_site(self.same_site);
		cookie.set_http_only(self.http_only);
		cookie.set_partitioned(self.partitioned);
		if let Some(age) = self.max_age {
			cookie.set_max_age(cookie::time::Duration::seconds(age.as_secs() as i64));
		}
		cookie
	}
	fn check_size(&self, cookie: &Cookie<'_>) -> Result<(), CookieError> {
		if cookie.to_string().len() > MAX_SET_COOKIE_SIZE {
			return Err(CookieError::OutputTooLarge);
		}
		validate_set_cookie(cookie)
	}
	fn associated_data(&self) -> Vec<u8> {
		let prefix_and_partition = [
			u8::from(self.name.starts_with("__Host-") || self.name.starts_with("__Dev-")),
			u8::from(self.partitioned),
		];
		crate::bytes::encode_length_prefixed([
			b"primitives-rs/cookies/v1/scope".as_slice(),
			self.name.as_bytes(),
			self.path.as_bytes(),
			self.domain.as_deref().unwrap_or("").as_bytes(),
			&prefix_and_partition,
		])
	}
}
