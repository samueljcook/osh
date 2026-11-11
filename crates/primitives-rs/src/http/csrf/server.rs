//! Encrypted double-submit CSRF protection with optional request binding and
//! exact-origin checks. Tokens work before login. Cycle the token after a
//! binding change; the framework-neutral operations return ordinary cookies.
//! The Tower layer rejects invalid requests before calling the application and
//! renews missing, invalid, expired, or differently bound tokens. When a request
//! carries two or more cookies with the token cookie's name, as browsers can
//! after an application changes the cookie's partition setting, the layer also
//! deletes both partition variants of the token cookie before it sets the new
//! token. Responses with an issued token use Cache-Control: private, no-store.
//!
//! Applications must cycle tokens after changing the request binding. The
//! binding extractor may return None before login and must authenticate any
//! identity that it extracts. Encrypted token cookies remain readable by
//! JavaScript so that clients can submit their exact value in the configured
//! header. The client does not receive a decryption key.
//!
//! ```
//! use std::sync::Arc;
//! use primitives_rs::http::cookies::manager::*;
//! use primitives_rs::crypto::{Key32, keysets::derive_keyset_from_latest_first_keys};
//! use primitives_rs::http::csrf::server::*;
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let keys = derive_keyset_from_latest_first_keys([Key32::generate()?], "app.cookies")?;
//! let manager = CookieManager::new(CookieManagerConfig {
//!     keys: CookieKeysetSource::from_keyset(keys), security: CookieSecurity::Https,
//!     default_same_site: SameSite::Lax, default_http_only: true, default_partitioned: false,
//! })?;
//! let csrf = CsrfProtector::new(CsrfProtectorConfig {
//!     cookie_manager: manager, extract_binding_fn: Arc::new(|_| Ok(None)),
//!     cookie_name: "csrf".into(), header_name: "x-csrf-token".parse()?,
//!     token_max_age: CookieMaxAge::from_secs(3600)?,
//!     allowed_origins: vec!["https://example.com".into()],
//! })?;
//! let token = csrf.create_token_cookie_for_binding(None)?;
//! let request = http::Request::builder().method("POST").uri("/submit")
//!     .header("origin", "https://example.com")
//!     .header("cookie", format!("{}={}", token.name(), token.value()))
//!     .header(csrf.header_name(), token.value()).body(())?;
//! csrf.verify_request(&request)?;
//! # Ok(())
//! # }
//! ```

use std::fmt;
use std::future::{Future, ready};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

use cookie::Cookie;
use http::header::{HeaderName, HeaderValue, ORIGIN, REFERER, SET_COOKIE};
use http::{HeaderMap, Request, Response};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tower_layer::Layer;
use tower_service::Service;

use crate::{
	encoding::binary_to_text::{ByteEncoding, DecodeOptions},
	http::cookies::manager::{
		CookieConfig, CookieDefinition, CookieError, CookieManager, CookieMaxAge, CookieScope,
		CookieSecurity, EncryptedCookieDefinition, EncryptedCookieError, MAX_SET_COOKIE_SIZE,
		SameSite,
	},
	random::secure_bytes::generate_random_byte_array,
	secret_bytes::SecretBytes,
};

const CSRF_NONCE_SIZE: usize = 16;

/// Maximum CSRF binding byte length.
pub const MAX_CSRF_BINDING_SIZE: usize = 1024;

/// Maximum byte length of one submitted CSRF token header value.
pub const MAX_CSRF_SUBMITTED_TOKEN_SIZE: usize = MAX_SET_COOKIE_SIZE;

/// Maximum byte length of one configured or request CSRF origin value.
pub const MAX_CSRF_ORIGIN_SIZE: usize = 2048;

/// Maximum number of configured CSRF allowed origins.
pub const MAX_CSRF_ALLOWED_ORIGINS: usize = 128;

mod error;
internal_modules!(private; header_helpers, method_helpers, origin_helpers);
mod host_helpers;

