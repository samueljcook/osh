//! The Vorma app: everything that the application gives Vorma.

use std::error::Error as StdError;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use primitives_rs::async_work::memoized_tasks::TaskRuntime;

use crate::build_output::{
	BuildOutput, BuildSnapshot, ClientBuildId, RuntimeMode, SkewProtection, TestBuild,
};
use crate::contract::{
	ContractError, DeclarationDescription, describe, write_contract_if_requested,
};
use crate::declarations::{ApiEndpoint, TaskMiddleware, ViewLoader};
use crate::dev::{CHECK_BUILD_DIR_ENV_VAR, is_dev, read_vercel_skew_protection};
use crate::document::{HeadDedupeRule, HeadRules};
use crate::href::{CurrentPlanTargetResolver, PageHrefs};
use crate::plan::{CurrentPlan, Declarations, Plan, PlanError};
use crate::service::{AppService, AppServiceInner, StaticPublicAssetsLayer};

/// The environment variable in which `vorma dev` names the dist directory,
/// which only `vorma.config.ts` knows, for the server that it starts.
const DEV_DIST_DIR_ENV_VAR: &str = "VORMA_DIST_DIR";

/// Inside the dist directory: the build directory that `vorma build` writes.
const PROD_BUILD_DIR_NAME: &str = "prod";

/// Inside the dist directory: the build directory that `vorma dev` writes.
const DEV_BUILD_DIR_NAME: &str = "dev";

/// The Vorma app: the application's declarations and its dist directory in
/// production. Creating it starts nothing and reads no files. [`App::load`]
/// reads a build directory in the dist directory.
pub struct App<State> {
	/// The dist directory in production: the directory that `core.distDir` of
	/// the Vorma configuration names, or a copy of it. `vorma build` writes the
	/// production build to the build directory `prod` in it, which the server
	/// reads in production. A relative path resolves against the process's
	/// working directory. Under `vorma dev`, the server reads the build
	/// directory `dev` in the dist directory that `vorma dev` names instead.
	pub dist_dir: PathBuf,
	/// Task middleware, in the order in which its contributions apply.
	///
	/// The middleware that matches a request starts concurrently: no
	/// middleware waits for an earlier one to finish. Only the commit of each
	/// middleware's contributions (its status, headers, cookies, and document
	/// changes) follows this order, whatever order the middleware finishes in.
	pub task_middlewares: Vec<TaskMiddleware<State>>,
	/// View loaders.
	pub view_loaders: Vec<ViewLoader<State>>,
	/// API endpoints.
	pub api_endpoints: Vec<ApiEndpoint<State>>,
	/// Head identities in addition to the built-in identities.
	pub head_dedupe_rules: Vec<HeadDedupeRule>,
	/// The largest request body, in bytes, that Vorma reads for an API
	/// endpoint with JSON or form input, including uploaded files.
	/// [`ApiEndpoint::set_max_request_body_size`] overrides it for one
	/// endpoint. A larger body fails the request with status 413. A limit
	/// that the host server sets still applies, and the lower limit wins.
	///
	/// A JSON body is collected in memory, so one request holds up to this
	/// many bytes of body. Vorma then checks the body's objects in one pass,
	/// which uses memory in proportion to the nesting depth of the input type
	/// and not to the size of the body, and the input type's `Deserialize`
	/// decodes it. The decoded input uses the memory that its type stores,
	/// and serde stores a copy of the value of an internally tagged or
	/// untagged enum, or of a struct with a flattened field, while it decodes
	/// that value, which can use several times the size of that part of the
	/// body.
	///
	/// A form body is decoded as it arrives. Its text values stay in memory,
	/// so one request holds up to this many bytes of text, and its uploaded
	/// files are stored in files, which the endpoint's
	/// [`FormInputLimits`](crate::FormInputLimits) bound in number and size.
	pub max_request_body_size: usize,
}

