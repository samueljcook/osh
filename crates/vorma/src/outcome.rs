//! What one handler result or failure contributes to the response, as a
//! value. Execution merges contributions in plan order, so the order in which
//! handlers finish does not change the response.
//!
//! Merge rules, later contribution over earlier:
//! - status: a later status replaces the earlier one;
//! - headers: changes are kept in order and replayed at finalization; a
//!   set replaces what came before it, and an append adds another field line;
//! - cookies: a later cookie replaces an earlier cookie with the same name,
//!   path, domain, and partitioned storage mode;
//! - head items: appended in order, then deduplicated;
//! - document attributes: merged one attribute at a time.

use cookie::Cookie;
use http::StatusCode;
use primitives_rs::serialization::json::FiniteFloats;
use serde::Serialize;
use serde_json::value::RawValue;

use crate::document::{DocumentParts, HeadRules, PreparedDocument};
use crate::result::{
	ApiEndpointRedirect, ApiEndpointResult, ApiEndpointTarget, Continue, Failure, FailureKind,
	HeaderChange, NotModified, PageTarget, Redirect, RedirectStatus, RedirectTarget, Render,
	Respond, ResponseMeta, ResponseMetaState, TerminalOutcome,
};

/// The types that an endpoint's caller relies on. An endpoint redirect's
/// target must have the redirecting endpoint's output and declared-error
/// types, and its input type when the follow-up request carries the body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ApiEndpointTypes {
	pub(crate) input: std::any::TypeId,
	pub(crate) output: std::any::TypeId,
	pub(crate) declared_error: std::any::TypeId,
}

/// The endpoint that returned an endpoint redirect.
#[derive(Clone, Debug)]
pub(crate) struct RedirectingApiEndpoint {
	pub(crate) method: http::Method,
	pub(crate) types: ApiEndpointTypes,
}

/// Resolves targets to hrefs with the request's plan.
pub(crate) trait ResolveTarget: Send + Sync {
	fn resolve_page_target(&self, target: &PageTarget) -> Result<String, String>;

	fn resolve_api_endpoint_target(
		&self,
		redirecting: &RedirectingApiEndpoint,
		target: &ApiEndpointTarget,
		status: RedirectStatus,
	) -> Result<String, String>;
}

/// What a request's response renders, which decides how Vorma checks the
/// results of the request's handlers.
#[derive(Clone, Copy)]
pub(crate) enum ResponseTarget<'a> {
	/// A page: a document or a navigation payload. Document contributions are
	/// prepared with the head rules, and a success status must allow a body.
	Page { head_rules: &'a HeadRules },
	/// An endpoint, which renders no document, so document contributions are
	/// neither prepared nor validated, and have no effect.
	ApiEndpoint,
}

/// How a redirect reaches the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RedirectKind {
	/// Sends the user to a page or a URL: a client redirect when the request
	/// accepts one, and a 3xx response otherwise.
	Nav,
	/// Gives an endpoint call another endpoint's response: always a 3xx
	/// response, which the caller's HTTP client follows.
	ApiEndpoint,
}

/// A handler's redirect with its `location`, status, and kind.
#[derive(Clone, Debug)]
pub(crate) struct Redirection {
	pub(crate) location: http::HeaderValue,
	pub(crate) status: RedirectStatus,
	pub(crate) kind: RedirectKind,
}

/// Response contributions from one handler, or from several merged in order.
#[derive(Clone, Debug, Default)]
pub(crate) struct Contribution {
	pub(crate) status: Option<StatusCode>,
	pub(crate) header_changes: Vec<HeaderChange>,
	pub(crate) cookies: Vec<Cookie<'static>>,
	pub(crate) document: PreparedDocument,
}

impl Contribution {
	/// Merges a later contribution into this one.
	pub(crate) fn merge(&mut self, later: Self) {
		if later.status.is_some() {
			self.status = later.status;
		}
		self.header_changes.extend(later.header_changes);
		for cookie in later.cookies {
			self.cookies.retain(|existing| {
				!primitives_rs::http::cookies::headers::same_cookie_identity(existing, &cookie)
			});
			self.cookies.push(cookie);
		}
		self.document.merge(later.document);
	}
}

