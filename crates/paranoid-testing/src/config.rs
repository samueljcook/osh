use crate::error::{PgTestHarnessError, PgTestHarnessPhase};
use std::time::Duration;

/// Every setting of the harness. The harness has no defaults: each field is the caller's choice.
///
/// Each duration must be at least one millisecond, and each count at least one.
///
/// # Environment
///
/// The harness also reads these environment variables of the test process:
///
/// - `DOCKER_HOST` is the address of the container runtime, as a URL with the scheme `unix`,
///   `tcp`, or `http`. When it is not set, the address is the first of these sockets that
///   exists: `/var/run/docker.sock`, `$XDG_RUNTIME_DIR/.docker/run/docker.sock`,
///   `$HOME/.docker/run/docker.sock`, and `$HOME/.docker/desktop/docker.sock`. A reservation
///   record names the runtime by this address.
/// - `DOCKER_TLS_VERIFY` must be unset or empty: the harness connects to the runtime without
///   TLS, so startup fails in the [`Environment`](PgTestHarnessPhase::Environment) phase when
///   the variable asks for TLS.
/// - `DOCKER_DEFAULT_PLATFORM`, when it is set and not empty, is the platform of the created
///   containers and of the pulled images. Otherwise the runtime chooses.
/// - `DOCKER_AUTH_CONFIG`, `DOCKER_CONFIG`, `HOME`, and `PATH` select the registry credentials of
///   an image pull, as the Docker CLI selects them: the JSON text of `DOCKER_AUTH_CONFIG`, or
///   else the file `config.json` in the directory `DOCKER_CONFIG`, or else in `$HOME/.docker`.
///   The file's `credHelpers` entry for the registry, then its `credsStore`, and then its
///   `auths` entry give the credentials, and a credential helper `docker-credential-<name>` is
///   found through `PATH`. Without a configuration, a pull sends no credentials. A pull fails
///   when the configuration names credentials that cannot be resolved.
/// - `TMPDIR`, through [`std::env::temp_dir`], is the directory of the port reservations. Port
///   exclusion between harnesses of different processes requires that the processes share this
///   directory.
/// - `PGOPTIONS` must not be set: `sqlx` would send it as startup options on every connection
///   of the harness, so startup fails in the [`Environment`](PgTestHarnessPhase::Environment)
///   phase and a connection in the [`ClientConnect`](PgTestHarnessPhase::ClientConnect) phase.
#[derive(Clone, Debug)]
pub struct PgTestHarnessConfig {
	/// The PostgreSQL image, as `name:tag` or `name@sha256:digest`. A container runtime that
	/// enforces fully qualified names, such as Podman, needs the registry in the name. Its entry
	/// point must be the one of the official PostgreSQL images: it starts as root, reads the
	/// superuser's password from the file that `POSTGRES_PASSWORD_FILE` names (a file of the user
	/// root with the mode 0600, which the harness copies into the container), initializes the
	/// empty data directory with `POSTGRES_USER`, `POSTGRES_DB`, and `POSTGRES_INITDB_ARGS`, and
	/// passes the password in `PGPASSWORD` to its own connections because `initdb` requires
	/// passwords. The harness counts PostgreSQL as ready
	/// after the second log line `database system is ready to accept connections`, which the
	/// server writes after that initialization.
	///
	/// The entry point stores the superuser's password in the variables `POSTGRES_PASSWORD` and
	/// `PGPASSWORD` while it initializes the data directory, and the harness relies on it to
	/// remove both from the environment of the server that it starts afterwards. The entry point
	/// of the official images of PostgreSQL 14.24, 15.19, 16.15, 17.11, and 18.6 removes them.
	/// The entry point of `postgres:16.10` starts the server with `POSTGRES_PASSWORD` in its
	/// environment, where every process of the operating system user `postgres` can read it. On
	/// PostgreSQL 15 and earlier, those processes include a program that a role with
	/// `CREATEROLE`, such as the writer, runs with `COPY ... FROM PROGRAM` after it grants
	/// itself `pg_execute_server_program`.
	pub pg_image: String,
	/// The PgBouncer image, in the same form, which both PgBouncer containers run. Its entry
	/// point must be PgBouncer, which receives the path of a configuration file as its only
	/// argument. The harness copies that file and the user list into the container as files of
	/// the user root with the mode 0600, so the image must start PgBouncer as root and let
	/// PgBouncer drop its privileges after it reads them, as the entry point
	/// `pgbouncer -u nobody` does. Its PgBouncer must be version
	/// 1.21.0 or later (see [`expected_pgbouncer_version`](Self::expected_pgbouncer_version)).
	pub pgbouncer_image: String,
	/// The PgBouncer version, as `major.minor.patch`, at least 1.21.0. Startup fails when
	/// PgBouncer reports another version.
	///
	/// The harness writes `max_prepared_statements` into the PgBouncer configuration and reads it
	/// back at startup, and PgBouncer has that setting from 1.21.0, so the harness accepts no
	/// earlier version.
	pub expected_pgbouncer_version: String,
	/// How long the complete startup may take, including runtime discovery, image pulls,
	/// creation, file copies, readiness, role provisioning, and verification. Individual
	/// readiness limits cannot extend this shared deadline. Cleanup has a separate budget.
	pub startup_timeout: Duration,
	/// How long cleanup after failed startup, explicit shutdown, or dropping the harness may
	/// take. Explicit failures report unresolved cleanup; every unresolved removal leaves its
	/// recovery reservations in place.
	pub cleanup_timeout: Duration,
	/// How long the PostgreSQL start request and readiness wait may take. Pulling an image,
	/// creating the container, and copying files into it are not part of this narrower time.
	pub pg_startup_timeout: Duration,
	/// How long each PgBouncer start request and readiness wait may take. Pulling an image,
	/// creating the container, and copying files into it are not part of this narrower time.
	pub pgbouncer_startup_timeout: Duration,
	/// How long a container may take to stop before the runtime kills it. The runtime counts
	/// whole seconds, so a fraction rounds up.
	pub container_stop_timeout: Duration,
	/// How long opening one of the harness's own connections may take. It also bounds the
	/// connections that [`connect_direct`](crate::PgTestHarness::connect_direct) and
	/// [`connect_pgbouncer_admin`](crate::PgTestHarness::connect_pgbouncer_admin) return.
	pub client_connect_timeout: Duration,
	/// How long one statement that the harness runs on its own connections may take.
	pub client_query_timeout: Duration,
	/// The statement timeout of the [`StatementTimeout`](crate::PgTestHarnessRole::StatementTimeout)
	/// role, at most 2,147,483,647 milliseconds.
	pub statement_timeout: Duration,
	/// PostgreSQL's `max_connections`, from 4 to 262,143. Both PgBouncer servers and every
	/// direct connection share this limit. PostgreSQL reserves three of these connections for
	/// superusers, so the roles of the harness share the rest, and it fails to start when this
	/// value and the number of its background processes together exceed 262,143.
	pub pg_max_connections: u32,
	/// The `max_client_conn` of each PgBouncer, from 1 to 2,147,483,647.
	pub pgbouncer_max_clients: u32,
	/// The `default_pool_size` of each PgBouncer, from 1 to 2,147,483,647. A PgBouncer keeps
	/// one pool for each pair of a database and a role.
	pub pgbouncer_pool_size: u32,
	/// The `max_prepared_statements` of each PgBouncer, from 0 to 2,147,483,647. The harness
	/// writes it into the configuration and verifies it at startup. Paranoid does not depend on the value.
	pub pgbouncer_max_prepared_statements: u32,
	/// How many times the harness draws a loopback port for each of its three ports before it
	/// gives up because another harness holds the selected port; from 1 to 2,147,483,647.
	pub port_reservation_attempts: u32,
}

