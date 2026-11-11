//! The request plan: one build's client views and the application's
//! declarations compiled into matchers. One nested matcher covers every view
//! position that has a loader, a client view, or both. One flat matcher per
//! method covers endpoints, and one flat matcher per task middleware covers
//! its own patterns.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use http::Method;
use primitives_rs::path_matcher::{
	FlatMatcher, FlatPattern, FlatPatternLanguage, FlatPatternOptions, NestedMatcher,
	NestedPattern, NestedPatternLanguage, NestedPatternOptions, Pattern, PatternComparison,
	Segment, compare_matching_patterns, patterns_can_match_one_path,
};
use vorma_path_wire::{
	DYNAMIC_PARAM_PREFIX, INDEX_SEGMENT, PathWireError, SPLAT_SEGMENT, canonicalize_pattern,
	decode_path_segment,
};

use crate::build_output::{BuildOutputError, BuildSnapshot, ClientModule};
use crate::declarations::{
	ApiEndpoint, ApiEndpointInputKind, FormInputLimits, TaskMiddleware, ViewLoader,
};
use crate::pattern::DecodedMatch;
use crate::search::{DEPLOYMENT_QUERY_KEY, SearchShape};

fn create_nested_language() -> NestedPatternLanguage {
	NestedPatternLanguage::new(NestedPatternOptions {
		dynamic_param_prefix: DYNAMIC_PARAM_PREFIX,
		splat_segment_identifier: SPLAT_SEGMENT,
		explicit_index_segment_identifier: INDEX_SEGMENT.to_owned(),
	})
	.expect("Vorma's nested pattern language is valid")
}

fn create_flat_language() -> FlatPatternLanguage {
	FlatPatternLanguage::new(FlatPatternOptions {
		dynamic_param_prefix: DYNAMIC_PARAM_PREFIX,
		splat_segment_identifier: SPLAT_SEGMENT,
	})
	.expect("Vorma's flat pattern language is valid")
}

/// Compiles an authored flat pattern after encoding it into matcher wire
/// spelling.
fn compile_flat_pattern(
	language: &FlatPatternLanguage,
	pattern: &str,
) -> Result<FlatPattern, String> {
	let wire = canonicalize_pattern(pattern).map_err(|error| error.to_string())?;
	language
		.compile_pattern(&wire)
		.map_err(|error| error.to_string())
}

/// Compiles an authored nested pattern after encoding it into matcher wire
/// spelling.
fn compile_nested_pattern(
	language: &NestedPatternLanguage,
	pattern: &str,
) -> Result<NestedPattern, String> {
	let wire = canonicalize_pattern(pattern).map_err(|error| error.to_string())?;
	language
		.compile_pattern(&wire)
		.map_err(|error| error.to_string())
}

/// The application's declarations, shared by every plan.
pub(crate) struct Declarations<State> {
	pub(crate) task_middlewares: Vec<TaskMiddleware<State>>,
	pub(crate) view_loaders: Vec<ViewLoader<State>>,
	pub(crate) api_endpoints: Vec<ApiEndpoint<State>>,
}

impl<State> Declarations<State> {
	/// The problems of the search shapes that declare a top-level field
	/// named `dpl`, which Vercel skew protection reserves while it is on.
	pub(crate) fn find_skew_protection_search_problems(&self) -> Vec<String> {
		let view_loaders = self.view_loaders.iter().filter_map(|declaration| {
			let search = (declaration.derive_search_shape_fn)().ok()??;
			Some((declaration.describe(), search))
		});
		let api_endpoints = self.api_endpoints.iter().filter_map(|declaration| {
			let reads_search = (declaration.determine_input_kind_fn)(&declaration.method).ok()?
				== ApiEndpointInputKind::Search;
			let search = reads_search
				.then(|| (declaration.derive_search_shape_fn)().ok())
				.flatten()?;
			Some((declaration.describe(), search))
		});
		view_loaders
			.chain(api_endpoints)
			.filter(|(_, search)| search.has_top_level_field(DEPLOYMENT_QUERY_KEY))
			.map(|(location, _)| {
				format!(
					"{location}: its search input declares the field {DEPLOYMENT_QUERY_KEY:?}, which Vercel skew protection reserves"
				)
			})
			.collect()
	}
}

/// A view loader with its search shape.
pub(crate) struct ViewLoaderNode<State> {
	pub(crate) declaration: ViewLoader<State>,
	pub(crate) search: Option<SearchShape>,
}

