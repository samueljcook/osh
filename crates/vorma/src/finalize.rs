//! Response finalization: ordered contributions composed into HTTP responses.
//!
//! Header changes are replayed in order: a set replaces every value that
//! the name had so far, and an add appends another field line with its value.
//! A header change that names one of Vorma's protocol headers is a
//! finalization error. An endpoint response with a JSON body keeps an
//! application's `content-type` when it is `application/json` or a media type
//! that ends in `+json`, and any other `content-type` on such a body is a
//! finalization error. Vorma sets the `content-type` of an HTML document and
//! of a navigation payload, whatever an application set, because the browser
//! client reads both as Vorma's own formats. A navigation redirect has two
//! forms, chosen by the request's `x-accepts-client-redirect` header, so Vorma
//! adds that header to the redirect's `Vary` header after the application's
//! values. HTTP forbids a body with status 204, 205, or 304, so a
//! response that Vorma would send with a body and one of those statuses is a
//! finalization error as well. A `HEAD` response's omitted body is ordinary
//! HTTP handling, not that error. An endpoint's finalization error is answered
//! with the failure envelope of an undeclared 500, which the client reports as
//! an undeclared failure; any other finalization error is answered with a
//! plain-text 500.

use std::fmt;

use bytes::Bytes;
use http::header::{CACHE_CONTROL, CONTENT_TYPE, LOCATION, SET_COOKIE, VARY};
use http::{HeaderMap, HeaderName, HeaderValue, Response, StatusCode};
use http_body_util::Full;
use primitives_rs::http::media_types::MediaTypeEssence;
use serde::Serialize;
use serde_json::value::RawValue;

use crate::build_output::ClientBuildId;
use crate::result::{HeaderChange, RedirectStatus};
use crate::{
	outcome::{Contribution, PublicError},
	payload::FailureWire,
};

/// The body of every response that Vorma makes itself.
pub(crate) type VormaBody = Full<Bytes>;

pub(crate) const CLIENT_BUILD_ID_HEADER: HeaderName =
	HeaderName::from_static("x-vorma-client-build-id");
pub(crate) const BUILD_SKEW_HEADER: HeaderName = HeaderName::from_static("x-vorma-build-skew");
pub(crate) const CLIENT_REDIRECT_HEADER: HeaderName = HeaderName::from_static("x-client-redirect");
pub(crate) const CLIENT_REDIRECT_STATUS_HEADER: HeaderName =
	HeaderName::from_static("x-client-redirect-status");
pub(crate) const NO_OUTPUT_HEADER: HeaderName = HeaderName::from_static("x-vorma-no-output");
pub(crate) const ACCEPTS_CLIENT_REDIRECT_HEADER: HeaderName =
	HeaderName::from_static("x-accepts-client-redirect");

const JSON_CONTENT_TYPE: &str = "application/json; charset=utf-8";
const HTML_CONTENT_TYPE: &str = "text/html; charset=utf-8";
const TEXT_CONTENT_TYPE: &str = "text/plain; charset=utf-8";
const VIEW_CACHE_CONTROL: &str = "private, max-age=0, must-revalidate, no-cache";
const BUILD_SKEW_BODY: &str = r#"{"ok":true}"#;

/// Whether a `Content-Type` value is `application/json` or a media type with
/// a `+json` suffix, such as `application/merge-patch+json`. Parameters such as
/// `charset` are allowed and are not checked, and the comparison ignores case.
/// A value with bytes outside ASCII is not JSON.
pub(crate) fn is_json_content_type(value: &HeaderValue) -> bool {
	value
		.to_str()
		.ok()
		.and_then(|text| MediaTypeEssence::parse(text).ok())
		.is_some_and(|essence| essence.is_json())
}

fn is_framework_owned(name: &HeaderName) -> bool {
	[
		&CLIENT_BUILD_ID_HEADER,
		&BUILD_SKEW_HEADER,
		&CLIENT_REDIRECT_HEADER,
		&CLIENT_REDIRECT_STATUS_HEADER,
		&NO_OUTPUT_HEADER,
	]
	.contains(&name)
}

/// A response that Vorma could not assemble.
#[derive(Debug)]
pub(crate) enum FinalizeError {
	FrameworkOwnedHeader(HeaderName),
	BodyForbiddenByStatus(StatusCode),
	NonJsonContentType(HeaderValue),
	Serialization(serde_json::Error),
}