pub use error::Error as CsrfError;
use error::Error;
use header_helpers::{append_set_cookie_headers, header_to_nonempty_str, submitted_token};
use host_helpers::validate_localhost_request_authorities;
use method_helpers::is_csrf_safe_method;
use origin_helpers::{normalize_allowed_origins, normalize_request_origin_header};

/// Callback type that extracts CSRF token binding bytes from HTTP headers.
pub type CsrfBindingExtractor =
	dyn Fn(&HeaderMap) -> Result<Option<CsrfBinding>, Error> + Send + Sync + 'static;

type CsrfFailureResponseFn<ResponseBody> =
	dyn Fn(Error) -> Response<ResponseBody> + Send + Sync + 'static;

type CsrfServiceFuture<ResponseBody, ServiceError> =
	Pin<Box<dyn Future<Output = Result<Response<ResponseBody>, ServiceError>> + Send>>;

/// Bytes that bind a CSRF token to the current stateless request identity.
pub struct CsrfBinding {
	bytes: SecretBytes,
}

impl CsrfBinding {
	/// Copies non-empty binding bytes into a zeroizing buffer.
	pub fn from_non_empty_bytes(bytes: &[u8]) -> Result<Self, Error> {
		if bytes.is_empty() {
			return Err(Error::EmptyBinding);
		}
		if bytes.len() > MAX_CSRF_BINDING_SIZE {
			return Err(Error::BindingTooLarge {
				actual: bytes.len(),
				max: MAX_CSRF_BINDING_SIZE,
			});
		}
		Ok(Self {
			bytes: SecretBytes::from_slice(bytes).map_err(|_| Error::AllocationFailed)?,
		})
	}

	fn as_bytes(&self) -> &[u8] {
		self.bytes.expose_secret()
	}
}

impl fmt::Debug for CsrfBinding {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("CsrfBinding")
			.field("len", &self.as_bytes().len())
			.finish()
	}
}

/// CSRF helper configuration.
pub struct CsrfProtectorConfig {
	/// Cookie manager used for encrypted token cookies.
	pub cookie_manager: CookieManager,
	/// Extracts the current token binding from HTTP headers.
	pub extract_binding_fn: Arc<CsrfBindingExtractor>,
	/// Host-only cookie suffix.
	pub cookie_name: String,
	/// HTTP header name used for submitted CSRF tokens.
	pub header_name: HeaderName,
	/// CSRF token cookie max-age.
	pub token_max_age: CookieMaxAge,
	/// Optional origin allowlist for unsafe requests.
	///
	/// When non-empty, unsafe requests must include an `Origin` or `Referer`
	/// header that matches this allowlist. At most [`MAX_CSRF_ALLOWED_ORIGINS`]
	/// exact HTTP(S) origins are accepted, each bounded by
	/// [`MAX_CSRF_ORIGIN_SIZE`]. An empty list intentionally disables this
	/// additional origin check.
	///
	/// A browser sends `Origin: null` and no `Referer` for a form submission
	/// from a page with the referrer policy `no-referrer`, so the protector
	/// rejects that submission. A page that submits a form without a script to
	/// a protected endpoint must use a referrer policy that keeps the origin,
	/// such as `same-origin` or `strict-origin-when-cross-origin`. A request
	/// that a script sends with `fetch` in its default mode carries the page's
	/// origin under `no-referrer` too.
	pub allowed_origins: Vec<String>,
}

impl fmt::Debug for CsrfProtectorConfig {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("CsrfProtectorConfig")
			.field("cookie_manager", &self.cookie_manager)
			.field("extract_binding_fn", &"[callback]")
			.field("cookie_name", &self.cookie_name)
			.field("header_name", &self.header_name)
			.field("token_max_age", &self.token_max_age)
			.field("allowed_origins", &self.allowed_origins)
			.finish()
	}
}