/// A success has a 2xx status. A 304 is possible only through
/// [`NotModified`], and a redirect has its redirect status. A page response
/// always has a body, which HTTP forbids with status 204 or 205.
fn check_success_status(status: StatusCode, target: ResponseTarget<'_>) -> Result<(), String> {
	if !status.is_success() {
		return Err(format!(
			"status {} is outside 200 through 299, which a success requires",
			status.as_u16()
		));
	}
	if matches!(target, ResponseTarget::Page { .. })
		&& matches!(status, StatusCode::NO_CONTENT | StatusCode::RESET_CONTENT)
	{
		return Err(format!(
			"status {} forbids a body, and a page response always has one",
			status.as_u16()
		));
	}
	Ok(())
}

/// A failure has a 4xx or 5xx status.
fn check_failure_status(status: StatusCode) -> Result<(), String> {
	if !(status.is_client_error() || status.is_server_error()) {
		return Err(format!(
			"status {} is outside 400 through 599, which a failure requires",
			status.as_u16()
		));
	}
	Ok(())
}

fn to_contribution(meta: ResponseMeta, document: PreparedDocument) -> Result<Contribution, String> {
	match meta.headers_and_cookies {
		ResponseMetaState::Valid {
			header_changes,
			cookies,
		} => Ok(Contribution {
			status: meta.status,
			header_changes,
			cookies,
			document,
		}),
		ResponseMetaState::Invalid { problem } => Err(problem),
	}
}

/// The public part of a handler failure, as the browser receives it. The
/// variant names the failure's kind, and the field name on the wire follows
/// the variant.
#[derive(Debug, Serialize)]
pub(crate) enum PublicError {
	/// The serialized form of the handler's declared error.
	#[serde(rename = "declared_error")]
	Declared(Box<RawValue>),
	/// The serialized form of a task middleware's rejection.
	#[serde(rename = "rejection")]
	Rejection(Box<RawValue>),
}

/// Which public kind a handler's declared error has.
#[derive(Clone, Copy)]
enum DeclaredErrorKind {
	/// An error that the application declares.
	Declared,
	/// A task middleware's rejection.
	Rejection,
}

/// How one handler ended.
#[derive(Debug)]
pub(crate) enum Outcome {
	/// The handler succeeded. `output` is absent for task middleware and for a
	/// handler without output.
	Continue {
		contribution: Contribution,
		output: Option<Box<RawValue>>,
	},
	/// The endpoint answered 304 without a body. The contribution carries the
	/// status.
	NotModified { contribution: Contribution },
	/// The handler redirected.
	Redirect {
		contribution: Contribution,
		redirection: Redirection,
	},
	/// The handler failed, or its result was invalid.
	Failure {
		contribution: Contribution,
		status: StatusCode,
		public_error: Option<PublicError>,
	},
}

impl Outcome {
	/// Whether this outcome ends the response for every later position.
	pub(crate) fn is_terminal(&self) -> bool {
		!matches!(self, Self::Continue { .. } | Self::NotModified { .. })
	}

	pub(crate) fn internal_server_error() -> Self {
		Self::Failure {
			contribution: Contribution::default(),
			status: StatusCode::INTERNAL_SERVER_ERROR,
			public_error: None,
		}
	}
}

/// What reports name: the handler's kind and its pattern, and the path of the
/// request that it handles, without the query, which can contain secrets. Task
/// middleware's pattern is its patterns joined with spaces.
#[derive(Clone, Debug)]
pub(crate) struct HandlerLabel {
	pub(crate) kind: HandlerKind,
	pub(crate) pattern: std::sync::Arc<str>,
	pub(crate) request_path: std::sync::Arc<str>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum HandlerKind {
	TaskMiddleware,
	ViewLoader,
	ApiEndpoint,
}

impl HandlerKind {
	pub(crate) fn span_name(self) -> &'static str {
		match self {
			Self::TaskMiddleware => "vorma.task_middleware",
			Self::ViewLoader => "vorma.view_loader",
			Self::ApiEndpoint => "vorma.api_endpoint",
		}
	}
}

