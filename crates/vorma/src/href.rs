//! Page and endpoint targets resolved to hrefs. A page target names one of
//! the application's view patterns; each loader at or above the pattern that
//! owns search fields must accept its own fields, every entry of the href's
//! query must reach a loader's input, and the href carries the fields as the
//! input supplied them. An endpoint target names an endpoint
//! pattern; the redirect's follow-up request must reach an endpoint with the
//! redirecting endpoint's types.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use http::Method;
use serde_json::{Map, Value};
use vorma_path_wire::{
	DYNAMIC_PARAM_PREFIX, INDEX_SEGMENT, SPLAT_SEGMENT, canonicalize_pattern, encode_path_segment,
};

use crate::outcome::{RedirectingApiEndpoint, ResolveTarget};
use crate::plan::{ApiEndpointNode, ClassifiedRequest, CurrentPlan, Plan, ViewNode};
use crate::result::{ApiEndpointTarget, PageTarget, RedirectStatus};
use crate::search::{
	DEPLOYMENT_QUERY_KEY, DeploymentQueryKey, PermittedStrings, QueryIndex, SearchShape,
	VIEW_PAYLOAD_QUERY_KEY, serialize_search_params,
};

/// Turns page targets into hrefs. Every handler context has one as
/// `page_hrefs`, and [`LoadedApp::page_hrefs`](crate::LoadedApp::page_hrefs)
/// returns one for code outside a request, such as host routes and background
/// jobs. Canonical links, `og:url` and hreflang alternates, pagination links,
/// and links in email can use it instead of formatting URLs by hand.
///
/// A [`Redirect`](crate::Redirect) to a [`PageTarget`] and
/// [`PageHrefs::to_href`] resolve the target by the same rules.
#[derive(Clone)]
pub struct PageHrefs {
	resolver: Arc<dyn ResolveTarget>,
}

impl PageHrefs {
	pub(crate) fn new(resolver: Arc<dyn ResolveTarget>) -> Self {
		Self { resolver }
	}

	/// Creates a `PageHrefs` that resolves every page target with
	/// `resolve_page_target_fn`, which returns the href or the reason that the
	/// target is invalid. A test uses it to build a handler context without a
	/// loaded app.
	pub fn from_resolver(
		resolve_page_target_fn: impl Fn(&PageTarget) -> Result<String, String> + Send + Sync + 'static,
	) -> Self {
		Self::new(Arc::new(FunctionResolver(resolve_page_target_fn)))
	}

	/// Returns the page's path, search, and hash as a relative href.
	///
	/// The target's pattern must be a page in the current build, and the
	/// search input must fit the input of every loader at or above the
	/// pattern. Each loader reads its own part of the search input: the
	/// fields of its input struct, or every key of the search input when its
	/// input is a map. Every key of the search input must belong to one of
	/// those loaders, and each loader must accept what it parses from the
	/// href's query, as it does for a request: a map of strings reads the
	/// number `2` as the text `"2"`. Every entry of the href's query must
	/// reach the input of one of those loaders: a string field reads only the
	/// first value of `y=first&y=second`, and a number field named `page`
	/// reads no key below `page`, such as `page.x`, so search input that
	/// spells such entries is invalid. An array in the search input can hold
	/// only scalars and null, because the href repeats the array's key for each
	/// item and has no key for an item's own items or fields. The search input
	/// cannot have the key `vorma-json`, and, while Vercel skew protection is
	/// on, the key `dpl`, because the server removes these keys from a page's
	/// query. Path segments and search parameters get Vorma's encoding, and
	/// the hash gets the URL standard's fragment encoding. The path that the
	/// target builds must reach the target's own page: when another page
	/// pattern or a `GET` API endpoint is more specific for that path, such as
	/// `/users/new` for the target `/users/:id` with `id` set to `new` when
	/// `/users/new` is also a page, the target is invalid. A target that does
	/// not meet these conditions returns an [`HrefError`].
	///
	/// The current build is the build that the request used, for a handler
	/// context, and the build that Vorma most recently planned, for
	/// [`LoadedApp::page_hrefs`](crate::LoadedApp::page_hrefs). A redirect
	/// resolves its target against the build that the request used. In
	/// development, while Vorma rejects the latest build because its metadata
	/// cannot be read or its client views and the declarations do not form a
	/// valid request plan, every target of
	/// [`LoadedApp::page_hrefs`](crate::LoadedApp::page_hrefs) returns an
	/// [`HrefError`].
	pub fn to_href(&self, target: &PageTarget) -> Result<String, HrefError> {
		self.resolver
			.resolve_page_target(target)
			.map_err(|problem| HrefError {
				message: format!("the page target {:?} is invalid: {problem}", target.pattern),
			})
	}
}