/// One view: a loader, a client definition, or both. A view that defines no
/// explicit component, such as one with a loader and no client definition or a
/// client definition without a `component`, is a pass-through view: the client
/// renders its child in its place.
pub(crate) struct ViewNode<State> {
	/// The authored identity exposed to application and browser code.
	pub(crate) pattern: String,
	pub(crate) compiled_pattern: NestedPattern,
	pub(crate) kind: ViewKind<State>,
	/// The loaders at and above this position with search fields that compose
	/// into its input, outermost first.
	pub(crate) search_owners: Vec<Arc<ViewLoaderNode<State>>>,
}

/// What defines a view position.
pub(crate) enum ViewKind<State> {
	/// A view loader without a view module in the build.
	Loader(Arc<ViewLoaderNode<State>>),
	/// A view module of the build without a view loader.
	ClientView(ClientModule),
	/// A view loader and the build's view module of its pattern.
	LoaderAndClientView {
		loader: Arc<ViewLoaderNode<State>>,
		client_view_module: ClientModule,
	},
}

impl<State> ViewKind<State> {
	pub(crate) fn loader(&self) -> Option<&Arc<ViewLoaderNode<State>>> {
		match self {
			Self::Loader(loader) | Self::LoaderAndClientView { loader, .. } => Some(loader),
			Self::ClientView(_) => None,
		}
	}

	pub(crate) fn client_view_module(&self) -> Option<&ClientModule> {
		match self {
			Self::ClientView(client_view_module)
			| Self::LoaderAndClientView {
				client_view_module, ..
			} => Some(client_view_module),
			Self::Loader(_) => None,
		}
	}
}

pub(crate) struct ApiEndpointNode<State> {
	pub(crate) declaration: ApiEndpoint<State>,
	pub(crate) input: ApiEndpointInputPlan,
}

/// How Vorma reads an endpoint's input.
pub(crate) enum ApiEndpointInputPlan {
	/// The endpoint reads no input.
	None,
	/// A `GET` endpoint reads the query with this search shape.
	Search(SearchShape),
	/// The endpoint reads a JSON body.
	Json,
	/// The endpoint reads a form body with these limits.
	FormData(FormInputLimits),
}

impl<State> ApiEndpointNode<State> {
	pub(crate) fn search_shape(&self) -> Option<&SearchShape> {
		match &self.input {
			ApiEndpointInputPlan::Search(shape) => Some(shape),
			ApiEndpointInputPlan::None
			| ApiEndpointInputPlan::Json
			| ApiEndpointInputPlan::FormData(_) => None,
		}
	}
}

pub(crate) struct MiddlewareNode<State> {
	pub(crate) declaration: TaskMiddleware<State>,
	/// The patterns that the middleware lists, joined with spaces, as its
	/// reports name them.
	pub(crate) pattern: Arc<str>,
	paths: MiddlewarePaths,
}

/// The request paths that task middleware runs for.
enum MiddlewarePaths {
	/// Every path, because the middleware lists no patterns.
	Every,
	/// The paths that match one of the middleware's patterns.
	Matching(FlatMatcher<()>),
}

/// One matched view position and the params and splat values of its own pattern.
pub(crate) struct ViewPosition<'plan, State> {
	pub(crate) node: &'plan ViewNode<State>,
	pub(crate) decoded_match: DecodedMatch,
}

/// The target of a request that matches a Vorma pattern.
pub(crate) enum ClassifiedRequest<'plan, State> {
	ApiEndpoint {
		node: &'plan ApiEndpointNode<State>,
		decoded_match: DecodedMatch,
	},
	/// Outermost to innermost.
	View {
		positions: Vec<ViewPosition<'plan, State>>,
	},
}

pub(crate) struct Plan<State> {
	pub(crate) snapshot: Arc<BuildSnapshot>,
	view_nodes: Vec<ViewNode<State>>,
	view_matcher: NestedMatcher<usize>,
	view_by_authored_pattern: HashMap<String, usize>,
	api_endpoint_nodes: Vec<ApiEndpointNode<State>>,
	api_endpoint_matchers: HashMap<Method, FlatMatcher<usize>>,
	api_endpoint_by_method_and_pattern: HashMap<(Method, &'static str), usize>,
	middleware: Vec<MiddlewareNode<State>>,
}

/// The plan for the current build, shared by the handler and the loaded app.
/// In development, a new build gets a new plan when the handler first sees it.
pub(crate) struct CurrentPlan<State> {
	state: Mutex<PlanState<State>>,
}

enum PlanState<State> {
	/// The latest build that a request read has this plan.
	Ready { plan: Arc<Plan<State>> },
	/// The latest build that a request read cannot be served. `plan` is the
	/// plan of the build before it.
	Rejected {
		plan: Arc<Plan<State>>,
		rejection: BuildRejection,
	},
}

/// Why the latest development build cannot be served.
enum BuildRejection {
	/// Its metadata cannot be read.
	Build(Arc<BuildOutputError>),
	/// It cannot form a plan. The snapshot and the error are kept so that
	/// Vorma neither plans nor reports the same snapshot again.
	Plan {
		snapshot: Arc<BuildSnapshot>,
		error: Arc<PlanError>,
	},
}

impl<State> PlanState<State> {
	fn plan(&self) -> &Arc<Plan<State>> {
		match self {
			Self::Ready { plan } | Self::Rejected { plan, .. } => plan,
		}
	}
}

impl<State: Send + Sync + 'static> CurrentPlan<State> {
	pub(crate) fn new(plan: Arc<Plan<State>>) -> Self {
		Self {
			state: Mutex::new(PlanState::Ready { plan }),
		}
	}