/// Input that fails its decoder answers 400 without calling the handler,
/// without a public payload, and without a report.
pub(crate) fn invalid_input_outcome() -> Outcome {
	Outcome::Failure {
		contribution: Contribution::default(),
		status: StatusCode::BAD_REQUEST,
		public_error: None,
	}
}

/// Reports an invalid server result and answers 500 without a payload.
pub(crate) fn invalid_server_result_outcome(label: &HandlerLabel, problem: &str) -> Outcome {
	tracing::error!(
		handler = label.kind.span_name(),
		pattern = &*label.pattern,
		path = &*label.request_path,
		problem,
		"Vorma received an invalid server result",
	);
	Outcome::internal_server_error()
}

/// Serializes successful output once; `None` when the handler has no output.
fn serialize_output<Output: Serialize + 'static>(
	output: &Output,
) -> Result<Option<Box<RawValue>>, String> {
	if std::any::TypeId::of::<Output>() == std::any::TypeId::of::<()>() {
		return Ok(None);
	}
	serde_json::value::to_raw_value(&FiniteFloats(output))
		.map(Some)
		.map_err(|error| format!("output cannot be serialized: {error}"))
}

fn nav_redirect_outcome(
	label: &HandlerLabel,
	redirect: Redirect,
	resolver: &dyn ResolveTarget,
) -> Outcome {
	let location = match &redirect.target {
		RedirectTarget::Url(url) => validate_redirect_url(url),
		RedirectTarget::Page(target) => resolver.resolve_page_target(target).map_err(|problem| {
			format!(
				"the redirect's page target {:?} is invalid: {problem}",
				target.pattern
			)
		}),
	};
	redirect_outcome(
		label,
		location,
		RedirectParts {
			status: redirect.status,
			meta: redirect.meta,
			kind: RedirectKind::Nav,
		},
	)
}

fn validate_redirect_url(url: &str) -> Result<String, String> {
	if !url.is_ascii() {
		return Err(format!(
			"the redirect URL {url:?} has characters outside ASCII; percent-encode its path and query, and write an internationalized host in its ASCII form"
		));
	}
	if url.bytes().any(|byte| byte <= 0x1f || byte == 0x7f) {
		return Err(format!(
			"the redirect URL {url:?} contains an ASCII control character"
		));
	}
	if let Some(scheme) = url_scheme_prefix(url) {
		if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
			return Err(format!(
				"the redirect URL {url:?} uses the {scheme:?} scheme, and only \"http\" and \"https\" schemes are allowed"
			));
		}
		if !has_non_empty_url_authority(url) {
			return Err(format!(
				"the redirect URL {url:?} uses the {scheme:?} scheme without a non-empty authority after \"://\""
			));
		}
	}
	Ok(url.to_owned())
}

fn has_non_empty_url_authority(url: &str) -> bool {
	let trimmed = url.trim_ascii_start();
	let Some((_, after_colon)) = trimmed.split_once(':') else {
		return false;
	};
	let Some(after_slashes) = after_colon.strip_prefix("//") else {
		return false;
	};
	let authority = after_slashes.split(['/', '?', '#']).next().unwrap_or("");
	!authority.is_empty() && !authority.bytes().any(|byte| byte.is_ascii_whitespace())
}

fn url_scheme_prefix(url: &str) -> Option<&str> {
	let before_delimiter = url
		.trim_ascii_start()
		.split(&['/', '?', '#'][..])
		.next()
		.unwrap_or("");
	let (scheme, _) = before_delimiter.split_once(':')?;
	let mut bytes = scheme.bytes();
	let first = bytes.next()?;
	if !first.is_ascii_alphabetic() {
		return None;
	}
	if bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.')) {
		Some(scheme)
	} else {
		None
	}
}

fn api_endpoint_redirect_outcome(
	label: &HandlerLabel,
	redirecting: &RedirectingApiEndpoint,
	redirect: ApiEndpointRedirect,
	resolver: &dyn ResolveTarget,
) -> Outcome {
	let location = resolver
		.resolve_api_endpoint_target(redirecting, &redirect.target, redirect.status)
		.map_err(|problem| {
			format!(
				"the redirect's endpoint target {:?} is invalid: {problem}",
				redirect.target.pattern
			)
		});
	redirect_outcome(
		label,
		location,
		RedirectParts {
			status: redirect.status,
			meta: redirect.meta,
			kind: RedirectKind::ApiEndpoint,
		},
	)
}