/// Framework-agnostic encrypted double-submit CSRF token issuer and verifier.
///
/// Optional origin checks and request binding strengthen the baseline token
/// comparison. This helper is not an XSS defense. In development it validates
/// only syntactic loopback request authorities; the server integration must
/// separately enforce listener and trusted-proxy locality.
pub struct CsrfProtector {
	uses_loopback_http: bool,
	extract_binding_fn: Arc<CsrfBindingExtractor>,
	cookie: EncryptedCookieDefinition<CsrfCookiePayload>,
	header_name: HeaderName,
	token_max_age: CookieMaxAge,
	allowed_origins: Vec<String>,
}

impl CsrfProtector {
	/// Creates a CSRF protector from cookie policy.
	///
	/// Construction fails with [`CsrfError::EncryptedCookie`] when a token with a
	/// binding of [`MAX_CSRF_BINDING_SIZE`] bytes would not fit in the token
	/// cookie's size bound, for example because the cookie name is long. This
	/// check reads no keys.
	pub fn new(config: CsrfProtectorConfig) -> Result<Self, Error> {
		let allowed_origins = normalize_allowed_origins(config.allowed_origins)?;
		let uses_loopback_http = config.cookie_manager.security() == CookieSecurity::LoopbackHttp;
		let cookie = config
			.cookie_manager
			.define_encrypted_cookie(CookieConfig {
				name: config.cookie_name,
				scope: CookieScope::HostOnly,
				max_age: Some(config.token_max_age),
				same_site: Some(SameSite::Lax),
				http_only: Some(false),
				partitioned: None,
			})?;
		cookie.check_capacity(&CsrfCookiePayload {
			nonce: [u8::MAX; CSRF_NONCE_SIZE],
			expires_at_unix_secs: i64::MIN,
			binding: Some(vec![u8::MAX; MAX_CSRF_BINDING_SIZE]),
		})?;
		Ok(Self {
			uses_loopback_http,
			extract_binding_fn: config.extract_binding_fn,
			cookie,
			header_name: config.header_name,
			token_max_age: config.token_max_age,
			allowed_origins,
		})
	}

	/// Returns the final emitted CSRF cookie name.
	pub fn cookie_name(&self) -> &str {
		self.cookie.definition().name()
	}

	/// Returns the token cookie's creation and deletion definition.
	pub fn cookie_definition(&self) -> &CookieDefinition {
		self.cookie.definition()
	}

	/// Returns the submitted-token HTTP header name.
	pub fn header_name(&self) -> &HeaderName {
		&self.header_name
	}

