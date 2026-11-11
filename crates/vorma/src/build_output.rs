//! The build output that `vorma build` or `vorma dev` writes into the build
//! directory: the build metadata and the published static public assets.
//!
//! In production the build never changes. In development, `vorma dev`
//! republishes the metadata, Vorma reads the metadata again for each request,
//! and a request that starts after a publication uses the new build from
//! start to finish.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use http::HeaderValue;
use primitives_rs::html::escaping::escape_style_text_content;
use primitives_rs::http::csp::{
	csp_sha256_inline_content_hash_source, csp_sha256_inline_hash_sources,
};
use serde::{Deserialize, Deserializer};
use tokio::sync::Mutex;

use crate::search::DeploymentQueryKey;

/// The format of `metadata.json`. The server refuses a file of another
/// format. The `vorma` package writes the same number as `formatVersion`, in
/// `BUILD_METADATA_FORMAT_VERSION`.
pub const BUILD_METADATA_FORMAT_VERSION: u32 = 1;

/// One browser module's build-to-runtime projection. A view module's default
/// export is its view. The metadata leaves out a list that is empty.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClientModule {
	pub(crate) import_url: String,
	#[serde(default)]
	pub(crate) dep_urls: Vec<String>,
	#[serde(default)]
	pub(crate) css_bundle_urls: Vec<String>,
	/// The static public assets that the module's JavaScript references. A page
	/// preloads the images and fonts among them.
	#[serde(default)]
	pub(crate) referenced_static_public_asset_urls: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RuntimeMode {
	Prod,
	Dev,
}

/// A build ID as Vorma sends it in `x-vorma-client-build-id`: one or more
/// visible ASCII characters, `!` through `~`, so it has no whitespace. The
/// browser client sends the ID back in the same header and compares the
/// header's text with its own ID, and a header carries these characters
/// unchanged.
///
/// The ID cannot be `_`, because a page read with the query parameter
/// `vorma-json=_` names no build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClientBuildId {
	text: String,
	header_value: HeaderValue,
}

/// The `vorma-json` value with which a page read names no build.
pub(crate) const UNNAMED_BUILD_QUERY_VALUE: &str = "_";

impl ClientBuildId {
	pub(crate) fn parse(text: String) -> Result<Self, String> {
		if text == UNNAMED_BUILD_QUERY_VALUE {
			return Err(format!(
				"the client build ID cannot be {text:?}, because a page read with `vorma-json={text}` names no build"
			));
		}
		match HeaderValue::from_str(&text) {
			Ok(header_value) if is_visible_ascii(&text) => Ok(Self { text, header_value }),
			_ => Err(format!(
				"the client build ID {text:?} is not one or more visible ASCII characters"
			)),
		}
	}

	pub(crate) fn as_str(&self) -> &str {
		&self.text
	}

	pub(crate) fn header_value(&self) -> &HeaderValue {
		&self.header_value
	}
}

/// Whether `text` is one or more visible ASCII characters, `!` through `~`.
fn is_visible_ascii(text: &str) -> bool {
	!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_graphic())
}

impl<'de> Deserialize<'de> for ClientBuildId {
	fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
		Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
	}
}

/// Everything one build determines about its own output.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BuildMetadata {
	pub(crate) client_build_id: ClientBuildId,
	/// The hash of the declarations that the build read from the server
	/// binary.
	pub(crate) declarations_hash: String,
	pub(crate) runtime_mode: RuntimeMode,
	/// The URL path under which the server serves the files of the build
	/// directory's `public` directory, such as `/` or `/static/`.
	pub(crate) static_public_assets_serve_url_path_prefix: String,
	/// The names of the files in the build directory's `public` directory
	/// that the static public assets layer serves, each with the immutable
	/// cache policy in production. The layer serves no other file of the
	/// directory.
	pub(crate) static_public_asset_file_names: Vec<String>,
	/// Each static public asset's source key and its published URL path.
	pub(crate) static_public_asset_urls: HashMap<String, String>,
	/// The critical CSS. [`BuildSnapshot::new`] escapes it for a `<style>`
	/// element.
	pub(crate) critical_css: String,
	/// The development HTML that Vite's plugins add to the head; absent from
	/// production metadata.
	pub(crate) dev_head_html: Option<String>,
	/// The development HTML that Vite's plugins add to the start of the body;
	/// absent from production metadata.
	pub(crate) dev_body_prepend_html: Option<String>,
	/// The development HTML that Vite's plugins add to the end of the body;
	/// absent from production metadata.
	pub(crate) dev_body_html: Option<String>,
	/// What the Content-Security-Policy needs from `vorma dev`; absent from
	/// production metadata.
	pub(crate) dev_csp: Option<DevCsp>,
	pub(crate) client_entry: ClientModule,
	/// Each view's module, by the pattern that the view states.
	pub(crate) client_views: BTreeMap<String, ClientModule>,
}

