//! Task middleware, view loader, and API endpoint declarations. Creating a
//! declaration starts nothing and registers nothing.

use std::any::TypeId;
use std::fmt;
use std::future::Future;
use std::panic::Location;
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::future::{self, BoxFuture, FutureExt};
use http::Method;
use primitives_rs::async_work::memoized_tasks::TaskCtx;
use primitives_rs::tsgen::{self, TsGen, TypePhase, TypeRef, TypeRegistry};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::ctx::{ApiEndpointCtx, HandlerFacts, RequestFacts, TaskMiddlewareCtx, ViewLoaderCtx};
use crate::form::{FormBody, FormInput};
use crate::json_objects::JsonObjectPositions;
use crate::outcome::{
	ApiEndpointTypes, HandlerLabel, Outcome, RedirectingApiEndpoint, from_api_endpoint_result,
	from_task_middleware_result, from_view_loader_result, invalid_input_outcome,
	invalid_server_result_outcome,
};
use crate::pattern::{ApiPatternParams, DecodedMatch, ViewPatternParams};
use crate::result::{ApiEndpointResult, Continue, Render, RequestRejected, TerminalOutcome};
use crate::search::{SearchShape, SearchShapeError, derive_search_shape};
use primitives_rs::http::form_data::{
	FormContentType, FormDecodeConfig, FormDecodeError, decode_form_data,
};

/// One handler invocation's request-scoped inputs.
pub(crate) struct Invocation<State, Input> {
	pub(crate) state: Arc<State>,
	pub(crate) facts: Arc<RequestFacts>,
	pub(crate) task_ctx: TaskCtx<State>,
	pub(crate) decoded_match: DecodedMatch,
	pub(crate) input: Input,
	/// What the handler's reports name.
	pub(crate) label: HandlerLabel,
}

type HandlerInvoker<Input> = Arc<dyn Fn(Input) -> BoxFuture<'static, Outcome> + Send + Sync>;

/// The TypeScript types of one handler's input, output, and declared errors.
/// `None` when the handler has no input or no output.
pub(crate) struct HandlerTypeRefs {
	pub(crate) input: Option<TypeRef>,
	pub(crate) output: Option<TypeRef>,
	pub(crate) errors: TypeRef,
}

type CollectTypes = fn(&mut TypeRegistry) -> tsgen::Result<HandlerTypeRefs>;

fn collect_types<Input: TsGen, Output: TsGen, DeclaredError: TsGen>(
	registry: &mut TypeRegistry,
) -> tsgen::Result<HandlerTypeRefs> {
	Input::collect_type_defs_for(TypePhase::Deserialize, registry)?;
	// Input inspection must succeed before an app can serve requests.
	primitives_rs::tsgen::TypeGraph::new(
		Input::type_ref_for(TypePhase::Deserialize),
		registry.clone(),
	)?;
	Output::collect_type_defs_for(TypePhase::Serialize, registry)?;
	DeclaredError::collect_type_defs_for(TypePhase::Serialize, registry)?;
	Ok(HandlerTypeRefs {
		input: (!is_unit::<Input>()).then(|| Input::type_ref_for(TypePhase::Deserialize)),
		output: (!is_unit::<Output>()).then(|| Output::type_ref_for(TypePhase::Serialize)),
		errors: DeclaredError::type_ref_for(TypePhase::Serialize),
	})
}

fn is_unit<Value: 'static>() -> bool {
	TypeId::of::<Value>() == TypeId::of::<()>()
}

/// A view loader's search shape. A loader with the input `()` has none.
fn derive_view_search_shape<Input: TsGen + 'static>()
-> Result<Option<SearchShape>, SearchShapeError> {
	if is_unit::<Input>() {
		return Ok(None);
	}
	derive_search_shape::<Input>().map(Some)
}

fn is_search_input_valid<Input: DeserializeOwned>(value: serde_json::Value) -> bool {
	serde_json::from_value::<Input>(value).is_ok()
}

impl<State> ViewLoader<State> {
	/// How messages identify the loader: its location, list, and pattern.
	pub(crate) fn describe(&self) -> String {
		format!("{} view_loaders {:?}", self.location, self.pattern)
	}
}

