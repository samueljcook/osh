//! Vorma's server runtime: an application's view loaders, API endpoints, and
//! task middleware, served to Vorma's TypeScript browser client through an
//! application-owned HTTP server.
//!
//! An application creates its [`App`] from declarations and calls
//! [`App::load`] on the first line of `main`, which reads the build directory.
//! In production, the build never changes; in development, Vorma reads the
//! build's metadata again for each request. The application creates a
//! [`memoized_tasks::TaskRuntime`] that holds its state, creates an
//! [`AppService`] with [`LoadedApp::service`], and mounts that Tower service
//! on its own server, together with the layer from
//! [`LoadedApp::static_public_assets_layer`] unless another file server
//! serves the static public assets. Code outside Vorma's requests, such as host
//! routes and background jobs, creates task contexts from the same runtime.
//!
//! An application depends only on `vorma`. The task API is [`memoized_tasks`], and
//! TypeScript generation is [`tsgen`] with the [`TsGen`] derive.
//!
//! Every handler returns `Result<Success, TerminalOutcome<DeclaredError>>`. The
//! success value is a builder: [`Render`] for a view loader,
//! [`ApiEndpointResult`] for an API endpoint (from [`Respond`],
//! [`ApiEndpointRedirect`], or [`NotModified`], with `.into()`), and [`Continue`]
//! for task middleware. A [`Redirect`] or a [`Failure`] ends a handler through
//! `Err`, and `?` converts either of them, and any standard error, into a
//! [`TerminalOutcome`].

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]
#![forbid(unsafe_code)]

mod app;
mod build_output;
mod contract;
mod ctx;
mod declarations;
mod dev;
mod document;
mod execution;
mod finalize;
mod form;
mod href;
mod json_objects;
mod outcome;
mod pattern;
mod payload;
mod plan;
mod result;
mod search;
mod service;
mod static_public_asset;

pub use app::{App, LoadedApp, StartupError, StartupErrorKind};
pub use build_output::{
	CspSources, SkewProtection, StaticPublicAssets, TestBuild, VercelDeploymentId,
};
pub use ctx::{
	ApiEndpointCtx, MatchedApiEndpoint, MatchedPage, PatternMatch, RequestMatch, TaskMiddlewareCtx,
	ViewLoaderCtx,
};
pub use declarations::{
	ApiEndpoint, ApiEndpointInput, ApiEndpointKind, FormInputLimits, TaskMiddleware, UploadDir,
	ViewLoader,
};
pub use dev::{is_build, is_dev};
pub use document::{
	AttrMatch, AttrValue, HeadChild, HeadDedupeRule, HeadEl, HeadItem, HeadReplacement,
};
pub use form::{FormInput, UploadedFile};
pub use href::{HrefError, PageHrefs};
pub use pattern::{ApiPatternParams, MissingParamError, ViewPatternParams};
pub use primitives_rs::async_work::memoized_tasks;
pub use result::{
	ApiEndpointRedirect, ApiEndpointResult, ApiEndpointTarget, Continue, Failure,
	InvalidResultError, NoDeclaredError, NotModified, PageTarget, Redirect, RedirectStatus,
	RedirectTarget, Render, RequestRejected, Respond, TerminalOutcome,
};
pub use service::{AppService, NotFound, StaticPublicAssetsLayer, StaticPublicAssetsService};
pub use static_public_asset::StaticPublicAssetBody;
pub use tsgen::TsGen;
pub use vorma_macros::{api_pattern, view_pattern};

/// TypeScript generation: the [`TsGen`](trait@tsgen::TsGen) trait that
/// declared inputs, outputs, and errors implement, the types that describe
/// generated TypeScript, and the `TsGen` derive. One import of
/// `vorma::tsgen::TsGen` brings in the trait and the derive.
pub mod tsgen {
	pub use primitives_rs::tsgen::*;
	pub use vorma_macros::TsGen;
}

/// Items that generated code and declaration internals name. They are not
/// application APIs.
#[doc(hidden)]
pub mod __private {
	pub use crate::declarations::{ApiEndpointInputKind, ApiEndpointInputSource, InputError};
	pub use crate::form::FormBody;
	pub use crate::pattern::DecodedMatch;
}

/// Items that only Vorma's own tests use. No stability promise attaches to
/// them.
#[cfg(feature = "__internal")]
#[doc(hidden)]
pub mod __internal {
	pub use crate::build_output::BUILD_METADATA_FORMAT_VERSION;
	pub use crate::search::{
		DEPLOYMENT_QUERY_KEY, PermittedStrings, SearchField, SearchParamsError, SearchScalar,
		SearchScalarKind, SearchSerializeError, SearchShape, SearchShapeError,
		VIEW_PAYLOAD_QUERY_KEY, derive_search_shape, parse_search_params, serialize_search_params,
	};
}