/// The development session's values that only `vorma dev` knows.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DevCsp {
	/// The origin from which pages load browser modules, styles, images, and
	/// fonts, such as `http://127.0.0.1:5173`.
	pub(crate) module_server_origin: String,
	/// The origin of the module server's WebSocket, such as
	/// `ws://127.0.0.1:5173`.
	pub(crate) web_socket_origin: String,
	/// The value of Vite's `html.cspNonce`, which `vorma dev` chooses unless the
	/// application sets it.
	pub(crate) nonce: Option<String>,
}

/// Each static public asset's published URL by its source file name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaticPublicAssets(HashMap<String, String>);

impl StaticPublicAssets {
	/// Returns the published URL of a file in the static public asset source
	/// directory, such as `"favicon.svg"`.
	pub fn get(&self, src_file_name: &str) -> Option<&str> {
		self.0.get(src_file_name).map(String::as_str)
	}
}

/// Builds the assets from pairs of a source file name and a published URL,
/// for example in a test that calls a handler directly.
impl<Name: Into<String>, Url: Into<String>, const COUNT: usize> From<[(Name, Url); COUNT]>
	for StaticPublicAssets
{
	fn from(pairs: [(Name, Url); COUNT]) -> Self {
		pairs.into_iter().collect()
	}
}

impl<Name: Into<String>, Url: Into<String>> FromIterator<(Name, Url)> for StaticPublicAssets {
	fn from_iter<Pairs: IntoIterator<Item = (Name, Url)>>(pairs: Pairs) -> Self {
		Self(
			pairs
				.into_iter()
				.map(|(name, url)| (name.into(), url.into()))
				.collect(),
		)
	}
}

/// The stand-in for a client build that [`App::load_for_tests`](crate::App::load_for_tests)
/// loads so that a test can send requests to the application's service
/// without running `vorma build`.
///
/// The stand-in is a production build that exists only in memory. It has no
/// client views, no critical CSS, and no files, and its client entry module
/// does not exist.
#[derive(Clone, Debug)]
pub struct TestBuild {
	/// The build's ID, which handlers read as `latest_known_client_build_id`
	/// and which a request's client build ID is compared with. As a build's ID,
	/// it must be one or more visible ASCII characters, `!` through `~`, so it
	/// has no whitespace, and it cannot be `_`, with which a page read names no
	/// build.
	pub client_build_id: String,
	/// The published URL of each static public asset by its source file name,
	/// which handlers read as `static_public_assets`. The service serves no
	/// file for these URLs.
	pub static_public_assets: StaticPublicAssets,
	/// Whether the test's app runs with Vercel's skew protection, which
	/// [`App::load`](crate::App::load) reads from the process environment.
	pub skew_protection: SkewProtection,
}

/// Whether Vercel's skew protection is on. [`App::load`](crate::App::load)
/// reads it from the environment variables `VERCEL_SKEW_PROTECTION_ENABLED`
/// and `VERCEL_DEPLOYMENT_ID`, and a [`TestBuild`] states it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkewProtection {
	/// Skew protection is off.
	Off,
	/// Skew protection is on for the Vercel deployment with this ID.
	Vercel {
		/// The deployment ID, as `VERCEL_DEPLOYMENT_ID` holds it.
		deployment_id: VercelDeploymentId,
	},
}

impl SkewProtection {
	/// The deployment ID while skew protection is on.
	pub(crate) fn deployment_id(&self) -> Option<&str> {
		match self {
			Self::Off => None,
			Self::Vercel { deployment_id } => Some(deployment_id.as_str()),
		}
	}

	/// Whether the query key `dpl` belongs to skew protection.
	pub(crate) fn deployment_query_key(&self) -> DeploymentQueryKey {
		match self {
			Self::Off => DeploymentQueryKey::Ordinary,
			Self::Vercel { .. } => DeploymentQueryKey::Reserved,
		}
	}
}

/// The ID of a Vercel deployment: one or more visible ASCII characters, `!`
/// through `~`, so it has no whitespace. The browser client sends it in the
/// `dpl` query parameter and the `x-deployment-id` header, which carries these
/// characters unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VercelDeploymentId(String);