impl<State> ApiEndpoint<State> {
	/// How messages identify the endpoint: its location, list, method, and
	/// pattern.
	pub(crate) fn describe(&self) -> String {
		format!(
			"{} api_endpoints {} {:?}",
			self.location, self.method, self.pattern
		)
	}
}

impl<State> TaskMiddleware<State> {
	/// How messages identify the middleware: its location and list.
	pub(crate) fn describe(&self) -> String {
		format!("{} task_middlewares", self.location)
	}
}

/// A view loader: the server half of one view.
///
/// A view that defines no explicit component is a pass-through view: the
/// client renders its child in its place. A view with a loader and no client
/// definition is one, and so is a view with a `createView` definition that has no
/// `component`; its client loader still runs and its `childErrorComponent`
/// still renders a failed child.
///
/// The handler runs in its own Tokio task, so a `tokio::task_local!` value that
/// the host sets is not visible to it. Per-request values that the host
/// computes travel in the request's extensions, which the context's `request_parts`
/// holds.
pub struct ViewLoader<State> {
	/// Where the application declared the loader.
	pub(crate) location: &'static Location<'static>,
	pub(crate) pattern: &'static str,
	pub(crate) derive_search_shape_fn: fn() -> Result<Option<SearchShape>, SearchShapeError>,
	/// Whether a search value's fields decode as this loader's input.
	pub(crate) is_search_input_valid_fn: fn(serde_json::Value) -> bool,
	pub(crate) collect_types_fn: CollectTypes,
	pub(crate) invoke_handler_fn: HandlerInvoker<Invocation<State, serde_json::Value>>,
}

impl<State> Clone for ViewLoader<State> {
	fn clone(&self) -> Self {
		Self {
			location: self.location,
			pattern: self.pattern,
			derive_search_shape_fn: self.derive_search_shape_fn,
			is_search_input_valid_fn: self.is_search_input_valid_fn,
			collect_types_fn: self.collect_types_fn,
			invoke_handler_fn: Arc::clone(&self.invoke_handler_fn),
		}
	}
}

impl<State> fmt::Debug for ViewLoader<State> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_tuple("ViewLoader")
			.field(&self.pattern)
			.finish()
	}
}

