//! Response security policies and credential-header sensitivity.
//!
//! Enable the `security-headers` Cargo feature. [`SecurityHeadersLayer`] applies
//! a configured policy after the inner service returns a response. It supplies
//! `X-Content-Type-Options: nosniff` and
//! `X-Permitted-Cross-Domain-Policies: none`, and removes `Server` and
//! `X-Powered-By`. Applications explicitly select HSTS, referrer, framing,
//! cross-origin, and browser-permission policies. There is no default profile.
//!
//! `None` leaves a header unmanaged, including any existing value. For managed
//! headers, [`ExistingSecurityHeaders::Preserve`] retains every existing value;
//! [`ExistingSecurityHeaders::Replace`] replaces every existing value with one
//! configured value. Preservation does not check whether an existing policy is
//! valid or sufficiently restrictive. Removal of the two fingerprint headers
//! applies in either mode. Unrelated headers, status, version, extensions, and
//! body remain intact. The layer does not read, buffer, or replace bodies.
//!
//! # Application policy
//!
//! ```
//! use http::{HeaderMap, HeaderValue};
//! use primitives_rs::http::security_headers::{
//!     CoepPolicy, CoopPolicy, CorpPolicy, ExistingSecurityHeaders, FrameOptions,
//!     HstsConfig, ReferrerPolicy, SecurityHeadersConfig, SecurityHeadersLayer,
//! };
//!
//! // This application serves HTTPS, including every subdomain, uses no
//! // cross-origin popups, and loads only resources that satisfy COEP.
//! let security_headers = SecurityHeadersLayer::new(SecurityHeadersConfig {
//!     existing_headers: ExistingSecurityHeaders::Preserve,
//!     hsts: Some(HstsConfig { max_age_secs: 31_536_000, include_subdomains: true }),
//!     referrer_policy: Some(ReferrerPolicy::NoReferrer),
//!     frame_options: Some(FrameOptions::Deny),
//!     coop: Some(CoopPolicy::SameOrigin),
//!     coep: Some(CoepPolicy::RequireCorp),
//!     corp: Some(CorpPolicy::SameOrigin),
//!     permissions_policy: Some(HeaderValue::from_static("camera=(), microphone=()")),
//! });
//!
//! // The same policy supports plain HTTP header maps and ordinary Tower hosts.
//! let mut headers = HeaderMap::new();
//! security_headers.apply_to_headers(&mut headers);
//! assert_eq!(headers["x-content-type-options"], "nosniff");
//! # let _ = security_headers;
//! ```
//!
//! Configure HSTS only on a client-facing HTTPS deployment. The layer does not
//! infer TLS from the request URI or trust forwarding headers. For mixed HTTP
//! and HTTPS listeners, attach separate policies. COOP can sever cross-origin
//! popup relationships, COEP can prevent resource loading, and CORP can prevent
//! other sites from using this server's resources. Choose them according to the
//! application's resource and interaction requirements.
//!
//! Permissions Policy accepts an ordinary [`HeaderValue`] because feature names,
//! origin allowlists, and browser support vary independently. The application
//! supplies valid Permissions Policy syntax; `HeaderValue` checks HTTP value
//! syntax, not that policy's grammar. Omitted directives retain browser-defined
//! behavior. There is no implicit deny-all list. CSP, CORS, reporting endpoints,
//! and report-only policies remain ordinary application response headers.
//!
//! Place the security layer outside response-producing middleware so that it
//! also processes redirects, failures, conditional responses, and static files.
//! A service error remains the original error; place any error-to-response
//! conversion inside this layer if its responses need these headers.
//!
//! # Sensitive headers and logging
//!
//! Use Tower HTTP's `sensitive-headers` feature directly with
//! [`CREDENTIAL_HEADER_NAMES`]. It marks every repeated value without changing
//! the header bytes. Add application-specific secret headers explicitly:
//!
//! ```
//! use http::HeaderName;
//! use primitives_rs::http::security_headers::CREDENTIAL_HEADER_NAMES;
//! use std::sync::Arc;
//! use tower_http::sensitive_headers::{
//!     SetSensitiveRequestHeadersLayer, SetSensitiveResponseHeadersLayer,
//! };
//!
//! let names: Arc<[HeaderName]> = CREDENTIAL_HEADER_NAMES.into_iter()
//!     .chain([HeaderName::from_static("x-api-key")])
//!     .collect::<Vec<_>>().into();
//! let request_marks = SetSensitiveRequestHeadersLayer::from_shared(names.clone());
//! let response_marks = SetSensitiveResponseHeadersLayer::from_shared(names);
//! # let _ = (request_marks, response_marks);
//! ```
//!
//! In a Tower `ServiceBuilder`, add `request_marks`, logging, and `response_marks`
//! in that order, before the application. This places request marking before
//! request logging and response marking before response logging. Put response
//! producers inside the response marker. Without intervening logging,
//! `SetSensitiveHeadersLayer` marks both directions in one layer.
//!
//! Sensitivity flags redact `HeaderValue`'s debug output and inform transports
//! that honor the flag. They do not redact code that explicitly reads header
//! bytes or converts them to strings. Keep logging code consistent with that
//! contract. This module does not install middleware into a framework.

mod policies;

pub use policies::{
	CoepPolicy, CoopPolicy, CorpPolicy, ExistingSecurityHeaders, FrameOptions, HstsConfig,
	ReferrerPolicy,
};