	/// The plan of the latest build that a request read, or the reason that
	/// Vorma rejects that build.
	pub(crate) fn read_serving_plan(&self) -> Result<Arc<Plan<State>>, String> {
		match &*self.state.lock().unwrap_or_else(PoisonError::into_inner) {
			PlanState::Ready { plan } => Ok(Arc::clone(plan)),
			PlanState::Rejected {
				rejection: BuildRejection::Build(error),
				..
			} => Err(format!(
				"Vorma rejects the latest development build: {error}"
			)),
			PlanState::Rejected {
				rejection: BuildRejection::Plan { error, .. },
				..
			} => Err(format!(
				"Vorma rejects the latest development build: {error}"
			)),
		}
	}

	/// Records that the latest build's metadata cannot be read.
	pub(crate) fn reject_build(&self, error: Arc<BuildOutputError>) {
		let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
		*state = PlanState::Rejected {
			plan: Arc::clone(state.plan()),
			rejection: BuildRejection::Build(error),
		};
	}

	/// The plan for `snapshot`: the current plan when it was compiled from
	/// that snapshot, and otherwise a new plan, which becomes the current one.
	/// A snapshot that cannot form a plan is reported to the log once, and
	/// later calls with the same snapshot return the same error.
	pub(crate) fn update(
		&self,
		declarations: &Declarations<State>,
		snapshot: Arc<BuildSnapshot>,
	) -> Result<Arc<Plan<State>>, Arc<PlanError>> {
		let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
		if Arc::ptr_eq(&state.plan().snapshot, &snapshot) {
			let plan = Arc::clone(state.plan());
			*state = PlanState::Ready {
				plan: Arc::clone(&plan),
			};
			return Ok(plan);
		}
		if let PlanState::Rejected {
			rejection: BuildRejection::Plan {
				snapshot: failed,
				error,
			},
			..
		} = &*state
			&& Arc::ptr_eq(failed, &snapshot)
		{
			return Err(Arc::clone(error));
		}
		match Plan::compile(declarations, Arc::clone(&snapshot)) {
			Ok(plan) => {
				let plan = Arc::new(plan);
				*state = PlanState::Ready {
					plan: Arc::clone(&plan),
				};
				Ok(plan)
			}
			Err(error) => {
				tracing::error!(error = %error, "Vorma could not plan the new build");
				let error = Arc::new(error);
				*state = PlanState::Rejected {
					plan: Arc::clone(state.plan()),
					rejection: BuildRejection::Plan {
						snapshot,
						error: Arc::clone(&error),
					},
				};
				Err(error)
			}
		}
	}
}

/// The declarations or the build's client views cannot form a plan.
#[derive(Debug)]
pub(crate) struct PlanError {
	pub(crate) problems: Vec<String>,
}

impl fmt::Display for PlanError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("the Vorma app's declarations are invalid:")?;
		for problem in &self.problems {
			write!(formatter, "\n- {problem}")?;
		}
		Ok(())
	}
}

impl std::error::Error for PlanError {}

/// Methods that an endpoint may declare. A `HEAD` request is answered as the
/// `GET` request for its URL.
fn is_api_endpoint_method(method: &Method) -> bool {
	[
		Method::GET,
		Method::POST,
		Method::PUT,
		Method::PATCH,
		Method::DELETE,
		Method::OPTIONS,
	]
	.contains(method)
}