impl<State: Send + Sync + 'static> ViewLoader<State> {
	/// Declares the loader of the view pattern that the handler's params type
	/// belongs to: a handler that takes
	/// `ViewLoaderCtx<State, UserParams, Input>` loads the pattern
	/// `UserParams::PATTERN`.
	///
	/// The handler's input is the pattern's search input: `()` reads none, and
	/// a serde struct declares the search shape that the browser shares.
	/// A string-keyed map as the input reads every key of the query, so no
	/// other loader at or above the same page can read search keys, and
	/// loading the app fails when one does. A map in a named field of a struct
	/// reads only the keys below that name, so it has no such limit.
	/// The contract cannot declare a top-level field named `vorma-json`, which
	/// belongs to Vorma's wire protocol, and loading the app fails when it
	/// does. While Vercel skew protection is on, it cannot declare one named
	/// `dpl` either because Vercel reserves that name, and loading the app
	/// fails then as well. While skew protection is off, `dpl` is an ordinary
	/// search name.
	/// Its output is the view's data: `()` means none. Its declared
	/// errors are the public errors that the browser's error components
	/// receive.
	///
	/// View patterns that share a dynamic segment position must give it one
	/// parameter name, as `/users/:user_id` and `/users/:user_id/posts` do;
	/// loading the app fails otherwise. Vorma and its build find the views at
	/// and above a view pattern, which contribute their search input and
	/// render the parent views, by comparing the patterns' segments with their
	/// parameter names, while request matching ignores parameter names, so a
	/// second name at a shared position would make the two disagree about a
	/// page's parent views.
	///
	/// When a `GET` request's path matches both a view pattern and the pattern
	/// of a `GET` [`ApiEndpoint`], the more specific pattern handles the
	/// request. Patterns compare by their segments in order, without the
	/// index segment: at the first position where they differ, a static
	/// segment is more specific than a dynamic segment, and a dynamic segment
	/// is more specific than a splat, and a pattern that ends at that position
	/// is more specific than a longer one. Static text, parameter names, and
	/// trailing slashes do not count. For example, the view pattern `/api/docs`
	/// handles `/api/docs` next to the endpoint pattern `/api/*`, and the
	/// endpoint pattern `/files/recent` handles `/files/recent` next to the
	/// view pattern `/files/*`. Loading the app fails for a view pattern and a
	/// `GET` endpoint pattern that can match one path and are equally
	/// specific, such as the view pattern `/users/_index` and the endpoint
	/// pattern `/users/`.
	#[track_caller]
	pub fn new<Params, Input, Output, DeclaredError, HandlerFn, Fut>(handler: HandlerFn) -> Self
	where
		Params: ViewPatternParams,
		Input: DeserializeOwned + TsGen,
		Output: Serialize + TsGen,
		DeclaredError: Serialize + TsGen,
		HandlerFn: Fn(ViewLoaderCtx<State, Params, Input>) -> Fut + Send + Sync + 'static,
		Fut: Future<Output = Result<Render<Output>, TerminalOutcome<DeclaredError>>>
			+ Send
			+ 'static,
	{
		let pattern = Params::PATTERN;
		let handler = Arc::new(handler);
		let invoke_handler =
			move |invocation: Invocation<State, serde_json::Value>| -> BoxFuture<'static, Outcome> {
				let handler = Arc::clone(&handler);
				// Decoding and the handler run inside the returned future, where
				// execution captures a panic, including one before the first await.
				async move {
					let label = invocation.label;
					let params = match Params::from_decoded_match(invocation.decoded_match) {
						Ok(params) => params,
						Err(error) => {
							return invalid_server_result_outcome(&label, &error.to_string());
						}
					};
					let Ok(input) = serde_json::from_value::<Input>(invocation.input) else {
						return invalid_input_outcome();
					};
					let facts = invocation.facts;
					let ctx = facts.to_view_loader_ctx(HandlerFacts {
						state: invocation.state,
						params,
						input,
						pattern,
						task_ctx: invocation.task_ctx,
					});
					from_view_loader_result(
						&label,
						handler(ctx).await,
						&*facts.target_resolver,
						facts.response_target(),
					)
				}
				.boxed()
			};
		Self {
			location: Location::caller(),
			pattern,
			derive_search_shape_fn: derive_view_search_shape::<Input>,
			is_search_input_valid_fn: is_search_input_valid::<Input>,
			collect_types_fn: collect_types::<Input, Output, DeclaredError>,
			invoke_handler_fn: Arc::new(invoke_handler),
		}
	}
}

/// How the browser treats an endpoint call. After a successful mutation,
/// Vorma refreshes the current page's data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiEndpointKind {
	/// A call that reads.
	Query,
	/// A call that changes something.
	Mutation,
}

/// Where an endpoint's input arrives.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApiEndpointInputKind {
	/// The endpoint reads no input and Vorma does not read the body.
	None,
	/// A `GET` endpoint reads the URL's search parameters.
	Search,
	/// The body is a JSON value.
	Json,
	/// The body is a form.
	FormData,
}

/// An endpoint's raw input, read according to its input kind.
#[doc(hidden)]
pub enum ApiEndpointInputSource {
	/// No input.
	None,
	/// Search parameters, parsed with the endpoint's search shape.
	Search(serde_json::Value),
	/// A collected JSON body.
	Json(Bytes),
	/// A form body, which is read as it is decoded.
	FormData {
		/// The form's media type, read from the request's content type.
		media_type: FormContentType,
		/// The endpoint's body limit, form limits, and upload directory.
		config: FormDecodeConfig,
		/// The body's chunks.
		body: FormBody,
	},
}

/// The limits of an API endpoint's form input and the directory of its
/// uploaded files, which [`ApiEndpoint::set_form_input_limits`] sets.
///
/// Vorma decodes a form as the body arrives. Each multipart part with a
/// `filename` or `filename*` parameter becomes a temporary file in
/// `upload_dir`, which exists until the handler drops its
/// [`UploadedFile`](crate::UploadedFile), and Vorma keeps every text value in
/// memory. The body limit of the endpoint
/// ([`App::max_request_body_size`](crate::App::max_request_body_size) or
/// [`ApiEndpoint::set_max_request_body_size`]) bounds the whole body, and these
/// limits bound its parts. A form that exceeds a limit fails the request with
/// status 413, and the handler does not run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormInputLimits {
	/// The largest number of parts in one form, counting each repeated name
	/// and each file. It also bounds the number of temporary files that one
	/// request creates.
	pub max_parts: usize,
	/// The largest size, in bytes, of one text value.
	pub max_text_size: usize,
	/// The largest size, in bytes, of one uploaded file.
	pub max_file_size: usize,
	/// The directory in which Vorma stores uploaded files.
	pub upload_dir: UploadDir,
}