/// A parsed image reference that the container runtime accepts unchanged.
#[derive(Clone, Debug)]
pub(crate) struct ImageReference {
	pub(crate) reference: String,
}

/// The configuration after its checks, in the units the harness uses.
#[derive(Clone, Debug)]
pub(crate) struct ParsedConfig {
	pub(crate) pg_image: ImageReference,
	pub(crate) pgbouncer_image: ImageReference,
	pub(crate) expected_pgbouncer_version: String,
	pub(crate) startup_timeout: Duration,
	pub(crate) cleanup_timeout: Duration,
	pub(crate) pg_startup_timeout: Duration,
	pub(crate) pgbouncer_startup_timeout: Duration,
	pub(crate) container_stop_timeout_secs: i32,
	pub(crate) client_connect_timeout: Duration,
	pub(crate) client_query_timeout: Duration,
	pub(crate) statement_timeout_millis: u32,
	pub(crate) pg_max_connections: u32,
	pub(crate) pgbouncer_max_clients: u32,
	pub(crate) pgbouncer_pool_size: u32,
	pub(crate) pgbouncer_max_prepared_statements: u32,
	pub(crate) port_reservation_attempts: u32,
}

impl PgTestHarnessConfig {
	pub(crate) fn parse(&self) -> Result<ParsedConfig, PgTestHarnessError> {
		let stop_timeout_secs =
			parse_duration(self.container_stop_timeout, "container_stop_timeout")?.as_secs()
				+ u64::from(self.container_stop_timeout.subsec_nanos() > 0);
		Ok(ParsedConfig {
			pg_image: parse_image(&self.pg_image, "pg_image")?,
			pgbouncer_image: parse_image(&self.pgbouncer_image, "pgbouncer_image")?,
			expected_pgbouncer_version: parse_version(&self.expected_pgbouncer_version)?,
			startup_timeout: parse_lifecycle_budget(self.startup_timeout, "startup_timeout")?,
			cleanup_timeout: parse_lifecycle_budget(self.cleanup_timeout, "cleanup_timeout")?,
			pg_startup_timeout: parse_lifecycle_budget(
				self.pg_startup_timeout,
				"pg_startup_timeout",
			)?,
			pgbouncer_startup_timeout: parse_lifecycle_budget(
				self.pgbouncer_startup_timeout,
				"pgbouncer_startup_timeout",
			)?,
			container_stop_timeout_secs: i32::try_from(stop_timeout_secs).map_err(|_| {
				config_error("container_stop_timeout must be at most 2,147,483,647 seconds")
			})?,
			client_connect_timeout: parse_duration(
				self.client_connect_timeout,
				"client_connect_timeout",
			)?,
			client_query_timeout: parse_duration(
				self.client_query_timeout,
				"client_query_timeout",
			)?,
			statement_timeout_millis: u32::try_from(
				parse_duration(self.statement_timeout, "statement_timeout")?.as_millis(),
			)
			.ok()
			.filter(|millis| *millis <= i32::MAX as u32)
			.ok_or_else(|| {
				config_error("statement_timeout must be at most 2,147,483,647 milliseconds")
			})?,
			pg_max_connections: parse_pg_max_connections(self.pg_max_connections)?,
			pgbouncer_max_clients: parse_count(
				self.pgbouncer_max_clients,
				"pgbouncer_max_clients",
			)?,
			pgbouncer_pool_size: parse_count(self.pgbouncer_pool_size, "pgbouncer_pool_size")?,
			pgbouncer_max_prepared_statements: parse_count_or_zero(
				self.pgbouncer_max_prepared_statements,
				"pgbouncer_max_prepared_statements",
			)?,
			port_reservation_attempts: parse_count(
				self.port_reservation_attempts,
				"port_reservation_attempts",
			)?,
		})
	}
}

