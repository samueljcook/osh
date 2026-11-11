//! What handlers return: a render, a response, a continuation, a redirect, or
//! a failure, each built with its HTTP metadata. Every handler returns
//! `Result<Success, TerminalOutcome<DeclaredError>>`.
//!
//! Each value is a builder. Methods that start with `set_` replace an earlier
//! value of the same kind, such as a status, a header's values, or a cookie
//! with the same name, path, and domain. Methods that start with `add_` add to
//! what came before, such as another value of a header or another head element.
//! Header changes apply in call order.
//!
//! Each value also has methods that read what its builder methods collected,
//! such as `status`, `headers`, `cookies`, and `output` so that a test can
//! call a handler and check the value that it returned. A reading method
//! reports this one value's contributions. The response of a request merges
//! the contributions of every handler that ran for it. A header or cookie
//! that a builder method cannot accept makes the value invalid, and the
//! header and cookie readers of an invalid value return an
//! [`InvalidResultError`].

use std::any::{TypeId, type_name};
use std::error::Error as StdError;
use std::fmt;

use cookie::Cookie;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use primitives_rs::serialization::json::FiniteFloats;
use primitives_rs::tsgen::{FieldDef, FieldPresence, TsGen, TypeDef, TypeRef, TypeRegistry};
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};

use crate::document::{AttrValue, DocumentParts, HeadEl, HeadItem};
use crate::pattern::{ApiPatternParams, ViewPatternParams};

/// One header change. Changes apply in call order, after the changes of
/// earlier results.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HeaderChange {
	/// Replaces every value that the header has so far with this value.
	Set {
		name: HeaderName,
		value: HeaderValue,
	},
	/// Adds another field line with this value and keeps the header's existing
	/// values.
	Add {
		name: HeaderName,
		value: HeaderValue,
	},
}

impl HeaderChange {
	/// Applies the change to the headers that earlier changes produced. An
	/// added value is its own field line, so values that cannot share one
	/// line stay intact.
	pub(crate) fn apply_to(&self, headers: &mut HeaderMap) {
		match self {
			Self::Set { name, value } => {
				headers.insert(name.clone(), value.clone());
			}
			Self::Add { name, value } => {
				headers.append(name.clone(), value.clone());
			}
		}
	}
}

/// The HTTP metadata that one result's builder methods collect.
#[derive(Clone, Debug, Default)]
pub(crate) struct ResponseMeta {
	pub(crate) status: Option<StatusCode>,
	pub(crate) headers_and_cookies: ResponseMetaState,
}

/// The headers and cookies that one result's builder methods collect.
#[derive(Clone, Debug)]
pub(crate) enum ResponseMetaState {
	/// Every header and cookie that the builder methods received is valid.
	Valid {
		header_changes: Vec<HeaderChange>,
		/// The cookies in call order, without a cookie that a later call
		/// replaced.
		cookies: Vec<Cookie<'static>>,
	},
	/// A builder method received an invalid header or cookie, which this
	/// problem describes. Vorma reports the problem when it reads the result,
	/// and later header and cookie calls have no effect.
	Invalid { problem: String },
}

impl Default for ResponseMetaState {
	fn default() -> Self {
		Self::Valid {
			header_changes: Vec::new(),
			cookies: Vec::new(),
		}
	}
}

impl ResponseMeta {
	fn change_header<Name, Value>(
		&mut self,
		name: Name,
		value: Value,
		create_header_change_fn: fn(HeaderName, HeaderValue) -> HeaderChange,
	) where
		Name: TryInto<HeaderName>,
		<Name as TryInto<HeaderName>>::Error: Into<http::Error>,
		Value: TryInto<HeaderValue>,
		<Value as TryInto<HeaderValue>>::Error: Into<http::Error>,
	{
		let ResponseMetaState::Valid { header_changes, .. } = &mut self.headers_and_cookies else {
			return;
		};
		let name = match name.try_into() {
			Ok(name) => name,
			Err(error) => {
				return self.make_invalid(format!("a header is invalid: {}", error.into()));
			}
		};
		match value.try_into() {
			Ok(value) => header_changes.push(create_header_change_fn(name, value)),
			Err(error) => {
				self.make_invalid(format!(
					"a header is invalid: header {name}: {}",
					error.into()
				));
			}
		}
	}

	fn make_invalid(&mut self, problem: String) {
		self.headers_and_cookies = ResponseMetaState::Invalid { problem };
	}