/// The directory in which Vorma stores the uploaded files of a form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UploadDir {
	/// The system's temporary directory, as [`std::env::temp_dir`] returns
	/// it. On some systems it is a memory-backed file system, such as `tmpfs`.
	SystemTempDir,
	/// This directory, which must exist when a request arrives.
	Path(PathBuf),
}

impl FormInputLimits {
	/// The configuration of the form decoder for a body limit of `max_size`.
	pub(crate) fn to_decode_config(&self, max_size: usize) -> FormDecodeConfig {
		FormDecodeConfig {
			limits: primitives_rs::http::form_data::FormLimits {
				max_size,
				max_parts: Some(self.max_parts),
				max_text_size: Some(self.max_text_size),
				max_file_size: Some(self.max_file_size),
			},
			upload_dir: match &self.upload_dir {
				UploadDir::SystemTempDir => None,
				UploadDir::Path(path) => Some(path.clone()),
			},
		}
	}
}

/// Why endpoint input could not be decoded.
#[doc(hidden)]
#[derive(Debug)]
pub enum InputError {
	/// The input does not fit its type.
	Invalid,
	/// The request's `Content-Type` is not one that the endpoint reads: for a
	/// JSON body, `application/json` or a media type that ends in `+json`; for a
	/// form, `application/x-www-form-urlencoded` or `multipart/form-data`.
	UnsupportedMediaType,
	/// The form exceeded the endpoint's input-size limit.
	PayloadTooLarge,
	/// The request body could not be read.
	BodyRead(Box<dyn std::error::Error + Send + Sync>),
	/// An uploaded file could not be stored.
	Storage(std::io::Error),
}

impl From<FormDecodeError> for InputError {
	fn from(error: FormDecodeError) -> Self {
		match error {
			FormDecodeError::MalformedMultipart => Self::Invalid,
			FormDecodeError::LimitExceeded(_) => Self::PayloadTooLarge,
			FormDecodeError::BodyRead(error) => Self::BodyRead(error),
			FormDecodeError::Storage(error) => Self::Storage(error),
		}
	}
}