impl<State> Plan<State> {
	pub(crate) fn compile(
		declarations: &Declarations<State>,
		snapshot: Arc<BuildSnapshot>,
	) -> Result<Self, PlanError> {
		let nested = create_nested_language();
		let flat = create_flat_language();
		let mut problems = Vec::new();

		let mut loaders: Vec<(NestedPattern, Arc<ViewLoaderNode<State>>)> = Vec::new();
		for declaration in &declarations.view_loaders {
			let registered = compile_nested_pattern(&nested, declaration.pattern);
			let search = (declaration.derive_search_shape_fn)();
			match (registered, search) {
				(Ok(registered), Ok(search)) => loaders.push((
					registered,
					Arc::new(ViewLoaderNode {
						declaration: declaration.clone(),
						search,
					}),
				)),
				(registered, search) => {
					if let Err(error) = registered {
						problems.push(format!("{}: {error}", declaration.describe()));
					}
					if let Err(error) = search {
						problems.push(format!(
							"{}: its search input {error}",
							declaration.describe()
						));
					}
				}
			}
		}

		let mut endpoints: Vec<(FlatPattern, ApiEndpointNode<State>)> = Vec::new();
		for declaration in &declarations.api_endpoints {
			let describe_declaration = || declaration.describe();
			if !is_api_endpoint_method(&declaration.method) {
				problems.push(format!(
					"{}: {} is not an endpoint method",
					describe_declaration(),
					declaration.method
				));
				continue;
			}
			let input_kind = match (declaration.determine_input_kind_fn)(&declaration.method) {
				Ok(input_kind) => input_kind,
				Err(problem) => {
					problems.push(format!("{}: {problem}", describe_declaration()));
					continue;
				}
			};
			if input_kind != ApiEndpointInputKind::FormData
				&& declaration.form_input_limits.is_some()
			{
				problems.push(format!(
					"{}: set_form_input_limits applies only to an endpoint with FormInput input, and this endpoint has no form input",
					describe_declaration()
				));
			}
			let input = match input_kind {
				ApiEndpointInputKind::None => ApiEndpointInputPlan::None,
				ApiEndpointInputKind::Json => ApiEndpointInputPlan::Json,
				ApiEndpointInputKind::Search => match (declaration.derive_search_shape_fn)() {
					Ok(search) => ApiEndpointInputPlan::Search(search),
					Err(error) => {
						problems.push(format!(
							"{}: its search input {error}",
							describe_declaration()
						));
						continue;
					}
				},
				ApiEndpointInputKind::FormData => match &declaration.form_input_limits {
					Some(limits) => ApiEndpointInputPlan::FormData(limits.clone()),
					None => {
						problems.push(format!(
							"{}: an endpoint with FormInput input must state its form limits with set_form_input_limits",
							describe_declaration()
						));
						continue;
					}
				},
			};
			match compile_flat_pattern(&flat, declaration.pattern) {
				Ok(registered) => endpoints.push((
					registered,
					ApiEndpointNode {
						declaration: declaration.clone(),
						input,
					},
				)),
				Err(error) => problems.push(format!("{}: {error}", describe_declaration())),
			}
		}

		let mut middleware = Vec::new();
		let mut middleware_patterns: Vec<(&TaskMiddleware<State>, &str, FlatPattern)> = Vec::new();
		for declaration in &declarations.task_middlewares {
			let paths = match &declaration.patterns {
				None => Some(MiddlewarePaths::Every),
				Some(patterns) => {
					let mut registered = Vec::new();
					for pattern in patterns {
						match compile_flat_pattern(&flat, pattern) {
							Ok(compiled) => {
								middleware_patterns.push((
									declaration,
									pattern.as_str(),
									compiled.clone(),
								));
								registered.push((pattern.as_str(), compiled));
							}
							Err(error) => problems.push(format!(
								"{}, pattern {pattern:?}: {error}",
								declaration.describe()
							)),
						}
					}
					let matcher = FlatMatcher::new(
						registered
							.iter()
							.map(|(_, compiled)| (compiled.clone(), ())),
					);
					match matcher {
						Ok(matcher) => Some(MiddlewarePaths::Matching(matcher)),
						Err(error) => {
							for duplicate in &error.duplicates {
								problems.push(format!(
									"{}, pattern {:?}: it matches the same paths as the pattern {:?} that the middleware lists before it",
									declaration.describe(),
									registered[duplicate.later_index].0,
									registered[duplicate.earlier_index].0
								));
							}
							None
						}
					}
				}
			};
			match &declaration.methods {
				None => {}
				Some(methods) => {
					for method in methods {
						if *method == Method::HEAD {
							problems.push(format!(
								"{}: a HEAD request selects middleware as the GET request for its URL, so list GET instead of HEAD",
								declaration.describe()
							));
						} else if !is_api_endpoint_method(method) {
							problems.push(format!(
								"{}: Vorma does not handle {method} requests, so middleware never runs for them",
								declaration.describe()
							));
						}
					}
				}
			}
			if let Some(paths) = paths {
				middleware.push(MiddlewareNode {
					pattern: Arc::from(
						declaration
							.patterns
							.as_deref()
							.unwrap_or_default()
							.join(" "),
					),
					declaration: declaration.clone(),
					paths,
				});
			}
		}

		check_duplicate_loaders(&loaders, &mut problems);

		let view_nodes = build_view_nodes(loaders, &snapshot, &nested, &mut problems);
		check_map_search_owners(&view_nodes, &mut problems);
		let view_registrations: Vec<_> = view_nodes
			.iter()
			.map(|node| (node.pattern.as_str(), node.compiled_pattern.as_pattern()))
			.collect();
		for conflict in find_param_conflicts(&view_registrations) {
			problems.push(format!("view patterns: {conflict}"));
		}
		let view_matcher = NestedMatcher::new(
			view_nodes
				.iter()
				.enumerate()
				.map(|(index, node)| (node.compiled_pattern.clone(), index)),
		)
		.map_err(|error| {
			for duplicate in &error.duplicates {
				problems.push(format!(
					"view patterns: {:?} matches the same paths as {:?}",
					view_nodes[duplicate.later_index].pattern,
					view_nodes[duplicate.earlier_index].pattern
				));
			}
		});
		let mut selecting_nothing: Vec<_> = declarations
			.task_middlewares
			.iter()
			.filter(|declaration| {
				declaration.patterns.as_ref().is_some_and(Vec::is_empty)
					|| declaration.methods.as_ref().is_some_and(Vec::is_empty)
			})
			.collect();
		for declaration in &declarations.task_middlewares {
			if selecting_nothing
				.iter()
				.any(|selected| std::ptr::eq(*selected, declaration))
			{
				continue;
			}
			if let Some(methods) = &declaration.methods
				&& !methods.is_empty()
				&& !has_target_for_methods(declaration, &view_nodes, &endpoints)
			{
				let method_names: Vec<_> = methods.iter().map(Method::as_str).collect();
				let pages = if methods.contains(&Method::GET) {
					"and no page exists"
				} else {
					"and pages are selected only by GET"
				};
				problems.push(format!(
					"{}: no API endpoint has one of this middleware's methods ({}) {pages}, so the middleware never runs",
					declaration.describe(),
					method_names.join(", ")
				));
				selecting_nothing.push(declaration);
			}
		}
		for (declaration, authored, filter) in &middleware_patterns {
			// Middleware with methods that select no target is reported once above.
			if selecting_nothing
				.iter()
				.any(|reported| std::ptr::eq(*reported, *declaration))
			{
				continue;
			}
			if !is_filter_applicable(declaration, filter.as_pattern(), &view_nodes, &endpoints) {
				problems.push(format!(
					"{}, pattern {authored:?}: no page and no API endpoint that this middleware's methods select matches any path of this pattern, so the middleware never runs for it",
					declaration.describe()
				));
			}
		}
		let mut registrations_by_method: Vec<(Method, Vec<(FlatPattern, usize)>)> = Vec::new();
		for (index, (registered, node)) in endpoints.iter().enumerate() {
			let method = &node.declaration.method;
			let registration = (registered.clone(), index);
			match registrations_by_method
				.iter_mut()
				.find(|(existing, _)| existing == method)
			{
				Some((_, registrations)) => registrations.push(registration),
				None => registrations_by_method.push((method.clone(), vec![registration])),
			}
		}
		let api_endpoint_matchers: Vec<_> = registrations_by_method
			.into_iter()
			.map(|(method, registrations)| {
				let endpoint_indexes: Vec<usize> =
					registrations.iter().map(|(_, index)| *index).collect();
				let matcher = FlatMatcher::new(registrations).map_err(|error| {
					for duplicate in &error.duplicates {
						let later = &endpoints[endpoint_indexes[duplicate.later_index]].1;
						let earlier = &endpoints[endpoint_indexes[duplicate.earlier_index]].1;
						problems.push(format!(
							"{}: another API endpoint has the same method and the pattern {:?}, which matches the same paths",
							later.declaration.describe(),
							earlier.declaration.pattern
						));
					}
				});
				(method, matcher)
			})
			.collect();
		for (registered, node) in &endpoints {
			if node.declaration.method != Method::GET {
				continue;
			}
			for view in &view_nodes {
				let comparison = || PatternComparison {
					left: registered.as_pattern(),
					right: view.compiled_pattern.as_pattern(),
				};
				if patterns_can_match_one_path(comparison())
					&& compare_matching_patterns(comparison()) == Ordering::Equal
				{
					problems.push(format!(
						"{}: this endpoint and the view pattern {:?} can match the same path, and neither pattern is more specific than the other",
						node.declaration.describe(),
						view.pattern
					));
				}
			}
		}
		if !problems.is_empty() {
			return Err(PlanError { problems });
		}

		let Ok(view_matcher) = view_matcher else {
			unreachable!("a view matcher that cannot be built adds a problem");
		};
		let api_endpoint_matchers = api_endpoint_matchers
			.into_iter()
			.map(|(method, matcher)| {
				let Ok(matcher) = matcher else {
					unreachable!("an endpoint matcher that cannot be built adds a problem");
				};
				(method, matcher)
			})
			.collect();
		let api_endpoint_nodes: Vec<_> = endpoints.into_iter().map(|(_, node)| node).collect();
		Ok(Self {
			snapshot,
			view_by_authored_pattern: view_nodes
				.iter()
				.enumerate()
				.map(|(index, node)| (node.pattern.clone(), index))
				.collect(),
			view_nodes,
			view_matcher,
			api_endpoint_by_method_and_pattern: api_endpoint_nodes
				.iter()
				.enumerate()
				.map(|(index, node)| {
					(
						(node.declaration.method.clone(), node.declaration.pattern),
						index,
					)
				})
				.collect(),
			api_endpoint_nodes,
			api_endpoint_matchers,
			middleware,
		})
	}