impl fmt::Display for FinalizeError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::FrameworkOwnedHeader(name) => write!(
				formatter,
				"a handler set the {name} header, which Vorma owns"
			),
			Self::BodyForbiddenByStatus(status) => write!(
				formatter,
				"the response has a body, which HTTP forbids with status {}",
				status.as_u16()
			),
			Self::NonJsonContentType(value) => write!(
				formatter,
				"a handler set the content-type header to {:?} on a JSON response, and a JSON response needs application/json or a media type that ends in +json",
				String::from_utf8_lossy(value.as_bytes())
			),
			Self::Serialization(error) => {
				write!(formatter, "the response could not be serialized: {error}")
			}
		}
	}
}

/// A redirect as the response carries it: its `location`, its status, and
/// its form.
pub(crate) struct ResolvedRedirect {
	pub(crate) location: HeaderValue,
	pub(crate) status: RedirectStatus,
	pub(crate) form: RedirectForm,
}

/// How a response carries a redirect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RedirectForm {
	/// An endpoint redirect: a 3xx response, which the caller's HTTP client
	/// follows.
	ApiEndpoint,
	/// A navigation redirect for a request without
	/// `x-accepts-client-redirect`: a 3xx response.
	NavResponse,
	/// A navigation redirect for a request with `x-accepts-client-redirect`:
	/// 200 with the redirect in headers, which the browser runtime follows
	/// itself.
	NavFollowedByClient,
}

fn replay_headers(
	contribution: &Contribution,
	client_build_id: &ClientBuildId,
) -> Result<HeaderMap, FinalizeError> {
	let mut headers = HeaderMap::new();
	for change in &contribution.header_changes {
		let (HeaderChange::Set { name, .. } | HeaderChange::Add { name, .. }) = change;
		if is_framework_owned(name) {
			return Err(FinalizeError::FrameworkOwnedHeader(name.clone()));
		}
		change.apply_to(&mut headers);
	}
	for cookie in &contribution.cookies {
		let value = primitives_rs::http::cookies::headers::to_set_cookie_header(cookie)
			.expect("Vorma checked that the cookie is a valid field value");
		headers.append(SET_COOKIE, value);
	}
	headers.insert(
		CLIENT_BUILD_ID_HEADER,
		client_build_id.header_value().clone(),
	);
	Ok(headers)
}

/// What a finalization asks for: a body, or a redirect, which has no body.
enum RequestedContent {
	Body(ResponseBody),
	Redirect(ResolvedRedirect),
}

/// The body of a response and the rule for its `content-type`.
enum ResponseBody {
	Empty,
	/// A navigation payload. Vorma sets its JSON `content-type`.
	NavPayloadJson(Bytes),
	/// An endpoint's JSON. An application's JSON `content-type` stays.
	ApiEndpointJson(Bytes),
	/// A document. Vorma sets its HTML `content-type`.
	Html(Bytes),
}

struct Finalization<'a> {
	contribution: &'a Contribution,
	client_build_id: &'a ClientBuildId,
	content: RequestedContent,
}

fn finalize_with(input: Finalization<'_>) -> Result<Response<VormaBody>, FinalizeError> {
	let mut headers = replay_headers(input.contribution, input.client_build_id)?;
	let mut status = input.contribution.status.unwrap_or(StatusCode::OK);
	let body = match input.content {
		RequestedContent::Body(body) => body,
		RequestedContent::Redirect(redirect) => {
			let status_code = redirect.status.to_status_code();
			match redirect.form {
				RedirectForm::NavFollowedByClient => {
					status = StatusCode::OK;
					headers.insert(CLIENT_REDIRECT_HEADER, redirect.location);
					headers.insert(
						CLIENT_REDIRECT_STATUS_HEADER,
						HeaderValue::from(status_code.as_u16()),
					);
				}
				RedirectForm::NavResponse | RedirectForm::ApiEndpoint => {
					status = status_code;
					headers.insert(LOCATION, redirect.location);
				}
			}
			if redirect.form != RedirectForm::ApiEndpoint {
				headers.append(VARY, ACCEPTS_CLIENT_REDIRECT_HEADER.into());
			}
			ResponseBody::Empty
		}
	};
	if !matches!(body, ResponseBody::Empty)
		&& matches!(
			status,
			StatusCode::NO_CONTENT | StatusCode::RESET_CONTENT | StatusCode::NOT_MODIFIED
		) {
		return Err(FinalizeError::BodyForbiddenByStatus(status));
	}
	let bytes = match body {
		ResponseBody::Empty => Bytes::new(),
		ResponseBody::ApiEndpointJson(bytes) => {
			if let Some(invalid) = headers
				.get_all(CONTENT_TYPE)
				.iter()
				.find(|value| !is_json_content_type(value))
			{
				return Err(FinalizeError::NonJsonContentType(invalid.clone()));
			}
			if !headers.contains_key(CONTENT_TYPE) {
				headers.insert(CONTENT_TYPE, HeaderValue::from_static(JSON_CONTENT_TYPE));
			}
			bytes
		}
		ResponseBody::NavPayloadJson(bytes) => {
			headers.insert(CONTENT_TYPE, HeaderValue::from_static(JSON_CONTENT_TYPE));
			bytes
		}
		ResponseBody::Html(bytes) => {
			headers.insert(CONTENT_TYPE, HeaderValue::from_static(HTML_CONTENT_TYPE));
			bytes
		}
	};
	let mut response = Response::new(Full::new(bytes));
	*response.headers_mut() = headers;
	*response.status_mut() = status;
	Ok(response)
}