/// The outcome of input that could not be decoded: 400 for invalid input or a
/// body that could not be read, 413 for a body that a body limit stopped, 415
/// for a body with an unsupported media type, and 500 for an upload that could
/// not be stored.
pub(crate) fn input_failure_outcome(label: &HandlerLabel, error: InputError) -> Outcome {
	match error {
		InputError::Invalid => invalid_input_outcome(),
		InputError::PayloadTooLarge => Outcome::Failure {
			contribution: crate::outcome::Contribution::default(),
			status: http::StatusCode::PAYLOAD_TOO_LARGE,
			public_error: None,
		},
		InputError::UnsupportedMediaType => Outcome::Failure {
			contribution: crate::outcome::Contribution::default(),
			status: http::StatusCode::UNSUPPORTED_MEDIA_TYPE,
			public_error: None,
		},
		InputError::BodyRead(error) => {
			let status = if is_length_limit_error(&*error) {
				http::StatusCode::PAYLOAD_TOO_LARGE
			} else {
				http::StatusCode::BAD_REQUEST
			};
			Outcome::Failure {
				contribution: crate::outcome::Contribution::default(),
				status,
				public_error: None,
			}
		}
		InputError::Storage(error) => {
			tracing::error!(
				handler = label.kind.span_name(),
				pattern = &*label.pattern,
				path = &*label.request_path,
				error = &error as &(dyn std::error::Error + 'static),
				"Vorma could not store an uploaded file",
			);
			Outcome::internal_server_error()
		}
	}
}

/// Whether a body limit caused `error`. The limit's error can be the outermost
/// error, as with Vorma's own limit, or it can sit anywhere in the source chain
/// of a host server's body error: axum wraps every body error in its own error
/// type.
fn is_length_limit_error(error: &(dyn std::error::Error + 'static)) -> bool {
	let mut current = Some(error);
	while let Some(error) = current {
		if error.is::<http_body_util::LengthLimitError>() {
			return true;
		}
		current = error.source();
	}
	false
}

/// An API endpoint's input type: `()` for none, [`FormInput`] for a form body,
/// or a serde type. A `GET` endpoint reads a serde type from the URL's search
/// parameters, and any other method reads it from the JSON body. A request
/// with a `Content-Type` other than `application/json` or a media type that ends in
/// `+json` gets a 415 failure because a cross-site form can send a body with
/// another media type without a CORS preflight.
///
/// A JSON body must have an object wherever the input type has a struct, at
/// any depth. serde's derived `Deserialize` would also accept an array of the
/// struct's field values in declaration order, but Vorma answers such a body
/// with a 400 failure and does not run the handler. In an enum, the structs
/// are those of the variant that the body's tag selects; in an untagged enum
/// with variants that have the same JSON shape, a body that one variant admits is
/// admitted. When a key repeats in one JSON object, every occurrence must pass
/// this check. Vorma checks the body in one pass before it decodes the
/// input, and it skips the parts of the body that the input type does not
/// declare as the input's own decoding skips an unknown field, without
/// storing them. A body that is not valid JSON gets a 400 failure, and so does
/// a body that nests arrays and objects deeper than 128 levels where the
/// input type declares a value.
///
/// That rule does not protect an endpoint without JSON input from a cross-site
/// request. An endpoint with input `()` reads no body and checks no media type,
/// and a form endpoint reads the media types that a cross-site form sends, so
/// both run for a cross-site form request. Vorma checks neither `Origin` nor
/// `Sec-Fetch-Site`. An application that authenticates requests by a cookie
/// must make that check itself, in the host's middleware or in a task
/// middleware.
///
/// A `GET`
/// endpoint's search shape cannot declare a top-level field named
/// `vorma-json`, which belongs to Vorma's wire protocol, and loading the app
/// fails when it does. While Vercel skew protection is on, it cannot declare
/// one named `dpl` either because Vercel reserves that name, and loading the
/// app fails then as well. While skew protection is off, `dpl` is an ordinary
/// search name.
pub trait ApiEndpointInput: TsGen + Sized + Send + 'static {
	/// Where this input arrives for an endpoint with this method.
	#[doc(hidden)]
	fn __input_kind(method: &Method) -> Result<ApiEndpointInputKind, &'static str>;

	/// Decodes the input from its raw source.
	#[doc(hidden)]
	fn __decode(source: ApiEndpointInputSource) -> BoxFuture<'static, Result<Self, InputError>>;

	/// Whether this search input, as an endpoint target supplies it, fits the
	/// input type.
	#[doc(hidden)]
	fn __is_search_input_valid(value: serde_json::Value) -> bool;
}

impl<Input: DeserializeOwned + TsGen + Send + 'static> ApiEndpointInput for Input {
	fn __input_kind(method: &Method) -> Result<ApiEndpointInputKind, &'static str> {
		Ok(if is_unit::<Input>() {
			ApiEndpointInputKind::None
		} else if method == Method::GET {
			ApiEndpointInputKind::Search
		} else {
			ApiEndpointInputKind::Json
		})
	}

	fn __decode(source: ApiEndpointInputSource) -> BoxFuture<'static, Result<Self, InputError>> {
		let decoded = match source {
			ApiEndpointInputSource::None => {
				serde_json::from_value(serde_json::Value::Null).map_err(|_| InputError::Invalid)
			}
			ApiEndpointInputSource::Search(value) => {
				serde_json::from_value(value).map_err(|_| InputError::Invalid)
			}
			ApiEndpointInputSource::Json(bytes) => {
				serde_json::from_slice(&bytes).map_err(|_| InputError::Invalid)
			}
			ApiEndpointInputSource::FormData { .. } => Err(InputError::Invalid),
		};
		future::ready(decoded).boxed()
	}

	fn __is_search_input_valid(value: serde_json::Value) -> bool {
		is_search_input_valid::<Input>(value)
	}
}