/// The app cannot load: `dist_dir` is empty, `vorma dev` did not name
/// the dist directory, the build directory's contents are missing or
/// unreadable, the build directory holds a build of the other mode than the
/// process runs in, the build directory was built for other declarations, the
/// declarations are invalid, `VERCEL_SKEW_PROTECTION_ENABLED` is set to a
/// value that is not a boolean, or it enables skew protection without a
/// `VERCEL_DEPLOYMENT_ID`.
///
/// Its message is the whole text of the failure, and its source is the cause
/// below that failure, such as the I/O error of a file that could not be read,
/// so a report that follows the sources prints no text twice. Its `Debug`
/// output is the message followed by each cause so that a `main` that returns
/// the error prints readable text.
pub struct StartupError {
	kind: StartupErrorKind,
	message: String,
	/// The error that the message describes. Its own source is this error's source.
	described: Option<Box<dyn StdError + Send + Sync>>,
}

impl fmt::Debug for StartupError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(&self.message)?;
		let mut source = StdError::source(self);
		while let Some(error) = source {
			write!(formatter, "\n\nCaused by: {error}")?;
			source = error.source();
		}
		Ok(())
	}
}

impl fmt::Display for StartupError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(&self.message)
	}
}

impl StdError for StartupError {
	fn source(&self) -> Option<&(dyn StdError + 'static)> {
		self.described
			.as_deref()
			.and_then(|described| described.source())
	}
}

/// The stage of [`App::load`] that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StartupErrorKind {
	/// The app's configuration is unusable: `dist_dir` is empty, `vorma
	/// dev` did not name the dist directory,
	/// `VERCEL_SKEW_PROTECTION_ENABLED` is set to a value that is not a
	/// boolean, or it enables skew protection, and `VERCEL_DEPLOYMENT_ID` is
	/// not set to a deployment ID of one or more visible ASCII characters.
	Config,
	/// The build directory's contents are missing, unreadable, or invalid,
	/// such as a client build ID that is `_` or is not one or more visible
	/// ASCII characters, or the build directory holds a development build in a
	/// process that is not a development process, or a production build in a
	/// development process. For [`App::load_for_tests`], the client build ID
	/// of the [`TestBuild`] is `_` or is not one or more visible ASCII
	/// characters.
	BuildOutput,
	/// The declarations are invalid: they cannot be described to the build.
	Declarations,
	/// The build directory was built for other declarations than the binary
	/// has.
	DeclarationsMismatch,
	/// The declarations and the build directory's client views do not make a
	/// valid request plan.
	Plan,
}

impl StartupError {
	/// The stage of [`App::load`] that failed.
	pub fn kind(&self) -> StartupErrorKind {
		self.kind
	}

	fn from_message(kind: StartupErrorKind, message: String) -> Self {
		Self {
			kind,
			message,
			described: None,
		}
	}

	fn from_error(kind: StartupErrorKind, error: impl StdError + Send + Sync + 'static) -> Self {
		Self {
			kind,
			message: error.to_string(),
			described: Some(Box::new(error)),
		}
	}
}

