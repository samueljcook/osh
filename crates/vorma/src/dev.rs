//! Facts about the process and about the deployment. They are read from the
//! environment when called.

use crate::build_output::{SkewProtection, VercelDeploymentId};
use crate::contract::CONTRACT_FILE_ENV_VAR;

/// The variable in which `vorma build` names the staged build directory when
/// it runs the server binary to check that the binary can load it.
pub(crate) const CHECK_BUILD_DIR_ENV_VAR: &str = "__VORMA_CHECK_BUILD_DIR";

/// Set to `1` in a process that `vorma dev` started.
const DEV_PROCESS_ENV_VAR: &str = "__VORMA_IS_DEV";

/// Vorma's handler answers a `GET` of this path in development with 200 and
/// its build ID; `vorma dev` treats a process as ready only after that answer.
pub(crate) const READINESS_PATH: &str = "/.vorma/healthz";

/// Whether `vorma dev` started this process.
pub fn is_dev() -> bool {
	std::env::var(DEV_PROCESS_ENV_VAR).is_ok_and(|value| value == "1")
}

/// Whether this process is a run of the binary that is part of a build:
/// `vorma build` or `vorma dev` runs the binary to read its declarations, and
/// `vorma build` runs it once more to check that it loads the build directory
/// that the build wrote. It is `false` in a process that only runs the
/// server, and it does not tell a production build from a development build.
///
/// Code that must run before [`App::load`](crate::App::load), such as
/// telemetry setup that reports `load`'s own failures, can use it to skip work
/// that a build does not need. The declarations must not depend on it because
/// a build's declarations must match the server's.
pub fn is_build() -> bool {
	std::env::var_os(CONTRACT_FILE_ENV_VAR).is_some()
		|| std::env::var_os(CHECK_BUILD_DIR_ENV_VAR).is_some()
}

const VERCEL_SKEW_PROTECTION_ENABLED_ENV_VAR: &str = "VERCEL_SKEW_PROTECTION_ENABLED";
const VERCEL_DEPLOYMENT_ID_ENV_VAR: &str = "VERCEL_DEPLOYMENT_ID";

/// Whether Vercel's skew protection is on, with the deployment ID. The
/// enabling variable takes the boolean literals that Effect's
/// `Config.Boolean` accepts, compared case-sensitively: `true`, `yes`, `on`,
/// `1`, and `y`, or `false`, `no`, `off`, `0`, and `n`. Any other value is a
/// configuration error, and so is an enabled skew protection without a
/// deployment ID of one or more visible ASCII characters.
pub(crate) fn read_vercel_skew_protection() -> Result<SkewProtection, String> {
	let Some(enabled) = std::env::var_os(VERCEL_SKEW_PROTECTION_ENABLED_ENV_VAR) else {
		return Ok(SkewProtection::Off);
	};
	let enabled = match enabled.to_str() {
		Some("true" | "yes" | "on" | "1" | "y") => true,
		Some("false" | "no" | "off" | "0" | "n") => false,
		_ => {
			return Err(format!(
				"{VERCEL_SKEW_PROTECTION_ENABLED_ENV_VAR} must be one of true, yes, on, 1, y, false, no, off, 0, or n"
			));
		}
	};
	if !enabled {
		return Ok(SkewProtection::Off);
	}
	match std::env::var(VERCEL_DEPLOYMENT_ID_ENV_VAR)
		.ok()
		.and_then(VercelDeploymentId::new)
	{
		Some(deployment_id) => Ok(SkewProtection::Vercel { deployment_id }),
		None => Err(format!(
			"{VERCEL_SKEW_PROTECTION_ENABLED_ENV_VAR} enables Vercel skew protection, but {VERCEL_DEPLOYMENT_ID_ENV_VAR} is not set to a deployment ID of one or more visible ASCII characters"
		)),
	}
}