impl<Form: DeserializeOwned + TsGen> ApiEndpointInput for FormInput<Form> {
	fn __input_kind(method: &Method) -> Result<ApiEndpointInputKind, &'static str> {
		if method == Method::GET {
			return Err("a GET endpoint reads search parameters, so it cannot take form input");
		}
		Ok(ApiEndpointInputKind::FormData)
	}

	fn __decode(source: ApiEndpointInputSource) -> BoxFuture<'static, Result<Self, InputError>> {
		async move {
			let ApiEndpointInputSource::FormData {
				media_type,
				body,
				config,
			} = source
			else {
				return Err(InputError::Invalid);
			};
			Ok(FormInput(
				decode_form_data(media_type, body, config)
					.await?
					.into_deserialized::<Form>()
					.map_err(|_| InputError::Invalid)?,
			))
		}
		.boxed()
	}

	fn __is_search_input_valid(_value: serde_json::Value) -> bool {
		false
	}
}

/// An API endpoint: one method and pattern that answers with data.
///
/// The handler runs in its own Tokio task, so a `tokio::task_local!` value that
/// the host sets is not visible to it. Per-request values that the host
/// computes travel in the request's extensions, which the context's `request_parts`
/// holds.
pub struct ApiEndpoint<State> {
	/// Where the application declared the endpoint.
	pub(crate) location: &'static Location<'static>,
	pub(crate) kind: ApiEndpointKind,
	pub(crate) method: Method,
	pub(crate) pattern: &'static str,
	pub(crate) determine_input_kind_fn: fn(&Method) -> Result<ApiEndpointInputKind, &'static str>,
	pub(crate) derive_search_shape_fn: fn() -> Result<SearchShape, SearchShapeError>,
	pub(crate) is_search_input_valid_fn: fn(serde_json::Value) -> bool,
	pub(crate) max_request_body_size: Option<usize>,
	/// `None` until [`Self::set_form_input_limits`] sets them.
	pub(crate) form_input_limits: Option<FormInputLimits>,
	pub(crate) types: ApiEndpointTypes,
	pub(crate) collect_types_fn: CollectTypes,
	pub(crate) invoke_handler_fn: HandlerInvoker<Invocation<State, ApiEndpointInputSource>>,
}

impl<State> Clone for ApiEndpoint<State> {
	fn clone(&self) -> Self {
		Self {
			location: self.location,
			kind: self.kind,
			method: self.method.clone(),
			pattern: self.pattern,
			determine_input_kind_fn: self.determine_input_kind_fn,
			derive_search_shape_fn: self.derive_search_shape_fn,
			is_search_input_valid_fn: self.is_search_input_valid_fn,
			max_request_body_size: self.max_request_body_size,
			form_input_limits: self.form_input_limits.clone(),
			types: self.types,
			collect_types_fn: self.collect_types_fn,
			invoke_handler_fn: Arc::clone(&self.invoke_handler_fn),
		}
	}
}

impl<State> fmt::Debug for ApiEndpoint<State> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("ApiEndpoint")
			.field("kind", &self.kind)
			.field("method", &self.method)
			.field("pattern", &self.pattern)
			.finish()
	}
}

