//! The Tower services that an application mounts: Vorma's application service and the
//! static public assets layer.

use std::convert::Infallible;
use std::error::Error as StdError;
use std::path::Path;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::{Buf, Bytes};
use futures_util::future::{self, BoxFuture, FutureExt};
use futures_util::{StreamExt, TryStreamExt};
use http::header::{ACCEPT_RANGES, CACHE_CONTROL, CONTENT_RANGE, CONTENT_TYPE};
use http::uri::PathAndQuery;
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode, Uri};
use http_body_util::{BodyExt, Either, Empty, Limited};
use primitives_rs::async_work::memoized_tasks::{TaskCtx, TaskRuntime};
use primitives_rs::http::range_requests::unsatisfied_content_range;
use serde_json::Value;
use tower_layer::Layer;
use tower_service::Service;
use vorma_path_wire::{canonicalize_request_path, decode_path_segment};

use crate::build_output::{
	BuildOutput, BuildSnapshot, ClientBuildId, RuntimeMode, SkewProtection,
	UNNAMED_BUILD_QUERY_VALUE,
};
use crate::ctx::{
	MatchedApiEndpoint, MatchedPage, PageRequestMode, PatternMatch, RequestFacts, RequestKind,
	RequestedBuild,
};
use crate::declarations::{ApiEndpointInputSource, InputError, Invocation, input_failure_outcome};
use crate::execution::{Committed, FailedPosition, PhaseInvocation, PhaseResult, run_phase};
use crate::finalize::{
	ACCEPTS_CLIENT_REDIRECT_HEADER, CLIENT_BUILD_ID_HEADER, RedirectForm, ResolvedRedirect,
	VormaBody, finalize_api_endpoint_failure, finalize_api_endpoint_or_report,
	finalize_api_endpoint_redirect, finalize_api_endpoint_success, finalize_build_skew,
	finalize_html, finalize_json, finalize_not_modified, finalize_or_report,
	finalize_page_redirect, finalize_readiness, finalize_status, is_json_content_type,
};
use crate::href::PlanTargetResolver;
use crate::outcome::{
	Contribution, HandlerKind, HandlerLabel, Outcome, RedirectKind, Redirection, ResolveTarget,
	invalid_input_outcome,
};
use crate::pattern::DecodedMatch;
use crate::payload::{
	CompiledPatternWire, DocumentInput, FailureWire, NavPayload, OrderedObject, RouteRecordWire,
	SsrPayload, prepare_assets, render_document, to_document_attrs_wire, to_head_wire,
};
use crate::plan::{
	ApiEndpointInputPlan, ApiEndpointNode, ClassifiedRequest, CurrentPlan, Declarations, Plan,
	ViewPosition,
};
use crate::search::{DEPLOYMENT_QUERY_KEY, DeploymentQueryKey, QueryIndex, VIEW_PAYLOAD_QUERY_KEY};
use crate::static_public_asset::{
	FileRange, StaticPublicAssetAnswer, StaticPublicAssetBody, decide_static_public_asset_answer,
	read_content_type,
};
use crate::{dev::READINESS_PATH, document::HeadRules};
use primitives_rs::http::form_data::{FormContentType, FormContentTypeError};

/// Vorma's application service, a Tower service. Mount it where the application
/// delegates requests to Vorma, such as an axum router's fallback service.
///
/// Mount the service so that it receives each request with its complete
/// application path, as the browser's address bar shows it. Vorma matches the
/// path against its patterns, and the browser client matches the path in the
/// address bar against the same patterns, so a mount or a host layer that
/// removes or rewrites part of the path makes the two disagree. For example,
/// axum's `Router::nest("/app", ...)` removes `/app` from the path that the
/// nested service sees, so the service would match `/app/docs` as `/docs`
/// while the browser matches it as `/app/docs`. A fallback service receives
/// the complete path.
///
/// Every handler of a request (task middleware, view loaders, and API
/// endpoints) runs in its own Tokio task, so a `tokio::task_local!` value that
/// the host sets around the call to this service is not visible to any handler.
/// A host that has per-request values puts them in the request's extensions,
/// which every handler reads from `request`.
///
/// A request that matches no Vorma pattern goes to the fallback, which
/// answers with an empty 404 unless [`AppService::set_fallback`] replaces it.
/// So does a request with a path that cannot be percent-decoded as UTF-8,
/// which matches no Vorma pattern. A request with a matched param or splat
/// value that cannot be decoded gets status 400.
///
/// Vorma collects a JSON request body in memory before it decodes it, and it
/// decodes a form body as the body arrives, with its uploaded files in
/// temporary files. It reads at most
/// [`App::max_request_body_size`](crate::App::max_request_body_size) of a
/// body, or the limit that
/// [`ApiEndpoint::set_max_request_body_size`](crate::ApiEndpoint::set_max_request_body_size)
/// sets for the endpoint, and answers a larger body with status 413. The
/// documentation of `max_request_body_size` describes the memory that one
/// request can use. A form body is also bounded by the endpoint's
/// [`FormInputLimits`](crate::FormInputLimits), which bound its number of
/// parts, each text value, and each uploaded file, and which name the
/// directory of its uploaded files. A limit of the host server applies as
/// well, for example tower-http's `RequestBodyLimitLayer`, and the lower
/// limit wins; axum's `DefaultBodyLimit` does not apply to a fallback
/// service.
///
/// A page request with the query parameter `vorma-json` asks for the page's
/// navigation payload, the JSON that Vorma's browser client reads, and the
/// parameter's value names the client's build. A request that names another
/// build than the server's gets the build-skew answer instead. The value `_`
/// names no build, so a request with `vorma-json=_` gets the navigation
/// payload whatever the server's build is: a developer can read the payload
/// that the browser client receives, for example with `curl`. An empty value
/// gets status 400.
///
/// The service creates each request's task context from the task runtime that
/// the application gave to [`LoadedApp::service`](crate::LoadedApp::service).
pub struct AppService<State, Fallback = NotFound> {
	inner: Arc<AppServiceInner<State>>,
	fallback: Fallback,
}