	/// Issues a fresh CSRF token cookie for the request's extracted binding.
	///
	/// The returned cookie's `Debug` output includes the token. Do not log it.
	pub fn create_token_cookie_for_request<B>(
		&self,
		request: &Request<B>,
	) -> Result<Cookie<'static>, Error> {
		self.validate_development_request_host_is_localhost(request)?;
		let binding = self.extract_binding(request)?;
		self.new_token_cookie(binding.as_ref())
	}

	/// Issues a fresh CSRF token cookie for an explicit binding.
	///
	/// The returned cookie's `Debug` output includes the token. Do not log it.
	pub fn create_token_cookie_for_binding(
		&self,
		binding: Option<&CsrfBinding>,
	) -> Result<Cookie<'static>, Error> {
		self.new_token_cookie(binding)
	}

	/// Issues a fresh token only when the request does not already carry a valid token.
	///
	/// When the request carries two or more cookies with the token cookie's
	/// name, this operation returns a new token, but that token alone does not
	/// remove the other cookie. [`CsrfLayer`] also sends deletion cookies for both
	/// partition variants in that case. The returned cookie's `Debug` output
	/// includes the token. Do not log it.
	pub fn issue_token_cookie_if_needed_for_request<B>(
		&self,
		request: &Request<B>,
	) -> Result<Option<Cookie<'static>>, Error> {
		match self.token_renewal_for_safe_request(request)? {
			TokenRenewal::Withheld => Ok(None),
			TokenRenewal::Issue(binding) | TokenRenewal::ReplaceDuplicates(binding) => {
				self.new_token_cookie(binding.as_ref()).map(Some)
			}
		}
	}

	fn token_renewal_for_safe_request<B>(
		&self,
		request: &Request<B>,
	) -> Result<TokenRenewal, Error> {
		self.validate_development_request_host_is_localhost(request)?;
		let binding = self.extract_binding(request)?;
		match self.cookie.read_value(request.headers()) {
			Ok(Some(payload))
				if payload.is_valid(current_unix_secs()?)
					&& binding_matches(payload.binding.as_deref(), binding.as_ref()) =>
			{
				Ok(TokenRenewal::Withheld)
			}
			Err(EncryptedCookieError::Cookie(CookieError::DuplicateCookie)) => {
				Ok(TokenRenewal::ReplaceDuplicates(binding))
			}
			Err(error) if !is_invalid_client_cookie(&error) => Err(error.into()),
			_ => Ok(TokenRenewal::Issue(binding)),
		}
	}

	/// Verifies CSRF protection for an HTTP request and returns the cookies that
	/// a failure response must set.
	///
	/// A failure that the client can repair carries a new token for the
	/// request's binding. When the request carries two or more cookies with the
	/// token cookie's name, the failure also carries deletion cookies for both
	/// partition variants of the token cookie, before the new token. These are
	/// the cookies that [`CsrfLayer`] sets on its failure responses. When this
	/// operation cannot create the renewal cookies, it returns that error with no
	/// cookies. The cookies' `Debug` output includes the token. Do not log them.
	pub fn verify_request_with_renewal<B>(
		&self,
		request: &Request<B>,
	) -> Result<(), CsrfRejection> {
		self.validate_development_request_host_is_localhost(request)
			.map_err(CsrfRejection::without_cookies)?;
		if is_csrf_safe_method(request.method().as_str()) {
			return Ok(());
		}
		let failure = match self.verify_unsafe_request(request) {
			Ok(()) => return Ok(()),
			Err(failure) => failure,
		};
		Err(match self.token_cookies(failure.renewal) {
			Ok(renewal_cookies) => CsrfRejection {
				error: failure.error,
				renewal_cookies,
			},
			Err(error) => CsrfRejection::without_cookies(error),
		})
	}

	/// Verifies CSRF protection for an HTTP request.
	pub fn verify_request<B>(&self, request: &Request<B>) -> Result<(), Error> {
		self.validate_development_request_host_is_localhost(request)?;
		if is_csrf_safe_method(request.method().as_str()) {
			return Ok(());
		}

		self.verify_unsafe_request(request)
			.map_err(|failure| failure.error)
	}

	fn verify_unsafe_request<B>(
		&self,
		request: &Request<B>,
	) -> Result<(), CsrfVerificationFailure> {
		self.validate_origin(request.headers())
			.map_err(CsrfVerificationFailure::without_token_renewal)?;
		let parsed_cookie = match self.cookie.definition().read_value(request.headers()) {
			Ok(Some(value)) => value,
			Ok(None) => {
				return Err(self.failure_with_renewal(
					request,
					Error::TokenMissing,
					TokenRenewal::Issue,
				)?);
			}
			Err(CookieError::DuplicateCookie) => {
				return Err(self.failure_with_renewal(
					request,
					CookieError::DuplicateCookie.into(),
					TokenRenewal::ReplaceDuplicates,
				)?);
			}
			Err(CookieError::MalformedHeader) => {
				return Err(self.failure_with_renewal(
					request,
					CookieError::MalformedHeader.into(),
					TokenRenewal::Issue,
				)?);
			}
			Err(error) => return Err(CsrfVerificationFailure::without_token_renewal(error.into())),
		};
		let submitted = submitted_token(request.headers(), &self.header_name)
			.map_err(CsrfVerificationFailure::without_token_renewal)?;
		if submitted
			.as_bytes()
			.ct_eq(parsed_cookie.as_bytes())
			.unwrap_u8()
			!= 1
		{
			return Err(CsrfVerificationFailure::without_token_renewal(
				Error::TokenMismatch,
			));
		}
		let payload: CsrfCookiePayload = match self.cookie.decode_value(parsed_cookie) {
			Ok(payload) => payload,
			Err(error) if is_invalid_client_cookie(&error) => {
				return Err(self.failure_with_renewal(
					request,
					error.into(),
					TokenRenewal::Issue,
				)?);
			}
			Err(error) => return Err(CsrfVerificationFailure::without_token_renewal(error.into())),
		};
		if !payload
			.is_valid(current_unix_secs().map_err(CsrfVerificationFailure::without_token_renewal)?)
		{
			return Err(self.failure_with_renewal(
				request,
				Error::TokenInvalidOrExpired,
				TokenRenewal::Issue,
			)?);
		}
		let binding = self
			.extract_binding(request)
			.map_err(CsrfVerificationFailure::without_token_renewal)?;
		if !binding_matches(payload.binding.as_deref(), binding.as_ref()) {
			return Err(CsrfVerificationFailure {
				error: Error::BindingMismatch,
				renewal: TokenRenewal::Issue(binding),
			});
		}
		Ok(())
	}

	fn failure_with_renewal<B>(
		&self,
		request: &Request<B>,
		error: Error,
		renewal_for_binding: fn(Option<CsrfBinding>) -> TokenRenewal,
	) -> Result<CsrfVerificationFailure, CsrfVerificationFailure> {
		let binding = self
			.extract_binding(request)
			.map_err(CsrfVerificationFailure::without_token_renewal)?;
		Ok(CsrfVerificationFailure {
			error,
			renewal: renewal_for_binding(binding),
		})
	}

	/// Returns the cookies that a response sets for a renewal, in the order that
	/// the response must set them.
	fn token_cookies(&self, renewal: TokenRenewal) -> Result<Vec<Cookie<'static>>, Error> {
		match renewal {
			TokenRenewal::Withheld => Ok(Vec::new()),
			TokenRenewal::Issue(binding) => Ok(vec![self.new_token_cookie(binding.as_ref())?]),
			TokenRenewal::ReplaceDuplicates(binding) => {
				let mut cookies = Vec::from(
					self.cookie
						.definition()
						.deletion_cookies_for_both_partition_modes(),
				);
				cookies.push(self.new_token_cookie(binding.as_ref())?);
				Ok(cookies)
			}
		}
	}

	fn new_token_cookie(&self, binding: Option<&CsrfBinding>) -> Result<Cookie<'static>, Error> {
		let payload = CsrfCookiePayload {
			nonce: generate_random_byte_array::<CSRF_NONCE_SIZE>()?,
			expires_at_unix_secs: current_unix_secs()?
				.checked_add(self.token_max_age.as_secs() as i64)
				.ok_or(Error::ExpirationOverflow)?,
			binding: binding.map(|binding| binding.as_bytes().to_owned()),
		};
		Ok(self.cookie.create_cookie(&payload)?)
	}

	fn extract_binding<B>(&self, request: &Request<B>) -> Result<Option<CsrfBinding>, Error> {
		(self.extract_binding_fn)(request.headers())
	}

	fn validate_origin(&self, headers: &HeaderMap) -> Result<(), Error> {
		if self.allowed_origins.is_empty() {
			return Ok(());
		}
		if let Some(origin_header) = header_to_nonempty_str(headers, &ORIGIN, "Origin")? {
			return self.validate_origin_header(origin_header, "Origin");
		}
		if let Some(referer_header) = header_to_nonempty_str(headers, &REFERER, "Referer")? {
			return self.validate_origin_header(referer_header, "Referer");
		}
		Err(Error::OriginAndRefererMissing)
	}

	fn validate_origin_header(&self, header: &str, label: &'static str) -> Result<(), Error> {
		let normalized = normalize_request_origin_header(header, label)?;
		if self
			.allowed_origins
			.iter()
			.any(|allowed| allowed == &normalized)
		{
			return Ok(());
		}
		Err(Error::OriginNotAllowed { origin: normalized })
	}

	fn validate_development_request_host_is_localhost<B>(
		&self,
		request: &Request<B>,
	) -> Result<(), Error> {
		if !self.uses_loopback_http {
			return Ok(());
		}
		validate_localhost_request_authorities(request)
	}
}