	fn headers(&self) -> Result<HeaderMap, InvalidResultError> {
		match &self.headers_and_cookies {
			ResponseMetaState::Valid { header_changes, .. } => {
				let mut headers = HeaderMap::new();
				for change in header_changes {
					change.apply_to(&mut headers);
				}
				Ok(headers)
			}
			ResponseMetaState::Invalid { problem } => Err(InvalidResultError {
				problem: problem.clone(),
			}),
		}
	}

	fn cookies(&self) -> Result<&[Cookie<'static>], InvalidResultError> {
		match &self.headers_and_cookies {
			ResponseMetaState::Valid { cookies, .. } => Ok(cookies),
			ResponseMetaState::Invalid { problem } => Err(InvalidResultError {
				problem: problem.clone(),
			}),
		}
	}

	fn set_cookie(&mut self, cookie: Cookie<'static>) {
		let ResponseMetaState::Valid { cookies, .. } = &mut self.headers_and_cookies else {
			return;
		};
		match primitives_rs::http::cookies::headers::validate_set_cookie(&cookie) {
			Ok(()) => {
				cookies.retain(|existing| {
					!primitives_rs::http::cookies::headers::same_cookie_identity(existing, &cookie)
				});
				cookies.push(cookie);
			}
			Err(error) => self.make_invalid(format!("a cookie is invalid: {error}")),
		}
	}
}

/// A result that a builder method made invalid with a header or a cookie
/// that it cannot accept. Vorma reports such a result and answers 500. The
/// header and cookie readers of an invalid result return this error with the
/// first problem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidResultError {
	problem: String,
}

impl fmt::Display for InvalidResultError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(formatter, "the result is invalid: {}", self.problem)
	}
}

impl StdError for InvalidResultError {}

/// Defines the header and cookie methods that every result value has.
macro_rules! response_meta_methods {
	($type:ty $(, $generic:ident)?) => {
		impl$(<$generic>)? $type {
			/// Replaces the header's values with this value. The name and value
			/// accept anything that converts to a header name or value, as
			/// `http`'s response builder does. An invalid name or value makes the
			/// result invalid: Vorma reports it and answers 500.
			pub fn set_header<Name, Value>(mut self, name: Name, value: Value) -> Self
			where
				Name: TryInto<HeaderName>,
				<Name as TryInto<HeaderName>>::Error: Into<http::Error>,
				Value: TryInto<HeaderValue>,
				<Value as TryInto<HeaderValue>>::Error: Into<http::Error>,
			{
				self.meta
					.change_header(name, value, |name, value| HeaderChange::Set { name, value });
				self
			}

			/// Adds a value to the header and keeps its existing values. The name
			/// and value convert as they do for `set_header`.
			pub fn add_header<Name, Value>(mut self, name: Name, value: Value) -> Self
			where
				Name: TryInto<HeaderName>,
				<Name as TryInto<HeaderName>>::Error: Into<http::Error>,
				Value: TryInto<HeaderValue>,
				<Value as TryInto<HeaderValue>>::Error: Into<http::Error>,
			{
				self.meta
					.change_header(name, value, |name, value| HeaderChange::Add { name, value });
				self
			}

			/// Sets a cookie, which replaces an earlier cookie with the same name,
			/// path, domain, and partitioned storage mode. A cookie with
			/// another scope does not replace it, and the response
			/// carries both. A `cookie::CookieBuilder` converts into a cookie.
			///
			/// Vorma writes the cookie's name and value exactly as given, without
			/// percent-encoding. The name must be a valid HTTP token, and the
			/// value must contain only RFC 6265's cookie characters: printable
			/// ASCII except space, `"`, `,`, `;`, and `\`. The `Path` and
			/// `Domain` must not contain `;` or a control character. Otherwise the
			/// result is invalid: Vorma reports it and answers 500. An application that
			/// needs other characters in a value, such as binary data, encodes the
			/// value itself, for example as base64url, and decodes it when it reads
			/// the cookie.
			pub fn set_cookie(mut self, cookie: impl Into<Cookie<'static>>) -> Self {
				self.meta.set_cookie(cookie.into());
				self
			}

			/// The headers that this value's `set_header` and `add_header` calls
			/// produce, applied in call order to no headers. Cookies are not
			/// among them: [`cookies`](Self::cookies) returns those. A header
			/// with an invalid name or value, or an invalid cookie, makes the
			/// value invalid, and this returns an [`InvalidResultError`] then.
			pub fn headers(&self) -> Result<HeaderMap, InvalidResultError> {
				self.meta.headers()
			}

			/// The cookies that this value sets, in the order of the `set_cookie`
			/// calls, without a cookie that a later call replaced. An invalid
			/// cookie, or a header with an invalid name or value, makes the value
			/// invalid, and this returns an [`InvalidResultError`] then.
			pub fn cookies(&self) -> Result<&[Cookie<'static>], InvalidResultError> {
				self.meta.cookies()
			}
		}
	};
}

