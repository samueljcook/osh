//! What task middleware, view loaders, and API endpoints receive. A context
//! holds request facts only; a handler contributes to the response through
//! its returned result. Every value that depends on the build comes from one
//! build snapshot, which a request keeps from start to finish, so its asset
//! URLs, build ID, critical CSS, and CSP sources always agree.

use std::sync::Arc;

use http::Method;
use http::request::Parts;
use primitives_rs::async_work::memoized_tasks::TaskCtx;

use crate::build_output::{CspSources, StaticPublicAssets};
use crate::document::HeadRules;
use crate::outcome::{ResolveTarget, ResponseTarget};
use crate::pattern::{ApiPatternParams, DecodedMatch, MissingParamError, ViewPatternParams};
use crate::{declarations::ApiEndpointKind, href::PageHrefs};

/// The request facts that every handler of one request shares.
pub(crate) struct RequestFacts {
	pub(crate) target_resolver: Arc<dyn ResolveTarget>,
	pub(crate) request_parts: Arc<Parts>,
	pub(crate) latest_known_client_build_id: String,
	pub(crate) static_public_assets: Arc<StaticPublicAssets>,
	pub(crate) kind: RequestKind,
}

/// What a request asks Vorma for.
pub(crate) enum RequestKind {
	/// A page: its document or its navigation payload.
	Page {
		matched: MatchedPage,
		head_rules: Arc<HeadRules>,
		/// The sources that Vorma's content in the page needs.
		csp_sources: Arc<CspSources>,
		mode: PageRequestMode,
	},
	/// An endpoint call.
	ApiEndpoint {
		matched: MatchedApiEndpoint,
		/// The browser build that `x-vorma-client-build-id` names, absent when
		/// the request has no such header.
		requested_client_build_id: Option<String>,
	},
}

/// What a page request asks for.
pub(crate) enum PageRequestMode {
	/// The page's HTML document, for a request without `vorma-json`.
	Document,
	/// The page's navigation payload, for a request with `vorma-json`.
	NavPayload(RequestedBuild),
}

/// The build that a navigation payload request names in `vorma-json`.
pub(crate) enum RequestedBuild {
	/// The browser client's build ID. A request from another build than the
	/// server's gets the build-skew answer.
	Named(String),
	/// The value `_`, which names no build: the request gets the payload
	/// whatever the server's build is.
	Unnamed,
}

impl RequestKind {
	fn to_request_match(&self) -> RequestMatch {
		match self {
			Self::Page { matched, .. } => RequestMatch::Page(matched.clone()),
			Self::ApiEndpoint { matched, .. } => RequestMatch::ApiEndpoint(matched.clone()),
		}
	}

	/// The build that the request names to handlers.
	fn requested_client_build_id(&self) -> Option<String> {
		match self {
			Self::Page {
				mode: PageRequestMode::NavPayload(RequestedBuild::Named(id)),
				..
			} => Some(id.clone()),
			Self::Page {
				mode:
					PageRequestMode::Document | PageRequestMode::NavPayload(RequestedBuild::Unnamed),
				..
			} => None,
			Self::ApiEndpoint {
				requested_client_build_id,
				..
			} => requested_client_build_id.clone(),
		}
	}

	/// The CSP sources that handlers receive: the page's, and empty lists for
	/// an endpoint call, which has no inline content.
	fn to_csp_sources(&self) -> Arc<CspSources> {
		match self {
			Self::Page { csp_sources, .. } => Arc::clone(csp_sources),
			Self::ApiEndpoint { .. } => Arc::new(CspSources::default()),
		}
	}
}

/// The page or endpoint that Vorma matched for a request.
#[derive(Clone, Debug)]
pub enum RequestMatch {
	/// A page request, with its view pattern matches.
	Page(MatchedPage),
	/// An endpoint call.
	ApiEndpoint(MatchedApiEndpoint),
}

/// The view pattern matches that Vorma found for a page request, outermost first.
#[derive(Clone, Debug)]
pub struct MatchedPage {
	pub(crate) pattern_matches: Vec<PatternMatch>,
}

/// One matched view pattern: the pattern as written and the params and splat
/// values that it names.
#[derive(Clone, Debug)]
pub struct PatternMatch {
	pub(crate) pattern: String,
	pub(crate) decoded_match: DecodedMatch,
}

impl PatternMatch {
	/// Creates a pattern match from its pattern as written, the decoded
	/// value of each named param, and the decoded values of its splat.
	pub fn new(
		pattern: impl Into<String>,
		params: Vec<(String, String)>,
		splat_values: Vec<String>,
	) -> Self {
		Self {
			pattern: pattern.into(),
			decoded_match: DecodedMatch::new(params, splat_values),
		}
	}

	/// The matched pattern as written.
	pub fn pattern(&self) -> &str {
		&self.pattern
	}
}