impl fmt::Debug for CsrfProtector {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("CsrfProtector")
			.field("cookie_name", &self.cookie_name())
			.field("header_name", &self.header_name)
			.field("token_max_age", &self.token_max_age)
			.field("uses_loopback_http", &self.uses_loopback_http)
			.field("allowed_origin_count", &self.allowed_origins.len())
			.finish()
	}
}

/// A failed CSRF verification and the cookies that its response must set.
///
/// The `Debug` output shows the error and the number of renewal cookies, without the cookies'
/// values.
pub struct CsrfRejection {
	/// The verification failure.
	pub error: Error,
	/// The cookies that renew the client's token, in the order that the response
	/// must set them. The list is empty when the client cannot repair the failure
	/// with a new token.
	pub renewal_cookies: Vec<Cookie<'static>>,
}

impl fmt::Debug for CsrfRejection {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("CsrfRejection")
			.field("error", &self.error)
			.field("renewal_cookie_count", &self.renewal_cookies.len())
			.finish()
	}
}

impl CsrfRejection {
	fn without_cookies(error: Error) -> Self {
		Self {
			error,
			renewal_cookies: Vec::new(),
		}
	}
}

/// The token cookies that a response sets.
enum TokenRenewal {
	/// The response sets no token cookie.
	Withheld,
	/// The response sets one new token for this binding.
	Issue(Option<CsrfBinding>),
	/// The response deletes both partition variants of the token cookie and
	/// then sets one new token for this binding.
	ReplaceDuplicates(Option<CsrfBinding>),
}