/// A page-target resolver from a function. It resolves no endpoint target.
struct FunctionResolver<Resolve>(Resolve);

impl<Resolve: Fn(&PageTarget) -> Result<String, String> + Send + Sync> ResolveTarget
	for FunctionResolver<Resolve>
{
	fn resolve_page_target(&self, target: &PageTarget) -> Result<String, String> {
		(self.0)(target)
	}

	fn resolve_api_endpoint_target(
		&self,
		_redirecting: &RedirectingApiEndpoint,
		_target: &ApiEndpointTarget,
		_status: RedirectStatus,
	) -> Result<String, String> {
		Err("a PageHrefs made from a function resolves page targets only".to_owned())
	}
}

impl fmt::Debug for PageHrefs {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.debug_struct("PageHrefs").finish_non_exhaustive()
	}
}

/// A [`PageTarget`] that cannot become an href: its pattern is not a page in
/// the current build, a parameter is missing, the search input does not fit
/// the loaders at or above the pattern, has a key that none of them accepts,
/// spells a query entry that none of them receives, or holds an array or an
/// object inside an array, or the path that the target builds belongs to a
/// different page or to a `GET` API endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HrefError {
	message: String,
}

impl fmt::Display for HrefError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(&self.message)
	}
}

impl std::error::Error for HrefError {}

/// Resolves targets outside a request with the plan of the latest build that
/// a request read.
pub(crate) struct CurrentPlanTargetResolver<State> {
	pub(crate) plan: Arc<CurrentPlan<State>>,
	pub(crate) deployment_query_key: DeploymentQueryKey,
}

impl<State: Send + Sync + 'static> ResolveTarget for CurrentPlanTargetResolver<State> {
	fn resolve_page_target(&self, target: &PageTarget) -> Result<String, String> {
		self.plan
			.read_serving_plan()?
			.resolve_page_target(target, self.deployment_query_key)
	}

	fn resolve_api_endpoint_target(
		&self,
		redirecting: &RedirectingApiEndpoint,
		target: &ApiEndpointTarget,
		status: RedirectStatus,
	) -> Result<String, String> {
		self.plan.read_serving_plan()?.resolve_api_endpoint_target(
			redirecting,
			target,
			status,
			self.deployment_query_key,
		)
	}
}

/// Resolves the targets of one request with the plan that the request uses.
pub(crate) struct PlanTargetResolver<State> {
	pub(crate) plan: Arc<Plan<State>>,
	pub(crate) deployment_query_key: DeploymentQueryKey,
}

impl<State: Send + Sync + 'static> ResolveTarget for PlanTargetResolver<State> {
	fn resolve_page_target(&self, target: &PageTarget) -> Result<String, String> {
		self.plan
			.resolve_page_target(target, self.deployment_query_key)
	}

	fn resolve_api_endpoint_target(
		&self,
		redirecting: &RedirectingApiEndpoint,
		target: &ApiEndpointTarget,
		status: RedirectStatus,
	) -> Result<String, String> {
		self.plan.resolve_api_endpoint_target(
			redirecting,
			target,
			status,
			self.deployment_query_key,
		)
	}
}