impl<State, Fallback: Clone> Clone for AppService<State, Fallback> {
	fn clone(&self) -> Self {
		Self {
			inner: Arc::clone(&self.inner),
			fallback: self.fallback.clone(),
		}
	}
}

pub(crate) struct AppServiceInner<State> {
	pub(crate) declarations: Arc<Declarations<State>>,
	pub(crate) head_rules: Arc<HeadRules>,
	pub(crate) build: Arc<BuildOutput>,
	pub(crate) plan: Arc<CurrentPlan<State>>,
	pub(crate) max_request_body_size: usize,
	pub(crate) tasks: TaskRuntime<State>,
	pub(crate) skew_protection: SkewProtection,
}

impl<State: Send + Sync + 'static> AppService<State> {
	pub(crate) fn new(inner: AppServiceInner<State>) -> Self {
		Self {
			inner: Arc::new(inner),
			fallback: NotFound,
		}
	}
}

impl<State, Fallback> AppService<State, Fallback> {
	/// Serves requests that match no Vorma pattern with this Tower service,
	/// such as a page for unknown URLs, as tower-http's `ServeDir::fallback`
	/// does.
	pub fn set_fallback<NewFallback>(
		self,
		fallback: NewFallback,
	) -> AppService<State, NewFallback> {
		AppService {
			inner: self.inner,
			fallback,
		}
	}
}

/// The default fallback of an [`AppService`]: an empty 404 response.
#[derive(Clone, Copy, Debug)]
pub struct NotFound;

impl<RequestBody> Service<Request<RequestBody>> for NotFound {
	type Response = Response<Empty<Bytes>>;
	type Error = Infallible;
	type Future = future::Ready<Result<Response<Empty<Bytes>>, Infallible>>;

	fn poll_ready(&mut self, _ctx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
		Poll::Ready(Ok(()))
	}

	fn call(&mut self, _request: Request<RequestBody>) -> Self::Future {
		let mut response = Response::new(Empty::new());
		*response.status_mut() = StatusCode::NOT_FOUND;
		future::ready(Ok(response))
	}
}

impl<State, Fallback, FallbackBody, RequestBody> Service<Request<RequestBody>>
	for AppService<State, Fallback>
where
	State: Send + Sync + 'static,
	Fallback: Service<Request<RequestBody>, Response = Response<FallbackBody>, Error = Infallible>
		+ Clone
		+ Send
		+ 'static,
	Fallback::Future: Send,
	RequestBody: http_body::Body + Send + 'static,
	RequestBody::Data: Send,
	RequestBody::Error: Into<Box<dyn StdError + Send + Sync>>,
{
	type Response = Response<Either<VormaBody, FallbackBody>>;
	type Error = Infallible;
	type Future = BoxFuture<'static, Result<Self::Response, Infallible>>;

	fn poll_ready(&mut self, ctx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
		self.fallback.poll_ready(ctx)
	}

	fn call(&mut self, request: Request<RequestBody>) -> Self::Future {
		// The ready clone serves this request; a fresh clone waits for the next.
		let clone = self.fallback.clone();
		let mut fallback = std::mem::replace(&mut self.fallback, clone);
		let inner = Arc::clone(&self.inner);
		async move {
			match inner.handle(request).await {
				Handled::Response(response) => Ok(response.map(Either::Left)),
				Handled::Unmatched(request) => fallback
					.call(request)
					.await
					.map(|response| response.map(Either::Right)),
			}
		}
		.boxed()
	}
}

/// How Vorma handled a request.
enum Handled<RequestBody> {
	Response(Response<VormaBody>),
	/// The request matches no Vorma pattern, so the fallback serves it.
	Unmatched(Request<RequestBody>),
}

/// The handlers that answer a matched request.
enum Dispatch<'plan, State> {
	ApiEndpoint {
		node: &'plan ApiEndpointNode<State>,
		decoded_match: DecodedMatch,
	},
	View {
		positions: Vec<ViewPosition<'plan, State>>,
		page_response: PageResponse,
	},
}

/// What a page request receives.
#[derive(Clone, Copy)]
enum PageResponse {
	/// The page's HTML document.
	Document,
	/// The page's navigation payload.
	NavPayload,
}

/// What every invocation of one request shares.
struct RequestSession<'plan, State> {
	plan: &'plan Arc<Plan<State>>,
	facts: Arc<RequestFacts>,
	task_ctx: TaskCtx<State>,
	accepts_client_redirect: bool,
	/// The request's path, without its query, for reports.
	request_path: Arc<str>,
}