/// Defines the status method of every result value except redirects, which use
/// their redirect status.
macro_rules! status_method {
	($type:ty $(, $generic:ident)?) => {
		impl$(<$generic>)? $type {
			/// Sets the response status, which replaces an earlier status. A
			/// success must have a 2xx status; any other status makes the result
			/// invalid: Vorma reports it and answers 500. A page response always
			/// has a body, so 204 and 205 are also invalid for a result that
			/// contributes to a page. A redirect has its redirect status, and
			/// [`NotModified`] answers 304.
			pub fn set_status(mut self, status: StatusCode) -> Self {
				self.meta.status = Some(status);
				self
			}

			/// The status that `set_status` set, or `None` when this value sets
			/// no status.
			pub fn status(&self) -> Option<StatusCode> {
				self.meta.status
			}
		}
	};
}

/// Defines the document methods of the results that render a page.
macro_rules! document_methods {
	($type:ty $(, $generic:ident)?) => {
		impl$(<$generic>)? $type {
			/// Sets an attribute of `html`, which replaces an earlier value of
			/// that attribute.
			pub fn set_html_attr(mut self, name: impl Into<String>, value: impl Into<AttrValue>) -> Self {
				self.document.html_attrs.push((name.into(), value.into()));
				self
			}

			/// Sets an attribute of `head`, which replaces an earlier value of
			/// that attribute.
			pub fn set_head_attr(mut self, name: impl Into<String>, value: impl Into<AttrValue>) -> Self {
				self.document.head_attrs.push((name.into(), value.into()));
				self
			}

			/// Sets an attribute of `body`, which replaces an earlier value of
			/// that attribute, such as an ancestor loader's `class`.
			pub fn set_body_attr(mut self, name: impl Into<String>, value: impl Into<AttrValue>) -> Self {
				self.document.body_attrs.push((name.into(), value.into()));
				self
			}

			/// Adds an element to the head. Vorma places and deduplicates head
			/// elements: a later element with the same identity, such as a later
			/// `<title>`, replaces an earlier one, as the identity's
			/// [`crate::HeadReplacement`] says. A later `remove_head_el` call
			/// removes the element.
			pub fn add_head_el(mut self, el: HeadEl) -> Self {
				self.document.head_items.push(HeadItem::El(el));
				self
			}

			/// Removes the head elements with the identity that `el` names that
			/// were placed before this call, by earlier contributions or by
			/// earlier calls of this handler: the one element of a per-element
			/// identity, or the whole set of a per-contribution identity. A
			/// later element with the identity, including one that this handler
			/// adds after the call, takes the same position. A removal of an
			/// identity that nothing placed does nothing.
			///
			/// `el` carries its tag and the attributes that its identity
			/// names, without children, and matches a built-in identity or a
			/// [`crate::HeadDedupeRule`] by the rules of `add_head_el`. Any
			/// other `el` is an invalid server result. Raw HTML and elements
			/// that match no identity cannot be removed; an application
			/// declares a rule to make an element removable.
			pub fn remove_head_el(mut self, el: HeadEl) -> Self {
				self.document.head_items.push(HeadItem::Remove(el));
				self
			}

			/// Adds markup to the head unchanged, without escaping or
			/// deduplication. A document writes it between the comments
			/// `<!--vorma-raw:KEY-->` and `<!--/vorma-raw:KEY-->`, and a
			/// navigation payload carries it with the key, where `KEY` is the
			/// lowercase hexadecimal text of the first 16 bytes of the BLAKE3
			/// hash of the markup, as [`HeadItem::DangerousRawHtml`] describes.
			pub fn add_head_dangerous_raw_html(mut self, html: impl Into<String>) -> Self {
				self.document.head_items.push(HeadItem::DangerousRawHtml(html.into()));
				self
			}

			/// The settings of `html`'s attributes, in the order of the
			/// `set_html_attr` calls. A later setting of an attribute replaces an
			/// earlier one when Vorma renders the document.
			pub fn html_attrs(&self) -> &[(String, AttrValue)] {
				&self.document.html_attrs
			}

			/// The settings of `head`'s attributes, in the order of the
			/// `set_head_attr` calls.
			pub fn head_attrs(&self) -> &[(String, AttrValue)] {
				&self.document.head_attrs
			}

			/// The settings of `body`'s attributes, in the order of the
			/// `set_body_attr` calls.
			pub fn body_attrs(&self) -> &[(String, AttrValue)] {
				&self.document.body_attrs
			}

			/// What this value contributes to the head, in call order: each
			/// added element, each removal, and each piece of raw markup.
			pub fn head_items(&self) -> &[HeadItem] {
				&self.document.head_items
			}
		}
	};
}