impl<State> Plan<State> {
	/// The href of a page target. While skew protection reserves the query
	/// key `dpl`, the target's search input cannot use it, because the server
	/// removes it from a page's query.
	fn resolve_page_target(
		&self,
		target: &PageTarget,
		deployment_query_key: DeploymentQueryKey,
	) -> Result<String, String> {
		let node = self
			.find_view_node(target.pattern)
			.ok_or_else(|| "no view loader or view declares this pattern".to_owned())?;
		let search_entries = resolve_search(node, target, deployment_query_key)?;
		let parts = HrefParts {
			kind: PatternKind::View,
			pattern: &node.pattern,
			params: &target.params,
			splat_values: &target.splat_values,
			search_entries,
			hash: target.hash.as_deref(),
		};
		let mut href = build_path(&parts)?;
		self.check_path_reaches_view(&href, node)?;
		append_search_and_hash(&mut href, parts);
		Ok(href)
	}

	/// The href of an endpoint target. While skew protection reserves the query
	/// key `dpl`, the target's search input cannot use it, because Vercel reads
	/// it to choose the deployment that serves the follow-up request.
	fn resolve_api_endpoint_target(
		&self,
		redirecting: &RedirectingApiEndpoint,
		target: &ApiEndpointTarget,
		status: RedirectStatus,
		deployment_query_key: DeploymentQueryKey,
	) -> Result<String, String> {
		let follow_up = follow_up_method(status, &redirecting.method);
		let node = self
			.find_api_endpoint_node(&follow_up, target.pattern)
			.ok_or_else(|| {
				format!(
					"the follow-up request uses {follow_up}, and no {follow_up} API endpoint declares this pattern"
				)
			})?;
		let types = &node.declaration.types;
		if types.output != redirecting.types.output {
			return Err(
				"its output type differs from the redirecting endpoint's output type".to_owned(),
			);
		}
		if types.declared_error != redirecting.types.declared_error {
			return Err(
				"its declared error type differs from the redirecting endpoint's declared error type"
					.to_owned(),
			);
		}
		let search_entries = if follow_up == redirecting.method && follow_up != Method::GET {
			if types.input != redirecting.types.input {
				return Err(format!(
					"the follow-up {follow_up} request carries this request's body, and its input type differs from the redirecting endpoint's input type"
				));
			}
			if target.input.is_some() {
				return Err(format!(
					"the follow-up {follow_up} request carries this request's body, so the target takes no search input"
				));
			}
			Vec::new()
		} else {
			resolve_api_endpoint_search(node, target, deployment_query_key)?
		};
		let parts = HrefParts {
			kind: PatternKind::ApiEndpoint,
			pattern: node.declaration.pattern,
			params: &target.params,
			splat_values: &target.splat_values,
			search_entries,
			hash: None,
		};
		let mut href = build_path(&parts)?;
		self.check_path_reaches_api_endpoint(&follow_up, &href, node)?;
		append_search_and_hash(&mut href, parts);
		Ok(href)
	}
}

impl<State> Plan<State> {
	/// Checks that a request with the follow-up method for `path` reaches
	/// `node`. A path that the target's params and splat values build can
	/// belong to a more specific endpoint pattern, such as `/api/files/recent`
	/// for the target `/api/files/*` with the splat value `recent` when
	/// `/api/files/recent` is also an endpoint with that method, or, for a
	/// `GET` request, to a more specific view pattern.
	fn check_path_reaches_api_endpoint(
		&self,
		method: &Method,
		path: &str,
		node: &ApiEndpointNode<State>,
	) -> Result<(), String> {
		match self.classify(method, path) {
			Ok(Some(ClassifiedRequest::ApiEndpoint { node: reached, .. }))
				if std::ptr::eq(reached, node) =>
			{
				Ok(())
			}
			Ok(Some(ClassifiedRequest::ApiEndpoint { node: reached, .. })) => Err(format!(
				"the path {path:?} belongs to the {method} API endpoint {:?}",
				reached.declaration.pattern
			)),
			Ok(Some(ClassifiedRequest::View { positions })) => Err(format!(
				"the path {path:?} belongs to the view pattern {:?}",
				positions
					.last()
					.map_or("", |leaf| leaf.node.pattern.as_str())
			)),
			Ok(None) => Err(format!(
				"the path {path:?} matches no {method} API endpoint"
			)),
			Err(error) => Err(format!("the path {path:?} is not a valid path: {error}")),
		}
	}