	/// A request belongs to the endpoint with the method and pattern that match it. A
	/// `GET` request can also belong to a view pattern; when both match, the more
	/// specific pattern wins, by [`compare_matching_patterns`]. Plan
	/// compilation rejects a `GET` endpoint and a view pattern that can match
	/// one path and that are equally specific, so no tie occurs.
	/// `None` for a request that matches no pattern.
	pub(crate) fn classify(
		&self,
		method: &Method,
		path: &str,
	) -> Result<Option<ClassifiedRequest<'_, State>>, PathWireError> {
		let endpoint = self
			.api_endpoint_matchers
			.get(method)
			.and_then(|matcher| matcher.find_best_match(path));
		let view = if method == Method::GET {
			self.view_matcher.find_nested_matches(path)
		} else {
			None
		};
		if let Some(endpoint) = endpoint {
			let wins = view
				.as_ref()
				.and_then(|matches| matches.last())
				.is_none_or(|leaf| {
					compare_matching_patterns(PatternComparison {
						left: endpoint.pattern,
						right: leaf.pattern,
					}) != Ordering::Greater
				});
			if wins {
				return Ok(Some(ClassifiedRequest::ApiEndpoint {
					node: &self.api_endpoint_nodes[*endpoint.value],
					decoded_match: decode_match(&endpoint.params, &endpoint.splat_values)?,
				}));
			}
		}
		let Some(matches) = view else {
			return Ok(None);
		};
		let mut positions = Vec::with_capacity(matches.len());
		for matched in matches {
			positions.push(ViewPosition {
				node: &self.view_nodes[*matched.value],
				decoded_match: decode_match(&matched.params, &matched.splat_values)?,
			});
		}
		Ok(Some(ClassifiedRequest::View { positions }))
	}

	/// The task middleware for a request, in declaration order. A request must
	/// match a listed pattern and a listed method, and middleware without a
	/// list of patterns or of methods runs for every path or every method.
	pub(crate) fn select_middleware(
		&self,
		method: &Method,
		path: &str,
	) -> Vec<&MiddlewareNode<State>> {
		self.middleware
			.iter()
			.filter(|node| {
				node.declaration.selects_method(method)
					&& match &node.paths {
						MiddlewarePaths::Every => true,
						MiddlewarePaths::Matching(matcher) => {
							matcher.find_best_match(path).is_some()
						}
					}
			})
			.collect()
	}

	/// The endpoint with this method and the pattern as the application wrote
	/// it.
	pub(crate) fn find_api_endpoint_node(
		&self,
		method: &Method,
		pattern: &'static str,
	) -> Option<&ApiEndpointNode<State>> {
		self.api_endpoint_by_method_and_pattern
			.get(&(method.clone(), pattern))
			.map(|index| &self.api_endpoint_nodes[*index])
	}

	/// A view position by the pattern that an application names: the position's
	/// index position first, then the position itself.
	pub(crate) fn find_view_node(&self, pattern: &str) -> Option<&ViewNode<State>> {
		let index_pattern = format!(
			"{}/{INDEX_SEGMENT}",
			if pattern == "/" { "" } else { pattern }
		);
		self.view_by_authored_pattern
			.get(&index_pattern)
			.or_else(|| self.view_by_authored_pattern.get(pattern))
			.map(|index| &self.view_nodes[*index])
	}
}