use http::header::{
	AUTHORIZATION, COOKIE, HeaderName, PROXY_AUTHORIZATION, REFERRER_POLICY, SERVER, SET_COOKIE,
	STRICT_TRANSPORT_SECURITY, X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use http::{HeaderMap, HeaderValue, Request, Response};
use pin_project_lite::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use tower_layer::Layer;
use tower_service::Service;

/// Standard credential-bearing header names for request and response marking.
///
/// Pass these names to Tower HTTP's sensitive-header layers and add any custom
/// headers that contain application secrets. Marking also applies when one of
/// these names occurs in the opposite direction from its usual use.
pub const CREDENTIAL_HEADER_NAMES: [HeaderName; 4] =
	[AUTHORIZATION, PROXY_AUTHORIZATION, COOKIE, SET_COOKIE];

/// Explicit application choices for [`SecurityHeadersLayer`].
///
/// Each `None` leaves the corresponding header unmanaged. Existing values are
/// retained unless a configured value and [`ExistingSecurityHeaders::Replace`]
/// both apply. The layer always manages MIME-sniffing and legacy cross-domain
/// policy protection and always removes `Server` and `X-Powered-By`.
#[derive(Clone, Debug)]
pub struct SecurityHeadersConfig {
	/// Whether configured values preserve or replace existing response values.
	pub existing_headers: ExistingSecurityHeaders,
	/// HTTP Strict Transport Security for an HTTPS deployment.
	pub hsts: Option<HstsConfig>,
	/// Referrer information that browsers may send on subsequent requests.
	pub referrer_policy: Option<ReferrerPolicy>,
	/// Legacy frame-embedding protection. CSP `frame-ancestors` remains independent.
	pub frame_options: Option<FrameOptions>,
	/// Cross-Origin-Opener-Policy for document and popup isolation.
	pub coop: Option<CoopPolicy>,
	/// Cross-Origin-Embedder-Policy for loading cross-origin resources.
	pub coep: Option<CoepPolicy>,
	/// Cross-Origin-Resource-Policy for use of this server's resources.
	pub corp: Option<CorpPolicy>,
	/// The complete Permissions Policy header, including any origin allowlists.
	/// The application owns policy syntax and feature selection.
	pub permissions_policy: Option<HeaderValue>,
}

/// A compiled response-header policy for ordinary HTTP and Tower services.
///
/// Construction formats values once. Clones and response futures share those
/// values. The layer requires no async runtime, body trait, or `Send` bound.
#[derive(Clone, Debug)]
pub struct SecurityHeadersLayer {
	headers: Arc<[(HeaderName, HeaderValue)]>,
	existing_headers: ExistingSecurityHeaders,
}

impl SecurityHeadersLayer {
	/// Compile the supplied application policy.
	pub fn new(config: SecurityHeadersConfig) -> Self {
		let mut headers = vec![
			(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
			(
				HeaderName::from_static("x-permitted-cross-domain-policies"),
				HeaderValue::from_static("none"),
			),
		];
		for (name, value) in [
			(
				STRICT_TRANSPORT_SECURITY,
				config.hsts.map(HeaderValue::from),
			),
			(
				REFERRER_POLICY,
				config.referrer_policy.map(HeaderValue::from),
			),
			(X_FRAME_OPTIONS, config.frame_options.map(HeaderValue::from)),
			(
				HeaderName::from_static("cross-origin-opener-policy"),
				config.coop.map(HeaderValue::from),
			),
			(
				HeaderName::from_static("cross-origin-embedder-policy"),
				config.coep.map(HeaderValue::from),
			),
			(
				HeaderName::from_static("cross-origin-resource-policy"),
				config.corp.map(HeaderValue::from),
			),
			(
				HeaderName::from_static("permissions-policy"),
				config.permissions_policy,
			),
		] {
			if let Some(value) = value {
				headers.push((name, value));
			}
		}
		Self {
			headers: headers.into(),
			existing_headers: config.existing_headers,
		}
	}

	/// Apply this policy directly to response headers without using a Tower service.
	pub fn apply_to_headers(&self, headers: &mut HeaderMap) {
		for (name, value) in self.headers.iter() {
			match self.existing_headers {
				ExistingSecurityHeaders::Preserve => {
					headers.entry(name).or_insert_with(|| value.clone());
				}
				ExistingSecurityHeaders::Replace => {
					headers.insert(name.clone(), value.clone());
				}
			}
		}
		headers.remove(SERVER);
		headers.remove("x-powered-by");
	}
}

impl<S> Layer<S> for SecurityHeadersLayer {
	type Service = SecurityHeadersService<S>;

	fn layer(&self, inner: S) -> Self::Service {
		SecurityHeadersService {
			inner,
			policy: self.clone(),
		}
	}
}

/// A service that applies its security policy to inner responses.
#[derive(Clone, Debug)]
pub struct SecurityHeadersService<S> {
	inner: S,
	policy: SecurityHeadersLayer,
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for SecurityHeadersService<S>
where
	S: Service<Request<ReqBody>, Response = Response<ResBody>>,
{
	type Response = Response<ResBody>;
	type Error = S::Error;
	type Future = SecurityHeadersFuture<S::Future>;

	fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.inner.poll_ready(cx)
	}

	fn call(&mut self, request: Request<ReqBody>) -> Self::Future {
		SecurityHeadersFuture {
			future: self.inner.call(request),
			policy: self.policy.clone(),
		}
	}
}

pin_project! {
	/// The response future produced by [`SecurityHeadersService`].
	#[derive(Debug)]
	pub struct SecurityHeadersFuture<F> {
		#[pin]
		future: F,
		policy: SecurityHeadersLayer,
	}
}

impl<F, ResBody, E> Future for SecurityHeadersFuture<F>
where
	F: Future<Output = Result<Response<ResBody>, E>>,
{
	type Output = F::Output;

	fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		let this = self.project();
		let mut response = ready!(this.future.poll(cx))?;
		this.policy.apply_to_headers(response.headers_mut());
		Poll::Ready(Ok(response))
	}
}