	/// Checks that a `GET` request for `path` reaches `node`. A path that
	/// the target's params and splat values build can belong to a more specific pattern, such
	/// as `/users/new` for the target `/users/:id` with `id` set to `new`
	/// when `/users/new` is also a view pattern, or to a `GET` endpoint.
	fn check_path_reaches_view(&self, path: &str, node: &ViewNode<State>) -> Result<(), String> {
		match self.classify(&Method::GET, path) {
			Ok(Some(ClassifiedRequest::View { positions }))
				if positions
					.last()
					.is_some_and(|leaf| std::ptr::eq(leaf.node, node)) =>
			{
				Ok(())
			}
			Ok(Some(ClassifiedRequest::View { positions })) => Err(format!(
				"the path {path:?} belongs to the view pattern {:?}",
				positions
					.last()
					.map_or("", |leaf| leaf.node.pattern.as_str())
			)),
			Ok(Some(ClassifiedRequest::ApiEndpoint { node, .. })) => Err(format!(
				"the path {path:?} belongs to the GET API endpoint {:?}",
				node.declaration.pattern
			)),
			Ok(None) => Err(format!("the path {path:?} matches no view pattern")),
			Err(error) => Err(format!("the path {path:?} is not a valid path: {error}")),
		}
	}
}

/// The method of the request that follows a redirect, as the Fetch standard
/// defines it: 303 turns any method except `HEAD` into `GET`, 302 turns `POST`
/// into `GET`, and 307 and 308 keep the method. An endpoint's declared method
/// is never `HEAD` because the `GET` endpoint answers `HEAD` requests.
fn follow_up_method(status: RedirectStatus, method: &Method) -> Method {
	match status {
		RedirectStatus::SeeOther => Method::GET,
		RedirectStatus::Found if method == Method::POST => Method::GET,
		RedirectStatus::Found
		| RedirectStatus::TemporaryRedirect
		| RedirectStatus::PermanentRedirect => method.clone(),
	}
}

/// The query entries of a follow-up `GET` request's search input, which must
/// fit the target endpoint's input type. The endpoint must receive every
/// entry in its input.
fn resolve_api_endpoint_search<State>(
	node: &ApiEndpointNode<State>,
	target: &ApiEndpointTarget,
	deployment_query_key: DeploymentQueryKey,
) -> Result<Vec<(String, String)>, String> {
	let input = match &target.input {
		None => None,
		Some(Ok(input)) => Some(input),
		Some(Err(error)) => return Err(format!("the search input cannot be serialized: {error}")),
	};
	let Some(shape) = node.search_shape() else {
		return match input {
			None => Ok(Vec::new()),
			Some(_) => Err("the endpoint takes no search input".to_owned()),
		};
	};
	let empty = Map::new();
	let object = match input {
		// An omitted search is an empty one; the input type decides whether
		// that fits it.
		None => &empty,
		Some(Value::Object(object)) => object,
		Some(_) => return Err("the search input is not an object".to_owned()),
	};
	check_search_keys(
		object,
		[shape],
		SearchKeyRules {
			kind: PatternKind::ApiEndpoint,
			deployment_query_key,
			unread_by: "the endpoint's input type does not read",
		},
	)?;
	let entries = spell_search(&Value::Object(project_search(shape, object)))?;
	let query = QueryIndex::new(&entries);
	let parsed = query
		.parse_search(shape)
		.ok()
		.filter(|value| (node.declaration.is_search_input_valid_fn)(value.clone()))
		.ok_or_else(|| "the search input does not fit the endpoint's input type".to_owned())?;
	check_entries_received(
		&entries,
		&[spell_search(&parsed)?],
		"the endpoint does not receive in its input",
	)?;
	Ok(entries)
}

/// How many times each entry appears.
fn count_entries(entries: &[(String, String)]) -> HashMap<&(String, String), usize> {
	let mut counts = HashMap::new();
	for entry in entries {
		*counts.entry(entry).or_insert(0) += 1;
	}
	counts
}