/// A view loader's successful result: the view's output with its document
/// contributions and HTTP metadata.
#[derive(Debug)]
#[must_use]
pub struct Render<Output> {
	pub(crate) output: Output,
	pub(crate) document: DocumentParts,
	pub(crate) meta: ResponseMeta,
}

impl<Output> Render<Output> {
	/// Renders the view with this output. `()` means that the loader has no
	/// output.
	pub fn new(output: Output) -> Self {
		Self {
			output,
			document: DocumentParts::default(),
			meta: ResponseMeta::default(),
		}
	}
}

impl<Output> Render<Output> {
	/// The view's output.
	pub fn output(&self) -> &Output {
		&self.output
	}
}

impl<Output> From<Output> for Render<Output> {
	/// Renders the view with the output and no other metadata.
	fn from(output: Output) -> Self {
		Self::new(output)
	}
}

status_method!(Render<Output>, Output);
response_meta_methods!(Render<Output>, Output);
document_methods!(Render<Output>, Output);

/// An API endpoint's successful result: its output with HTTP metadata.
#[derive(Debug)]
#[must_use]
pub struct Respond<Output> {
	pub(crate) output: Output,
	pub(crate) meta: ResponseMeta,
}

impl<Output> Respond<Output> {
	/// Responds with this output. `()` means that the endpoint has no output,
	/// and the response has no body.
	pub fn new(output: Output) -> Self {
		Self {
			output,
			meta: ResponseMeta::default(),
		}
	}
}

impl<Output> Respond<Output> {
	/// The endpoint's output.
	pub fn output(&self) -> &Output {
		&self.output
	}
}

status_method!(Respond<Output>, Output);
response_meta_methods!(Respond<Output>, Output);

/// Task middleware's successful result: the request continues with these
/// contributions. Document contributions apply to page responses.
#[derive(Debug, Default)]
#[must_use]
pub struct Continue {
	pub(crate) document: DocumentParts,
	pub(crate) meta: ResponseMeta,
}

impl Continue {
	/// Lets the request continue without contributions.
	pub fn new() -> Self {
		Self::default()
	}
}

impl From<()> for Continue {
	/// Lets the request continue without contributions.
	fn from((): ()) -> Self {
		Self::new()
	}
}

status_method!(Continue);
response_meta_methods!(Continue);
document_methods!(Continue);

/// A redirect status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedirectStatus {
	/// 302 Found.
	Found,
	/// 303 See Other.
	SeeOther,
	/// 307 Temporary Redirect.
	TemporaryRedirect,
	/// 308 Permanent Redirect.
	PermanentRedirect,
}

impl RedirectStatus {
	pub(crate) fn to_status_code(self) -> StatusCode {
		match self {
			Self::Found => StatusCode::FOUND,
			Self::SeeOther => StatusCode::SEE_OTHER,
			Self::TemporaryRedirect => StatusCode::TEMPORARY_REDIRECT,
			Self::PermanentRedirect => StatusCode::PERMANENT_REDIRECT,
		}
	}
}

/// A page given by one of the application's view patterns, with its params, its
/// search input, and a hash. Two targets are equal when they have the same
/// pattern, params, search input, and hash.
#[derive(Clone, Debug, PartialEq)]
#[must_use]
pub struct PageTarget {
	pub(crate) pattern: &'static str,
	pub(crate) params: Vec<(String, String)>,
	pub(crate) splat_values: Vec<String>,
	pub(crate) input: Option<Result<serde_json::Value, String>>,
	pub(crate) hash: Option<String>,
}