/// What a redirect contributes besides its location.
struct RedirectParts {
	status: RedirectStatus,
	meta: ResponseMeta,
	kind: RedirectKind,
}

fn redirect_outcome(
	label: &HandlerLabel,
	location: Result<String, String>,
	redirect: RedirectParts,
) -> Outcome {
	let location = location.and_then(|location| {
		http::HeaderValue::try_from(location)
			.map_err(|_| "the redirect location is not a valid header value".to_owned())
	});
	let location = match location {
		Ok(location) => location,
		Err(problem) => return invalid_server_result_outcome(label, &problem),
	};
	// The redirect's status replaces earlier statuses, as any later status
	// does.
	let mut meta = redirect.meta;
	meta.status = Some(redirect.status.to_status_code());
	match to_contribution(meta, PreparedDocument::default()) {
		Ok(contribution) => Outcome::Redirect {
			contribution,
			redirection: Redirection {
				location,
				status: redirect.status,
				kind: redirect.kind,
			},
		},
		Err(problem) => invalid_server_result_outcome(label, &problem),
	}
}

/// Prepares a handler's document contributions for a page. An endpoint
/// renders no document, so its handlers' document contributions are neither
/// prepared nor validated, and have no effect.
fn prepare_document(
	document: DocumentParts,
	target: ResponseTarget<'_>,
) -> Result<PreparedDocument, String> {
	match target {
		ResponseTarget::Page { head_rules } => document
			.prepare(head_rules)
			.map_err(|error| error.to_string()),
		ResponseTarget::ApiEndpoint => Ok(PreparedDocument::default()),
	}
}

/// A success value that carries a contribution, and, for a render, output.
fn continue_outcome<Output: Serialize + 'static>(
	label: &HandlerLabel,
	output: Option<&Output>,
	document: DocumentParts,
	meta: ResponseMeta,
	target: ResponseTarget<'_>,
) -> Outcome {
	if let Some(status) = meta.status
		&& let Err(problem) = check_success_status(status, target)
	{
		return invalid_server_result_outcome(label, &problem);
	}
	let output = match output.map(serialize_output).transpose() {
		Ok(output) => output.flatten(),
		Err(problem) => return invalid_server_result_outcome(label, &problem),
	};
	let document = match prepare_document(document, target) {
		Ok(document) => document,
		Err(problem) => return invalid_server_result_outcome(label, &problem),
	};
	match to_contribution(meta, document) {
		Ok(contribution) => Outcome::Continue {
			contribution,
			output,
		},
		Err(problem) => invalid_server_result_outcome(label, &problem),
	}
}

fn terminal_outcome<DeclaredError: Serialize>(
	label: &HandlerLabel,
	declared_error_kind: DeclaredErrorKind,
	terminal: TerminalOutcome<DeclaredError>,
	resolver: &dyn ResolveTarget,
	target: ResponseTarget<'_>,
) -> Outcome {
	match terminal {
		TerminalOutcome::Redirect(redirect) => nav_redirect_outcome(label, *redirect, resolver),
		TerminalOutcome::Failure(failure) => {
			from_failure(label, declared_error_kind, *failure, target)
		}
	}
}

pub(crate) fn from_view_loader_result<Output: Serialize + 'static, DeclaredError: Serialize>(
	label: &HandlerLabel,
	result: Result<Render<Output>, TerminalOutcome<DeclaredError>>,
	resolver: &dyn ResolveTarget,
	target: ResponseTarget<'_>,
) -> Outcome {
	match result {
		Ok(Render {
			output,
			document,
			meta,
		}) => continue_outcome(label, Some(&output), document, meta, target),
		Err(terminal) => terminal_outcome(
			label,
			DeclaredErrorKind::Declared,
			terminal,
			resolver,
			target,
		),
	}
}