impl<State: Send + Sync + 'static> App<State> {
	fn clone_declarations(&self) -> Declarations<State> {
		Declarations {
			task_middlewares: self.task_middlewares.clone(),
			view_loaders: self.view_loaders.clone(),
			api_endpoints: self.api_endpoints.clone(),
		}
	}

	/// The hash of the declarations that `vorma build` records in the build
	/// directory's metadata and that [`App::load`] compares. It exists for
	/// code that writes build metadata without `vorma build`, such as a test.
	#[cfg(feature = "__internal")]
	#[doc(hidden)]
	pub fn __declarations_hash(&self) -> Result<String, StartupError> {
		describe(&self.clone_declarations())
			.map(|description| description.declarations_hash)
			.map_err(|error| StartupError::from_error(StartupErrorKind::Declarations, error))
	}

	/// The build directory to read: the staged one that `vorma build` names in
	/// a check run, `dev` in the dist directory that `vorma dev` names in
	/// development, and `prod` in `dist_dir` otherwise.
	fn build_dir_to_read(&self) -> Result<PathBuf, StartupError> {
		if let Some(staged) = std::env::var_os(CHECK_BUILD_DIR_ENV_VAR) {
			return Ok(PathBuf::from(staged));
		}
		if is_dev() {
			return match std::env::var_os(DEV_DIST_DIR_ENV_VAR) {
				Some(value) if !value.is_empty() => {
					Ok(PathBuf::from(value).join(DEV_BUILD_DIR_NAME))
				}
				_ => Err(StartupError::from_message(
					StartupErrorKind::Config,
					format!(
						"`vorma dev` started this process without naming the dist directory in {DEV_DIST_DIR_ENV_VAR}"
					),
				)),
			};
		}
		if self.dist_dir.as_os_str().is_empty() {
			return Err(StartupError::from_message(
				StartupErrorKind::Config,
				"dist_dir is empty; set it to the dist directory that `vorma build` wrote the production build in"
					.to_owned(),
			));
		}
		Ok(self.dist_dir.join(PROD_BUILD_DIR_NAME))
	}

	/// Reads the build directory and returns the [`LoadedApp`] that makes the
	/// service and the static public assets layer. In production, the build
	/// never changes; in development, Vorma reads the build's metadata again
	/// for each request, and a request uses the build that it read from
	/// start to finish. It checks the
	/// declarations against each other, against the client views of the build
	/// directory's build, and against the declarations that the build directory
	/// was built for, and it reports every problem. A build directory built for
	/// other declarations does not load, in production and in development:
	/// run `vorma build` with `build.serverStartCmd` running this binary, and
	/// deploy the build directory that it writes together with the binary.
	/// A process runs only a build of its own mode: a development build loads
	/// only in a process that `vorma dev` started ([`crate::is_dev`]), and a
	/// production build loads only in a process that `vorma dev` did not
	/// start.
	///
	/// Call this on the first line of `main`, before the application reads
	/// secrets, connects to databases, or starts any other setup. When the
	/// `vorma` build runs the server binary to read its declarations, this
	/// writes them for the build and ends the process before it reads the build
	/// directory, so the build runs only the code that precedes this call.
	/// After the build writes the build directory, it runs the binary once more
	/// in a check run: this loads the build's build directory, ends the process
	/// with status 0 when it loads, and otherwise prints the problems on
	/// standard error and ends the process with status 1. Code that has to run
	/// before this call can use [`crate::is_build`] to tell a build's run of
	/// the binary from a run of the server. The declarations must not depend on
	/// [`crate::is_build`] because a build's declarations must match the
	/// server's.
	pub async fn load(self) -> Result<LoadedApp<State>, StartupError> {
		let declarations = self.clone_declarations();
		let description = describe(&declarations);
		write_contract_if_requested(&description);
		let loaded = self.load_build_dir(declarations, description).await;
		if std::env::var_os(CHECK_BUILD_DIR_ENV_VAR).is_some() {
			match loaded {
				Ok(_) => std::process::exit(0),
				Err(error) => {
					// The `Debug` output has the message and each cause.
					eprintln!("{error:?}");
					std::process::exit(1);
				}
			}
		}
		loaded
	}

	async fn load_build_dir(
		self,
		declarations: Declarations<State>,
		description: Result<DeclarationDescription, ContractError>,
	) -> Result<LoadedApp<State>, StartupError> {
		let description = match description {
			Ok(description) => description,
			Err(error) => {
				let snapshot = async {
					let build = BuildOutput::load(&self.build_dir_to_read().ok()?)
						.await
						.ok()?;
					build.read_snapshot().await.ok()
				}
				.await;
				return Err(report_description_error(error, &declarations, snapshot));
			}
		};
		let build_dir = self.build_dir_to_read()?;
		let build = BuildOutput::load(&build_dir)
			.await
			.map_err(|error| StartupError::from_error(StartupErrorKind::BuildOutput, error))?;
		let snapshot = build
			.read_snapshot()
			.await
			.map_err(|error| StartupError::from_error(StartupErrorKind::BuildOutput, error))?;
		match (snapshot.metadata.runtime_mode, is_dev()) {
			(RuntimeMode::Dev, false) => {
				return Err(StartupError::from_message(
					StartupErrorKind::BuildOutput,
					format!(
						"the build directory {} holds a development build, but `vorma dev` did not start this process, so it runs production builds only. Run `vorma build`, which writes the production build to `prod` in the dist directory.",
						build_dir.display()
					),
				));
			}
			(RuntimeMode::Prod, true) => {
				return Err(StartupError::from_message(
					StartupErrorKind::BuildOutput,
					format!(
						"the build directory {} holds a production build, but `vorma dev` started this process, so it runs development builds only. `vorma dev` writes the development build to `dev` in the dist directory that it names.",
						build_dir.display()
					),
				));
			}
			(RuntimeMode::Dev, true) | (RuntimeMode::Prod, false) => {}
		}
		if snapshot.metadata.declarations_hash != description.declarations_hash {
			return Err(StartupError::from_message(
				StartupErrorKind::DeclarationsMismatch,
				format!(
					"the build directory {} was built for other declarations than this binary has. Run `vorma build` with `build.serverStartCmd` running this binary, and deploy the build directory that it writes together with the binary.",
					build_dir.display()
				),
			));
		}
		let skew_protection = read_vercel_skew_protection()
			.map_err(|message| StartupError::from_message(StartupErrorKind::Config, message))?;
		self.finish_load(declarations, build, snapshot, skew_protection)
	}

	/// Loads the application for a test with a stand-in for the client build,
	/// without a build directory and without `vorma build`. The [`LoadedApp`]
	/// makes the same service as [`App::load`] makes, so a test sends requests
	/// to it, for example with `tower::ServiceExt::oneshot`, and reads the
	/// responses: the status, the headers, the cookies, the document, and the
	/// payload of a whole request, with the contributions of task middleware
	/// and of every loader merged.
	///
	/// It checks the declarations as [`App::load`] does and reports every
	/// problem, including the problems that otherwise appear only when
	/// `vorma build` reads the declarations.
	///
	/// The stand-in build has no client views, so a page has the positions of
	/// the declared view loaders only, and a pattern that only a client view
	/// has matches no page. The documents that the service renders name a
	/// client entry module that does not exist, so they cannot run in a
	/// browser. Use [`App::load`] in the application's `main`.
	///
	/// This does not read the build's, the development session's, or Vercel's
	/// environment variables: it never writes declarations for a build, never
	/// ends the process, always loads the stand-in as a production build, and
	/// takes skew protection from the [`TestBuild`].
	pub fn load_for_tests(self, build: TestBuild) -> Result<LoadedApp<State>, StartupError> {
		let client_build_id =
			ClientBuildId::parse(build.client_build_id.clone()).map_err(|problem| {
				StartupError::from_message(
					StartupErrorKind::BuildOutput,
					format!("the test build is invalid: {problem}"),
				)
			})?;
		let skew_protection = build.skew_protection.clone();
		let declarations = self.clone_declarations();
		let (declarations_hash, description_error) = match describe(&declarations) {
			Ok(description) => (description.declarations_hash, None),
			Err(error) => (String::new(), Some(error)),
		};
		let snapshot = Arc::new(BuildSnapshot::new(
			build.into_metadata(client_build_id, declarations_hash),
		));
		if let Some(error) = description_error {
			return Err(report_description_error(
				error,
				&declarations,
				Some(snapshot),
			));
		}
		let build = BuildOutput::from_snapshot(Arc::clone(&snapshot));
		self.finish_load(declarations, build, snapshot, skew_protection)
	}

	/// Compiles the request plan for a build that matches the declarations,
	/// and reports the plan's problems together with the search contracts
	/// that skew protection rejects.
	fn finish_load(
		self,
		declarations: Declarations<State>,
		build: BuildOutput,
		snapshot: Arc<BuildSnapshot>,
		skew_protection: SkewProtection,
	) -> Result<LoadedApp<State>, StartupError> {
		let plan = Plan::compile(&declarations, snapshot);
		let skew_protection_problems = match skew_protection {
			SkewProtection::Off => Vec::new(),
			SkewProtection::Vercel { .. } => declarations.find_skew_protection_search_problems(),
		};
		let plan = match (plan, skew_protection_problems.is_empty()) {
			(Ok(plan), true) => plan,
			(plan, _) => {
				let mut problems = plan.err().map_or_else(Vec::new, |error| error.problems);
				problems.extend(skew_protection_problems);
				return Err(StartupError::from_error(
					StartupErrorKind::Plan,
					PlanError { problems },
				));
			}
		};
		Ok(LoadedApp {
			declarations: Arc::new(declarations),
			head_rules: Arc::new(HeadRules::new(&self.head_dedupe_rules)),
			build: Arc::new(build),
			plan: Arc::new(CurrentPlan::new(Arc::new(plan))),
			max_request_body_size: self.max_request_body_size,
			skew_protection,
		})
	}
}