/// The query entries that a search value spells. An empty object spells no
/// entry because an href with an empty search has no query. A value that the
/// grammar cannot spell, an array or an object inside an array, is an error
/// that names the array's key.
fn spell_search(search: &Value) -> Result<Vec<(String, String)>, String> {
	match search {
		Value::Object(fields) if fields.is_empty() => Ok(Vec::new()),
		search => serialize_search_params(search)
			.map_err(|error| format!("the search input has no query spelling: {error}")),
	}
}

/// Fails when the owners of a search do not receive every entry of the query
/// that an href spells, and names every such entry. `received` holds the
/// entries that each owner's parsed input spells, so an owner receives an
/// entry when its input spells the entry at least as many times as the query
/// does. An owner can parse a query without receiving every entry: a string
/// field reads only the first of a key's values, and a scalar field reads
/// no keys below its own key.
fn check_entries_received(
	entries: &[(String, String)],
	received: &[Vec<(String, String)>],
	missed_by: &str,
) -> Result<(), String> {
	let wanted = count_entries(entries);
	let received: Vec<HashMap<&(String, String), usize>> = received
		.iter()
		.map(|entries| count_entries(entries))
		.collect();
	let mut checked = HashSet::new();
	let mut missed = Vec::new();
	for entry in entries {
		if !checked.insert(entry) {
			continue;
		}
		let is_received = received
			.iter()
			.any(|counts| counts.get(entry).copied().unwrap_or(0) >= wanted[entry]);
		if !is_received {
			missed.push(entry);
		}
	}
	if missed.is_empty() {
		return Ok(());
	}
	Err(format!(
		"the href's query has entries that {missed_by}: {}",
		missed
			.iter()
			.map(|(key, value)| format!("{key:?} = {value:?}"))
			.collect::<Vec<_>>()
			.join(", ")
	))
}

/// The part of a search input that a search shape reads: the fields of an
/// object shape, and every key for a record shape, which reads every key
/// of the query.
fn project_search(shape: &SearchShape, object: &Map<String, Value>) -> Map<String, Value> {
	match shape {
		SearchShape::Object { fields, .. } => fields
			.iter()
			.filter_map(|field| {
				object
					.get(&field.name)
					.map(|value| (field.name.clone(), value.clone()))
			})
			.collect(),
		SearchShape::Record { .. } => object.clone(),
		SearchShape::Scalar(_) | SearchShape::Array { .. } => Map::new(),
	}
}

/// Whether a search shape reads the top-level key `key`.
fn accepts_search_key(shape: &SearchShape, key: &str) -> bool {
	match shape {
		SearchShape::Object { fields, .. } => fields.iter().any(|field| field.name == key),
		SearchShape::Record { permitted_keys, .. } => match permitted_keys {
			PermittedStrings::AnyString => true,
			PermittedStrings::OneOf(keys) => keys.iter().any(|permitted| permitted == key),
		},
		SearchShape::Scalar(_) | SearchShape::Array { .. } => false,
	}
}

/// What a target's search input can use as its top-level keys.
struct SearchKeyRules<'a> {
	/// Whether the target is a page or an endpoint.
	kind: PatternKind,
	deployment_query_key: DeploymentQueryKey,
	/// The end of the message for keys that no shape reads.
	unread_by: &'a str,
}

/// Why a target's search input cannot use the top-level key `key`, or `None`
/// when it can.
fn find_reserved_key_reason(
	kind: PatternKind,
	deployment_query_key: DeploymentQueryKey,
	key: &str,
) -> Option<&'static str> {
	match (kind, deployment_query_key, key) {
		(PatternKind::View, _, VIEW_PAYLOAD_QUERY_KEY) => Some(
			"which the server removes from a page's query because Vorma's wire protocol uses it",
		),
		(PatternKind::ApiEndpoint, _, VIEW_PAYLOAD_QUERY_KEY) => {
			Some("which Vorma's wire protocol reserves")
		}
		(PatternKind::View, DeploymentQueryKey::Reserved, DEPLOYMENT_QUERY_KEY) => {
			Some("which the server removes from a page's query while Vercel skew protection is on")
		}
		(PatternKind::ApiEndpoint, DeploymentQueryKey::Reserved, DEPLOYMENT_QUERY_KEY) => Some(
			"which Vercel skew protection reads to choose the deployment that serves the request",
		),
		_ => None,
	}
}