/// Whether a declared target has a method that the middleware selects: an
/// endpoint with one of its methods or, when the methods include `GET`, a
/// page. Middleware without a list of methods selects every method.
fn has_target_for_methods<State>(
	middleware: &TaskMiddleware<State>,
	view_nodes: &[ViewNode<State>],
	endpoints: &[(FlatPattern, ApiEndpointNode<State>)],
) -> bool {
	(middleware.selects_method(&Method::GET) && !view_nodes.is_empty())
		|| endpoints
			.iter()
			.any(|(_, node)| middleware.selects_method(&node.declaration.method))
}

/// Whether a request that reaches the middleware can match `filter`. Every
/// such request matches a view position, for a `GET` request, or an endpoint
/// pattern with the request's method. The check holds when the filter and
/// one of those patterns can match one path.
fn is_filter_applicable<State>(
	middleware: &TaskMiddleware<State>,
	filter: &Pattern,
	view_nodes: &[ViewNode<State>],
	endpoints: &[(FlatPattern, ApiEndpointNode<State>)],
) -> bool {
	let selects = |method: &Method| middleware.selects_method(method);
	let overlaps = |target: &Pattern| {
		patterns_can_match_one_path(PatternComparison {
			left: filter,
			right: target,
		})
	};
	(selects(&Method::GET)
		&& view_nodes
			.iter()
			.any(|node| overlaps(node.compiled_pattern.as_pattern())))
		|| endpoints.iter().any(|(registered, node)| {
			selects(&node.declaration.method) && overlaps(registered.as_pattern())
		})
}