impl<State> RequestSession<'_, State> {
	/// What reports about a handler of this request name.
	fn handler_label(&self, kind: HandlerKind, pattern: Arc<str>) -> HandlerLabel {
		HandlerLabel {
			kind,
			pattern,
			request_path: Arc::clone(&self.request_path),
		}
	}

	fn client_build_id(&self) -> &ClientBuildId {
		&self.plan.snapshot.metadata.client_build_id
	}

	/// A navigation redirect is a client redirect when the request accepts
	/// one; an endpoint redirect is always a 3xx response. A page request's
	/// redirect has the view cache policy, and an endpoint request's redirect
	/// that Vorma cannot assemble gets the endpoint failure envelope.
	fn finalize_redirect(
		&self,
		redirection: Redirection,
		contribution: &Contribution,
	) -> Response<VormaBody> {
		let client_build_id = self.client_build_id();
		let form = match redirection.kind {
			RedirectKind::ApiEndpoint => RedirectForm::ApiEndpoint,
			RedirectKind::Nav if self.accepts_client_redirect => RedirectForm::NavFollowedByClient,
			RedirectKind::Nav => RedirectForm::NavResponse,
		};
		let redirect = ResolvedRedirect {
			location: redirection.location,
			status: redirection.status,
			form,
		};
		match self.facts.kind {
			RequestKind::Page { .. } => finalize_or_report(
				finalize_page_redirect(redirect, contribution, client_build_id),
				client_build_id,
			),
			RequestKind::ApiEndpoint { .. } => finalize_api_endpoint_or_report(
				finalize_api_endpoint_redirect(redirect, contribution, client_build_id),
				client_build_id,
			),
		}
	}
}