impl PageTarget {
	/// Targets the page of the view pattern that these params belong to.
	pub fn new<Params: ViewPatternParams>(params: Params) -> Self {
		let decoded_match = params.into_decoded_match();
		Self {
			pattern: Params::PATTERN,
			params: decoded_match.params,
			splat_values: decoded_match.splat_values,
			input: None,
			hash: None,
		}
	}

	/// Sets the search input. Each loader at or above the pattern that
	/// declares search input reads its own part from it: the fields of its
	/// input struct, or every key when its input is a map. An array in the
	/// input can contain only scalars and null because the href repeats the
	/// array's key for each item. Input that cannot be serialized, such as
	/// input with a float that is NaN or an infinity, or with an array or an
	/// object inside an array, makes the result invalid: Vorma reports it and
	/// answers 500.
	pub fn set_input<Input: Serialize>(mut self, input: &Input) -> Self {
		self.input =
			Some(serde_json::to_value(FiniteFloats(input)).map_err(|error| error.to_string()));
		self
	}

	/// Sets the URL fragment, written without its leading `#`.
	pub fn set_hash(mut self, hash: impl Into<String>) -> Self {
		self.hash = Some(hash.into());
		self
	}
}

/// An endpoint given by one of the application's API patterns, with its
/// params and, for a follow-up `GET` request, its search input. Two targets
/// are equal when they have the same pattern, params, and search input.
#[derive(Clone, Debug, PartialEq)]
#[must_use]
pub struct ApiEndpointTarget {
	pub(crate) pattern: &'static str,
	pub(crate) params: Vec<(String, String)>,
	pub(crate) splat_values: Vec<String>,
	pub(crate) input: Option<Result<serde_json::Value, String>>,
}

impl ApiEndpointTarget {
	/// Targets the endpoints of the pattern that these params belong to. The
	/// redirect's status and the request's method decide which method's
	/// endpoint receives the follow-up request.
	pub fn new<Params: ApiPatternParams>(params: Params) -> Self {
		let decoded_match = params.into_decoded_match();
		Self {
			pattern: Params::PATTERN,
			params: decoded_match.params,
			splat_values: decoded_match.splat_values,
			input: None,
		}
	}

	/// Sets search input for a follow-up `GET` request. The input must fit the
	/// endpoint's input type, and the endpoint must receive every entry of the
	/// href's query in its input. The input cannot have the key `vorma-json`,
	/// which Vorma's wire protocol reserves, and, while Vercel skew protection
	/// is on, the key `dpl`, which Vercel reads to choose the deployment that
	/// serves the request. An array in the input can contain only scalars and
	/// null because the href repeats the array's key for each item. Input
	/// that breaks these rules or cannot be serialized, such as input with a
	/// float that is NaN or an infinity, makes the result invalid: Vorma
	/// reports it and answers 500.
	pub fn set_input<Input: Serialize>(mut self, input: &Input) -> Self {
		self.input =
			Some(serde_json::to_value(FiniteFloats(input)).map_err(|error| error.to_string()));
		self
	}
}

/// Where a navigation redirect sends the browser.
#[derive(Clone, Debug, PartialEq)]
pub enum RedirectTarget {
	/// A URL that Vorma does not declare, used as the `location` unchanged.
	/// It must consist of ASCII characters without control characters because
	/// an HTTP header carries bytes that a browser reads one character each:
	/// percent-encode its path and query, and write an internationalized host
	/// in its ASCII form. When the URL starts with a scheme, the scheme must be
	/// `http` or `https` (in any ASCII letter case) followed by `//` and a
	/// non-empty authority; relative URLs without a scheme are also valid. A
	/// URL that violates these requirements makes the result invalid, so Vorma
	/// reports it and answers 500.
	Url(String),
	/// A page given by one of the application's view patterns.
	Page(PageTarget),
}

impl From<PageTarget> for RedirectTarget {
	fn from(target: PageTarget) -> Self {
		Self::Page(target)
	}
}

/// A navigation redirect: it sends the user to a page or a URL. The Vorma
/// client receives it as a client redirect when it needs one; any other
/// caller receives a 3xx response. View loaders, API endpoints, and task
/// middleware can return it.
#[derive(Debug)]
#[must_use]
pub struct Redirect {
	pub(crate) target: RedirectTarget,
	pub(crate) status: RedirectStatus,
	pub(crate) meta: ResponseMeta,
}