impl VercelDeploymentId {
	/// The deployment ID, or `None` for text that is not one or more visible
	/// ASCII characters.
	pub fn new(deployment_id: impl Into<String>) -> Option<Self> {
		let deployment_id = deployment_id.into();
		is_visible_ascii(&deployment_id).then_some(Self(deployment_id))
	}

	/// The deployment ID's text.
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl TestBuild {
	/// The metadata of the stand-in build with this client build ID, built
	/// for the declarations with this hash.
	pub(crate) fn into_metadata(
		self,
		client_build_id: ClientBuildId,
		declarations_hash: String,
	) -> BuildMetadata {
		BuildMetadata {
			client_build_id,
			declarations_hash,
			runtime_mode: RuntimeMode::Prod,
			static_public_assets_serve_url_path_prefix: "/".to_owned(),
			static_public_asset_file_names: Vec::new(),
			static_public_asset_urls: self.static_public_assets.0,
			critical_css: String::new(),
			dev_head_html: None,
			dev_body_prepend_html: None,
			dev_body_html: None,
			dev_csp: None,
			client_entry: ClientModule {
				import_url: "/vorma-test-build-client-entry.js".to_owned(),
				dep_urls: Vec::new(),
				css_bundle_urls: Vec::new(),
				referenced_static_public_asset_urls: Vec::new(),
			},
			client_views: BTreeMap::new(),
		}
	}
}

/// For each Content-Security-Policy directive, every source that Vorma's own
/// content needs. Each source is complete and quoted where CSP requires it,
/// such as `'self'`, `'sha256-…'`, or `http://127.0.0.1:5173`, so an
/// application can put a list into its directive as it is. A directive that
/// Vorma's content does not use has an empty list.
///
/// The lists are the same for every request of one build or development
/// session. The only nonce is the development session's, described below.
///
/// In production:
///
/// - `script_src` contains `'self'`, for the browser modules that pages load,
///   plus any external origins (`scheme://authority` or scheme-relative
///   `authority`) of built script URLs.
/// - `style_src` contains `'self'`, for the stylesheets that pages load, the
///   hash of any critical CSS that pages inline, and any external origins of
///   built stylesheet URLs.
/// - `connect_src` contains `'self'`, for the navigation and endpoint requests
///   that the browser client makes.
/// - `img_src` and `font_src` contain `'self'`, for the files that built
///   stylesheets reference, plus any external origins of built image and font
///   asset URLs.
/// - `worker_src` is empty.
///
/// Under `vorma dev`, the lists also contain the module server's origin in
/// `script_src`, `style_src`, `img_src`, and `font_src`; the module server's
/// WebSocket origin in `connect_src`; `blob:` in `worker_src`, for Vite's
/// reconnect worker; the hashes of the development head's inline content; and
/// the nonce of Vite's `html.cspNonce` in `script_src` and `style_src`.
/// `vorma dev` chooses a random nonce for each session and sets it as
/// `html.cspNonce` on its Vite server, so a strict `style-src` admits the live
/// critical CSS update with no Vite setting. An `html.cspNonce` in the
/// application's Vite configuration replaces it. Production lists contain no
/// nonce.
///
/// An endpoint call has no inline content, so the middleware context of an
/// endpoint call has empty lists.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CspSources {
	/// Sources for `script-src`.
	pub script_src: Vec<String>,
	/// Sources for `style-src`.
	pub style_src: Vec<String>,
	/// Sources for `connect-src`.
	pub connect_src: Vec<String>,
	/// Sources for `img-src`.
	pub img_src: Vec<String>,
	/// Sources for `font-src`.
	pub font_src: Vec<String>,
	/// Sources for `worker-src`.
	pub worker_src: Vec<String>,
}

/// One build's metadata with the values that every request served from it
/// uses.
#[derive(Debug)]
pub(crate) struct BuildSnapshot {
	pub(crate) metadata: BuildMetadata,
	/// The names of the files that the static public assets layer serves.
	pub(crate) static_public_asset_file_names: HashSet<String>,
	pub(crate) static_public_assets: Arc<StaticPublicAssets>,
	/// The sources that a page response needs for Vorma's own content.
	pub(crate) page_csp_sources: Arc<CspSources>,
}

/// The quoted hash source of the content of an inline `<style>` or `<script>`
/// element that Vorma writes, as a browser hashes the text that it builds from
/// that content.
fn to_csp_hash(content: &str) -> String {
	quote_csp_source(&csp_sha256_inline_content_hash_source(content))
}