impl<State: Send + Sync + 'static> AppServiceInner<State> {
	/// The plan for the current build. In development, a new build gets a new
	/// plan when a request first sees it. `None` after a build that cannot be
	/// read or planned, which the read or the planning has reported. The plan's
	/// state changes before another read of the build starts, so the state
	/// follows the latest read.
	async fn read_plan(&self) -> Option<Arc<Plan<State>>> {
		self.build
			.read_snapshot_then(|snapshot| match snapshot {
				Ok(snapshot) => self.plan.update(&self.declarations, snapshot).ok(),
				Err(error) => {
					self.plan.reject_build(error);
					None
				}
			})
			.await
	}

	fn spawn_invocation<Input>(
		&self,
		session: &RequestSession<'_, State>,
		invoke_handler_fn: &(
		     dyn Fn(Invocation<State, Input>) -> BoxFuture<'static, Outcome> + Send + Sync
		 ),
		decoded_match: DecodedMatch,
		input: Input,
		label: HandlerLabel,
	) -> PhaseInvocation {
		let future = invoke_handler_fn(Invocation {
			state: Arc::clone(self.tasks.state()),
			facts: Arc::clone(&session.facts),
			task_ctx: session.task_ctx.clone(),
			decoded_match,
			input,
			label: label.clone(),
		});
		PhaseInvocation { future, label }
	}

	async fn handle<RequestBody>(&self, request: Request<RequestBody>) -> Handled<RequestBody>
	where
		RequestBody: http_body::Body + Send + 'static,
		RequestBody::Data: Send,
		RequestBody::Error: Into<Box<dyn StdError + Send + Sync>>,
	{
		let Some(plan) = self.read_plan().await else {
			return Handled::Response(finalize_status(StatusCode::INTERNAL_SERVER_ERROR, None));
		};
		let (mut parts, body) = request.into_parts();
		let snapshot: &BuildSnapshot = &plan.snapshot;
		let client_build_id = &snapshot.metadata.client_build_id;
		let is_dev = snapshot.metadata.runtime_mode == RuntimeMode::Dev;
		if is_dev && parts.method == Method::GET && parts.uri.path() == READINESS_PATH {
			return Handled::Response(finalize_readiness(client_build_id));
		}
		let bad_request = || {
			Handled::Response(finalize_status(
				StatusCode::BAD_REQUEST,
				Some(client_build_id),
			))
		};
		// A path that cannot be decoded matches no Vorma pattern.
		let Ok(path) = canonicalize_request_path(parts.uri.path()) else {
			return Handled::Unmatched(Request::from_parts(parts, body));
		};
		// A HEAD request is answered as the GET request for its URL; the
		// server omits the body.
		let method = if parts.method == Method::HEAD {
			Method::GET
		} else {
			parts.method.clone()
		};
		let classified = match plan.classify(&method, &path) {
			Ok(Some(classified)) => classified,
			Ok(None) => return Handled::Unmatched(Request::from_parts(parts, body)),
			// A matched param or splat value that cannot be decoded is
			// Vorma's own input.
			Err(_) => return bad_request(),
		};
		let mut search_entries: Vec<(String, String)> =
			form_urlencoded::parse(parts.uri.query().unwrap_or_default().as_bytes())
				.into_owned()
				.collect();
		let deployment_query_key = self.skew_protection.deployment_query_key();
		let (kind, dispatch) = match classified {
			ClassifiedRequest::View { positions } => {
				let Ok(mode) = read_page_request_mode(&search_entries) else {
					return bad_request();
				};
				// A navigation payload is only served to the build that can
				// render it.
				let page_response = match &mode {
					PageRequestMode::Document => PageResponse::Document,
					PageRequestMode::NavPayload(RequestedBuild::Named(id))
						if id != client_build_id.as_str() =>
					{
						return Handled::Response(finalize_build_skew(client_build_id));
					}
					PageRequestMode::NavPayload(_) => PageResponse::NavPayload,
				};
				search_entries.retain(|(key, _)| !is_transport_param(key, deployment_query_key));
				parts.uri = without_transport_params(&parts.uri, deployment_query_key);
				let kind = RequestKind::Page {
					matched: MatchedPage {
						pattern_matches: positions
							.iter()
							.map(|position| PatternMatch {
								pattern: position.node.pattern.clone(),
								decoded_match: position.decoded_match.clone(),
							})
							.collect(),
					},
					head_rules: Arc::clone(&self.head_rules),
					csp_sources: Arc::clone(&snapshot.page_csp_sources),
					mode,
				};
				(
					kind,
					Dispatch::View {
						positions,
						page_response,
					},
				)
			}
			// An endpoint request from another build executes normally; its
			// response carries this build's ID, which the browser reports as a
			// mismatch.
			ClassifiedRequest::ApiEndpoint {
				node,
				decoded_match,
			} => {
				let Ok(requested_client_build_id) = read_requested_client_build_id(&parts.headers)
				else {
					return bad_request();
				};
				let kind = RequestKind::ApiEndpoint {
					matched: MatchedApiEndpoint {
						kind: node.declaration.kind,
						method: node.declaration.method.clone(),
						pattern: node.declaration.pattern,
						decoded_match: decoded_match.clone(),
					},
					requested_client_build_id,
				};
				(
					kind,
					Dispatch::ApiEndpoint {
						node,
						decoded_match,
					},
				)
			}
		};
		let accepts_client_redirect = parts.headers.contains_key(ACCEPTS_CLIENT_REDIRECT_HEADER);
		let target_resolver: Arc<dyn ResolveTarget> = Arc::new(PlanTargetResolver {
			plan: Arc::clone(&plan),
			deployment_query_key,
		});
		let request_path: Arc<str> = Arc::from(parts.uri.path());
		let facts = Arc::new(RequestFacts {
			target_resolver,
			request_parts: Arc::new(parts),
			latest_known_client_build_id: client_build_id.as_str().to_owned(),
			static_public_assets: Arc::clone(&snapshot.static_public_assets),
			kind,
		});
		let session = RequestSession {
			plan: &plan,
			facts,
			task_ctx: self.tasks.task_ctx(),
			accepts_client_redirect,
			request_path,
		};
		let middleware = plan
			.select_middleware(&method, &path)
			.into_iter()
			.map(|node| {
				let invocation = self.spawn_invocation(
					&session,
					&*node.declaration.invoke_handler_fn,
					DecodedMatch::default(),
					(),
					session.handler_label(HandlerKind::TaskMiddleware, Arc::clone(&node.pattern)),
				);
				((), Some(invocation))
			})
			.collect();
		let middleware = run_phase(middleware, Contribution::default()).await;
		Handled::Response(match dispatch {
			Dispatch::ApiEndpoint {
				node,
				decoded_match,
			} => {
				self.respond_api_endpoint(ApiEndpointRespondInput {
					session: &session,
					middleware,
					node,
					decoded_match,
					search_entries: &search_entries,
					body,
				})
				.await
			}
			Dispatch::View {
				positions,
				page_response,
			} => {
				self.respond_view(ViewRespondInput {
					session: &session,
					middleware,
					positions,
					search_entries: &search_entries,
					page_response,
				})
				.await
			}
		})
	}

	async fn respond_api_endpoint<RequestBody>(
		&self,
		request: ApiEndpointRespondInput<'_, '_, State, RequestBody>,
	) -> Response<VormaBody>
	where
		RequestBody: http_body::Body + Send + 'static,
		RequestBody::Data: Send,
		RequestBody::Error: Into<Box<dyn StdError + Send + Sync>>,
	{
		let session = request.session;
		let client_build_id = session.client_build_id();
		let contribution = match request.middleware {
			PhaseResult::Completed { contribution, .. } => contribution,
			PhaseResult::Redirect {
				contribution,
				redirection,
			} => return session.finalize_redirect(redirection, &contribution),
			PhaseResult::Failed {
				contribution,
				failed,
				..
			} => {
				return finalize_api_endpoint_or_report(
					finalize_api_endpoint_failure(
						failed.status,
						failed.public_error.as_ref(),
						&contribution,
						client_build_id,
					),
					client_build_id,
				);
			}
		};
		let node: &ApiEndpointNode<State> = request.node;
		let body_limit = node
			.declaration
			.max_request_body_size
			.unwrap_or(self.max_request_body_size);
		let label = session.handler_label(
			HandlerKind::ApiEndpoint,
			Arc::from(node.declaration.pattern),
		);
		let headers = &session.facts.request_parts.headers;
		let source = match &node.input {
			ApiEndpointInputPlan::None => Ok(ApiEndpointInputSource::None),
			ApiEndpointInputPlan::Search(shape) => QueryIndex::new(request.search_entries)
				.parse_search(shape)
				.map(ApiEndpointInputSource::Search)
				.map_err(|_| InputError::Invalid),
			ApiEndpointInputPlan::Json => {
				let mut content_types = headers.get_all(CONTENT_TYPE).iter();
				match (content_types.next(), content_types.next()) {
					(Some(content_type), None) if is_json_content_type(content_type) => {
						match Limited::new(request.body, body_limit).collect().await {
							Ok(collected) => Ok(ApiEndpointInputSource::Json(collected.to_bytes())),
							Err(error) => Err(InputError::BodyRead(error)),
						}
					}
					// Several values are not one media type, as for a form.
					(Some(_), Some(_)) => Err(InputError::Invalid),
					(Some(_), None) | (None, _) => Err(InputError::UnsupportedMediaType),
				}
			}
			// A form is read while it is decoded, so uploaded files stream to
			// temporary files instead of memory.
			ApiEndpointInputPlan::FormData(limits) => {
				match FormContentType::from_headers(headers) {
					Ok(media_type) => Ok(ApiEndpointInputSource::FormData {
						media_type,
						config: limits.to_decode_config(body_limit),
						body: request
							.body
							.into_data_stream()
							.map_ok(|mut data| data.copy_to_bytes(data.remaining()))
							.map_err(Into::into)
							.boxed(),
					}),
					Err(FormContentTypeError::Invalid) => Err(InputError::Invalid),
					Err(FormContentTypeError::Unsupported) => Err(InputError::UnsupportedMediaType),
				}
			}
		};
		let invocation = match source {
			Ok(source) => self.spawn_invocation(
				session,
				&*node.declaration.invoke_handler_fn,
				request.decoded_match,
				source,
				label,
			),
			Err(error) => PhaseInvocation {
				future: future::ready(input_failure_outcome(&label, error)).boxed(),
				label,
			},
		};
		let response = match run_phase(vec![((), Some(invocation))], contribution).await {
			PhaseResult::Completed {
				contribution,
				positions,
			} => {
				let committed = positions.into_iter().next();
				match committed {
					Some(Committed::NotModified { .. }) => {
						finalize_not_modified(&contribution, client_build_id)
					}
					Some(Committed::Completed { output, .. }) => finalize_api_endpoint_success(
						output.as_deref(),
						&contribution,
						client_build_id,
					),
					Some(Committed::NoHandler { .. }) | None => {
						finalize_api_endpoint_success(None, &contribution, client_build_id)
					}
				}
			}
			PhaseResult::Redirect {
				contribution,
				redirection,
			} => return session.finalize_redirect(redirection, &contribution),
			PhaseResult::Failed {
				contribution,
				failed,
				..
			} => finalize_api_endpoint_failure(
				failed.status,
				failed.public_error.as_ref(),
				&contribution,
				client_build_id,
			),
		};
		finalize_api_endpoint_or_report(response, client_build_id)
	}

	async fn respond_view(
		&self,
		request: ViewRespondInput<'_, '_, '_, State>,
	) -> Response<VormaBody> {
		let session = request.session;
		let contribution = match request.middleware {
			PhaseResult::Completed { contribution, .. } => contribution,
			PhaseResult::Redirect {
				contribution,
				redirection,
			} => return session.finalize_redirect(redirection, &contribution),
			// A rejected page request renders the application boundary: the
			// first position fails with the rejection, and no position renders,
			// so no position's search is read.
			PhaseResult::Failed {
				contribution,
				failed,
				..
			} => {
				return self.render_view(RenderViewInput {
					session,
					positions: &request.positions,
					searches: &[],
					committed: Vec::new(),
					failed_position: Some(FailedPosition { index: 0, ..failed }),
					contribution,
					page_response: request.page_response,
				});
			}
		};
		// Each loader's search is parsed once from one index of the query: its
		// decoder reads it here and the payload carries it to the browser.
		let query = QueryIndex::new(request.search_entries);
		let searches: Vec<Option<Result<Value, ()>>> = request
			.positions
			.iter()
			.map(|position| {
				let search = position.node.kind.loader()?.search.as_ref()?;
				Some(query.parse_search(search).map_err(|_| ()))
			})
			.collect();
		let mut phase = Vec::with_capacity(request.positions.len());
		for (index, position) in request.positions.iter().enumerate() {
			let Some(loader) = position.node.kind.loader() else {
				phase.push((index, None));
				continue;
			};
			let label = session.handler_label(
				HandlerKind::ViewLoader,
				Arc::from(loader.declaration.pattern),
			);
			let invocation = match &searches[index] {
				Some(Err(())) => PhaseInvocation {
					future: future::ready(invalid_input_outcome()).boxed(),
					label,
				},
				search => {
					let input = match search {
						Some(Ok(value)) => value.clone(),
						_ => Value::Null,
					};
					self.spawn_invocation(
						session,
						&*loader.declaration.invoke_handler_fn,
						position.decoded_match.clone(),
						input,
						label,
					)
				}
			};
			phase.push((index, Some(invocation)));
		}
		let (committed, failed_position, contribution) = match run_phase(phase, contribution).await
		{
			PhaseResult::Redirect {
				contribution,
				redirection,
			} => return session.finalize_redirect(redirection, &contribution),
			PhaseResult::Completed {
				contribution,
				positions,
			} => (positions, None, contribution),
			PhaseResult::Failed {
				contribution,
				before,
				failed,
				..
			} => (before, Some(failed), contribution),
		};
		self.render_view(RenderViewInput {
			session,
			positions: &request.positions,
			searches: &searches,
			committed,
			failed_position,
			contribution,
			page_response: request.page_response,
		})
	}

	fn render_view(&self, input: RenderViewInput<'_, '_, '_, State>) -> Response<VormaBody> {
		let session = input.session;
		let metadata = &session.plan.snapshot.metadata;
		let client_build_id = &metadata.client_build_id;
		let mut routes = Vec::with_capacity(input.committed.len());
		let mut views = Vec::with_capacity(input.committed.len());
		for committed in &input.committed {
			let (index, output) = match committed {
				Committed::Completed {
					position_key,
					output,
				} => (*position_key, output.as_deref()),
				Committed::NotModified { position_key } | Committed::NoHandler { position_key } => {
					(*position_key, None)
				}
			};
			let position = &input.positions[index];
			let node = position.node;
			let client_view_module = node.kind.client_view_module();
			if let Some(module) = client_view_module {
				views.push(module);
			}
			let registered = node.compiled_pattern.as_pattern().as_str();
			routes.push(RouteRecordWire {
				pattern: &node.pattern,
				params: OrderedObject(
					position
						.decoded_match
						.params
						.iter()
						.map(|(name, value)| (name.as_str(), value.as_str()))
						.collect(),
				),
				splat_values: &position.decoded_match.splat_values,
				compiled_pattern: CompiledPatternWire {
					pattern: (registered != node.pattern).then_some(registered),
					segments: node.compiled_pattern.as_pattern().segments(),
				},
				search_shape: node.kind.loader().and_then(|loader| loader.search.as_ref()),
				search_input: match &input.searches[index] {
					Some(Ok(value)) => Some(value),
					_ => None,
				},
				import_url: client_view_module.map(|module| module.import_url.as_str()),
				loader_output: output,
			});
		}
		let failure = input.failed_position.as_ref().map(|error| FailureWire {
			index: Some(error.index),
			status: error.status.as_u16(),
			public_error: error.public_error.as_ref(),
		});
		let contribution = &input.contribution;
		let document = &contribution.document;
		let head = self.head_rules.deduplicate(document.head_changes());
		let assets = prepare_assets(&metadata.client_entry, views.iter().copied());
		let response = match input.page_response {
			PageResponse::NavPayload => {
				let payload = NavPayload {
					routes,
					failure,
					head: to_head_wire(&head),
					document_attrs: to_document_attrs_wire(document),
					deps: &assets.nav_dep_urls,
					css_bundles: &assets.nav_css_bundle_urls,
					static_public_asset_preloads: &assets.nav_static_public_asset_preloads,
				};
				serde_json::to_string(&payload)
					.map_err(crate::finalize::FinalizeError::Serialization)
					.and_then(|json| finalize_json(json, contribution, client_build_id))
			}
			PageResponse::Document => {
				let module_preload_urls: Vec<&str> =
					std::iter::once(metadata.client_entry.import_url.as_str())
						.chain(routes.iter().filter_map(|route| route.import_url))
						.chain(assets.document_dep_urls.iter().map(String::as_str))
						.collect();
				let payload = SsrPayload {
					routes,
					failure,
					client_build_id: client_build_id.as_str(),
					document_attrs: to_document_attrs_wire(document),
					is_dev: metadata.runtime_mode == RuntimeMode::Dev,
					deployment_id: self.skew_protection.deployment_id(),
				};
				serde_json::to_string(&payload)
					.map_err(crate::finalize::FinalizeError::Serialization)
					.and_then(|payload_json| {
						let html = render_document(DocumentInput {
							html_attrs: &document.html_attrs,
							head_attrs: &document.head_attrs,
							body_attrs: &document.body_attrs,
							head: &head,
							critical_css: &metadata.critical_css,
							dev_head_html: metadata.dev_head_html.as_deref(),
							dev_body_prepend_html: metadata.dev_body_prepend_html.as_deref(),
							dev_body_html: metadata.dev_body_html.as_deref(),
							css_bundle_urls: &assets.document_css_bundle_urls,
							module_preload_urls,
							static_public_asset_preloads: &assets
								.document_static_public_asset_preloads,
							entry_import_url: &metadata.client_entry.import_url,
							payload_json: &payload_json,
						});
						finalize_html(html, contribution, client_build_id)
					})
			}
		};
		finalize_or_report(response, client_build_id)
	}
}

