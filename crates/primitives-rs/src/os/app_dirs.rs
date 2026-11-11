//! User and application directory locations. Enable `app-dirs`.
//!
//! [`BaseDirs::discover`] uses the `dirs` crate for native user locations. Linux
//! uses XDG config, data, state, cache, and runtime locations. macOS uses
//! `Library/Application Support` for config and data and `Library/Caches` for
//! cache. Windows uses roaming config/data and local cache locations. Platforms
//! without a distinct state or runtime location return `None` for that location.
//!
//! On Linux, config and data searches include the user location first, followed
//! by `XDG_CONFIG_DIRS` or `XDG_DATA_DIRS` in order. Unset or empty search variables
//! use `/etc/xdg` and `/usr/local/share:/usr/share`, respectively. Relative XDG
//! paths are ignored, as required by the [XDG specification](https://specifications.freedesktop.org/basedir/latest/).
//! Other platforms have no system search locations in this facility.
//!
//! [`AppDirName`] is one literal filename, such as `org.example.editor`. Its case
//! and spelling are preserved on every platform. [`BaseDirs::for_app`] appends
//! that name to each available base location and constructs ordered config/data
//! search paths without duplicate paths. The first search path is the user path
//! when one is available. Applications choose how to combine files from different
//! locations; this module does not read or merge configuration files.
//!
//! Discovery does not create or open directories, check their existence, or verify
//! filesystem ownership or permissions. In particular, a discovered runtime path
//! is not evidence that its permissions or session lifetime meet XDG requirements.
//! There is no fallback runtime directory. Discovery reads the environment when
//! called; the returned paths do not change when the environment later changes.
//! [`BaseDirs::new`] accepts explicit locations for portable installations,
//! profiles, and deterministic verification without changing process environment.
//!
//! ```no_run
//! use primitives_rs::os::app_dirs::{AppDirName, BaseDirs};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let bases = BaseDirs::discover()?;
//! let app = bases.for_app(&AppDirName::new("org.example.editor")?);
//! let config_path = app.config_dir().ok_or("config dir unavailable")?;
//! # #[cfg(feature = "fs")]
//! let config = primitives_rs::fs::Dir::open(config_path)?;
//! // File operations use the selected directory and the application's permissions policy.
//! for candidate_dir in app.config_search_dirs() {
//!     // The application can inspect these locations in preference order.
//!     # let _ = candidate_dir;
//! }
//! # let _ = config_path;
//! # Ok(())
//! # }
//! ```

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

/// One literal application directory name, without separators, `.`/`..`, or NUL.
///
/// Both `/` and `\` are rejected on every platform. Parsing checks native path
/// components; filesystem-specific length and reserved-name rules apply when
/// opening or creating the directory. Parsing preserves case and spelling.
/// Use a stable filesystem name rather than a localized display name.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AppDirName(OsString);

impl AppDirName {
	/// Parses a single native filename and preserves its exact spelling.
	pub fn new(name: impl AsRef<OsStr>) -> Result<Self, InvalidAppDirName> {
		let name = name.as_ref();
		if name
			.as_encoded_bytes()
			.iter()
			.any(|byte| matches!(byte, 0 | b'/' | b'\\'))
		{
			return Err(InvalidAppDirName);
		}
		let mut components = Path::new(name).components();
		if !matches!(components.next(), Some(Component::Normal(part)) if part == name)
			|| components.next().is_some()
		{
			return Err(InvalidAppDirName);
		}
		Ok(Self(name.to_owned()))
	}

	/// Returns the unchanged directory name.
	pub fn as_os_str(&self) -> &OsStr {
		&self.0
	}
}

/// An application directory name was not a single ordinary filename.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("expected one application directory name without separators, dot components, or NUL")]
pub struct InvalidAppDirName;

/// Explicit user locations and ordered system config/data search locations.
///
/// `None` means unavailable; an empty system list means no system search paths.
/// Supplied paths must be absolute and contain no NUL bytes. Paths need not exist.
#[derive(Clone, Debug)]
pub struct BaseDirPaths {
	/// The user's home directory.
	pub home_dir: Option<PathBuf>,
	/// The user-specific configuration location.
	pub config_dir: Option<PathBuf>,
	/// The user-specific persistent application data location.
	pub data_dir: Option<PathBuf>,
	/// The user-specific disposable cache location.
	pub cache_dir: Option<PathBuf>,
	/// The user-specific persistent local state location, when provided by the platform.
	pub state_dir: Option<PathBuf>,
	/// The user-specific session runtime location, when provided by the platform.
	pub runtime_dir: Option<PathBuf>,
	/// Additional configuration locations, in descending preference order.
	pub system_config_dirs: Vec<PathBuf>,
	/// Additional data locations, in descending preference order.
	pub system_data_dirs: Vec<PathBuf>,
}

/// The kind of location that failed absolute-path validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirPathKind {
	/// The home location.
	Home,
	/// The user configuration location.
	Config,
	/// The user data location.
	Data,
	/// The user cache location.
	Cache,
	/// The user state location.
	State,
	/// The user runtime location.
	Runtime,
	/// A system configuration search location.
	SystemConfig,
	/// A system data search location.
	SystemData,
}

/// A supplied or discovered location was relative or contained a NUL byte.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{kind:?} directory must be an absolute path without NUL bytes")]
pub struct InvalidDirPath {
	/// The invalid location's kind.
	pub kind: DirPathKind,
}

/// Validated, immutable user locations and system search paths.
#[derive(Clone, Debug)]
pub struct BaseDirs(BaseDirPaths);

