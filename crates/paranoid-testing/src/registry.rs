use bollard::auth::DockerCredentials;
use docker_credential::{CredentialRetrievalError, DockerCredential};
use primitives_rs::processes::child_processes::{
	ChildProcessOptions, CmdExecutionOptions, OutputHandling, ProcessCmd, ProcessEnvironment,
	TerminationOptions, TerminationScope, execute_cmd,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegistryConfig {
	cred_helpers: Option<HashMap<String, String>>,
	creds_store: Option<String>,
	#[serde(rename = "auths")]
	_auths: Option<HashMap<String, StaticCredentials>>,
}

#[derive(Deserialize)]
struct StaticCredentials {
	#[serde(rename = "auth")]
	_auth: Option<String>,
	#[serde(rename = "username")]
	_username: Option<String>,
	#[serde(rename = "password")]
	_password: Option<String>,
	#[serde(rename = "identitytoken")]
	_identitytoken: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct HelperResponse {
	username: String,
	secret: String,
}

/// The server address under which the Docker CLI keeps the credentials of Docker Hub.
const DOCKER_HUB_SERVER_ADDRESS: &str = "https://index.docker.io/v1/";

/// What a credential helper writes to its standard output when it has no credentials for the
/// server, as the Docker CLI recognizes it.
const HELPER_CREDENTIALS_NOT_FOUND: &str = "credentials not found in native keychain";

/// A failure to resolve registry credentials that the Docker configuration names. The reason
/// names the configuration, the registry, and the helper, and never contains a credential or
/// the output of a helper.
#[derive(Debug)]
pub(crate) struct RegistryCredentialError {
	reason: String,
}

impl fmt::Display for RegistryCredentialError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(&self.reason)
	}
}

impl std::error::Error for RegistryCredentialError {}

fn credential_error(reason: String) -> RegistryCredentialError {
	RegistryCredentialError { reason }
}

/// Resolves the registry credentials of an image pull as the Docker CLI resolves them, or
/// `None` when the configuration names none for the image's registry. The configuration is the
/// JSON text of `DOCKER_AUTH_CONFIG`, or else `config.json` in `DOCKER_CONFIG` or in
/// `$HOME/.docker`; a missing file is no configuration. The registry's `credHelpers` entry
/// comes first, then `credsStore`, and then the `auths` entry; a helper that reports that it
/// has no credentials gives none.
///
/// The caller owns the deadline: dropping the future kills an executing helper. Credentials
/// and helper output never enter an error or diagnostic.
pub(crate) async fn credentials_for_image(
	image_reference: &str,
) -> Result<Option<DockerCredentials>, RegistryCredentialError> {
	let Some(configuration) = read_auth_config().await? else {
		return Ok(None);
	};
	let config: RegistryConfig = serde_json::from_str(&configuration.text).map_err(|_| {
		credential_error(format!(
			"the Docker configuration in {} is not a JSON object of the Docker configuration form",
			configuration.source
		))
	})?;
	let server = registry_server_address(image_reference);
	let helper = config
		.cred_helpers
		.as_ref()
		.and_then(|helpers| helpers.get(server))
		.filter(|helper| !helper.is_empty())
		.or(config
			.creds_store
			.as_ref()
			.filter(|helper| !helper.is_empty()));
	let credential = match helper {
		Some(helper) => credential_from_helper(server, helper).await?,
		None => credential_from_auths(&configuration, server)?,
	};
	let Some(credential) = credential else {
		return Ok(None);
	};
	let mut result = DockerCredentials {
		serveraddress: Some(server.to_owned()),
		..DockerCredentials::default()
	};
	match credential {
		DockerCredential::IdentityToken(token) => result.identitytoken = Some(token),
		DockerCredential::UsernamePassword(username, password) => {
			result.username = Some(username);
			result.password = Some(password);
		}
	}
	Ok(Some(result))
}

/// The text of a Docker configuration and a description of where it came from.
struct AuthConfiguration {
	source: String,
	text: String,
}

/// The Docker configuration, or `None` when no configuration exists.
async fn read_auth_config() -> Result<Option<AuthConfiguration>, RegistryCredentialError> {
	match std::env::var("DOCKER_AUTH_CONFIG") {
		Ok(text) => {
			return Ok(Some(AuthConfiguration {
				source: "DOCKER_AUTH_CONFIG".to_owned(),
				text,
			}));
		}
		Err(std::env::VarError::NotUnicode(_)) => {
			return Err(credential_error(
				"DOCKER_AUTH_CONFIG is not valid Unicode".to_owned(),
			));
		}
		Err(std::env::VarError::NotPresent) => {}
	}
	let Some(dir) = std::env::var_os("DOCKER_CONFIG")
		.map(PathBuf::from)
		.or_else(|| std::env::home_dir().map(|home| home.join(".docker")))
	else {
		return Ok(None);
	};
	let path = dir.join("config.json");
	match tokio::fs::read_to_string(&path).await {
		Ok(text) => Ok(Some(AuthConfiguration {
			source: path.display().to_string(),
			text,
		})),
		Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
		Err(error) => Err(credential_error(format!(
			"could not read the Docker configuration {}: {error}",
			path.display()
		))),
	}
}