impl<State: Send + Sync + 'static> ApiEndpoint<State> {
	/// Declares an endpoint of the API pattern that the handler's params type
	/// belongs to: a handler that takes
	/// `ApiEndpointCtx<State, UserApiParams, Input>` serves the pattern
	/// `UserApiParams::PATTERN`. `kind` decides how the browser treats a
	/// call, and `method` decides where input arrives, as
	/// [`ApiEndpointInput`] describes.
	///
	/// Vorma reads at most
	/// [`App::max_request_body_size`](crate::App::max_request_body_size) of
	/// a JSON or form body, including uploaded files, unless
	/// [`Self::set_max_request_body_size`] sets another limit for this
	/// endpoint. An endpoint with [`FormInput`] input must also state its form
	/// limits with [`Self::set_form_input_limits`], and loading the app fails
	/// for one that does not.
	///
	/// When a `GET` request's path matches both a `GET` endpoint's pattern and
	/// a view pattern, the more specific pattern handles the request, by the
	/// rule that [`ViewLoader::new`] describes. Loading the app fails for a
	/// `GET` endpoint pattern and a view pattern that can match one path and
	/// are equally specific.
	#[track_caller]
	pub fn new<Params, Input, Output, DeclaredError, HandlerFn, Fut>(
		kind: ApiEndpointKind,
		method: Method,
		handler: HandlerFn,
	) -> Self
	where
		Params: ApiPatternParams,
		Input: ApiEndpointInput,
		Output: Serialize + TsGen + 'static,
		DeclaredError: Serialize + TsGen + 'static,
		HandlerFn: Fn(ApiEndpointCtx<State, Params, Input>) -> Fut + Send + Sync + 'static,
		Fut: Future<Output = Result<ApiEndpointResult<Output>, TerminalOutcome<DeclaredError>>>
			+ Send
			+ 'static,
	{
		let types = ApiEndpointTypes {
			input: TypeId::of::<Input>(),
			output: TypeId::of::<Output>(),
			declared_error: TypeId::of::<DeclaredError>(),
		};
		let redirecting = Arc::new(RedirectingApiEndpoint {
			method: method.clone(),
			types,
		});
		let pattern = Params::PATTERN;
		let handler = Arc::new(handler);
		let json_objects = Arc::new(JsonObjectPositions::of::<Input>());
		let invoke_handler = move |invocation: Invocation<State, ApiEndpointInputSource>| -> BoxFuture<'static, Outcome> {
			let redirecting = Arc::clone(&redirecting);
			let handler = Arc::clone(&handler);
			let json_objects = Arc::clone(&json_objects);
			async move {
				let label = invocation.label;
				let params = match Params::from_decoded_match(invocation.decoded_match) {
					Ok(params) => params,
					Err(error) => return invalid_server_result_outcome(&label, &error.to_string()),
				};
				// The input type's own decoding then reads the same bytes so that it keeps
				// rejecting what it rejects, such as a repeated field.
				if let (ApiEndpointInputSource::Json(bytes), Some(objects)) =
					(&invocation.input, json_objects.as_ref())
					&& !objects.admits(bytes)
				{
					return input_failure_outcome(&label, InputError::Invalid);
				}
				let input = match Input::__decode(invocation.input).await {
					Ok(input) => input,
					Err(error) => return input_failure_outcome(&label, error),
				};
				let ctx = invocation.facts.to_api_endpoint_ctx(HandlerFacts {
					state: invocation.state,
					params,
					input,
					pattern,
					task_ctx: invocation.task_ctx,
				});
				from_api_endpoint_result(
					&label,
					&redirecting,
					handler(ctx).await,
					&*invocation.facts.target_resolver,
					invocation.facts.response_target(),
				)
			}
			.boxed()
		};
		Self {
			location: Location::caller(),
			kind,
			method,
			pattern,
			determine_input_kind_fn: Input::__input_kind,
			derive_search_shape_fn: derive_search_shape::<Input>,
			is_search_input_valid_fn: Input::__is_search_input_valid,
			max_request_body_size: None,
			form_input_limits: None,
			types,
			collect_types_fn: collect_types::<Input, Output, DeclaredError>,
			invoke_handler_fn: Arc::new(invoke_handler),
		}
	}

	/// Sets the largest request body, in bytes, that Vorma reads for this
	/// endpoint, instead of [`App::max_request_body_size`](crate::App::max_request_body_size).
	/// The limit applies to an endpoint with JSON or form input, and a form
	/// body's limit includes its uploaded files. A larger body fails the
	/// request with status 413. A limit that the host server sets still
	/// applies, and the lower limit wins. The memory and files that one
	/// request can use are described on
	/// [`App::max_request_body_size`](crate::App::max_request_body_size).
	pub fn set_max_request_body_size(mut self, size: usize) -> Self {
		self.max_request_body_size = Some(size);
		self
	}

	/// Sets the form limits and the upload directory of an endpoint with
	/// [`FormInput`] input. An application must call it for an endpoint with
	/// [`FormInput`] input and must not call it for any other endpoint:
	/// loading the app fails for an endpoint with form input and no form
	/// limits, and for an endpoint without form input that sets them. The
	/// limits apply in addition to the endpoint's body limit.
	pub fn set_form_input_limits(mut self, limits: FormInputLimits) -> Self {
		self.form_input_limits = Some(limits);
		self
	}
}