/// What a page request asks for, from its `vorma-json` query parameter:
/// without it, the document; with it, the navigation payload for the build
/// that its value names, or for no build with `_`. Several values or an
/// empty value are an error.
fn read_page_request_mode(entries: &[(String, String)]) -> Result<PageRequestMode, ()> {
	let mut values = entries
		.iter()
		.filter(|(key, _)| key == VIEW_PAYLOAD_QUERY_KEY)
		.map(|(_, value)| value.as_str());
	match (values.next(), values.next()) {
		(None, _) => Ok(PageRequestMode::Document),
		(Some(_), Some(_)) | (Some(""), None) => Err(()),
		(Some(UNNAMED_BUILD_QUERY_VALUE), None) => {
			Ok(PageRequestMode::NavPayload(RequestedBuild::Unnamed))
		}
		(Some(id), None) => Ok(PageRequestMode::NavPayload(RequestedBuild::Named(
			id.to_owned(),
		))),
	}
}

/// The build that an endpoint request names in `x-vorma-client-build-id`.
/// Several values, an empty value, or a value that is not visible ASCII are
/// an error.
fn read_requested_client_build_id(headers: &HeaderMap) -> Result<Option<String>, ()> {
	let mut values = headers.get_all(CLIENT_BUILD_ID_HEADER).iter();
	match (values.next(), values.next()) {
		(None, _) => Ok(None),
		(Some(_), Some(_)) => Err(()),
		(Some(value), None) => match value.to_str() {
			Ok(id) if !id.is_empty() => Ok(Some(id.to_owned())),
			_ => Err(()),
		},
	}
}