/// Prevents implicit reuse of request-dependent view data without replacing an
/// authored policy.
fn with_view_cache_control(mut response: Response<VormaBody>) -> Response<VormaBody> {
	if !response.headers().contains_key(CACHE_CONTROL) {
		response
			.headers_mut()
			.insert(CACHE_CONTROL, HeaderValue::from_static(VIEW_CACHE_CONTROL));
	}
	response
}

/// A navigation's JSON payload.
pub(crate) fn finalize_json(
	json: String,
	contribution: &Contribution,
	client_build_id: &ClientBuildId,
) -> Result<Response<VormaBody>, FinalizeError> {
	finalize_with(Finalization {
		contribution,
		client_build_id,
		content: RequestedContent::Body(ResponseBody::NavPayloadJson(Bytes::from(json))),
	})
	.map(with_view_cache_control)
}

/// A first load's HTML.
pub(crate) fn finalize_html(
	html: String,
	contribution: &Contribution,
	client_build_id: &ClientBuildId,
) -> Result<Response<VormaBody>, FinalizeError> {
	finalize_with(Finalization {
		contribution,
		client_build_id,
		content: RequestedContent::Body(ResponseBody::Html(Bytes::from(html))),
	})
	.map(with_view_cache_control)
}

/// An endpoint's success: its output as JSON, or, when it
/// declares no output, no body and the header that tells the browser so.
pub(crate) fn finalize_api_endpoint_success(
	output: Option<&RawValue>,
	contribution: &Contribution,
	client_build_id: &ClientBuildId,
) -> Result<Response<VormaBody>, FinalizeError> {
	let body = output.map(|output| Bytes::copy_from_slice(output.get().as_bytes()));
	let has_body = body.is_some();
	let mut response = finalize_with(Finalization {
		contribution,
		client_build_id,
		content: RequestedContent::Body(
			body.map_or(ResponseBody::Empty, ResponseBody::ApiEndpointJson),
		),
	})?;
	if !has_body {
		response
			.headers_mut()
			.insert(NO_OUTPUT_HEADER, HeaderValue::from_static("1"));
	}
	Ok(response)
}

/// An endpoint's `304 Not Modified` answer, which has no body and no content
/// type.
pub(crate) fn finalize_not_modified(
	contribution: &Contribution,
	client_build_id: &ClientBuildId,
) -> Result<Response<VormaBody>, FinalizeError> {
	finalize_with(Finalization {
		contribution,
		client_build_id,
		content: RequestedContent::Body(ResponseBody::Empty),
	})
}

#[derive(Serialize)]
struct EndpointErrorEnvelope<'a> {
	failure: FailureWire<'a>,
}

/// An endpoint's public failure envelope, with the failure's status.
pub(crate) fn finalize_api_endpoint_failure(
	status: StatusCode,
	public_error: Option<&PublicError>,
	contribution: &Contribution,
	client_build_id: &ClientBuildId,
) -> Result<Response<VormaBody>, FinalizeError> {
	let envelope = EndpointErrorEnvelope {
		failure: FailureWire {
			index: None,
			status: status.as_u16(),
			public_error,
		},
	};
	let body = serde_json::to_vec(&envelope).map_err(FinalizeError::Serialization)?;
	finalize_with(Finalization {
		contribution,
		client_build_id,
		content: RequestedContent::Body(ResponseBody::ApiEndpointJson(Bytes::from(body))),
	})
}