/// The startup error for declarations that cannot be described. When a build
/// is available, the plan's checks run against it as well, and their problems
/// that the description has not reported join the message so that one run
/// reports every problem.
fn report_description_error<State: Send + Sync + 'static>(
	error: ContractError,
	declarations: &Declarations<State>,
	snapshot: Option<Arc<BuildSnapshot>>,
) -> StartupError {
	let plan_error = snapshot.and_then(|snapshot| Plan::compile(declarations, snapshot).err());
	let mut message = error.to_string();
	if let Some(plan_error) = plan_error {
		let unreported: Vec<String> = plan_error
			.problems
			.into_iter()
			.filter(|problem| !error.problems().contains(problem))
			.collect();
		if !unreported.is_empty() {
			message.push('\n');
			message.push_str(
				&PlanError {
					problems: unreported,
				}
				.to_string(),
			);
		}
	}
	StartupError {
		kind: StartupErrorKind::Declarations,
		message,
		described: Some(Box::new(error)),
	}
}

/// An [`App`] that has read its build directory. It makes the service and the
/// static public assets layer that the application mounts.
pub struct LoadedApp<State> {
	declarations: Arc<Declarations<State>>,
	head_rules: Arc<HeadRules>,
	build: Arc<BuildOutput>,
	plan: Arc<CurrentPlan<State>>,
	max_request_body_size: usize,
	skew_protection: SkewProtection,
}