/// Whether a query key belongs to the transport: `vorma-json` always, and
/// `dpl` while Vercel skew protection is on.
fn is_transport_param(key: &str, deployment_query_key: DeploymentQueryKey) -> bool {
	key == VIEW_PAYLOAD_QUERY_KEY
		|| (deployment_query_key == DeploymentQueryKey::Reserved && key == DEPLOYMENT_QUERY_KEY)
}

/// The URI without the transport parameters, with the rest of the query kept
/// as it arrived.
fn without_transport_params(uri: &Uri, deployment_query_key: DeploymentQueryKey) -> Uri {
	let Some(query) = uri.query() else {
		return uri.clone();
	};
	let kept: Vec<&str> = query
		.split('&')
		.filter(|pair| {
			let key = form_urlencoded::parse(pair.as_bytes()).next();
			!key.is_some_and(|(key, _)| is_transport_param(&key, deployment_query_key))
		})
		.collect();
	let path = uri.path();
	let path_and_query = if kept.is_empty() {
		path.to_owned()
	} else {
		format!("{path}?{}", kept.join("&"))
	};
	let mut uri_parts = uri.clone().into_parts();
	uri_parts.path_and_query = Some(
		path_and_query
			.parse::<PathAndQuery>()
			.expect("a path and a subset of its query are a valid path and query"),
	);
	Uri::from_parts(uri_parts).expect("replacing the path and query of a valid URI keeps it valid")
}