fn quote_csp_source(source: &str) -> String {
	format!("'{source}'")
}

impl BuildSnapshot {
	/// The snapshot of a build. The critical CSS is escaped once for its
	/// `<style>` element, and its CSP hash is the hash of the text that a
	/// browser builds from the escaped text, which the document carries: each
	/// CR LF pair and each other CR become LF, and each NUL becomes U+FFFD.
	pub(crate) fn new(mut metadata: BuildMetadata) -> Self {
		metadata.critical_css = escape_style_text_content(&metadata.critical_css);
		Self {
			static_public_asset_file_names: metadata
				.static_public_asset_file_names
				.iter()
				.cloned()
				.collect(),
			static_public_assets: Arc::new(StaticPublicAssets(
				metadata.static_public_asset_urls.clone(),
			)),
			page_csp_sources: Arc::new(read_csp_sources(&metadata)),
			metadata,
		}
	}
}

fn read_csp_sources(metadata: &BuildMetadata) -> CspSources {
	let own_origin = || vec!["'self'".to_owned()];
	let mut sources = CspSources {
		script_src: own_origin(),
		style_src: own_origin(),
		connect_src: own_origin(),
		img_src: own_origin(),
		font_src: own_origin(),
		worker_src: Vec::new(),
	};
	if !metadata.critical_css.is_empty() {
		sources.style_src.push(to_csp_hash(&metadata.critical_css));
	}
	for html in [
		&metadata.dev_head_html,
		&metadata.dev_body_prepend_html,
		&metadata.dev_body_html,
	]
	.into_iter()
	.flatten()
	{
		// The hashes of the inline script and style contents of the
		// development HTML that Vite and Vorma generate.
		let inline = csp_sha256_inline_hash_sources(html);
		sources.script_src.extend(
			inline
				.script_src
				.iter()
				.map(|source| quote_csp_source(source)),
		);
		sources.style_src.extend(
			inline
				.style_src
				.iter()
				.map(|source| quote_csp_source(source)),
		);
	}
	if let Some(dev_csp) = &metadata.dev_csp {
		let origin = &dev_csp.module_server_origin;
		for list in [
			&mut sources.script_src,
			&mut sources.style_src,
			&mut sources.img_src,
			&mut sources.font_src,
		] {
			list.push(origin.clone());
		}
		sources.connect_src.push(dev_csp.web_socket_origin.clone());
		sources.worker_src.push("blob:".to_owned());
		if let Some(nonce) = &dev_csp.nonce {
			let source = format!("'nonce-{nonce}'");
			sources.script_src.push(source.clone());
			sources.style_src.push(source);
		}
	}
	for module in std::iter::once(&metadata.client_entry).chain(metadata.client_views.values()) {
		for script_url in std::iter::once(&module.import_url).chain(module.dep_urls.iter()) {
			if let Some(origin) = extract_external_csp_source(script_url) {
				push_unique_csp_source(&mut sources.script_src, origin);
			}
		}
		for style_url in &module.css_bundle_urls {
			if let Some(origin) = extract_external_csp_source(style_url) {
				push_unique_csp_source(&mut sources.style_src, origin);
			}
		}
		for asset_url in &module.referenced_static_public_asset_urls {
			record_static_public_asset_csp_source(&mut sources, asset_url);
		}
	}
	let mut static_asset_entries: Vec<(&str, &str)> = metadata
		.static_public_asset_urls
		.iter()
		.map(|(key, url)| (key.as_str(), url.as_str()))
		.collect();
	static_asset_entries.sort_unstable_by(|left, right| left.0.cmp(right.0));
	for (_, asset_url) in static_asset_entries {
		record_static_public_asset_csp_source(&mut sources, asset_url);
	}
	sources
}

fn push_unique_csp_source(sources: &mut Vec<String>, candidate: String) {
	if !sources.iter().any(|existing| existing == &candidate) {
		sources.push(candidate);
	}
}

fn extract_external_csp_source(url: &str) -> Option<String> {
	if let Some((scheme, rest)) = url.split_once("://") {
		if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
			return None;
		}
		let authority = extract_host_authority(rest)?;
		return Some(format!("{}://{authority}", scheme.to_ascii_lowercase()));
	}
	if let Some(rest) = url.strip_prefix("//") {
		let authority = extract_host_authority(rest)?;
		return Some(authority.to_owned());
	}
	None
}

fn extract_host_authority(rest: &str) -> Option<&str> {
	let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
	let host_port = authority
		.rsplit_once('@')
		.map_or(authority, |(_, host_port)| host_port);
	(!host_port.is_empty()).then_some(host_port)
}