impl Redirect {
	/// Redirects to a page or a URL with this redirect status.
	pub fn new(target: impl Into<RedirectTarget>, status: RedirectStatus) -> Self {
		Self {
			target: target.into(),
			status,
			meta: ResponseMeta::default(),
		}
	}
}

impl Redirect {
	/// Where the redirect sends the user.
	pub fn target(&self) -> &RedirectTarget {
		&self.target
	}

	/// The redirect status.
	pub fn status(&self) -> RedirectStatus {
		self.status
	}
}

response_meta_methods!(Redirect);

/// An endpoint redirect: an endpoint call receives another of the
/// application's endpoints' responses. It is a 3xx response that the caller's
/// HTTP client follows. A 303 response turns any method except `HEAD` into
/// `GET`, a 302 response turns `POST` into `GET`, and 307 and 308 responses
/// keep the method and the body. The target endpoint must have the redirecting
/// endpoint's output and declared-error types, and its input type when the
/// body is sent again; otherwise the result is invalid.
#[derive(Debug)]
#[must_use]
pub struct ApiEndpointRedirect {
	pub(crate) target: ApiEndpointTarget,
	pub(crate) status: RedirectStatus,
	pub(crate) meta: ResponseMeta,
}

impl ApiEndpointRedirect {
	/// Redirects the call to an endpoint with this redirect status.
	pub fn new(target: ApiEndpointTarget, status: RedirectStatus) -> Self {
		Self {
			target,
			status,
			meta: ResponseMeta::default(),
		}
	}
}

impl ApiEndpointRedirect {
	/// The endpoint that answers the call.
	pub fn target(&self) -> &ApiEndpointTarget {
		&self.target
	}

	/// The redirect status.
	pub fn status(&self) -> RedirectStatus {
		self.status
	}
}

response_meta_methods!(ApiEndpointRedirect);

/// A `304 Not Modified` answer from a `GET` API endpoint. Vorma sends it
/// without a body, with its protocol headers and the headers and cookies set
/// on it. Any other endpoint method makes the result invalid: Vorma reports it
/// and answers 500.
///
/// Its purpose is a handler that checks a cheap validator, such as a version
/// number, before it does its work. The handler compares the request's
/// `If-None-Match` header with the validator, for example with
/// `primitives_rs::http::conditional_responses::if_none_match_matches`, and
/// returns `NotModified::new()` with the validator's
/// `ETag` header when they match. The browser's HTTP cache then answers the
/// call with its stored body.
///
/// Vorma's browser client can read this answer only when the browser's HTTP
/// cache revalidated the call: the browser sent the `If-None-Match` header
/// from its stored response, and it answers the call with the stored body.
/// A conditional header that application code sets on a call, for example
/// in the client's `prepareRequestFn`, is not supported: the browser then
/// gives the client the 304 itself, which has no body that the client can
/// read.
///
/// An `ETag` computed from a response body belongs in the host server's
/// middleware, such as Primitives' `EtagLayer`, which wraps Vorma's handler.
/// Include `x-vorma-client-build-id` in its additional representation headers
/// so that a changed browser build invalidates otherwise identical bodies.
/// Such middleware saves only transfer because the handler has already done its work.
#[derive(Debug, Default)]
#[must_use]
pub struct NotModified {
	pub(crate) meta: ResponseMeta,
}

impl NotModified {
	/// Answers 304 without a body.
	pub fn new() -> Self {
		Self::default()
	}
}

response_meta_methods!(NotModified);

/// An API endpoint's successful result. A handler returns
/// `Ok(value.into())`, where the value is a [`Respond`], an
/// [`ApiEndpointRedirect`], a [`NotModified`], or the endpoint's output. A
/// navigation redirect is a [`TerminalOutcome`].
#[derive(Debug)]
pub enum ApiEndpointResult<Output> {
	/// Responds with output.
	Respond(Respond<Output>),
	/// Answers with another of the application's endpoints instead.
	Redirect(ApiEndpointRedirect),
	/// Answers 304 without a body.
	NotModified(NotModified),
}

impl<Output: Serialize> From<Output> for ApiEndpointResult<Output> {
	/// Responds with the output and no other metadata.
	fn from(output: Output) -> Self {
		Self::Respond(Respond::new(output))
	}
}