struct CsrfVerificationFailure {
	error: Error,
	renewal: TokenRenewal,
}

impl CsrfVerificationFailure {
	fn without_token_renewal(error: Error) -> Self {
		Self {
			error,
			renewal: TokenRenewal::Withheld,
		}
	}
}

/// Tower layer that applies CSRF verification and token refresh behavior.
///
/// The resulting service delegates readiness to the inner service. When an
/// application response already sets the CSRF cookie name, that response owns
/// the value and this layer does not append a competing cookie.
pub struct CsrfLayer<ResponseBody> {
	protector: Arc<CsrfProtector>,
	failure_response_fn: Arc<CsrfFailureResponseFn<ResponseBody>>,
}
impl<ResponseBody> Clone for CsrfLayer<ResponseBody> {
	fn clone(&self) -> Self {
		Self {
			protector: self.protector.clone(),
			failure_response_fn: self.failure_response_fn.clone(),
		}
	}
}

impl<ResponseBody> CsrfLayer<ResponseBody> {
	/// Creates a Tower layer from a CSRF protector and rejection response callback.
	pub fn new(
		protector: impl Into<Arc<CsrfProtector>>,
		failure_response_fn: impl Fn(Error) -> Response<ResponseBody> + Send + Sync + 'static,
	) -> Self {
		Self {
			protector: protector.into(),
			failure_response_fn: Arc::new(failure_response_fn),
		}
	}
}

impl<S, ResponseBody> Layer<S> for CsrfLayer<ResponseBody> {
	type Service = CsrfService<S, ResponseBody>;

	fn layer(&self, inner: S) -> Self::Service {
		CsrfService {
			inner,
			protector: self.protector.clone(),
			failure_response_fn: self.failure_response_fn.clone(),
		}
	}
}

impl<ResponseBody> fmt::Debug for CsrfLayer<ResponseBody> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("CsrfLayer")
			.field("protector", &self.protector)
			.field("failure_response_fn", &"[callback]")
			.finish()
	}
}

