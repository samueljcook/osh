//! Bounded ETag generation and `If-None-Match` handling for ordinary HTTP services.
//!
//! Enable the `conditional-responses` Cargo feature. [`EtagLayer`] runs on a
//! Tokio runtime with timers enabled. Applications retain cache policy,
//! authentication, authorization, date-based validation, and mutation
//! preconditions. Date-based validation must ignore If-Modified-Since when a
//! request has If-None-Match, as [`EtagLayer`] describes. Body-derived ETags
//! save transfer after the handler runs; [`if_none_match_matches`] also
//! supports cheap handler-level validation.
//!
//! ```
//! use std::time::Duration;
//! use primitives_rs::http::conditional_responses::{EtagConfig, EtagLayer, EtagStrength};
//!
//! let etags = EtagLayer::new(EtagConfig {
//!     max_buffer_size: 1024 * 1024,
//!     buffer_timeout: Duration::from_millis(100),
//!     strength: EtagStrength::Weak,
//!     additional_representation_header_names: vec![],
//! })?.skip_request_fn(|request| request.uri.path() == "/healthz");
//! // Apply `etags` through the host's normal Tower layer composition.
//! # Ok::<(), primitives_rs::http::conditional_responses::EtagConfigError>(())
//! ```
//!
//! A response-body timeout layer can remain outside this layer to limit later
//! body reads; this layer separately bounds its own buffering read. If an inner
//! wrapper removes the size hint, a valid Content-Length still permits bounded
//! generation. Without either length, generation is bypassed. Neither layer
//! replaces the host's handler deadline or transport write timeout.

mod body;
mod entity_tags;

pub use body::EtagBody;
pub use entity_tags::if_none_match_matches;

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::header::{
	CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_ENCODING, CONTENT_LANGUAGE, CONTENT_LENGTH,
	CONTENT_RANGE, CONTENT_TYPE, ETAG, IF_MATCH, IF_NONE_MATCH, IF_RANGE, IF_UNMODIFIED_SINCE,
	RANGE, SET_COOKIE, TRAILER,
};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode};
use http_body::Body;
use tower_layer::Layer;
use tower_service::Service;

/// Whether a generated validator promises byte identity or semantic equivalence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EtagStrength {
	/// Allows equivalent representations to have different encoded bytes.
	Weak,
	/// Identifies the final representation bytes. Apply ETag generation after
	/// compression or other body transformations on the response path.
	Strong,
}

/// Explicit resource bounds and representation metadata for generated ETags.
#[derive(Clone, Debug)]
pub struct EtagConfig {
	/// Maximum total body size in bytes eligible for buffering.
	pub max_buffer_size: usize,
	/// Total time allowed for the buffering read, including all body frames.
	/// This does not limit the handler or subsequent transmission of a body
	/// that bypasses ETag generation.
	pub buffer_timeout: Duration,
	/// Whether generated ETags are weak or strong.
	pub strength: EtagStrength,
	/// Additional response headers that distinguish representations. All values,
	/// their order, and absence versus an empty value affect the generated tag.
	/// Content-Type, Content-Encoding, Content-Language, and Content-Disposition
	/// are always included. Applications must still set Vary when request
	/// headers select a representation.
	pub additional_representation_header_names: Vec<HeaderName>,
}

/// An invalid ETag resource bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EtagConfigError {
	/// The buffer size must be positive.
	ZeroBufferSize,
	/// The buffering timeout must be positive.
	ZeroBufferTimeout,
	/// The platform cannot represent the buffering deadline.
	BufferTimeoutTooLarge,
}

impl fmt::Display for EtagConfigError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::ZeroBufferSize => "ETag buffer size must be positive",
			Self::ZeroBufferTimeout => "ETag buffer timeout must be positive",
			Self::BufferTimeoutTooLarge => "ETag buffer timeout exceeds the platform's clock range",
		})
	}
}

impl std::error::Error for EtagConfigError {}

type SkipRequestFn = Arc<dyn Fn(&http::request::Parts) -> bool + Send + Sync>;

/// A Tower layer that adds ETags to eligible GET responses and answers matching
/// GET/HEAD requests with 304.
///
/// The layer handles only status 200. It bypasses responses with Set-Cookie,
/// Cache-Control: no-store, Content-Range, or declared trailers. It preserves an
/// existing ETag and can use it without reading the body. It never generates a
/// tag from a HEAD body because that body need not contain the GET representation.
///
/// Generation requires an exact body size hint or a single Content-Length,
/// within the configured bound. Unknown-length streams pass through without
/// polling. A size overrun, buffering timeout, trailer, or body error stops
/// generation and preserves the consumed prefix and remaining body. No handler
/// failure or body failure is replaced with a synthesized response.
///
/// Requests with Range, If-Range, If-Match, or If-Unmodified-Since bypass this
/// layer; the host must evaluate those conditions in their required order. The
/// layer does not evaluate If-Modified-Since or mutation preconditions.
///
/// When a request has If-None-Match, the handler must ignore If-Modified-Since
/// (RFC 9110 section 13.2.2) and must not answer 304 from a date. This layer
/// compares If-None-Match with a body-derived tag only after the handler
/// returns a body, so a handler that answers 304 from If-Modified-Since can
/// serve a stale representation when the resource changed within the same
/// Last-Modified second and the client's tag no longer matches.
///
/// Generated tags use a BLAKE3 hash of length-delimited body and representation
/// metadata. They are cache validators, not authenticated integrity proofs.
/// The layer sets no Cache-Control or Vary policy. Install it only on routes
/// where buffering and HTTP validation are appropriate. For strong tags, put
/// body transformations inside this layer so that it hashes their final output.
#[derive(Clone)]
pub struct EtagLayer {
	config: Arc<EtagConfig>,
	skip_request_fn: Option<SkipRequestFn>,
}