/// Task middleware: request work that runs before the request's view loaders
/// or API endpoint.
///
/// The middleware that matches a request starts concurrently: no middleware
/// waits for an earlier one to finish. Only the commit of each middleware's
/// contributions (its status, headers, cookies, and document changes) follows
/// the order in [`App::task_middlewares`](crate::App::task_middlewares),
/// whatever order the middleware finishes in. A middleware that redirects or
/// fails ends the request, and the contributions of the middleware after it
/// do not apply.
///
/// The handler runs in its own Tokio task, so a `tokio::task_local!` value that
/// the host sets is not visible to it. Per-request values that the host
/// computes travel in the request's extensions, which the context's `request_parts`
/// holds.
pub struct TaskMiddleware<State> {
	/// Where the application declared the middleware.
	pub(crate) location: &'static Location<'static>,
	/// `None` until [`Self::set_patterns`] sets them: the middleware runs for
	/// every path.
	pub(crate) patterns: Option<Vec<String>>,
	/// `None` until [`Self::set_methods`] sets them: the middleware runs for
	/// every method.
	pub(crate) methods: Option<Vec<Method>>,
	pub(crate) invoke_handler_fn: HandlerInvoker<Invocation<State, ()>>,
}

impl<State> TaskMiddleware<State> {
	/// Whether the middleware runs for requests with `method`.
	pub(crate) fn selects_method(&self, method: &Method) -> bool {
		self.methods
			.as_ref()
			.is_none_or(|methods| methods.contains(method))
	}
}

impl<State> Clone for TaskMiddleware<State> {
	fn clone(&self) -> Self {
		Self {
			location: self.location,
			patterns: self.patterns.clone(),
			methods: self.methods.clone(),
			invoke_handler_fn: Arc::clone(&self.invoke_handler_fn),
		}
	}
}

impl<State> fmt::Debug for TaskMiddleware<State> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("TaskMiddleware")
			.field("patterns", &self.patterns)
			.field("methods", &self.methods)
			.finish()
	}
}

impl<State: Send + Sync + 'static> TaskMiddleware<State> {
	/// Declares middleware that runs for every request that Vorma handles
	/// unless [`Self::set_patterns`] or [`Self::set_methods`] restricts it.
	#[track_caller]
	pub fn new<HandlerFn, Fut>(handler: HandlerFn) -> Self
	where
		HandlerFn: Fn(TaskMiddlewareCtx<State>) -> Fut + Send + Sync + 'static,
		Fut: Future<Output = Result<Continue, TerminalOutcome<RequestRejected>>> + Send + 'static,
	{
		let handler = Arc::new(handler);
		let invoke_handler =
			move |invocation: Invocation<State, ()>| -> BoxFuture<'static, Outcome> {
				let Invocation {
					state,
					facts,
					task_ctx,
					label,
					..
				} = invocation;
				let handler = Arc::clone(&handler);
				// The handler runs inside the returned future, where execution
				// captures a panic, including one before its first await.
				async move {
					let ctx = facts.to_task_middleware_ctx(state, task_ctx);
					from_task_middleware_result(
						&label,
						handler(ctx).await,
						&*facts.target_resolver,
						facts.response_target(),
					)
				}
				.boxed()
			};
		Self {
			location: Location::caller(),
			patterns: None,
			methods: None,
			invoke_handler_fn: Arc::new(invoke_handler),
		}
	}

	/// Runs the middleware only for requests with a path that matches one of
	/// these flat patterns. The list replaces any earlier list. Middleware
	/// without this call runs for every path. An empty list selects no request.
	///
	/// A pattern that ends with a splat matches the paths below its prefix and
	/// not the prefix itself: `/admin/*` matches `/admin/users` and does not
	/// match `/admin`. A guard for an area lists both patterns, such as
	/// `["/admin", "/admin/*"]`.
	pub fn set_patterns<Pattern: Into<String>>(
		mut self,
		patterns: impl IntoIterator<Item = Pattern>,
	) -> Self {
		self.patterns = Some(patterns.into_iter().map(Into::into).collect());
		self
	}

	/// Runs the middleware only for requests with one of these methods, where
	/// a `HEAD` request counts as `GET`. The list replaces any earlier list.
	/// Middleware without this call runs for every method. Listing `HEAD`, a
	/// method that Vorma does not handle is a startup error. An empty list selects no request.
	pub fn set_methods(mut self, methods: impl IntoIterator<Item = Method>) -> Self {
		self.methods = Some(methods.into_iter().collect());
		self
	}
}