fn record_static_public_asset_csp_source(sources: &mut CspSources, url: &str) {
	let Some(origin) = extract_external_csp_source(url) else {
		return;
	};
	match crate::payload::static_public_asset_destination(url) {
		Some(crate::payload::StaticPublicAssetDestination::Font) => {
			push_unique_csp_source(&mut sources.font_src, origin);
		}
		Some(crate::payload::StaticPublicAssetDestination::Image) => {
			push_unique_csp_source(&mut sources.img_src, origin);
		}
		None => {
			let path = url.split(['?', '#']).next().unwrap_or(url);
			let extension = path.rsplit_once('.').map_or("", |(_, extension)| extension);
			if extension.eq_ignore_ascii_case("css") {
				push_unique_csp_source(&mut sources.style_src, origin);
			} else if extension.eq_ignore_ascii_case("js") || extension.eq_ignore_ascii_case("mjs")
			{
				push_unique_csp_source(&mut sources.script_src, origin);
			}
		}
	}
}

/// The build output is missing or unreadable.
#[derive(Debug)]
pub(crate) struct BuildOutputError {
	pub(crate) message: String,
	pub(crate) source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl fmt::Display for BuildOutputError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(&self.message)
	}
}

impl std::error::Error for BuildOutputError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		self.source
			.as_deref()
			.map(|source| source as &(dyn std::error::Error + 'static))
	}
}

/// The build directory and its current build.
pub(crate) struct BuildOutput {
	source: BuildSource,
}

enum BuildSource {
	Prod {
		public_dir: PathBuf,
		snapshot: Arc<BuildSnapshot>,
	},
	Dev {
		metadata_path: PathBuf,
		public_dir: PathBuf,
		current: Mutex<DevBuild>,
	},
	InMemory {
		snapshot: Arc<BuildSnapshot>,
	},
}

struct ParsedBuild {
	text: String,
	snapshot: Arc<BuildSnapshot>,
}

/// A rejected observation retains the last parsed build and its exact failure.
enum DevBuild {
	Ready {
		current: Arc<ParsedBuild>,
	},
	Rejected {
		current: Arc<ParsedBuild>,
		text: String,
		error: Arc<BuildOutputError>,
	},
}

impl DevBuild {
	fn current(&self) -> &Arc<ParsedBuild> {
		match self {
			Self::Ready { current } | Self::Rejected { current, .. } => current,
		}
	}
}

impl BuildOutput {
	/// Reads the build output that the build directory holds.
	pub(crate) async fn load(build_dir: &Path) -> Result<Self, BuildOutputError> {
		let metadata_path = build_dir.join("metadata.json");
		let text = tokio::fs::read_to_string(&metadata_path)
			.await
			.map_err(|error| BuildOutputError {
				message: format!(
					"Vorma found no metadata.json in the build directory {}. Run vorma build or vorma dev before starting the application.",
					build_dir.display()
				),
				source: Some(Box::new(error)),
			})?;
		let metadata = parse_metadata(&metadata_path, &text)?;
		let snapshot = Arc::new(BuildSnapshot::new(metadata));
		let public_dir = build_dir.join("public");
		Ok(Self {
			source: match snapshot.metadata.runtime_mode {
				RuntimeMode::Prod => BuildSource::Prod {
					public_dir,
					snapshot,
				},
				RuntimeMode::Dev => BuildSource::Dev {
					metadata_path,
					public_dir,
					current: Mutex::new(DevBuild::Ready {
						current: Arc::new(ParsedBuild { text, snapshot }),
					}),
				},
			},
		})
	}

	/// A production build that exists only in memory. It has no build
	/// directory, so it has no file of a static public asset.
	pub(crate) fn from_snapshot(snapshot: Arc<BuildSnapshot>) -> Self {
		Self {
			source: BuildSource::InMemory { snapshot },
		}
	}

	/// The directory that holds the published static public assets.
	pub(crate) fn public_dir(&self) -> Option<&Path> {
		match &self.source {
			BuildSource::Prod { public_dir, .. } | BuildSource::Dev { public_dir, .. } => {
				Some(public_dir)
			}
			BuildSource::InMemory { .. } => None,
		}
	}