fn config_error(reason: impl Into<String>) -> PgTestHarnessError {
	PgTestHarnessError::new(PgTestHarnessPhase::Config, reason)
}

/// The largest count that the configuration accepts: PostgreSQL and PgBouncer read these
/// settings as signed 32-bit integers.
const MAX_COUNT: u32 = i32::MAX as u32;

fn parse_count(value: u32, name: &str) -> Result<u32, PgTestHarnessError> {
	if value == 0 {
		return Err(config_error(format!("{name} must be at least 1")));
	}
	parse_count_or_zero(value, name)
}

fn parse_count_or_zero(value: u32, name: &str) -> Result<u32, PgTestHarnessError> {
	if value > MAX_COUNT {
		return Err(config_error(format!("{name} must be at most {MAX_COUNT}")));
	}
	Ok(value)
}

/// PostgreSQL reserves `superuser_reserved_connections` (by default 3) of its connections for
/// superusers and fails to start unless `max_connections` exceeds that number.
const MIN_PG_MAX_CONNECTIONS: u32 = 4;

/// The largest `max_connections` that PostgreSQL accepts.
const MAX_PG_MAX_CONNECTIONS: u32 = 262_143;

fn parse_pg_max_connections(value: u32) -> Result<u32, PgTestHarnessError> {
	if !(MIN_PG_MAX_CONNECTIONS..=MAX_PG_MAX_CONNECTIONS).contains(&value) {
		return Err(config_error(format!(
			"pg_max_connections must be from {MIN_PG_MAX_CONNECTIONS} to {MAX_PG_MAX_CONNECTIONS}"
		)));
	}
	Ok(value)
}