struct ApiEndpointRespondInput<'session, 'plan, State, RequestBody> {
	session: &'session RequestSession<'plan, State>,
	middleware: PhaseResult<()>,
	node: &'plan ApiEndpointNode<State>,
	decoded_match: DecodedMatch,
	search_entries: &'session [(String, String)],
	body: RequestBody,
}

struct ViewRespondInput<'session, 'plan, 'entries, State> {
	session: &'session RequestSession<'plan, State>,
	middleware: PhaseResult<()>,
	positions: Vec<ViewPosition<'plan, State>>,
	search_entries: &'entries [(String, String)],
	page_response: PageResponse,
}

struct RenderViewInput<'session, 'plan, 'request, State> {
	session: &'session RequestSession<'plan, State>,
	positions: &'request [ViewPosition<'plan, State>],
	/// Each position's search, for every position when a position commits.
	searches: &'request [Option<Result<Value, ()>>],
	committed: Vec<Committed<usize>>,
	failed_position: Option<FailedPosition>,
	contribution: Contribution,
	page_response: PageResponse,
}

/// Serves each published static public asset and Vorma's browser modules,
/// and passes every other request on. Without it, or another server for the
/// build directory's `public` directory, every request for the application's
/// JavaScript gets a 404. A host that serves that directory through a CDN or
/// another file server does not install it.
///
/// The layer percent-decodes the request path after the build's URL path
/// prefix for static public assets, and it serves the published file with
/// that decoded name. A path that cannot be decoded, or that names no
/// published file, passes on to the inner service.
///
/// The layer answers ranges of `GET` and `HEAD` requests for a file as Go's
/// `http.FileServer` does, so that browsers can play published video and
/// audio:
///
/// - Every `200`, `206`, and `416` answer carries `Accept-Ranges: bytes`. No
///   answer carries `Last-Modified` or `ETag`: a production file is immutable
///   under its URL, and a browser revalidates a development file by
///   downloading it again.
/// - A `Range` header with one satisfiable range gets `206` with
///   `Content-Range`, and the layer streams only the selected bytes. A
///   `bytes` header that selects no byte of a non-empty file, a `bytes`
///   header that is not a list of byte ranges, and a request with several
///   `Range` header lines get `416` with `Content-Range: bytes */<length>`.
///   An empty file ignores a list of byte ranges.
/// - A `Range` header with several satisfiable ranges gets `200` with the
///   whole file, which RFC 9110 permits. The layer never answers with
///   `multipart/byteranges`.
/// - Unlike Go's `http.FileServer`, the layer ignores a `Range` header with a
///   range unit other than `bytes` and answers `200` with the whole file, as
///   RFC 9110 section 14.2 requires.
/// - A request with `If-Range` gets `200` with the whole file because the
///   file has no validator to compare. Conditional headers other than
///   `If-Range` have no effect.
/// - A `HEAD` request gets the status and headers of the `GET` request.
///
/// Unlike Go's `http.FileServer`, the layer sets `Cache-Control`. A `200` or
/// `206` answer carries `public, max-age=31536000, immutable` in production
/// and `no-cache` in development, where `vorma dev` writes a file again
/// under its name. A `416` answer carries `no-store`, because it depends on
/// the request's `Range` header, which a cache key does not include.
///
/// In development, the layer reads the build's metadata for each request that
/// it receives, and it uses that read only for its own answer; the
/// application service reads the metadata again for a request that reaches
/// it. When the metadata cannot be read, the layer passes the request on.
#[derive(Clone)]
pub struct StaticPublicAssetsLayer {
	build: Arc<BuildOutput>,
}

impl StaticPublicAssetsLayer {
	pub(crate) fn new(build: Arc<BuildOutput>) -> Self {
		Self { build }
	}
}

impl<Inner> Layer<Inner> for StaticPublicAssetsLayer {
	type Service = StaticPublicAssetsService<Inner>;

	fn layer(&self, inner: Inner) -> Self::Service {
		StaticPublicAssetsService {
			inner,
			build: Arc::clone(&self.build),
		}
	}
}

/// The service that [`StaticPublicAssetsLayer`] makes.
#[derive(Clone)]
pub struct StaticPublicAssetsService<Inner> {
	inner: Inner,
	build: Arc<BuildOutput>,
}

const IMMUTABLE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
const DEV_CACHE_CONTROL: &str = "no-cache";
const RANGE_NOT_SATISFIABLE_CACHE_CONTROL: &str = "no-store";

impl<Inner, RequestBody, ResponseBody> Service<Request<RequestBody>>
	for StaticPublicAssetsService<Inner>
where
	Inner:
		Service<Request<RequestBody>, Response = Response<ResponseBody>> + Clone + Send + 'static,
	Inner::Future: Send,
	RequestBody: Send + 'static,
{
	type Response = Response<Either<StaticPublicAssetBody, ResponseBody>>;
	type Error = Inner::Error;
	type Future = BoxFuture<'static, Result<Self::Response, Inner::Error>>;

	fn poll_ready(&mut self, ctx: &mut Context<'_>) -> Poll<Result<(), Inner::Error>> {
		self.inner.poll_ready(ctx)
	}

	fn call(&mut self, request: Request<RequestBody>) -> Self::Future {
		// The ready clone serves this request; a fresh clone waits for the next.
		let clone = self.inner.clone();
		let mut inner = std::mem::replace(&mut self.inner, clone);
		let build = Arc::clone(&self.build);
		let asset_request = StaticPublicAssetRequest {
			method: request.method().clone(),
			path: request.uri().path().to_owned(),
			headers: request.headers().clone(),
		};
		async move {
			let snapshot = build.read_snapshot().await;
			if let Ok(snapshot) = &snapshot
				&& let Some(response) =
					serve_static_public_asset(&build, snapshot, &asset_request).await
			{
				return Ok(response.map(Either::Left));
			}
			inner
				.call(request)
				.await
				.map(|response| response.map(Either::Right))
		}
		.boxed()
	}
}