/// Tower service produced by [`CsrfLayer`].
///
/// `poll_ready` delegates directly to the inner service, including for calls
/// that CSRF validation may reject before invoking it.
pub struct CsrfService<S, ResponseBody> {
	inner: S,
	protector: Arc<CsrfProtector>,
	failure_response_fn: Arc<CsrfFailureResponseFn<ResponseBody>>,
}
impl<S: Clone, ResponseBody> Clone for CsrfService<S, ResponseBody> {
	fn clone(&self) -> Self {
		Self {
			inner: self.inner.clone(),
			protector: self.protector.clone(),
			failure_response_fn: self.failure_response_fn.clone(),
		}
	}
}

impl<S, RequestBody, ResponseBody> Service<Request<RequestBody>> for CsrfService<S, ResponseBody>
where
	S: Service<Request<RequestBody>, Response = Response<ResponseBody>>,
	S::Future: Send + 'static,
	S::Error: Send + 'static,
	RequestBody: 'static,
	ResponseBody: Send + 'static,
{
	type Response = Response<ResponseBody>;
	type Error = S::Error;
	type Future = CsrfServiceFuture<ResponseBody, S::Error>;

	fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.inner.poll_ready(cx)
	}

	fn call(&mut self, request: Request<RequestBody>) -> Self::Future {
		if is_csrf_safe_method(request.method().as_str()) {
			return self.call_safe_request(request);
		}
		if let Err(error) = self
			.protector
			.validate_development_request_host_is_localhost(&request)
		{
			let response = (self.failure_response_fn)(error);
			return Box::pin(ready(Ok(response)));
		}
		match self.protector.verify_unsafe_request(&request) {
			Ok(()) => Box::pin(self.inner.call(request)),
			Err(failure) => {
				let response = self.response_for_failure(failure);
				Box::pin(ready(Ok(response)))
			}
		}
	}
}

impl<S, ResponseBody> CsrfService<S, ResponseBody> {
	fn call_safe_request<RequestBody>(
		&mut self,
		request: Request<RequestBody>,
	) -> CsrfServiceFuture<ResponseBody, S::Error>
	where
		S: Service<Request<RequestBody>, Response = Response<ResponseBody>>,
		S::Future: Send + 'static,
		S::Error: Send + 'static,
		RequestBody: 'static,
		ResponseBody: Send + 'static,
	{
		let cookies = match self
			.protector
			.token_renewal_for_safe_request(&request)
			.and_then(|renewal| self.protector.token_cookies(renewal))
		{
			Ok(cookies) => cookies,
			Err(error) => {
				let response = (self.failure_response_fn)(error);
				return Box::pin(ready(Ok(response)));
			}
		};
		let future = self.inner.call(request);
		let failure_response_fn = self.failure_response_fn.clone();
		Box::pin(async move {
			let mut response = future.await?;
			if let Err(error) = append_set_cookie_headers(response.headers_mut(), &cookies) {
				return Ok((failure_response_fn)(error));
			}
			Ok(response)
		})
	}

	fn response_for_failure(&self, failure: CsrfVerificationFailure) -> Response<ResponseBody> {
		let mut response = (self.failure_response_fn)(failure.error);
		let cookies = match self.protector.token_cookies(failure.renewal) {
			Ok(cookies) => cookies,
			Err(error) => return (self.failure_response_fn)(error),
		};
		if let Err(error) = append_set_cookie_headers(response.headers_mut(), &cookies) {
			return (self.failure_response_fn)(error);
		}
		response
	}
}

impl<S, ResponseBody> fmt::Debug for CsrfService<S, ResponseBody>
where
	S: fmt::Debug,
{
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("CsrfService")
			.field("inner", &self.inner)
			.field("protector", &self.protector)
			.field("failure_response_fn", &"[callback]")
			.finish()
	}
}

fn is_invalid_client_cookie(error: &EncryptedCookieError) -> bool {
	matches!(
		error,
		EncryptedCookieError::Cookie(_)
			| EncryptedCookieError::InvalidEncryptedValue
			| EncryptedCookieError::Deserialization
	)
}