/// Fails for a search input with a key that the transport reserves or with
/// keys that none of the shapes reads, and names every such key.
fn check_search_keys<'a>(
	object: &Map<String, Value>,
	shapes: impl IntoIterator<Item = &'a SearchShape> + Clone,
	rules: SearchKeyRules<'_>,
) -> Result<(), String> {
	let reserved = object.keys().find_map(|key| {
		find_reserved_key_reason(rules.kind, rules.deployment_query_key, key)
			.map(|reason| (key, reason))
	});
	if let Some((key, reason)) = reserved {
		return Err(format!("the search input has the key {key:?}, {reason}"));
	}
	let unread: Vec<&str> = object
		.keys()
		.filter(|key| {
			!shapes
				.clone()
				.into_iter()
				.any(|shape| accepts_search_key(shape, key))
		})
		.map(String::as_str)
		.collect();
	if unread.is_empty() {
		return Ok(());
	}
	Err(format!(
		"the search input has keys that {}: {}",
		rules.unread_by,
		unread
			.iter()
			.map(|key| format!("{key:?}"))
			.collect::<Vec<_>>()
			.join(", ")
	))
}

/// The query entries that spell the search parts that the pattern's owners
/// read. Each owner must accept the value that it parses from the query, as
/// the loaders of a request for the href parse it, and every entry of the
/// query must reach an owner's input.
fn resolve_search<State>(
	node: &ViewNode<State>,
	target: &PageTarget,
	deployment_query_key: DeploymentQueryKey,
) -> Result<Vec<(String, String)>, String> {
	let input = match &target.input {
		None => None,
		Some(Ok(input)) => Some(input),
		Some(Err(error)) => return Err(format!("the search input cannot be serialized: {error}")),
	};
	if node.search_owners.is_empty() {
		return match input {
			None => Ok(Vec::new()),
			Some(_) => Err("no loader at or above this pattern declares search input".to_owned()),
		};
	}
	let empty = Map::new();
	let object = match input {
		// An omitted search is an empty one; each owner decides whether that
		// satisfies it.
		None => &empty,
		Some(Value::Object(object)) => object,
		Some(_) => return Err("the search input is not an object".to_owned()),
	};
	let owners = || {
		node.search_owners
			.iter()
			.filter_map(|owner| Some((owner, owner.search.as_ref()?)))
	};
	check_search_keys(
		object,
		owners().map(|(_, shape)| shape),
		SearchKeyRules {
			kind: PatternKind::View,
			deployment_query_key,
			unread_by: "no loader at or above this pattern reads",
		},
	)?;
	let mut search = Map::new();
	for (_, shape) in owners() {
		search.extend(project_search(shape, object));
	}
	let entries = spell_search(&Value::Object(search))?;
	let query = QueryIndex::new(&entries);
	let mut received = Vec::new();
	for (owner, shape) in owners() {
		let parsed = query
			.parse_search(shape)
			.ok()
			.filter(|value| (owner.declaration.is_search_input_valid_fn)(value.clone()))
			.ok_or_else(|| {
				format!(
					"the search input does not satisfy the input of the view loader {:?}",
					owner.declaration.pattern
				)
			})?;
		received.push(spell_search(&parsed)?);
	}
	check_entries_received(
		&entries,
		&received,
		"no loader at or above this pattern receives in its input",
	)?;
	Ok(entries)
}

/// Which grammar a pattern uses: nested view patterns or flat endpoint
/// patterns.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PatternKind {
	View,
	ApiEndpoint,
}

struct HrefParts<'a> {
	kind: PatternKind,
	pattern: &'a str,
	params: &'a [(String, String)],
	splat_values: &'a [String],
	/// The query entries, which are empty for an href without a query.
	search_entries: Vec<(String, String)>,
	hash: Option<&'a str>,
}