fn parse_duration(value: Duration, name: &str) -> Result<Duration, PgTestHarnessError> {
	if value < Duration::from_millis(1) {
		return Err(config_error(format!(
			"{name} must be at least 1 millisecond"
		)));
	}
	Ok(value)
}

fn parse_lifecycle_budget(value: Duration, name: &str) -> Result<Duration, PgTestHarnessError> {
	let value = parse_duration(value, name)?;
	if primitives_rs::time::Deadline::new_after(value).is_err() {
		return Err(config_error(format!(
			"{name} exceeds the runtime's clock range"
		)));
	}
	Ok(value)
}

/// The oldest PgBouncer release with `max_prepared_statements`, which the harness writes into
/// the configuration and verifies.
const MINIMUM_PGBOUNCER_VERSION: [u32; 3] = [1, 21, 0];

fn parse_version(value: &str) -> Result<String, PgTestHarnessError> {
	let numbers: Option<Vec<u32>> = value
		.split('.')
		.map(|part| {
			part.bytes()
				.all(|byte| byte.is_ascii_digit())
				.then(|| part.parse().ok())
				.flatten()
		})
		.collect();
	let Some(numbers) = numbers.filter(|numbers| numbers.len() == 3) else {
		return Err(config_error(
			"expected_pgbouncer_version must have major.minor.patch form",
		));
	};
	if numbers.as_slice() < MINIMUM_PGBOUNCER_VERSION.as_slice() {
		return Err(config_error(
			"expected_pgbouncer_version must be at least 1.21.0, the first release with max_prepared_statements, which the harness sets",
		));
	}
	Ok(value.to_owned())
}