fn decode_match(
	params: &std::collections::BTreeMap<&str, &str>,
	splat_values: &[&str],
) -> Result<DecodedMatch, PathWireError> {
	let mut decoded_params = Vec::with_capacity(params.len());
	for (name, value) in params {
		decoded_params.push(((*name).to_owned(), decode_path_segment(value)?));
	}
	let mut decoded_splat = Vec::with_capacity(splat_values.len());
	for value in splat_values {
		decoded_splat.push(decode_path_segment(value)?);
	}
	Ok(DecodedMatch::new(decoded_params, decoded_splat))
}

/// Reports each loader with the registered pattern of an earlier loader.
/// The nested matcher cannot report them, because the view positions merge
/// loaders by registered pattern before Vorma builds the matcher.
fn check_duplicate_loaders<State>(
	loaders: &[(NestedPattern, Arc<ViewLoaderNode<State>>)],
	problems: &mut Vec<String>,
) {
	let mut seen = std::collections::HashSet::new();
	for (registered, node) in loaders {
		if !seen.insert(registered.as_pattern().as_str()) {
			problems.push(format!(
				"{}: another view loader has the same pattern",
				node.declaration.describe()
			));
		}
	}
}

/// View positions from the loaders and the build's client views, keyed by
/// registered pattern in first-registration order, with each position's
/// search owners. A client view joins the loader with its registered
/// pattern. Two client views with one registered pattern are a problem.
fn build_view_nodes<State>(
	loaders: Vec<(NestedPattern, Arc<ViewLoaderNode<State>>)>,
	snapshot: &BuildSnapshot,
	nested: &NestedPatternLanguage,
	problems: &mut Vec<String>,
) -> Vec<ViewNode<State>> {
	let mut nodes: Vec<ViewNode<State>> = Vec::new();
	let mut by_registered: HashMap<String, usize> = HashMap::new();
	for (registered, loader) in loaders {
		// `check_duplicate_loaders` reports a second loader of a pattern. The
		// first loader makes the pattern's position.
		if by_registered.contains_key(registered.as_pattern().as_str()) {
			continue;
		}
		by_registered.insert(registered.as_pattern().as_str().to_owned(), nodes.len());
		nodes.push(ViewNode {
			pattern: loader.declaration.pattern.to_owned(),
			compiled_pattern: registered,
			kind: ViewKind::Loader(loader),
			search_owners: Vec::new(),
		});
	}
	for (pattern, module) in &snapshot.metadata.client_views {
		let registered = match compile_nested_pattern(nested, pattern) {
			Ok(registered) => registered,
			Err(error) => {
				problems.push(format!("the build's client view {pattern:?}: {error}"));
				continue;
			}
		};
		let Some(&index) = by_registered.get(registered.as_pattern().as_str()) else {
			by_registered.insert(registered.as_pattern().as_str().to_owned(), nodes.len());
			nodes.push(ViewNode {
				pattern: pattern.clone(),
				compiled_pattern: registered,
				kind: ViewKind::ClientView(module.clone()),
				search_owners: Vec::new(),
			});
			continue;
		};
		let node = &mut nodes[index];
		match &node.kind {
			ViewKind::Loader(loader) => {
				node.kind = ViewKind::LoaderAndClientView {
					loader: Arc::clone(loader),
					client_view_module: module.clone(),
				};
				node.pattern = pattern.clone();
			}
			ViewKind::ClientView(_) | ViewKind::LoaderAndClientView { .. } => {
				problems.push(format!(
					"the build's client view {pattern:?} has the registered pattern {:?} of the client view {:?}, so both match the same paths",
					registered.as_pattern().as_str(),
					node.pattern
				));
			}
		}
	}
	link_search_owners(&mut nodes);
	nodes
}