	/// The current build. In development, this rereads the metadata file and
	/// makes a new snapshot when its text changed.
	///
	/// A failure is reported to the log. Metadata text that is not valid is
	/// reported once, and later reads of the same text return the same error
	/// without parsing it again. A file that cannot be read is reported at
	/// every read.
	pub(crate) async fn read_snapshot(&self) -> Result<Arc<BuildSnapshot>, Arc<BuildOutputError>> {
		self.read_snapshot_then(|snapshot| snapshot).await
	}

	/// Reads the current build as [`Self::read_snapshot`] does and passes the
	/// result to `decide` before another read of the development build can
	/// start, so that the decisions about development builds follow the order
	/// of their reads.
	pub(crate) async fn read_snapshot_then<Decision>(
		&self,
		decide: impl FnOnce(Result<Arc<BuildSnapshot>, Arc<BuildOutputError>>) -> Decision,
	) -> Decision {
		let (metadata_path, state) = match &self.source {
			BuildSource::Prod { snapshot, .. } | BuildSource::InMemory { snapshot } => {
				return decide(Ok(Arc::clone(snapshot)));
			}
			BuildSource::Dev {
				metadata_path,
				current,
				..
			} => (metadata_path, current),
		};
		let mut state = state.lock().await;
		let snapshot = read_dev_snapshot(metadata_path, &mut state).await;
		decide(snapshot)
	}
}

/// Reads the development build's metadata file and updates the state of the
/// development build, which the caller holds locked.
async fn read_dev_snapshot(
	metadata_path: &Path,
	state: &mut DevBuild,
) -> Result<Arc<BuildSnapshot>, Arc<BuildOutputError>> {
	let text = match tokio::fs::read_to_string(metadata_path).await {
		Ok(text) => text,
		Err(error) => {
			return Err(report_build_output_error(BuildOutputError {
				message: format!("Vorma could not read {}", metadata_path.display()),
				source: Some(Box::new(error)),
			}));
		}
	};
	if text == state.current().text {
		let current = Arc::clone(state.current());
		let snapshot = Arc::clone(&current.snapshot);
		*state = DevBuild::Ready { current };
		return Ok(snapshot);
	}
	if let DevBuild::Rejected {
		text: failed_text,
		error,
		..
	} = &*state
		&& *failed_text == text
	{
		return Err(Arc::clone(error));
	}
	match parse_metadata(metadata_path, &text) {
		Ok(metadata) => {
			let snapshot = Arc::new(BuildSnapshot::new(metadata));
			*state = DevBuild::Ready {
				current: Arc::new(ParsedBuild {
					text,
					snapshot: Arc::clone(&snapshot),
				}),
			};
			Ok(snapshot)
		}
		Err(error) => {
			let error = report_build_output_error(error);
			*state = DevBuild::Rejected {
				current: Arc::clone(state.current()),
				text,
				error: Arc::clone(&error),
			};
			Err(error)
		}
	}
}

fn report_build_output_error(error: BuildOutputError) -> Arc<BuildOutputError> {
	tracing::error!(
		error = &error as &(dyn std::error::Error + 'static),
		"Vorma could not read the build"
	);
	Arc::new(error)
}

/// The only field that every format of the metadata file shares.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MetadataFormat {
	format_version: Option<u32>,
}

/// The client build ID of a metadata file, as text.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MetadataClientBuildId {
	client_build_id: String,
}

fn parse_metadata(path: &Path, text: &str) -> Result<BuildMetadata, BuildOutputError> {
	let not_metadata = |error: serde_json::Error| BuildOutputError {
		message: format!("{} is not Vorma build metadata", path.display()),
		source: Some(Box::new(error)),
	};
	let format: MetadataFormat = serde_json::from_str(text).map_err(not_metadata)?;
	if format.format_version != Some(BUILD_METADATA_FORMAT_VERSION) {
		let found = format
			.format_version
			.map_or_else(|| "none".to_owned(), |version| version.to_string());
		return Err(BuildOutputError {
			message: format!(
				"{} has build metadata format version {found}, but this version of the vorma crate reads format version {BUILD_METADATA_FORMAT_VERSION}. Rebuild with matching versions of the vorma package and the vorma crate.",
				path.display()
			),
			source: None,
		});
	}
	serde_json::from_str(text).map_err(|error| {
		// A client build ID that cannot be a header value is named in the
		// message.
		match serde_json::from_str::<MetadataClientBuildId>(text)
			.map(|metadata| ClientBuildId::parse(metadata.client_build_id))
		{
			Ok(Err(problem)) => BuildOutputError {
				message: format!("{} is invalid: {problem}", path.display()),
				source: None,
			},
			Ok(Ok(_)) | Err(_) => not_metadata(error),
		}
	})
}