pub(crate) fn from_api_endpoint_result<Output: Serialize + 'static, DeclaredError: Serialize>(
	label: &HandlerLabel,
	redirecting: &RedirectingApiEndpoint,
	result: Result<ApiEndpointResult<Output>, TerminalOutcome<DeclaredError>>,
	resolver: &dyn ResolveTarget,
	target: ResponseTarget<'_>,
) -> Outcome {
	match result {
		Ok(ApiEndpointResult::Respond(Respond { output, meta })) => {
			continue_outcome(label, Some(&output), DocumentParts::default(), meta, target)
		}
		Ok(ApiEndpointResult::Redirect(redirect)) => {
			api_endpoint_redirect_outcome(label, redirecting, redirect, resolver)
		}
		Ok(ApiEndpointResult::NotModified(NotModified { mut meta })) => {
			if redirecting.method != http::Method::GET {
				return invalid_server_result_outcome(
					label,
					"NotModified is valid only for a GET endpoint",
				);
			}
			meta.status = Some(StatusCode::NOT_MODIFIED);
			match to_contribution(meta, PreparedDocument::default()) {
				Ok(contribution) => Outcome::NotModified { contribution },
				Err(problem) => invalid_server_result_outcome(label, &problem),
			}
		}
		Err(terminal) => terminal_outcome(
			label,
			DeclaredErrorKind::Declared,
			terminal,
			resolver,
			target,
		),
	}
}

pub(crate) fn from_task_middleware_result<DeclaredError: Serialize>(
	label: &HandlerLabel,
	result: Result<Continue, TerminalOutcome<DeclaredError>>,
	resolver: &dyn ResolveTarget,
	target: ResponseTarget<'_>,
) -> Outcome {
	match result {
		Ok(Continue { document, meta }) => {
			continue_outcome::<()>(label, None, document, meta, target)
		}
		Err(terminal) => terminal_outcome(
			label,
			DeclaredErrorKind::Rejection,
			terminal,
			resolver,
			target,
		),
	}
}

/// A failure's status is its own. Its headers, cookies, and document
/// contributions apply. Invalid failure metadata gives 500 without a payload
/// or headers.
fn from_failure<DeclaredError: Serialize>(
	label: &HandlerLabel,
	declared_error_kind: DeclaredErrorKind,
	failure: Failure<DeclaredError>,
	target: ResponseTarget<'_>,
) -> Outcome {
	let Failure {
		kind,
		status,
		document,
		meta,
	} = failure;
	if let FailureKind::Invalid(problem) = &kind {
		return invalid_server_result_outcome(label, problem);
	}
	if let Err(problem) = check_failure_status(status) {
		return invalid_server_result_outcome(
			label,
			&format!("failure metadata is invalid: {problem}"),
		);
	}
	let document = match prepare_document(document, target) {
		Ok(document) => document,
		Err(problem) => {
			return invalid_server_result_outcome(
				label,
				&format!("failure metadata is invalid: {problem}"),
			);
		}
	};
	let contribution = match to_contribution(meta, document) {
		Ok(contribution) => contribution,
		Err(problem) => {
			return invalid_server_result_outcome(
				label,
				&format!("failure metadata is invalid: {problem}"),
			);
		}
	};
	let public_error = match kind {
		FailureKind::Declared(error) => {
			match serde_json::value::to_raw_value(&FiniteFloats(&error))
				.map_err(|error| error.to_string())
			{
				Ok(public_error) => {
					tracing::debug!(
						handler = label.kind.span_name(),
						pattern = &*label.pattern,
						path = &*label.request_path,
						status = status.as_u16(),
						error = public_error.get(),
						"Vorma handler failed with a declared error",
					);
					Some(match declared_error_kind {
						DeclaredErrorKind::Declared => PublicError::Declared(public_error),
						DeclaredErrorKind::Rejection => PublicError::Rejection(public_error),
					})
				}
				Err(problem) => {
					return invalid_server_result_outcome(
						label,
						&format!("the declared error is invalid: {problem}"),
					);
				}
			}
		}
		FailureKind::Invalid(_) => unreachable!("an invalid failure returned earlier"),
		FailureKind::Undeclared(error) => {
			tracing::error!(
				handler = label.kind.span_name(),
				pattern = &*label.pattern,
				path = &*label.request_path,
				status = status.as_u16(),
				error = &*error as &(dyn std::error::Error + 'static),
				"Vorma handler failed",
			);
			None
		}
	};
	Outcome::Failure {
		contribution,
		status,
		public_error,
	}
}