/// Reports each view loader with a map as its search input that shares a page
/// with another view loader that reads search keys. A map reads every key of a
/// query, so it would read the other loader's keys too, and no TypeScript type
/// describes the search inputs that both loaders accept. A map in a named
/// field of a struct reads only the keys below its own name.
fn check_map_search_owners<State>(nodes: &[ViewNode<State>], problems: &mut Vec<String>) {
	let mut reported = std::collections::HashSet::new();
	for node in nodes {
		for (index, map_owner) in node.search_owners.iter().enumerate() {
			if !matches!(map_owner.search, Some(SearchShape::Record { .. })) {
				continue;
			}
			for (other_index, other) in node.search_owners.iter().enumerate() {
				let reads_search_keys = match &other.search {
					// The earlier map of two maps reports the pair.
					Some(SearchShape::Record { .. }) => other_index > index,
					Some(SearchShape::Object { fields, .. }) => !fields.is_empty(),
					Some(SearchShape::Scalar(_) | SearchShape::Array { .. }) | None => false,
				};
				if other_index == index
					|| !reads_search_keys
					|| !reported.insert((Arc::as_ptr(map_owner), Arc::as_ptr(other)))
				{
					continue;
				}
				problems.push(format!(
					"{}: its search input is a map, which reads every key of a query, and the view loader {:?} reads search input for the same page {:?}; make the map a named field of a struct, so that each loader reads only its own keys",
					map_owner.declaration.describe(),
					other.declaration.pattern,
					node.pattern
				));
			}
		}
	}
}

/// A position's branch identity: its consuming segments, which exclude index
/// and splat segments.
fn branch_key(segments: &[Segment]) -> Vec<Segment> {
	segments
		.iter()
		.filter(|segment| !matches!(segment, Segment::Index | Segment::Splat))
		.cloned()
		.collect()
}

/// Each position's outer-to-inner chain of registered positions, and from it
/// the loaders that own search fields.
fn link_search_owners<State>(nodes: &mut [ViewNode<State>]) {
	let mut branches: HashMap<Vec<Segment>, usize> = HashMap::new();
	for (index, node) in nodes.iter().enumerate() {
		if !matches!(
			node.compiled_pattern.as_pattern().segments().last(),
			Some(Segment::Index | Segment::Splat)
		) {
			branches.insert(
				node.compiled_pattern.as_pattern().segments().to_vec(),
				index,
			);
		}
	}
	let owners: Vec<Vec<Arc<ViewLoaderNode<State>>>> = nodes
		.iter()
		.enumerate()
		.map(|(index, target)| {
			let consuming = branch_key(target.compiled_pattern.as_pattern().segments());
			let mut chain = Vec::new();
			for depth in 0..=consuming.len() {
				if let Some(&branch) = branches.get(&consuming[..depth]) {
					chain.push(branch);
				}
			}
			if matches!(
				target.compiled_pattern.as_pattern().segments().last(),
				Some(Segment::Index | Segment::Splat)
			) {
				chain.push(index);
			}
			chain
				.into_iter()
				.filter_map(|position| {
					nodes[position]
						.kind
						.loader()
						.filter(|loader| loader.search.is_some())
						.map(Arc::clone)
				})
				.collect()
		})
		.collect();
	for (node, owners) in nodes.iter_mut().zip(owners) {
		node.search_owners = owners;
	}
}

/// View patterns must use one parameter name at each shared dynamic
/// position. Vorma and its build find the views at and above a view pattern,
/// which contribute search input and render the parent views, by comparing
/// pattern segments with their parameter names, while the nested matcher
/// ignores parameter names, so a second name at a shared position would make
/// the two disagree. Each conflict names the pattern that arrived second.
fn find_param_conflicts(registrations: &[(&str, &Pattern)]) -> Vec<String> {
	#[derive(Default)]
	struct Branch {
		static_children: HashMap<String, Branch>,
		dynamic: Option<(String, Box<Branch>)>,
	}
	let mut root = Branch::default();
	let mut seen = std::collections::HashSet::new();
	let mut conflicts = Vec::new();
	for (authored, pattern) in registrations {
		if !seen.insert(pattern.as_str().to_owned()) {
			continue;
		}
		let mut branch = &mut root;
		for segment in pattern.segments() {
			match segment {
				Segment::Splat | Segment::Index => break,
				Segment::Static { value } => {
					branch = branch.static_children.entry(value.clone()).or_default();
				}
				Segment::Dynamic { param_name } => {
					let dynamic = branch
						.dynamic
						.get_or_insert_with(|| (param_name.clone(), Box::default()));
					if dynamic.0 != *param_name {
						conflicts.push(format!(
							"{authored:?} names the dynamic segment {param_name:?} where another pattern names it {:?}",
							dynamic.0
						));
						break;
					}
					branch = &mut dynamic.1;
				}
			}
		}
	}
	conflicts
}