impl EtagLayer {
	/// Parses explicit bounds and captures a canonical representation-header list.
	pub fn new(mut config: EtagConfig) -> Result<Self, EtagConfigError> {
		if config.max_buffer_size == 0 {
			return Err(EtagConfigError::ZeroBufferSize);
		}
		if config.buffer_timeout.is_zero() {
			return Err(EtagConfigError::ZeroBufferTimeout);
		}
		if Instant::now().checked_add(config.buffer_timeout).is_none() {
			return Err(EtagConfigError::BufferTimeoutTooLarge);
		}
		config.additional_representation_header_names.extend([
			CONTENT_TYPE,
			CONTENT_ENCODING,
			CONTENT_LANGUAGE,
			CONTENT_DISPOSITION,
		]);
		config
			.additional_representation_header_names
			.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
		config.additional_representation_header_names.dedup();
		Ok(Self {
			config: Arc::new(config),
			skip_request_fn: None,
		})
	}

	/// Excludes requests selected by an application predicate. Excluded requests
	/// retain their response, and the layer does not poll their body.
	pub fn skip_request_fn<F>(mut self, skip_request_fn: F) -> Self
	where
		F: Fn(&http::request::Parts) -> bool + Send + Sync + 'static,
	{
		self.skip_request_fn = Some(Arc::new(skip_request_fn));
		self
	}
}

impl fmt::Debug for EtagLayer {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("EtagLayer")
			.field("config", &self.config)
			.field("has_skip_request_fn", &self.skip_request_fn.is_some())
			.finish()
	}
}

impl<S> Layer<S> for EtagLayer {
	type Service = EtagService<S>;

	fn layer(&self, inner: S) -> Self::Service {
		EtagService {
			inner,
			layer: self.clone(),
		}
	}
}

/// The HTTP service produced by [`EtagLayer`].
#[derive(Clone, Debug)]
pub struct EtagService<S> {
	inner: S,
	layer: EtagLayer,
}

impl<S, RequestBody, ResponseBody> Service<Request<RequestBody>> for EtagService<S>
where
	S: Service<Request<RequestBody>, Response = Response<ResponseBody>>,
	S::Future: Send + 'static,
	ResponseBody: Body<Data = Bytes> + Send + 'static,
	ResponseBody::Error: Send,
{
	type Response = Response<EtagBody<ResponseBody>>;
	type Error = S::Error;
	type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

	fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.inner.poll_ready(cx)
	}

	fn call(&mut self, request: Request<RequestBody>) -> Self::Future {
		let (parts, body) = request.into_parts();
		let eligible_request = matches!(parts.method, Method::GET | Method::HEAD)
			&& ![RANGE, IF_RANGE, IF_MATCH, IF_UNMODIFIED_SINCE]
				.iter()
				.any(|name| parts.headers.contains_key(name))
			&& !self
				.layer
				.skip_request_fn
				.as_ref()
				.is_some_and(|skip| skip(&parts));
		let method = parts.method.clone();
		let mut request_headers = HeaderMap::new();
		if eligible_request {
			for value in parts.headers.get_all(IF_NONE_MATCH) {
				request_headers.append(IF_NONE_MATCH, value.clone());
			}
		}
		let config = Arc::clone(&self.layer.config);
		let future = self.inner.call(Request::from_parts(parts, body));
		Box::pin(async move {
			let response = future.await?;
			if !eligible_request || !response_is_eligible(&response) {
				return Ok(response.map(EtagBody::passthrough));
			}
			if response.headers().contains_key(ETAG) {
				let mut tags = response.headers().get_all(ETAG).iter();
				let matches = tags
					.next()
					.is_some_and(|tag| if_none_match_matches(&request_headers, tag))
					&& tags.next().is_none();
				return Ok(if matches {
					not_modified(response.into_parts().0)
				} else {
					response.map(EtagBody::passthrough)
				});
			}
			if method == Method::HEAD {
				return Ok(response.map(EtagBody::passthrough));
			}
			let Some(expected_size) = buffering_size(&response, config.max_buffer_size) else {
				return Ok(response.map(EtagBody::passthrough));
			};
			let (mut parts, body) = response.into_parts();
			let buffered = body::buffer_body(
				body,
				expected_size,
				config.max_buffer_size,
				config.buffer_timeout,
			)
			.await;
			let bytes = match buffered {
				body::BufferingOutcome::Completed(bytes) => bytes,
				body::BufferingOutcome::Interrupted(body) => {
					return Ok(Response::from_parts(parts, body));
				}
			};
			if bytes.len() != expected_size {
				return Ok(Response::from_parts(parts, EtagBody::buffered(bytes)));
			}
			let tag = generate_etag(&bytes, &parts.headers, &config);
			let matches = if_none_match_matches(&request_headers, &tag);
			parts.headers.insert(ETAG, tag);
			let response = Response::from_parts(parts, EtagBody::buffered(bytes));
			Ok(if matches {
				not_modified(response.into_parts().0)
			} else {
				response
			})
		})
	}
}