/// The path of an href from a pattern, its params, and its splat values, by the rules of the
/// client's href builder (`packages/vorma-client/src/protocol/href.ts`). A `:name`
/// segment takes the param of that name and `*` takes the splat values. In a
/// view pattern, a trailing `_index` names its parent path and is omitted, and a
/// non-root splat needs a value. An API pattern keeps its trailing slash,
/// and an empty splat after other segments becomes a trailing slash.
fn build_path(parts: &HrefParts<'_>) -> Result<String, String> {
	let spelled = canonicalize_pattern(parts.pattern).map_err(|error| error.to_string())?;
	let has_trailing_slash =
		parts.kind == PatternKind::ApiEndpoint && spelled.len() > 1 && spelled.ends_with('/');
	let pattern_segments: Vec<&str> = if spelled == "/" {
		Vec::new()
	} else if has_trailing_slash {
		spelled[1..spelled.len() - 1].split('/').collect()
	} else {
		spelled[1..].split('/').collect()
	};
	let mut trailing_slash = has_trailing_slash;
	let mut segments = Vec::with_capacity(pattern_segments.len());
	let mut has_splat = false;
	for (index, segment) in pattern_segments.iter().enumerate() {
		let is_last = index + 1 == pattern_segments.len();
		if parts.kind == PatternKind::View && is_last && *segment == INDEX_SEGMENT {
			continue;
		}
		if segment.len() == SPLAT_SEGMENT.len_utf8() && segment.starts_with(SPLAT_SEGMENT) {
			has_splat = true;
			if parts.splat_values.is_empty() {
				match parts.kind {
					PatternKind::View if pattern_segments.len() > 1 => {
						return Err(format!(
							"the view pattern {:?} cannot select its non-root splat with no values",
							parts.pattern
						));
					}
					PatternKind::ApiEndpoint if !segments.is_empty() => trailing_slash = true,
					PatternKind::View | PatternKind::ApiEndpoint => {}
				}
			}
			for value in parts.splat_values {
				segments.push(encode_path_segment(value).map_err(|error| error.to_string())?);
			}
			continue;
		}
		if let Some(name) = segment.strip_prefix(DYNAMIC_PARAM_PREFIX) {
			let value = parts
				.params
				.iter()
				.find(|(param, _)| param == name)
				.map(|(_, value)| value)
				.ok_or_else(|| format!("the parameter {name} is missing"))?;
			segments.push(encode_path_segment(value).map_err(|error| error.to_string())?);
			continue;
		}
		segments.push((*segment).to_owned());
	}
	if !has_splat && !parts.splat_values.is_empty() {
		return Err(format!(
			"the pattern {:?} does not accept splat values",
			parts.pattern
		));
	}
	Ok(if segments.is_empty() {
		"/".to_owned()
	} else if trailing_slash {
		format!("/{}/", segments.join("/"))
	} else {
		format!("/{}", segments.join("/"))
	})
}

/// Appends the search and the hash of an href to its path.
fn append_search_and_hash(href: &mut String, parts: HrefParts<'_>) {
	if !parts.search_entries.is_empty() {
		let mut serializer = form_urlencoded::Serializer::new(String::new());
		serializer.extend_pairs(&parts.search_entries);
		href.push('?');
		href.push_str(&serializer.finish());
	}
	if let Some(hash) = parts.hash.filter(|hash| !hash.is_empty()) {
		href.push('#');
		push_fragment(href, hash.strip_prefix('#').unwrap_or(hash));
	}
}

/// Percent-encodes a fragment with the URL standard's fragment percent-encode
/// set, as the `hash` setter of a URL does.
fn push_fragment(href: &mut String, fragment: &str) {
	for character in fragment.chars() {
		// The URL parser removes ASCII tabs and newlines before it encodes.
		if matches!(character, '\t' | '\n' | '\r') {
			continue;
		}
		let encode = matches!(character, '\u{0}'..='\u{1F}' | ' ' | '"' | '<' | '>' | '`')
			|| character > '\u{7E}';
		if !encode {
			href.push(character);
			continue;
		}
		let mut buffer = [0; 4];
		for byte in character.encode_utf8(&mut buffer).bytes() {
			href.push_str(&format!("%{byte:02X}"));
		}
	}
}