fn parse_image(value: &str, name: &str) -> Result<ImageReference, PgTestHarnessError> {
	let invalid = || {
		config_error(format!(
			"{name} must be NAME:TAG or NAME@sha256:DIGEST without whitespace or NUL"
		))
	};
	if value
		.chars()
		.any(|character| character.is_whitespace() || character == '\0')
	{
		return Err(invalid());
	}
	// A colon before the last slash belongs to a registry port, so it does not start a tag.
	let tag_separator = value
		.rfind(':')
		.filter(|separator| value.rfind('/').is_none_or(|slash| *separator > slash));
	let Some(separator) = tag_separator else {
		return Err(invalid());
	};
	let (image_name, tag) = (&value[..separator], &value[separator + 1..]);
	if image_name.is_empty() || tag.is_empty() {
		return Err(invalid());
	}
	Ok(ImageReference {
		reference: value.to_owned(),
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	const PG_IMAGE: &str = "docker.io/library/postgres@sha256:38471f330eb885e04de130b768d6db4e10469e2311879c7e5c699f6d2d8a1c74";
	const PGBOUNCER_IMAGE: &str = "docker.io/hyperized/pgbouncer@sha256:7948b561791a7888f69e6d94b763c11847f63a0092ab5bd446108d5ce2c4b192";

	fn config() -> PgTestHarnessConfig {
		PgTestHarnessConfig {
			pg_image: PG_IMAGE.to_owned(),
			pgbouncer_image: PGBOUNCER_IMAGE.to_owned(),
			expected_pgbouncer_version: "1.25.2".to_owned(),
			startup_timeout: Duration::from_secs(300),
			cleanup_timeout: Duration::from_secs(60),
			pg_startup_timeout: Duration::from_secs(60),
			pgbouncer_startup_timeout: Duration::from_secs(60),
			container_stop_timeout: Duration::from_secs(10),
			client_connect_timeout: Duration::from_secs(10),
			client_query_timeout: Duration::from_secs(30),
			statement_timeout: Duration::from_millis(50),
			pg_max_connections: 100,
			pgbouncer_max_clients: 500,
			pgbouncer_pool_size: 20,
			pgbouncer_max_prepared_statements: 200,
			port_reservation_attempts: 100,
		}
	}

	fn rejection(config: &PgTestHarnessConfig) -> PgTestHarnessError {
		config.parse().expect_err("the configuration is invalid")
	}

	#[test]
	fn explicit_settings_pass_through_unchanged() {
		let parsed = config().parse().unwrap();
		assert_eq!(parsed.pg_image.reference, PG_IMAGE);
		assert_eq!(parsed.pgbouncer_image.reference, PGBOUNCER_IMAGE);
		assert_eq!(parsed.expected_pgbouncer_version, "1.25.2");
		assert_eq!(parsed.pg_startup_timeout, Duration::from_secs(60));
		assert_eq!(parsed.container_stop_timeout_secs, 10);
		assert_eq!(parsed.statement_timeout_millis, 50);
		assert_eq!(parsed.pg_max_connections, 100);
		assert_eq!(parsed.pgbouncer_max_clients, 500);
		assert_eq!(parsed.pgbouncer_pool_size, 20);
		assert_eq!(parsed.pgbouncer_max_prepared_statements, 200);
		assert_eq!(parsed.port_reservation_attempts, 100);
	}

	#[test]
	fn a_registry_port_does_not_start_a_tag() {
		let mut with_port = config();
		with_port.pg_image = "localhost:5000/postgres:16".to_owned();
		let parsed = with_port.parse().unwrap();
		assert_eq!(parsed.pg_image.reference, "localhost:5000/postgres:16");
		with_port.pg_image = "localhost:5000/postgres".to_owned();
		assert_eq!(rejection(&with_port).phase(), PgTestHarnessPhase::Config);
	}

	#[test]
	fn ambiguous_images_are_rejected() {
		for value in [
			"",
			"image name:1",
			"image\0name:1",
			"\nimage:1",
			"image",
			":1",
			"image:",
			"image@sha256:abc def",
		] {
			let mut postgres = config();
			postgres.pg_image = value.to_owned();
			let error = rejection(&postgres);
			assert_eq!(error.phase(), PgTestHarnessPhase::Config);
			assert!(error.reason().starts_with("pg_image"), "{value:?}");
			let mut pgbouncer = config();
			pgbouncer.pgbouncer_image = value.to_owned();
			let error = rejection(&pgbouncer);
			assert_eq!(error.phase(), PgTestHarnessPhase::Config);
			assert!(error.reason().starts_with("pgbouncer_image"), "{value:?}");
		}
	}

	#[test]
	fn versions_need_major_minor_patch_digits() {
		for value in [
			"", "1.25", "latest", "1.25.2\n", "1.25.2.1", "1..2", "1.25.x", "v1.25.2",
		] {
			let mut candidate = config();
			candidate.expected_pgbouncer_version = value.to_owned();
			let error = rejection(&candidate);
			assert_eq!(error.phase(), PgTestHarnessPhase::Config);
			assert!(
				error.reason().starts_with("expected_pgbouncer_version"),
				"{value:?}"
			);
		}
	}

	#[test]
	fn versions_older_than_the_first_release_with_max_prepared_statements_are_rejected() {
		for value in ["1.20.9", "1.0.0", "0.99.99"] {
			let mut candidate = config();
			candidate.expected_pgbouncer_version = value.to_owned();
			let error = rejection(&candidate);
			assert_eq!(error.phase(), PgTestHarnessPhase::Config);
			assert!(
				error
					.reason()
					.starts_with("expected_pgbouncer_version must be at least 1.21.0"),
				"{value:?}"
			);
		}
		for value in ["1.21.0", "1.21.1", "1.100.0", "2.0.0"] {
			let mut candidate = config();
			candidate.expected_pgbouncer_version = value.to_owned();
			candidate.parse().unwrap();
		}
	}

	#[test]
	fn the_prepared_statement_limit_is_between_zero_and_the_largest_signed_32_bit_integer() {
		for accepted in [0, 1, 2_147_483_647] {
			let mut candidate = config();
			candidate.pgbouncer_max_prepared_statements = accepted;
			assert_eq!(
				candidate.parse().unwrap().pgbouncer_max_prepared_statements,
				accepted
			);
		}
		for rejected in [2_147_483_648, u32::MAX] {
			let mut candidate = config();
			candidate.pgbouncer_max_prepared_statements = rejected;
			let error = rejection(&candidate);
			assert_eq!(error.phase(), PgTestHarnessPhase::Config);
			assert_eq!(
				error.reason(),
				"pgbouncer_max_prepared_statements must be at most 2147483647"
			);
		}
	}

	#[test]
	fn counts_are_between_one_and_the_largest_signed_32_bit_integer() {
		type Set = fn(&mut PgTestHarnessConfig, u32);
		let fields: [(&str, Set); 3] = [
			("pgbouncer_max_clients", |config, value| {
				config.pgbouncer_max_clients = value
			}),
			("pgbouncer_pool_size", |config, value| {
				config.pgbouncer_pool_size = value
			}),
			("port_reservation_attempts", |config, value| {
				config.port_reservation_attempts = value
			}),
		];
		for (name, set) in fields {
			let mut zero = config();
			set(&mut zero, 0);
			let error = rejection(&zero);
			assert_eq!(error.phase(), PgTestHarnessPhase::Config);
			assert_eq!(error.reason(), format!("{name} must be at least 1"));
			for accepted in [1, 2_147_483_647] {
				let mut candidate = config();
				set(&mut candidate, accepted);
				candidate.parse().unwrap();
			}
			for rejected in [2_147_483_648, u32::MAX] {
				let mut candidate = config();
				set(&mut candidate, rejected);
				let error = rejection(&candidate);
				assert_eq!(error.phase(), PgTestHarnessPhase::Config);
				assert_eq!(error.reason(), format!("{name} must be at most 2147483647"));
			}
		}
	}

	#[test]
	fn pg_max_connections_leaves_a_connection_beside_the_superuser_reservation_and_is_at_most_postgresqls_limit()
	 {
		for accepted in [4, 262_143] {
			let mut candidate = config();
			candidate.pg_max_connections = accepted;
			assert_eq!(candidate.parse().unwrap().pg_max_connections, accepted);
		}
		for rejected in [0, 1, 3, 262_144, u32::MAX] {
			let mut candidate = config();
			candidate.pg_max_connections = rejected;
			let error = rejection(&candidate);
			assert_eq!(error.phase(), PgTestHarnessPhase::Config);
			assert_eq!(
				error.reason(),
				"pg_max_connections must be from 4 to 262143"
			);
		}
	}

	#[test]
	fn readiness_budgets_must_fit_the_runtime_clock() {
		for (name, set) in [
			(
				"pg_startup_timeout",
				(|config, value| config.pg_startup_timeout = value)
					as fn(&mut PgTestHarnessConfig, Duration),
			),
			("pgbouncer_startup_timeout", |config, value| {
				config.pgbouncer_startup_timeout = value
			}),
		] {
			let mut candidate = config();
			set(&mut candidate, Duration::MAX);
			let error = rejection(&candidate);
			assert_eq!(error.phase(), PgTestHarnessPhase::Config);
			assert_eq!(
				error.reason(),
				format!("{name} exceeds the runtime's clock range")
			);
		}
	}

	#[test]
	fn durations_need_at_least_one_millisecond() {
		type Set = fn(&mut PgTestHarnessConfig, Duration);
		let fields: [(&str, Set); 6] = [
			("pg_startup_timeout", |config, value| {
				config.pg_startup_timeout = value
			}),
			("pgbouncer_startup_timeout", |config, value| {
				config.pgbouncer_startup_timeout = value
			}),
			("container_stop_timeout", |config, value| {
				config.container_stop_timeout = value
			}),
			("client_connect_timeout", |config, value| {
				config.client_connect_timeout = value
			}),
			("client_query_timeout", |config, value| {
				config.client_query_timeout = value
			}),
			("statement_timeout", |config, value| {
				config.statement_timeout = value
			}),
		];
		for (name, set) in fields {
			for rejected in [
				Duration::ZERO,
				Duration::from_nanos(1),
				Duration::from_micros(999),
			] {
				let mut candidate = config();
				set(&mut candidate, rejected);
				let error = rejection(&candidate);
				assert_eq!(error.phase(), PgTestHarnessPhase::Config);
				assert_eq!(
					error.reason(),
					format!("{name} must be at least 1 millisecond")
				);
			}
			let mut candidate = config();
			set(&mut candidate, Duration::from_millis(1));
			candidate.parse().unwrap();
		}
	}

	#[test]
	fn the_statement_timeout_is_at_most_the_largest_pg_integer_of_millis() {
		let mut candidate = config();
		candidate.statement_timeout = Duration::from_millis(2_147_483_647);
		assert_eq!(
			candidate.parse().unwrap().statement_timeout_millis,
			2_147_483_647
		);
		for rejected in [Duration::from_millis(2_147_483_648), Duration::MAX] {
			candidate.statement_timeout = rejected;
			let error = rejection(&candidate);
			assert_eq!(error.phase(), PgTestHarnessPhase::Config);
			assert_eq!(
				error.reason(),
				"statement_timeout must be at most 2,147,483,647 milliseconds"
			);
		}
	}

	#[test]
	fn the_stop_timeout_counts_whole_secs_rounded_up_and_has_a_bound() {
		let mut candidate = config();
		candidate.container_stop_timeout = Duration::from_millis(1);
		assert_eq!(candidate.parse().unwrap().container_stop_timeout_secs, 1);
		candidate.container_stop_timeout = Duration::from_millis(10_001);
		assert_eq!(candidate.parse().unwrap().container_stop_timeout_secs, 11);
		candidate.container_stop_timeout = Duration::from_secs(2_147_483_647);
		assert_eq!(
			candidate.parse().unwrap().container_stop_timeout_secs,
			i32::MAX
		);
		candidate.container_stop_timeout = Duration::from_secs(2_147_483_648);
		assert_eq!(
			rejection(&candidate).reason(),
			"container_stop_timeout must be at most 2,147,483,647 seconds"
		);
	}
}