fn response_is_eligible<B>(response: &Response<B>) -> bool {
	response.status() == StatusCode::OK
		&& ![SET_COOKIE, CONTENT_RANGE, TRAILER]
			.iter()
			.any(|name| response.headers().contains_key(name))
		&& !response
			.headers()
			.get_all(CACHE_CONTROL)
			.iter()
			.any(cache_control_prevents_storage)
}

enum CacheDirectivePosition {
	Unquoted,
	Quoted,
	Escaped,
}

fn cache_control_prevents_storage(value: &HeaderValue) -> bool {
	let bytes = value.as_bytes();
	let mut start = 0;
	let mut position = CacheDirectivePosition::Unquoted;
	for (index, &byte) in bytes.iter().enumerate() {
		position = match (position, byte) {
			(CacheDirectivePosition::Escaped, _) => CacheDirectivePosition::Quoted,
			(CacheDirectivePosition::Quoted, b'\\') => CacheDirectivePosition::Escaped,
			(CacheDirectivePosition::Quoted, b'"') => CacheDirectivePosition::Unquoted,
			(CacheDirectivePosition::Unquoted, b'"') => CacheDirectivePosition::Quoted,
			(CacheDirectivePosition::Unquoted, b',') => {
				if is_no_store_directive(&bytes[start..index]) {
					return true;
				}
				start = index + 1;
				CacheDirectivePosition::Unquoted
			}
			(position, _) => position,
		};
	}
	// Do not transform a response with an unterminated cache directive.
	!matches!(position, CacheDirectivePosition::Unquoted) || is_no_store_directive(&bytes[start..])
}

fn is_no_store_directive(directive: &[u8]) -> bool {
	directive
		.split(|&byte| byte == b'=')
		.next()
		.unwrap_or_default()
		.trim_ascii()
		.eq_ignore_ascii_case(b"no-store")
}

fn buffering_size<B: Body>(response: &Response<B>, max_buffer_size: usize) -> Option<usize> {
	let mut lengths = response.headers().get_all(CONTENT_LENGTH).iter();
	let declared_size = match lengths.next() {
		Some(value) => {
			let text = value.to_str().ok()?;
			if text.is_empty()
				|| !text.bytes().all(|b| b.is_ascii_digit())
				|| lengths.next().is_some()
			{
				return None;
			}
			Some(text.parse::<u64>().ok()?)
		}
		None => None,
	};
	let hint = response.body().size_hint().exact();
	if let (Some(hint), Some(declared)) = (hint, declared_size)
		&& hint != declared
	{
		return None;
	}
	let size = usize::try_from(hint.or(declared_size)?).ok()?;
	(size <= max_buffer_size).then_some(size)
}

fn generate_etag(bytes: &[u8], headers: &HeaderMap, config: &EtagConfig) -> HeaderValue {
	let mut hasher = blake3::Hasher::new_derive_key("primitives-rs/http-etag/v1");
	hasher.update(&(config.additional_representation_header_names.len() as u64).to_le_bytes());
	for name in &config.additional_representation_header_names {
		hash_component(&mut hasher, name.as_str().as_bytes());
		let values = headers.get_all(name);
		hasher.update(&(values.iter().count() as u64).to_le_bytes());
		for value in values {
			hash_component(&mut hasher, value.as_bytes());
		}
	}
	hash_component(&mut hasher, bytes);
	let prefix = match config.strength {
		EtagStrength::Weak => "W/",
		EtagStrength::Strong => "",
	};
	HeaderValue::from_str(&format!("{prefix}\"{}\"", hasher.finalize().to_hex()))
		.expect("hex digest is valid in an ETag")
}

fn hash_component(hasher: &mut blake3::Hasher, bytes: &[u8]) {
	hasher.update(&(bytes.len() as u64).to_le_bytes());
	hasher.update(bytes);
}

fn not_modified<B: Body<Data = Bytes>>(mut parts: http::response::Parts) -> Response<EtagBody<B>> {
	prepare_not_modified(&mut parts);
	Response::from_parts(parts, EtagBody::buffered(Bytes::new()))
}

fn prepare_not_modified(parts: &mut http::response::Parts) {
	parts.status = StatusCode::NOT_MODIFIED;
	for name in [
		"content-type",
		"content-length",
		"content-encoding",
		"content-language",
		"content-md5",
		"content-range",
		"content-disposition",
		"digest",
		"transfer-encoding",
		"trailer",
	] {
		parts.headers.remove(name);
	}
}