/// The encrypted token payload. The nonce and binding serialize as unpadded
/// Base64URL strings, so a token with a binding of [`MAX_CSRF_BINDING_SIZE`]
/// bytes fits in one cookie.
#[derive(Deserialize, Serialize)]
struct CsrfCookiePayload {
	#[serde(
		serialize_with = "serialize_base64url",
		deserialize_with = "deserialize_nonce"
	)]
	nonce: [u8; CSRF_NONCE_SIZE],
	#[serde(rename = "expires_at_unix")]
	expires_at_unix_secs: i64,
	#[serde(
		serialize_with = "serialize_optional_base64url",
		deserialize_with = "deserialize_binding"
	)]
	binding: Option<Vec<u8>>,
}

fn serialize_base64url<S: serde::Serializer>(
	bytes: &[u8],
	serializer: S,
) -> Result<S::Ok, S::Error> {
	let text = zeroize::Zeroizing::new(ByteEncoding::Base64UrlUnpadded.encode(bytes));
	serializer.serialize_str(&text)
}

fn serialize_optional_base64url<S: serde::Serializer>(
	bytes: &Option<Vec<u8>>,
	serializer: S,
) -> Result<S::Ok, S::Error> {
	match bytes {
		Some(bytes) => {
			let text = zeroize::Zeroizing::new(ByteEncoding::Base64UrlUnpadded.encode(bytes));
			serializer.serialize_some(text.as_str())
		}
		None => serializer.serialize_none(),
	}
}

fn deserialize_nonce<'de, D: serde::Deserializer<'de>>(
	deserializer: D,
) -> Result<[u8; CSRF_NONCE_SIZE], D::Error> {
	let text = zeroize::Zeroizing::new(String::deserialize(deserializer)?);
	let mut nonce = zeroize::Zeroizing::new([0; CSRF_NONCE_SIZE]);
	match ByteEncoding::Base64UrlUnpadded.decode_into(&text, &mut *nonce) {
		Ok(CSRF_NONCE_SIZE) => Ok(*nonce),
		_ => Err(serde::de::Error::custom("invalid CSRF token nonce")),
	}
}

fn deserialize_binding<'de, D: serde::Deserializer<'de>>(
	deserializer: D,
) -> Result<Option<Vec<u8>>, D::Error> {
	let Some(text) = Option::<String>::deserialize(deserializer)?.map(zeroize::Zeroizing::new)
	else {
		return Ok(None);
	};
	ByteEncoding::Base64UrlUnpadded
		.decode(
			&text,
			DecodeOptions {
				max_decoded_size: MAX_CSRF_BINDING_SIZE,
			},
		)
		.map(Some)
		.map_err(|_| serde::de::Error::custom("invalid CSRF token binding"))
}

impl CsrfCookiePayload {
	fn is_valid(&self, now_unix_secs: i64) -> bool {
		now_unix_secs < self.expires_at_unix_secs
			&& self
				.binding
				.as_ref()
				.is_none_or(|value| !value.is_empty() && value.len() <= MAX_CSRF_BINDING_SIZE)
	}
}

fn current_unix_secs() -> Result<i64, Error> {
	let elapsed = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map_err(|_| Error::ClockBeforeUnixEpoch)?;
	i64::try_from(elapsed.as_secs()).map_err(|_| Error::ExpirationOverflow)
}

fn binding_matches(left: Option<&[u8]>, right: Option<&CsrfBinding>) -> bool {
	match (left, right.map(CsrfBinding::as_bytes)) {
		(None, None) => true,
		(Some(left), Some(right)) => left.ct_eq(right).unwrap_u8() == 1,
		_ => false,
	}
}

impl Drop for CsrfCookiePayload {
	fn drop(&mut self) {
		use zeroize::Zeroize;
		self.nonce.zeroize();
		if let Some(binding) = &mut self.binding {
			binding.zeroize();
		}
	}
}