/// The credentials of the configuration's `auths` entry for `server`, read as `docker_credential`
/// reads them, without the configuration's helpers.
fn credential_from_auths(
	configuration: &AuthConfiguration,
	server: &str,
) -> Result<Option<DockerCredential>, RegistryCredentialError> {
	let malformed = || {
		credential_error(format!(
			"the auths entry for {server} in the Docker configuration in {} cannot be decoded",
			configuration.source
		))
	};
	let mut static_config: Value =
		serde_json::from_str(&configuration.text).map_err(|_| malformed())?;
	let fields = static_config.as_object_mut().ok_or_else(malformed)?;
	fields.remove("credHelpers");
	fields.remove("credsStore");
	let encoded = serde_json::to_vec(&static_config).map_err(|_| malformed())?;
	match docker_credential::get_credential_from_reader(encoded.as_slice(), server) {
		Ok(credential) => Ok(Some(credential)),
		Err(CredentialRetrievalError::NoCredentialConfigured) => Ok(None),
		Err(_) => Err(malformed()),
	}
}

/// Runs `docker-credential-<helper> get` for `server`. A helper that reports that it has no
/// credentials gives `None`.
async fn credential_from_helper(
	server: &str,
	helper: &str,
) -> Result<Option<DockerCredential>, RegistryCredentialError> {
	let failure = |what: &str| {
		credential_error(format!(
			"the credential helper docker-credential-{helper} {what} for the registry {server}"
		))
	};
	// Credential responses are small JSON objects. The limit bounds a defective
	// helper without recording either its output or its discarded diagnostics.
	let command = ProcessCmd {
		program: format!("docker-credential-{helper}").into(),
		args: vec!["get".into()],
		current_dir: None,
		environment: ProcessEnvironment::Inherit {
			remove: Vec::new(),
			set: Default::default(),
		},
	};
	let output = execute_cmd(
		&command,
		server.as_bytes(),
		CmdExecutionOptions {
			process: ChildProcessOptions {
				termination_scope: TerminationScope::ProcessGroup,
				termination: TerminationOptions {
					grace_period: Duration::ZERO,
					reap_timeout: Duration::from_secs(5),
				},
			},
			stdout: OutputHandling::Capture {
				max_size: 1024 * 1024,
			},
			stderr: OutputHandling::Discard,
		},
		// The caller's deadline drops this future, which kills the helper, so nothing cancels
		// this token. `execute_cmd` requires one.
		&CancellationToken::new(),
	)
	.await
	.map_err(|_| failure("could not run or wrote too much output"))?;
	let stdout = output.stdout.as_deref().unwrap_or_default();
	if !output.status.success() {
		if String::from_utf8_lossy(stdout).trim() == HELPER_CREDENTIALS_NOT_FOUND {
			return Ok(None);
		}
		return Err(failure(&format!("failed with {}", output.status)));
	}
	let response: HelperResponse =
		serde_json::from_slice(stdout).map_err(|_| failure("wrote a malformed response"))?;
	Ok(Some(if response.username == "<token>" {
		DockerCredential::IdentityToken(response.secret)
	} else {
		DockerCredential::UsernamePassword(response.username, response.secret)
	}))
}

/// The server address under which the Docker CLI keeps the credentials of the image's
/// registry. As in the Docker CLI, the first component of a name with a `/` is a registry when
/// it contains `.` or `:`, is `localhost`, or contains an uppercase letter; every other name is
/// on Docker Hub, which also has the registry names `docker.io` and `index.docker.io`.
fn registry_server_address(image_reference: &str) -> &str {
	let registry = match image_reference.split_once('/') {
		Some((first, _))
			if first.contains(['.', ':'])
				|| first == "localhost"
				|| first.bytes().any(|byte| byte.is_ascii_uppercase()) =>
		{
			first
		}
		_ => return DOCKER_HUB_SERVER_ADDRESS,
	};
	match registry {
		"docker.io" | "index.docker.io" => DOCKER_HUB_SERVER_ADDRESS,
		_ => registry,
	}
}