impl BaseDirs {
	/// Validates explicit locations without reading the environment or filesystem.
	pub fn new(paths: BaseDirPaths) -> Result<Self, InvalidDirPath> {
		for (kind, path) in [
			(DirPathKind::Home, &paths.home_dir),
			(DirPathKind::Config, &paths.config_dir),
			(DirPathKind::Data, &paths.data_dir),
			(DirPathKind::Cache, &paths.cache_dir),
			(DirPathKind::State, &paths.state_dir),
			(DirPathKind::Runtime, &paths.runtime_dir),
		] {
			if let Some(path) = path {
				check_absolute_path(path, kind)?;
			}
		}
		for (kind, paths) in [
			(DirPathKind::SystemConfig, &paths.system_config_dirs),
			(DirPathKind::SystemData, &paths.system_data_dirs),
		] {
			for path in paths {
				check_absolute_path(path, kind)?;
			}
		}
		Ok(Self(paths))
	}

	/// Discovers platform user locations and Linux XDG search paths.
	///
	/// Missing locations remain `None`; a relative home directory is an error.
	/// No directories are created and no process-global state is changed.
	pub fn discover() -> Result<Self, InvalidDirPath> {
		#[cfg(target_os = "linux")]
		let (system_config_dirs, system_data_dirs) = (
			xdg_search_dirs("XDG_CONFIG_DIRS", "/etc/xdg"),
			xdg_search_dirs("XDG_DATA_DIRS", "/usr/local/share:/usr/share"),
		);
		#[cfg(not(target_os = "linux"))]
		let (system_config_dirs, system_data_dirs) = (Vec::new(), Vec::new());
		Self::new(BaseDirPaths {
			home_dir: dirs::home_dir(),
			config_dir: dirs::config_dir(),
			data_dir: dirs::data_dir(),
			cache_dir: dirs::cache_dir(),
			state_dir: dirs::state_dir(),
			runtime_dir: dirs::runtime_dir(),
			system_config_dirs,
			system_data_dirs,
		})
	}

	/// Returns the validated base locations without changing their order.
	pub fn paths(&self) -> &BaseDirPaths {
		&self.0
	}

	/// Returns the home location, which is independent of application namespaces.
	pub fn home_dir(&self) -> Option<&Path> {
		self.0.home_dir.as_deref()
	}

	/// Appends the exact name to each available location and prepares search order.
	pub fn for_app(&self, name: &AppDirName) -> AppDirs {
		let append = |path: &Option<PathBuf>| path.as_ref().map(|path| path.join(name.as_os_str()));
		let config_dir = append(&self.0.config_dir);
		let data_dir = append(&self.0.data_dir);
		AppDirs {
			config_search_dirs: application_search_dirs(
				&config_dir,
				&self.0.system_config_dirs,
				name,
			),
			data_search_dirs: application_search_dirs(&data_dir, &self.0.system_data_dirs, name),
			config_dir,
			data_dir,
			cache_dir: append(&self.0.cache_dir),
			state_dir: append(&self.0.state_dir),
			runtime_dir: append(&self.0.runtime_dir),
		}
	}
}

/// Application-specific write locations and ordered config/data search paths.
#[derive(Clone, Debug)]
pub struct AppDirs {
	config_dir: Option<PathBuf>,
	data_dir: Option<PathBuf>,
	cache_dir: Option<PathBuf>,
	state_dir: Option<PathBuf>,
	runtime_dir: Option<PathBuf>,
	config_search_dirs: Vec<PathBuf>,
	data_search_dirs: Vec<PathBuf>,
}

impl AppDirs {
	/// Returns the user-specific configuration location.
	pub fn config_dir(&self) -> Option<&Path> {
		self.config_dir.as_deref()
	}
	/// Returns the user-specific persistent data location.
	pub fn data_dir(&self) -> Option<&Path> {
		self.data_dir.as_deref()
	}
	/// Returns the user-specific disposable cache location.
	pub fn cache_dir(&self) -> Option<&Path> {
		self.cache_dir.as_deref()
	}
	/// Returns the platform's distinct local state location, when available.
	pub fn state_dir(&self) -> Option<&Path> {
		self.state_dir.as_deref()
	}
	/// Returns the platform's session runtime location, when available.
	pub fn runtime_dir(&self) -> Option<&Path> {
		self.runtime_dir.as_deref()
	}
	/// Iterates configuration locations in preference order without duplicate paths.
	/// These paths have not been checked for existence or readability.
	pub fn config_search_dirs(&self) -> impl ExactSizeIterator<Item = &Path> {
		self.config_search_dirs.iter().map(PathBuf::as_path)
	}
	/// Iterates data locations in preference order without duplicate paths.
	/// These paths have not been checked for existence or readability.
	pub fn data_search_dirs(&self) -> impl ExactSizeIterator<Item = &Path> {
		self.data_search_dirs.iter().map(PathBuf::as_path)
	}
}

fn check_absolute_path(path: &Path, kind: DirPathKind) -> Result<(), InvalidDirPath> {
	if !path.is_absolute() || path.as_os_str().as_encoded_bytes().contains(&0) {
		return Err(InvalidDirPath { kind });
	}
	Ok(())
}

fn application_search_dirs(
	user: &Option<PathBuf>,
	system: &[PathBuf],
	name: &AppDirName,
) -> Vec<PathBuf> {
	let mut paths: Vec<PathBuf> = user.iter().cloned().collect();
	for base in system {
		let path = base.join(name.as_os_str());
		if !paths.contains(&path) {
			paths.push(path);
		}
	}
	paths
}

#[cfg(target_os = "linux")]
fn xdg_search_dirs(variable: &str, default: &str) -> Vec<PathBuf> {
	let value = std::env::var_os(variable)
		.filter(|value| !value.is_empty())
		.unwrap_or_else(|| OsString::from(default));
	std::env::split_paths(&value)
		.filter(|path| path.is_absolute())
		.collect()
}