impl<Output> From<Respond<Output>> for ApiEndpointResult<Output> {
	fn from(respond: Respond<Output>) -> Self {
		Self::Respond(respond)
	}
}

impl<Output> From<ApiEndpointRedirect> for ApiEndpointResult<Output> {
	fn from(redirect: ApiEndpointRedirect) -> Self {
		Self::Redirect(redirect)
	}
}

impl<Output> From<NotModified> for ApiEndpointResult<Output> {
	fn from(not_modified: NotModified) -> Self {
		Self::NotModified(not_modified)
	}
}

/// The declared public error type of a handler that declares none.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum NoDeclaredError {}

impl TsGen for NoDeclaredError {
	fn type_ref() -> TypeRef {
		TypeRef::Union(Vec::new())
	}
}

/// The fixed public failure of task middleware. The browser receives its
/// `client_msg`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestRejected {
	/// Text that the browser receives.
	pub client_msg: String,
}

impl Serialize for RequestRejected {
	fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		let mut state = serializer.serialize_struct("RequestRejected", 1)?;
		state.serialize_field("client_msg", &self.client_msg)?;
		state.end()
	}
}

const REQUEST_REJECTED_TYPE_KEY: &str = "vorma::RequestRejected";

impl TsGen for RequestRejected {
	fn type_ref() -> TypeRef {
		TypeRef::named_with_key(REQUEST_REJECTED_TYPE_KEY, "RequestRejected")
	}

	fn collect_type_defs(registry: &mut TypeRegistry) -> primitives_rs::tsgen::Result<()> {
		registry.try_define(TypeDef::new(
			REQUEST_REJECTED_TYPE_KEY,
			"RequestRejected",
			TypeRef::Object(vec![FieldDef::new(
				"client_msg",
				TypeRef::String,
				FieldPresence::Required,
			)]),
		))?;
		Ok(())
	}
}

/// A handler failure: an expected outcome such as "not found", or an error
/// that the application does not declare. It states its HTTP status, and its
/// headers and cookies apply.
///
/// [`Failure::from_declared`] sends the declared error's serialized form to
/// the browser; it is an outcome that the application chose, so Vorma logs it
/// at the debug level. [`Failure::from_undeclared`] has no public payload;
/// Vorma logs it at the error level with its cause chain.
///
/// `?` converts a failure into a [`TerminalOutcome`], and it converts any error
/// that implements [`std::error::Error`], `Send`, and `Sync` into an undeclared
/// failure with status 500. A boxed error or an `anyhow::Error` does not
/// implement [`std::error::Error`], so it needs [`Failure::from_undeclared`].
/// `?` on a value of the handler's declared error type is an invalid server
/// result because it would hide the error from the browser: Vorma reports it
/// and answers 500. Use [`Failure::from_declared`] for a declared error.
///
/// # Document contributions
///
/// A failure has the document methods of [`Render`] and [`Continue`]. They
/// apply when the failure ends a page request, from a view loader or from task
/// middleware, and they have no effect when it ends an API endpoint's request.
///
/// The page then renders the application's error boundary with the document
/// that the positions contributed in order: the contributions of the positions
/// above the failing one remain, then the failure's contributions merge after
/// them, and the positions after the failing one contribute nothing. A failing
/// handler overrides or unsets what it does not want from the positions above
/// it, with [`AttrValue::Unset`] for an attribute or with a head element that
/// replaces an earlier one, such as another `<title>`.
#[must_use]
pub struct Failure<DeclaredError> {
	pub(crate) kind: FailureKind<DeclaredError>,
	pub(crate) status: StatusCode,
	pub(crate) document: DocumentParts,
	pub(crate) meta: ResponseMeta,
}

pub(crate) enum FailureKind<DeclaredError> {
	Declared(DeclaredError),
	Undeclared(Box<dyn StdError + Send + Sync>),
	/// The failure stands for an invalid server result, with the problem.
	Invalid(String),
}

impl<DeclaredError> Failure<DeclaredError> {
	fn new(kind: FailureKind<DeclaredError>, status: StatusCode) -> Self {
		Self {
			kind,
			status,
			document: DocumentParts::default(),
			meta: ResponseMeta::default(),
		}
	}