impl MatchedPage {
	/// Creates the matched page of a request from its pattern matches,
	/// outermost first. A test uses it to build a handler context.
	pub fn new(pattern_matches: Vec<PatternMatch>) -> Self {
		Self { pattern_matches }
	}

	/// The params of the pattern match that has `Params`' pattern, or
	/// `Ok(None)` when no pattern match has that pattern. A pattern match that
	/// lacks a param that `Params` names, which only a pattern match that a
	/// test creates can lack, returns the [`MissingParamError`].
	pub fn find_params<Params: ViewPatternParams>(
		&self,
	) -> Result<Option<Params>, MissingParamError> {
		let pattern = Params::PATTERN;
		self.pattern_matches
			.iter()
			.find(|pattern_match| pattern_match.pattern == pattern)
			.map(|pattern_match| Params::from_decoded_match(pattern_match.decoded_match.clone()))
			.transpose()
	}

	/// The pattern matches, outermost first.
	pub fn pattern_matches(&self) -> impl Iterator<Item = &PatternMatch> {
		self.pattern_matches.iter()
	}
}

/// The API endpoint that Vorma matched for a request.
#[derive(Clone, Debug)]
pub struct MatchedApiEndpoint {
	/// How the browser treats a call to the endpoint.
	pub kind: ApiEndpointKind,
	/// The endpoint's declared method. A `HEAD` request has the method `GET`.
	pub method: Method,
	/// The endpoint's pattern as written.
	pub pattern: &'static str,
	pub(crate) decoded_match: DecodedMatch,
}

impl MatchedApiEndpoint {
	/// Creates the matched endpoint of a request from its kind, declared
	/// method, pattern as written, the decoded value of each named param, and
	/// the decoded values of its splat. A test uses it to build a handler
	/// context.
	pub fn new(
		kind: ApiEndpointKind,
		method: Method,
		pattern: &'static str,
		params: Vec<(String, String)>,
		splat_values: Vec<String>,
	) -> Self {
		Self {
			kind,
			method,
			pattern,
			decoded_match: DecodedMatch::new(params, splat_values),
		}
	}

	/// The endpoint's params when its pattern is `Params`' pattern, and
	/// `Ok(None)` otherwise. A matched endpoint that lacks a param that
	/// `Params` names, which only a matched endpoint that a test creates can
	/// lack, returns the [`MissingParamError`].
	pub fn find_params<Params: ApiPatternParams>(
		&self,
	) -> Result<Option<Params>, MissingParamError> {
		(Params::PATTERN == self.pattern)
			.then(|| Params::from_decoded_match(self.decoded_match.clone()))
			.transpose()
	}
}

/// What task middleware receives.
pub struct TaskMiddlewareCtx<State> {
	/// The state of the task runtime that the application gave to
	/// [`crate::LoadedApp::service`].
	pub state: Arc<State>,
	/// The request's method, URI, headers, and extensions. Vorma reads the
	/// body itself.
	///
	/// For a page request, the URI omits Vorma's transport parameters,
	/// `vorma-json` and, while Vercel skew protection is on, `dpl`, so a link
	/// built from it opens the page as a document even when the request is a
	/// client navigation's data request.
	/// The method, the path, the headers, and the extensions are as they
	/// arrived, and the host server still sees the raw request. For an
	/// endpoint call, the URI is as it arrived.
	pub request_parts: Arc<Parts>,
	/// The request's task context, which the request's task middleware and
	/// handlers share.
	pub task_ctx: TaskCtx<State>,
	/// Each static public asset's published URL by its source file name.
	pub static_public_assets: Arc<StaticPublicAssets>,
	/// Turns page targets into hrefs.
	pub page_hrefs: PageHrefs,
	/// The sources that Vorma's own content needs for each Content-Security-Policy
	/// directive. The lists are empty for an endpoint call.
	pub csp_sources: Arc<CspSources>,
	/// The build that this server uses for the response: the latest build
	/// that it knows of.
	pub latest_known_client_build_id: String,
	/// The build that the request names: the browser's build on an endpoint
	/// call or a navigation data request. `None` when the request names no
	/// build, as on a first page load, from a caller outside Vorma, or on a
	/// navigation data request with `vorma-json=_`, which
	/// [`AppService`](crate::AppService) describes.
	pub requested_client_build_id: Option<String>,
	/// The page or endpoint that Vorma matched for this request. Middleware
	/// runs after the match and before any loader or endpoint starts.
	pub request_match: RequestMatch,
}