/// The published file that a request path names: the path after the URL
/// path prefix for static public assets, percent-decoded. `None` for a path
/// that cannot be decoded or that names no published file.
fn find_static_public_asset_file_name<'a>(
	snapshot: &'a BuildSnapshot,
	path: &str,
) -> Option<&'a str> {
	let encoded =
		path.strip_prefix(&snapshot.metadata.static_public_assets_serve_url_path_prefix)?;
	let file_name = encoded
		.split('/')
		.map(decode_path_segment)
		.collect::<Result<Vec<_>, _>>()
		.ok()?
		.join("/");
	snapshot
		.static_public_asset_file_names
		.get(&file_name)
		.map(String::as_str)
}

/// The parts of a request that the static public assets layer reads.
struct StaticPublicAssetRequest {
	method: Method,
	path: String,
	headers: HeaderMap,
}

/// A `GET` or `HEAD` request for a published file.
///
/// The answer follows `http.FileServer`'s ranges: `Accept-Ranges`, `206` for
/// one satisfiable range, and `416` for a range that selects nothing. See
/// [`decide_static_public_asset_answer`].
async fn serve_static_public_asset(
	build: &BuildOutput,
	snapshot: &BuildSnapshot,
	request: &StaticPublicAssetRequest,
) -> Option<Response<StaticPublicAssetBody>> {
	let method = &request.method;
	if method != Method::GET && method != Method::HEAD {
		return None;
	}
	let file_name = find_static_public_asset_file_name(snapshot, &request.path)?;
	let path = build.public_dir()?.join(file_name);
	let answer = match answer_static_public_asset_request(&path, method, &request.headers).await {
		Ok(answer) => answer,
		Err(error) => {
			tracing::error!(path = %path.display(), error = &error as &(dyn StdError + 'static), "Vorma could not read a static public asset");
			return Some(create_status_response(StatusCode::INTERNAL_SERVER_ERROR).await);
		}
	};
	let mut response = Response::new(answer.body);
	*response.status_mut() = answer.status;
	let headers = response.headers_mut();
	let cache_control = match (answer.status, snapshot.metadata.runtime_mode) {
		(StatusCode::RANGE_NOT_SATISFIABLE, _) => RANGE_NOT_SATISFIABLE_CACHE_CONTROL,
		(_, RuntimeMode::Dev) => DEV_CACHE_CONTROL,
		(_, RuntimeMode::Prod) => IMMUTABLE_CACHE_CONTROL,
	};
	headers.insert(CACHE_CONTROL, HeaderValue::from_static(cache_control));
	headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
	if matches!(answer.status, StatusCode::OK | StatusCode::PARTIAL_CONTENT)
		&& let Ok(content_type) = HeaderValue::from_str(&read_content_type(file_name))
	{
		headers.insert(CONTENT_TYPE, content_type);
	}
	if let Some(content_range) = answer.content_range {
		headers.insert(CONTENT_RANGE, content_range);
	}
	Some(response)
}

/// The status, body, and range header of the answer to a request
/// for the file at `path`.
struct StaticPublicAssetResponseParts {
	status: StatusCode,
	body: StaticPublicAssetBody,
	content_range: Option<HeaderValue>,
}

/// Opens the file and decides the answer. The body of a `GET` answer streams
/// only the selected bytes, and the body of a `HEAD` answer reports their
/// length without opening a stream.
async fn answer_static_public_asset_request(
	path: &Path,
	method: &Method,
	request_headers: &HeaderMap,
) -> std::io::Result<StaticPublicAssetResponseParts> {
	let file = tokio::fs::File::open(path).await?;
	let metadata = file.metadata().await?;
	let length = metadata.len();
	let (status, selected, content_range) =
		match decide_static_public_asset_answer(request_headers, length) {
			StaticPublicAssetAnswer::Whole => {
				(StatusCode::OK, FileRange { start: 0, length }, None)
			}
			StaticPublicAssetAnswer::Partial(range) => (
				StatusCode::PARTIAL_CONTENT,
				FileRange {
					start: range.start(),
					length: range.length().get(),
				},
				Some(range.content_range()),
			),
			StaticPublicAssetAnswer::RangeNotSatisfiable => (
				StatusCode::RANGE_NOT_SATISFIABLE,
				FileRange {
					start: 0,
					length: 0,
				},
				Some(unsatisfied_content_range(length)),
			),
		};
	let body = if method == Method::HEAD || selected.length == 0 {
		StaticPublicAssetBody::from_length_only(selected.length)
	} else {
		StaticPublicAssetBody::from_file(file, selected).await?
	};
	Ok(StaticPublicAssetResponseParts {
		status,
		body,
		content_range,
	})
}

/// The status response that Vorma makes, with its body in memory.
async fn create_status_response(status: StatusCode) -> Response<StaticPublicAssetBody> {
	let (parts, body) = finalize_status(status, None).into_parts();
	let bytes = match body.collect().await {
		Ok(collected) => collected.to_bytes(),
		Err(never) => match never {},
	};
	Response::from_parts(parts, StaticPublicAssetBody::from_bytes(bytes))
}