impl<State: Send + Sync + 'static> LoadedApp<State> {
	/// Makes the service that the application mounts. The service's state is
	/// the state of `task_runtime`, and the service creates each request's
	/// task context from `task_runtime`.
	///
	/// The application creates `task_runtime` and passes a clone so that code
	/// outside Vorma's requests, such as host routes and background jobs,
	/// creates task contexts from the same runtime and can run and invalidate
	/// the same shared tasks. Dropping the service does not close the runtime.
	/// The runtime closes when every clone is dropped or when the application
	/// calls [`TaskRuntime::close`].
	pub fn service(&self, task_runtime: TaskRuntime<State>) -> AppService<State> {
		AppService::new(AppServiceInner {
			declarations: Arc::clone(&self.declarations),
			head_rules: Arc::clone(&self.head_rules),
			build: Arc::clone(&self.build),
			plan: Arc::clone(&self.plan),
			max_request_body_size: self.max_request_body_size,
			tasks: task_runtime,
			skew_protection: self.skew_protection.clone(),
		})
	}

	/// Returns the [`PageHrefs`] for code outside a request, such as host
	/// routes and background jobs. They resolve targets against the build that
	/// Vorma most recently planned: the build that this app loaded, or, in
	/// development, the build that the latest request to the service used.
	/// While Vorma rejects the latest development build, every target returns
	/// an [`HrefError`](crate::HrefError).
	pub fn page_hrefs(&self) -> PageHrefs {
		PageHrefs::new(Arc::new(CurrentPlanTargetResolver {
			plan: Arc::clone(&self.plan),
			deployment_query_key: self.skew_protection.deployment_query_key(),
		}))
	}

	/// Makes the layer that serves the build directory's static public assets
	/// and Vorma's browser modules.
	pub fn static_public_assets_layer(&self) -> StaticPublicAssetsLayer {
		StaticPublicAssetsLayer::new(Arc::clone(&self.build))
	}
}