	/// A failure with a declared error and its status, which must be a 4xx or
	/// 5xx status; any other status makes the result invalid: Vorma reports it
	/// and answers 500. The browser receives the error's serialized form.
	pub fn from_declared(error: DeclaredError, status: StatusCode) -> Self {
		Self::new(FailureKind::Declared(error), status)
	}

	/// A failure without a public payload, with its status, which must be a
	/// 4xx or 5xx status; any other status makes the result invalid: Vorma
	/// reports it and answers 500.
	pub fn from_undeclared(
		error: impl Into<Box<dyn StdError + Send + Sync>>,
		status: StatusCode,
	) -> Self {
		Self::new(FailureKind::Undeclared(error.into()), status)
	}
}

impl<DeclaredError> Failure<DeclaredError> {
	/// The failure's status.
	pub fn status(&self) -> StatusCode {
		self.status
	}

	/// The declared error of a failure that [`Failure::from_declared`] made,
	/// or `None` for any other failure.
	pub fn declared_error(&self) -> Option<&DeclaredError> {
		match &self.kind {
			FailureKind::Declared(error) => Some(error),
			FailureKind::Undeclared(_) | FailureKind::Invalid(_) => None,
		}
	}

	/// The error of a failure that [`Failure::from_undeclared`] or `?` made,
	/// or `None` for any other failure.
	pub fn undeclared_error(&self) -> Option<&(dyn StdError + Send + Sync + 'static)> {
		match &self.kind {
			FailureKind::Undeclared(error) => Some(error.as_ref()),
			FailureKind::Declared(_) | FailureKind::Invalid(_) => None,
		}
	}
}

response_meta_methods!(Failure<DeclaredError>, DeclaredError);
document_methods!(Failure<DeclaredError>, DeclaredError);

impl<DeclaredError> fmt::Debug for Failure<DeclaredError> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		let kind = match &self.kind {
			FailureKind::Declared(_) => "declared",
			FailureKind::Undeclared(_) => "undeclared",
			FailureKind::Invalid(_) => "invalid",
		};
		formatter
			.debug_struct("Failure")
			.field("kind", &kind)
			.field("status", &self.status)
			.field("meta", &self.meta)
			.finish()
	}
}

/// What ends a handler without its success value: a navigation redirect or a
/// failure. Every handler kind returns `Result<Success, TerminalOutcome<DeclaredError>>`,
/// so a helper that is generic over `DeclaredError` can redirect or fail in any
/// handler.
///
/// `?` converts a [`Redirect`], a [`Failure`], or any error that implements
/// [`std::error::Error`], `Send`, and `Sync`; the error becomes an undeclared
/// failure with status 500. `?` on a value of the declared error type is an
/// invalid server result; see [`Failure`]. A helper that uses `?` on standard
/// errors names the bound `DeclaredError: 'static`.
///
/// A redirect or a failure that a handler returns directly is written
/// `Err(value.into())`. The variants contain boxed values so that a handler's
/// `Result` stays small.
#[derive(Debug)]
#[must_use]
pub enum TerminalOutcome<DeclaredError> {
	/// Sends the user to a page or a URL instead of finishing the handler.
	Redirect(Box<Redirect>),
	/// Ends the handler with a failure.
	Failure(Box<Failure<DeclaredError>>),
}

impl<DeclaredError> From<Redirect> for TerminalOutcome<DeclaredError> {
	fn from(redirect: Redirect) -> Self {
		Self::Redirect(Box::new(redirect))
	}
}

impl<DeclaredError> From<Failure<DeclaredError>> for TerminalOutcome<DeclaredError> {
	fn from(failure: Failure<DeclaredError>) -> Self {
		Self::Failure(Box::new(failure))
	}
}

impl<DeclaredError: 'static, Error> From<Error> for TerminalOutcome<DeclaredError>
where
	Error: StdError + Send + Sync + 'static,
{
	fn from(error: Error) -> Self {
		if TypeId::of::<Error>() == TypeId::of::<DeclaredError>() {
			let name = type_name::<Error>();
			return Self::from(Failure::new(
				FailureKind::Invalid(format!(
					"`?` converted a value of the declared error type {name}; return \
					 `Failure::from_declared(error, status)` instead"
				)),
				StatusCode::INTERNAL_SERVER_ERROR,
			));
		}
		Self::from(Failure::from_undeclared(
			error,
			StatusCode::INTERNAL_SERVER_ERROR,
		))
	}
}