/// What a view loader receives.
pub struct ViewLoaderCtx<State, Params, Input> {
	/// The state of the task runtime that the application gave to
	/// [`crate::LoadedApp::service`].
	pub state: Arc<State>,
	/// The loader's decoded pattern params.
	pub params: Params,
	/// The loader's decoded search input.
	pub input: Input,
	/// The loader's pattern as written.
	pub pattern: &'static str,
	/// The request's method, URI, headers, and extensions.
	///
	/// The URI omits Vorma's transport parameters, `vorma-json` and, while
	/// Vercel skew protection is on, `dpl`, so a link built from it opens the
	/// page as a document even when the request is a client navigation's data
	/// request. The method, the path, the
	/// headers, and the extensions are as they arrived, and the host server
	/// still sees the raw request.
	pub request_parts: Arc<Parts>,
	/// The request's task context, which the request's task middleware and
	/// handlers share.
	pub task_ctx: TaskCtx<State>,
	/// Each static public asset's published URL by its source file name.
	pub static_public_assets: Arc<StaticPublicAssets>,
	/// Turns page targets into hrefs.
	pub page_hrefs: PageHrefs,
	/// The sources that Vorma's own content needs for each Content-Security-Policy
	/// directive.
	pub csp_sources: Arc<CspSources>,
	/// The build that this server uses for the response.
	pub latest_known_client_build_id: String,
}

/// What an API endpoint receives.
pub struct ApiEndpointCtx<State, Params, Input> {
	/// The state of the task runtime that the application gave to
	/// [`crate::LoadedApp::service`].
	pub state: Arc<State>,
	/// The endpoint's decoded params.
	pub params: Params,
	/// The endpoint's decoded input.
	pub input: Input,
	/// The endpoint's pattern as written.
	pub pattern: &'static str,
	/// The request's method, URI, headers, and extensions. Vorma reads the
	/// body itself.
	pub request_parts: Arc<Parts>,
	/// The request's task context, which the request's task middleware and
	/// handlers share.
	pub task_ctx: TaskCtx<State>,
	/// Each static public asset's published URL by its source file name.
	pub static_public_assets: Arc<StaticPublicAssets>,
	/// Turns page targets into hrefs.
	pub page_hrefs: PageHrefs,
	/// The build that this server uses for the response.
	pub latest_known_client_build_id: String,
	/// The build that the request names, if any.
	pub requested_client_build_id: Option<String>,
}

impl RequestFacts {
	fn to_page_hrefs(&self) -> PageHrefs {
		PageHrefs::new(Arc::clone(&self.target_resolver))
	}

	/// What the request's response renders.
	pub(crate) fn response_target(&self) -> ResponseTarget<'_> {
		match &self.kind {
			RequestKind::Page { head_rules, .. } => ResponseTarget::Page { head_rules },
			RequestKind::ApiEndpoint { .. } => ResponseTarget::ApiEndpoint,
		}
	}

	pub(crate) fn to_task_middleware_ctx<State>(
		&self,
		state: Arc<State>,
		task_ctx: TaskCtx<State>,
	) -> TaskMiddlewareCtx<State> {
		TaskMiddlewareCtx {
			state,
			request_parts: Arc::clone(&self.request_parts),
			task_ctx,
			static_public_assets: Arc::clone(&self.static_public_assets),
			page_hrefs: self.to_page_hrefs(),
			csp_sources: self.kind.to_csp_sources(),
			latest_known_client_build_id: self.latest_known_client_build_id.clone(),
			requested_client_build_id: self.kind.requested_client_build_id(),
			request_match: self.kind.to_request_match(),
		}
	}

	pub(crate) fn to_view_loader_ctx<State, Params, Input>(
		&self,
		handler_facts: HandlerFacts<State, Params, Input>,
	) -> ViewLoaderCtx<State, Params, Input> {
		ViewLoaderCtx {
			state: handler_facts.state,
			params: handler_facts.params,
			input: handler_facts.input,
			pattern: handler_facts.pattern,
			request_parts: Arc::clone(&self.request_parts),
			task_ctx: handler_facts.task_ctx,
			static_public_assets: Arc::clone(&self.static_public_assets),
			page_hrefs: self.to_page_hrefs(),
			csp_sources: self.kind.to_csp_sources(),
			latest_known_client_build_id: self.latest_known_client_build_id.clone(),
		}
	}

	pub(crate) fn to_api_endpoint_ctx<State, Params, Input>(
		&self,
		handler_facts: HandlerFacts<State, Params, Input>,
	) -> ApiEndpointCtx<State, Params, Input> {
		ApiEndpointCtx {
			state: handler_facts.state,
			params: handler_facts.params,
			input: handler_facts.input,
			pattern: handler_facts.pattern,
			request_parts: Arc::clone(&self.request_parts),
			task_ctx: handler_facts.task_ctx,
			static_public_assets: Arc::clone(&self.static_public_assets),
			page_hrefs: self.to_page_hrefs(),
			latest_known_client_build_id: self.latest_known_client_build_id.clone(),
			requested_client_build_id: self.kind.requested_client_build_id(),
		}
	}
}

/// The facts of one view loader or API endpoint invocation that differ by handler.
pub(crate) struct HandlerFacts<State, Params, Input> {
	pub(crate) state: Arc<State>,
	pub(crate) params: Params,
	pub(crate) input: Input,
	pub(crate) pattern: &'static str,
	pub(crate) task_ctx: TaskCtx<State>,
}