/// A redirect of a page request, which carries no body and has the view
/// cache policy unless an application set one, because a handler's decision
/// to redirect is request-dependent view data.
pub(crate) fn finalize_page_redirect(
	redirect: ResolvedRedirect,
	contribution: &Contribution,
	client_build_id: &ClientBuildId,
) -> Result<Response<VormaBody>, FinalizeError> {
	finalize_with(Finalization {
		contribution,
		client_build_id,
		content: RequestedContent::Redirect(redirect),
	})
	.map(with_view_cache_control)
}

/// A redirect of an endpoint request, which carries no body.
pub(crate) fn finalize_api_endpoint_redirect(
	redirect: ResolvedRedirect,
	contribution: &Contribution,
	client_build_id: &ClientBuildId,
) -> Result<Response<VormaBody>, FinalizeError> {
	finalize_with(Finalization {
		contribution,
		client_build_id,
		content: RequestedContent::Redirect(redirect),
	})
}

/// The answer to a navigation from a build that is not the server's.
pub(crate) fn finalize_build_skew(client_build_id: &ClientBuildId) -> Response<VormaBody> {
	let mut response = Response::new(Full::new(Bytes::from_static(BUILD_SKEW_BODY.as_bytes())));
	let headers = response.headers_mut();
	headers.insert(CONTENT_TYPE, HeaderValue::from_static(JSON_CONTENT_TYPE));
	headers.insert(
		CLIENT_BUILD_ID_HEADER,
		client_build_id.header_value().clone(),
	);
	headers.insert(BUILD_SKEW_HEADER, HeaderValue::from_static("1"));
	headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
	response
}

/// A plain-text response for a Vorma protocol or execution failure.
pub(crate) fn finalize_status(
	status: StatusCode,
	client_build_id: Option<&ClientBuildId>,
) -> Response<VormaBody> {
	let text = status.canonical_reason().unwrap_or_default();
	let mut response = Response::new(Full::new(Bytes::from(text)));
	*response.status_mut() = status;
	let headers = response.headers_mut();
	headers.insert(CONTENT_TYPE, HeaderValue::from_static(TEXT_CONTENT_TYPE));
	if let Some(client_build_id) = client_build_id {
		headers.insert(
			CLIENT_BUILD_ID_HEADER,
			client_build_id.header_value().clone(),
		);
	}
	response
}

/// Reports a response that Vorma could not assemble and answers 500.
pub(crate) fn finalize_or_report(
	result: Result<Response<VormaBody>, FinalizeError>,
	client_build_id: &ClientBuildId,
) -> Response<VormaBody> {
	result.unwrap_or_else(|error| {
		tracing::error!(error = %error, "Vorma could not assemble a response");
		finalize_status(StatusCode::INTERNAL_SERVER_ERROR, Some(client_build_id))
	})
}

/// Reports an endpoint response that Vorma could not assemble and answers with
/// the failure envelope of an undeclared 500, which the client reads as an
/// undeclared failure.
pub(crate) fn finalize_api_endpoint_or_report(
	result: Result<Response<VormaBody>, FinalizeError>,
	client_build_id: &ClientBuildId,
) -> Response<VormaBody> {
	result.unwrap_or_else(|error| {
		tracing::error!(error = %error, "Vorma could not assemble a response");
		finalize_or_report(
			finalize_api_endpoint_failure(
				StatusCode::INTERNAL_SERVER_ERROR,
				None,
				&Contribution {
					status: Some(StatusCode::INTERNAL_SERVER_ERROR),
					..Contribution::default()
				},
				client_build_id,
			),
			client_build_id,
		)
	})
}

/// The readiness answer in development.
pub(crate) fn finalize_readiness(client_build_id: &ClientBuildId) -> Response<VormaBody> {
	let mut response = Response::new(Full::new(Bytes::from_static(b"ok")));
	let headers = response.headers_mut();
	headers.insert(CONTENT_TYPE, HeaderValue::from_static(TEXT_CONTENT_TYPE));
	headers.insert(
		CLIENT_BUILD_ID_HEADER,
		client_build_id.header_value().clone(),
	);
	response
}
