use crate::config::{ImageReference, ParsedConfig};
use crate::error::{PgTestHarnessError, PgTestHarnessPhase};
use crate::harness::PgTestHarnessPooler;
use crate::port::{CreationTarget, PortReservation, RecordedContainer, RecordedRuntime};
use crate::registry::credentials_for_image;
use bollard::container::LogOutput;
use bollard::errors::Error as DockerError;
use bollard::models::{ContainerCreateBody, HostConfig};
use bollard::query_parameters::{
	CreateContainerOptionsBuilder, CreateImageOptionsBuilder, LogsOptionsBuilder,
	RemoveContainerOptionsBuilder, StartContainerOptions, StopContainerOptionsBuilder,
	UploadToContainerOptionsBuilder,
};
use bollard::{API_DEFAULT_VERSION, Docker, body_stream};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::Instant;

pub(crate) const DATABASE_NAME: &str = "test";
pub(crate) const PGBOUNCER_ADMIN_DATABASE_NAME: &str = "pgbouncer";
/// The cluster's bootstrap superuser. Only the harness's own administration uses it: role
/// provisioning and the creation and removal of databases. PgBouncer does not accept it.
pub(crate) const SUPERUSER_ROLE_NAME: &str = "paranoid_superuser";
pub(crate) const WRITER_ROLE_NAME: &str = "test";
pub(crate) const SUBJECT_TO_ROW_SECURITY_ROLE_NAME: &str = "paranoid_nobypass";
pub(crate) const READ_ONLY_ROLE_NAME: &str = "paranoid_read_only";
pub(crate) const STATEMENT_TIMEOUT_ROLE_NAME: &str = "paranoid_statement_timeout";
/// The roles that PgBouncer accepts: every role of [`crate::PgTestHarnessRole`].
const PGBOUNCER_ROLE_NAMES: [&str; 4] = [
	WRITER_ROLE_NAME,
	SUBJECT_TO_ROW_SECURITY_ROLE_NAME,
	READ_ONLY_ROLE_NAME,
	STATEMENT_TIMEOUT_ROLE_NAME,
];
const RUN_LABEL: &str = "paranoid-testing.run";
const PGBOUNCER_CONFIG_PATH: &str = "/tmp/paranoid-pgbouncer.ini";
/// The file from which the official PostgreSQL entry point reads the superuser's password.
const SUPERUSER_PASSWORD_PATH: &str = "/tmp/paranoid-superuser-password";
const PGBOUNCER_USERLIST_PATH: &str = "/tmp/paranoid-userlist.txt";

/// This struct pairs the end of a lifecycle budget with the name of the configuration setting
/// that defines that budget so that an error can name the setting to raise.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Deadline {
	end: Instant,
	setting: &'static str,
}

impl Deadline {
	pub(crate) fn after(
		budget: Duration,
		setting: &'static str,
	) -> Result<Self, PgTestHarnessError> {
		Instant::now()
			.checked_add(budget)
			.map(|end| Self { end, setting })
			.ok_or_else(|| {
				PgTestHarnessError::new(
					PgTestHarnessPhase::Config,
					format!("{setting} exceeds the runtime's clock range"),
				)
			})
	}

	/// The earlier of the two deadlines.
	pub(crate) fn min(self, other: Self) -> Self {
		if other.end < self.end { other } else { self }
	}

	pub(crate) fn remaining(self) -> Option<Duration> {
		self.end.checked_duration_since(Instant::now())
	}

	pub(crate) fn has_passed(self) -> bool {
		Instant::now() >= self.end
	}

	pub(crate) fn check(self, phase: PgTestHarnessPhase) -> Result<(), PgTestHarnessError> {
		if self.has_passed() {
			Err(self.elapsed_error(phase))
		} else {
			Ok(())
		}
	}

	pub(crate) fn elapsed_error(self, phase: PgTestHarnessPhase) -> PgTestHarnessError {
		PgTestHarnessError::new(phase, format!("the {} elapsed", self.setting))
	}
}

pub(crate) async fn until<T>(
	deadline: Deadline,
	phase: PgTestHarnessPhase,
	work: impl Future<Output = Result<T, PgTestHarnessError>>,
) -> Result<T, PgTestHarnessError> {
	deadline.check(phase)?;
	tokio::time::timeout_at(deadline.end, work)
		.await
		.unwrap_or_else(|_| Err(deadline.elapsed_error(phase)))
}

/// The local container runtime: the address at which the harness reached it and a client.
pub(crate) struct ContainerRuntime {
	pub(crate) address: RuntimeAddress,
	pub(crate) docker: Docker,
	request_timeout_secs: u64,
}

/// The address of the container runtime, as a URL with the scheme `unix`, `tcp`, or `http`,
/// and where the harness found it. A reservation record names the runtime by the URL.
#[derive(Clone, Debug)]
pub(crate) enum RuntimeAddress {
	/// The value of `DOCKER_HOST`.
	DockerHost(String),
	/// The first default socket that exists because `DOCKER_HOST` is not set.
	DefaultSocket(String),
}

impl RuntimeAddress {
	pub(crate) fn url(&self) -> &str {
		match self {
			Self::DockerHost(url) | Self::DefaultSocket(url) => url,
		}
	}

	/// Whether a reservation record names this runtime. A record with a three-part identifier
	/// names its runtime by its process's `DOCKER_HOST`, or by an empty text when that process
	/// had none and therefore used the first default socket that existed.
	pub(crate) fn is_recorded_as(&self, recorded: &RecordedRuntime) -> bool {
		match recorded {
			RecordedRuntime::Address(url) => url == self.url(),
			RecordedRuntime::DockerHost(value) => match self {
				Self::DockerHost(url) => url == value,
				Self::DefaultSocket(_) => value.is_empty(),
			},
		}
	}
}

impl ContainerRuntime {
	/// Finds the runtime's address, opens a client, and checks that the runtime answers.
	///
	/// The address is the value of `DOCKER_HOST`. When `DOCKER_HOST` is not set, it is the
	/// first socket of [`default_runtime_sockets`] that exists. Each request may take
	/// `request_budget`, rounded up to whole seconds.
	pub(crate) async fn connect(
		deadline: Deadline,
		request_budget: Duration,
	) -> Result<Self, PgTestHarnessError> {
		let address = resolve_runtime_address()?;
		let request_timeout_secs = request_budget
			.as_secs()
			.saturating_add(u64::from(request_budget.subsec_nanos() != 0));
		let docker = connect_runtime(address.url(), request_timeout_secs)?;
		until(deadline, PgTestHarnessPhase::Environment, async {
			docker.ping().await.map(|_| ()).map_err(|error| {
				PgTestHarnessError::with_source(
					PgTestHarnessPhase::Environment,
					format!("the container runtime at {} did not answer", address.url()),
					error,
				)
			})
		})
		.await?;
		Ok(Self {
			address,
			docker,
			request_timeout_secs,
		})
	}

	/// Opens another client of the same runtime. A client opens its connections in the async
	/// runtime that sends its requests, so cleanup in an async runtime of its own uses a client
	/// of its own.
	pub(crate) fn reconnect(&self) -> Result<Docker, PgTestHarnessError> {
		connect_runtime(self.address.url(), self.request_timeout_secs)
	}
}

/// The sockets of Docker, rootless Docker, and Docker Desktop, in the order in which the harness
/// looks for them when `DOCKER_HOST` is not set.
fn default_runtime_sockets() -> Vec<PathBuf> {
	let mut sockets = vec![PathBuf::from("/var/run/docker.sock")];
	if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
		sockets.push(PathBuf::from(dir).join(".docker/run/docker.sock"));
	}
	if let Some(home) = std::env::home_dir() {
		sockets.push(home.join(".docker/run/docker.sock"));
		sockets.push(home.join(".docker/desktop/docker.sock"));
	}
	sockets
}

/// The reason that the harness gives when `DOCKER_TLS_VERIFY` is set.
const DOCKER_TLS_VERIFY_REJECTION: &str = "the DOCKER_TLS_VERIFY environment variable is set; the harness connects to the container runtime without TLS, so unset it";

fn resolve_runtime_address() -> Result<RuntimeAddress, PgTestHarnessError> {
	let environment_error =
		|reason: String| PgTestHarnessError::new(PgTestHarnessPhase::Environment, reason);
	if std::env::var_os("DOCKER_TLS_VERIFY").is_some_and(|value| !value.is_empty()) {
		return Err(environment_error(DOCKER_TLS_VERIFY_REJECTION.to_owned()));
	}
	match std::env::var("DOCKER_HOST") {
		Ok(address) => Ok(RuntimeAddress::DockerHost(address)),
		Err(std::env::VarError::NotUnicode(_)) => Err(environment_error(
			"DOCKER_HOST is not valid Unicode".to_owned(),
		)),
		Err(std::env::VarError::NotPresent) => {
			let sockets = default_runtime_sockets();
			sockets
				.iter()
				.filter(|socket| socket.exists())
				.find_map(|socket| socket.to_str())
				.map(|socket| RuntimeAddress::DefaultSocket(format!("unix://{socket}")))
				.ok_or_else(|| {
					environment_error(format!(
						"DOCKER_HOST is not set, and none of the default container runtime sockets exists: {}",
						sockets
							.iter()
							.map(|socket| socket.display().to_string())
							.collect::<Vec<_>>()
							.join(", ")
					))
				})
		}
	}
}

fn connect_runtime(address: &str, request_timeout_secs: u64) -> Result<Docker, PgTestHarnessError> {
	let connected = match address.split_once("://").map(|(scheme, _)| scheme) {
		#[cfg(unix)]
		Some("unix") => Docker::connect_with_unix(address, request_timeout_secs, API_DEFAULT_VERSION),
		Some("tcp" | "http") => {
			Docker::connect_with_http(address, request_timeout_secs, API_DEFAULT_VERSION)
		}
		_ => {
			return Err(PgTestHarnessError::new(
				PgTestHarnessPhase::Environment,
				format!(
					"the container runtime address {address} does not start with unix://, tcp://, or http://"
				),
			));
		}
	};
	connected.map_err(|error| {
		PgTestHarnessError::with_source(
			PgTestHarnessPhase::Environment,
			"could not connect to the local container runtime",
			error,
		)
	})
}

/// A container that the runtime created for the harness, by its identifier.
pub(crate) struct HarnessContainer {
	id: String,
}

/// The lifecycle of one of the harness's containers. The record in the reservation of the
/// PostgreSQL port is its durable copy: before the harness sends a create request, it records
/// that a container can exist ([`RecordedContainer::PossiblyPresent`]), and after a definite
/// rejection or a removal, it records that none exists ([`RecordedContainer::Absent`]).
pub(crate) enum ContainerLifecycle {
	/// The harness has not asked the runtime to create the container, or the runtime rejected
	/// the request, so no container exists.
	NotCreated,
	/// The harness sent a create request and has no answer that settles it: the request was
	/// interrupted, or it failed in a way that does not establish whether the runtime created
	/// the container. The harness never starts such a container, so it holds no port.
	CreationUncertain,
	/// The runtime created the container.
	Created(HarnessContainer),
	/// The harness removed the container.
	Removed,
}

impl ContainerLifecycle {
	/// Whether no container of this lifecycle exists.
	pub(crate) fn is_settled(&self) -> bool {
		match self {
			Self::NotCreated | Self::Removed => true,
			Self::CreationUncertain | Self::Created(_) => false,
		}
	}

	fn enter_created(&mut self, container: HarnessContainer) -> &HarnessContainer {
		*self = Self::Created(container);
		match self {
			Self::Created(container) => container,
			Self::NotCreated | Self::CreationUncertain | Self::Removed => {
				unreachable!("the lifecycle has just entered the created state")
			}
		}
	}
}

/// The lifecycles of the harness's three containers.
pub(crate) struct HarnessContainers {
	pub(crate) pg: ContainerLifecycle,
	pub(crate) pgbouncer_transaction: ContainerLifecycle,
	pub(crate) pgbouncer_statement: ContainerLifecycle,
}

impl HarnessContainers {
	pub(crate) fn not_created() -> Self {
		Self {
			pg: ContainerLifecycle::NotCreated,
			pgbouncer_transaction: ContainerLifecycle::NotCreated,
			pgbouncer_statement: ContainerLifecycle::NotCreated,
		}
	}

	pub(crate) fn are_settled(&self) -> bool {
		self.pg.is_settled()
			&& self.pgbouncer_transaction.is_settled()
			&& self.pgbouncer_statement.is_settled()
	}
}

pub(crate) struct ContainerStartup<'a> {
	pub(crate) docker: &'a Docker,
	pub(crate) run_id: &'a str,
	/// The reservation of the PostgreSQL port, which records the containers.
	pub(crate) record: &'a PortReservation,
	/// The password of the harness's roles.
	pub(crate) role_password: &'a str,
	/// The password of the cluster's superuser.
	pub(crate) superuser_password: &'a str,
	pub(crate) end: Deadline,
}

pub(crate) fn pgbouncer_target(pooler: PgTestHarnessPooler) -> CreationTarget {
	match pooler {
		PgTestHarnessPooler::Transaction => CreationTarget::PgBouncerTransaction,
		PgTestHarnessPooler::Statement => CreationTarget::PgBouncerStatement,
	}
}

fn container_server(target: CreationTarget) -> &'static str {
	match target {
		CreationTarget::Postgres => "postgres",
		CreationTarget::PgBouncerTransaction => "pgbouncer-transaction",
		CreationTarget::PgBouncerStatement => "pgbouncer-statement",
	}
}

fn container_label(target: CreationTarget) -> &'static str {
	match target {
		CreationTarget::Postgres => "PostgreSQL",
		CreationTarget::PgBouncerTransaction | CreationTarget::PgBouncerStatement => "PgBouncer",
	}
}

fn stop_phase(target: CreationTarget) -> PgTestHarnessPhase {
	match target {
		CreationTarget::Postgres => PgTestHarnessPhase::PgStop,
		CreationTarget::PgBouncerTransaction | CreationTarget::PgBouncerStatement => {
			PgTestHarnessPhase::PgBouncerStop
		}
	}
}

fn container_name(run_id: &str, target: CreationTarget) -> String {
	format!("paranoid-testing-{run_id}-{}", container_server(target))
}

/// The platform of created containers and pulled images: `DOCKER_DEFAULT_PLATFORM`, or the
/// runtime's choice when the variable is unset or empty.
fn default_platform() -> String {
	std::env::var("DOCKER_DEFAULT_PLATFORM")
		.ok()
		.filter(|platform| !platform.trim().is_empty())
		.unwrap_or_default()
}

pub(crate) async fn start_pg(
	config: &ParsedConfig,
	pg_port: u16,
	startup: &ContainerStartup<'_>,
	lifecycle: &mut ContainerLifecycle,
) -> Result<(), PgTestHarnessError> {
	let phase = PgTestHarnessPhase::PgStart;
	let body = ContainerCreateBody {
		image: Some(config.pg_image.reference.clone()),
		env: Some(vec![
			format!("POSTGRES_DB={DATABASE_NAME}"),
			format!("POSTGRES_USER={SUPERUSER_ROLE_NAME}"),
			// The entry point reads the password from an uploaded file, so the password is in
			// no environment variable of the container and in no create request.
			format!("POSTGRES_PASSWORD_FILE={SUPERUSER_PASSWORD_PATH}"),
			// Without `--auth`, initdb trusts every connection from 127.0.0.1 and from the
			// container's Unix socket, so PostgreSQL would check no password.
			"POSTGRES_INITDB_ARGS=--locale=C --encoding=UTF8 --auth=scram-sha-256".to_owned(),
			"LANG=C".to_owned(),
			"LC_ALL=C".to_owned(),
		]),
		cmd: Some(vec![
			"postgres".to_owned(),
			"-p".to_owned(),
			pg_port.to_string(),
			"-c".to_owned(),
			"listen_addresses=127.0.0.1".to_owned(),
			"-c".to_owned(),
			format!("max_connections={}", config.pg_max_connections),
		]),
		..container_body(startup.run_id)
	};
	until(startup.end, phase, async {
		let container = create_container(
			startup,
			lifecycle,
			CreationTarget::Postgres,
			&config.pg_image,
			body,
			phase,
		)
		.await?;
		upload_files(
			startup.docker,
			container,
			&[ContainerFile {
				path: SUPERUSER_PASSWORD_PATH,
				content: startup.superuser_password,
			}],
			phase,
		)
		.await?;
		let ready_end =
			Deadline::after(config.pg_startup_timeout, "pg_startup_timeout")?.min(startup.end);
		start_and_wait(
			startup.docker,
			container,
			ready_end,
			phase,
			b"database system is ready to accept connections",
			2,
		)
		.await
	})
	.await
}

/// The two ports of one PgBouncer server.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PgBouncerPorts {
	/// The port of PostgreSQL, which PgBouncer connects to.
	pub(crate) pg_port: u16,
	/// The port that PgBouncer listens on.
	pub(crate) pgbouncer_port: u16,
}

pub(crate) async fn start_pgbouncer(
	config: &ParsedConfig,
	ports: PgBouncerPorts,
	pooler: PgTestHarnessPooler,
	startup: &ContainerStartup<'_>,
	lifecycle: &mut ContainerLifecycle,
) -> Result<(), PgTestHarnessError> {
	let phase = PgTestHarnessPhase::PgBouncerStart;
	let body = ContainerCreateBody {
		image: Some(config.pgbouncer_image.reference.clone()),
		cmd: Some(vec![PGBOUNCER_CONFIG_PATH.to_owned()]),
		..container_body(startup.run_id)
	};
	until(startup.end, phase, async {
		let container = create_container(
			startup,
			lifecycle,
			pgbouncer_target(pooler),
			&config.pgbouncer_image,
			body,
			phase,
		)
		.await?;
		upload_files(
			startup.docker,
			container,
			&[
				ContainerFile {
					path: PGBOUNCER_CONFIG_PATH,
					content: &create_pgbouncer_config(config, ports, pooler),
				},
				ContainerFile {
					path: PGBOUNCER_USERLIST_PATH,
					content: &create_pgbouncer_userlist(startup.role_password),
				},
			],
			phase,
		)
		.await?;
		let ready_end = Deadline::after(
			config.pgbouncer_startup_timeout,
			"pgbouncer_startup_timeout",
		)?
		.min(startup.end);
		start_and_wait(
			startup.docker,
			container,
			ready_end,
			phase,
			b"process up: PgBouncer ",
			1,
		)
		.await
	})
	.await
}

fn container_body(run_id: &str) -> ContainerCreateBody {
	ContainerCreateBody {
		labels: Some(HashMap::from([(RUN_LABEL.to_owned(), run_id.to_owned())])),
		host_config: Some(HostConfig {
			network_mode: Some("host".to_owned()),
			..HostConfig::default()
		}),
		..ContainerCreateBody::default()
	}
}

/// Creates the container of `target`, which must not have been created, and pulls its image
/// when the runtime does not have it. The lifecycle and its record follow every step so that a
/// caller that drops this future leaves the creation uncertain.
async fn create_container<'a>(
	startup: &ContainerStartup<'_>,
	lifecycle: &'a mut ContainerLifecycle,
	target: CreationTarget,
	image: &ImageReference,
	body: ContainerCreateBody,
	phase: PgTestHarnessPhase,
) -> Result<&'a HarnessContainer, PgTestHarnessError> {
	let docker = startup.docker;
	let options = CreateContainerOptionsBuilder::new()
		.name(&container_name(startup.run_id, target))
		.platform(&default_platform())
		.build();
	begin_creation(startup.record, lifecycle, target, phase)?;
	let mut response = docker
		.create_container(Some(options.clone()), body.clone())
		.await;
	if matches!(
		&response,
		Err(DockerError::DockerResponseServerError {
			status_code: 404,
			..
		})
	) {
		// The runtime does not have the image, so it created nothing.
		settle_not_created(startup.record, lifecycle, target)
			.map_err(|error| record_error(phase, error))?;
		pull_image(docker, image, phase).await?;
		begin_creation(startup.record, lifecycle, target, phase)?;
		response = docker.create_container(Some(options), body).await;
	}
	match response {
		Ok(created) => Ok(lifecycle.enter_created(HarnessContainer { id: created.id })),
		Err(error) => {
			if establishes_non_creation(&error) {
				// The lifecycle is `NotCreated` even when the record cannot be written. The
				// record then still names the container as possibly present, which matters
				// only if the reservations outlive the harness: when every container is
				// settled, the harness releases them and deletes the record. The creation
				// failure is the error to report.
				let _ = settle_not_created(startup.record, lifecycle, target);
			}
			Err(operation_error(
				phase,
				"could not create the container",
				error,
			))
		}
	}
}

/// Records that a container of `target` can exist, and only then lets the caller send its create
/// request.
fn begin_creation(
	record: &PortReservation,
	lifecycle: &mut ContainerLifecycle,
	target: CreationTarget,
	phase: PgTestHarnessPhase,
) -> Result<(), PgTestHarnessError> {
	record
		.record_container(target, RecordedContainer::PossiblyPresent)
		.map_err(|error| record_error(phase, error))?;
	*lifecycle = ContainerLifecycle::CreationUncertain;
	Ok(())
}

fn settle_not_created(
	record: &PortReservation,
	lifecycle: &mut ContainerLifecycle,
	target: CreationTarget,
) -> std::io::Result<()> {
	*lifecycle = ContainerLifecycle::NotCreated;
	record.record_container(target, RecordedContainer::Absent)
}

fn record_error(phase: PgTestHarnessPhase, error: std::io::Error) -> PgTestHarnessError {
	PgTestHarnessError::with_source(
		phase,
		"could not record the container in the reservation of the PostgreSQL port",
		error,
	)
}

/// Whether a failed create request establishes that the runtime created no container: the
/// runtime rejected the request, or the client could not connect to the runtime, so it never
/// sent the request. Any other failure leaves the creation uncertain.
fn establishes_non_creation(error: &DockerError) -> bool {
	match error {
		DockerError::DockerResponseServerError { status_code, .. } => {
			(400..500).contains(status_code)
		}
		DockerError::HyperLegacyError { err } => err.is_connect(),
		_ => false,
	}
}

async fn pull_image(
	docker: &Docker,
	image: &ImageReference,
	phase: PgTestHarnessPhase,
) -> Result<(), PgTestHarnessError> {
	let credentials = credentials_for_image(&image.reference)
		.await
		.map_err(|error| {
			PgTestHarnessError::with_source(
				phase,
				"could not pull the configured image because the registry credentials that the Docker configuration names for it could not be resolved",
				error,
			)
		})?;
	let options = CreateImageOptionsBuilder::new()
		.from_image(&image.reference)
		.platform(&default_platform())
		.build();
	let mut stream = docker.create_image(Some(options), None, credentials);
	while let Some(next) = stream.next().await {
		next.map_err(|error| operation_error(phase, "could not pull the configured image", error))?;
	}
	Ok(())
}

/// A file that the harness copies into a container before it starts the container.
struct ContainerFile<'a> {
	/// The absolute path in the container.
	path: &'a str,
	content: &'a str,
}

/// Copies `files` into the container as files of the user root with the mode 0600. The body of
/// the request is a stream, so the client's request log, which shows a complete body, does not
/// show the files, which contain passwords.
async fn upload_files(
	docker: &Docker,
	container: &HarnessContainer,
	files: &[ContainerFile<'_>],
	phase: PgTestHarnessPhase,
) -> Result<(), PgTestHarnessError> {
	let archive = create_archive(files).map_err(|error| {
		PgTestHarnessError::with_source(
			phase,
			"could not prepare the archive of the container's files",
			error,
		)
	})?;
	docker
		.upload_to_container(
			&container.id,
			Some(UploadToContainerOptionsBuilder::new().path("/").build()),
			body_stream(futures_util::stream::iter([archive.into()])),
		)
		.await
		.map_err(|error| operation_error(phase, "could not copy files into the container", error))
}

fn create_archive(files: &[ContainerFile<'_>]) -> Result<Vec<u8>, std::io::Error> {
	let mut archive = tar::Builder::new(Vec::new());
	for file in files {
		let mut header = tar::Header::new_gnu();
		header.set_size(file.content.len() as u64);
		header.set_mode(0o600);
		header.set_cksum();
		archive.append_data(
			&mut header,
			file.path.trim_start_matches('/'),
			file.content.as_bytes(),
		)?;
	}
	archive.into_inner()
}

/// How many of the last bytes of each output stream of a container a readiness error shows.
const OUTPUT_TAIL_LEN: usize = 2048;

/// The last bytes of a container's standard output and standard error.
#[derive(Default)]
struct ContainerOutputTail {
	stdout: Vec<u8>,
	stderr: Vec<u8>,
}

impl ContainerOutputTail {
	fn keep(tail: &mut Vec<u8>, bytes: &[u8]) {
		tail.extend_from_slice(bytes);
		if tail.len() > OUTPUT_TAIL_LEN {
			tail.drain(..tail.len() - OUTPUT_TAIL_LEN);
		}
	}

	/// The detail that a readiness error appends to its reason, or nothing when the container
	/// wrote no output.
	fn describe(&self) -> String {
		if self.stdout.is_empty() && self.stderr.is_empty() {
			return String::new();
		}
		format!(
			"; the last output of the container: stdout {:?}, stderr {:?}",
			String::from_utf8_lossy(&self.stdout),
			String::from_utf8_lossy(&self.stderr)
		)
	}
}

async fn start_and_wait(
	docker: &Docker,
	container: &HarnessContainer,
	deadline: Deadline,
	phase: PgTestHarnessPhase,
	marker: &[u8],
	occurrences: usize,
) -> Result<(), PgTestHarnessError> {
	let mut tail = ContainerOutputTail::default();
	let outcome = until(deadline, phase, async {
		docker
			.start_container(&container.id, None::<StartContainerOptions>)
			.await
			.map_err(|error| operation_error(phase, "could not start the container", error))?;
		let options = LogsOptionsBuilder::new()
			.follow(true)
			.stdout(true)
			.stderr(true)
			.build();
		let mut logs = docker.logs(&container.id, Some(options));
		let mut stdout = Vec::new();
		let mut stderr = Vec::new();
		let mut found = 0;
		while let Some(frame) = logs.next().await {
			let frame = frame.map_err(|error| {
				operation_error(phase, "could not read container readiness logs", error)
			})?;
			let (buffer, kept, bytes) = match &frame {
				LogOutput::StdOut { message } => (&mut stdout, &mut tail.stdout, message.as_ref()),
				LogOutput::StdErr { message } => (&mut stderr, &mut tail.stderr, message.as_ref()),
				_ => continue,
			};
			ContainerOutputTail::keep(kept, bytes);
			buffer.extend_from_slice(bytes);
			while let Some(offset) = buffer.windows(marker.len()).position(|part| part == marker) {
				buffer.drain(..offset + marker.len());
				found += 1;
				if found >= occurrences {
					return Ok(());
				}
			}
			let keep = marker.len().saturating_sub(1);
			if buffer.len() > keep {
				buffer.drain(..buffer.len() - keep);
			}
		}
		Err(PgTestHarnessError::new(
			phase,
			"container logs ended before readiness",
		))
	})
	.await;
	outcome.map_err(|error| error.with_detail(&tail.describe()))
}

fn operation_error(
	phase: PgTestHarnessPhase,
	reason: &'static str,
	error: DockerError,
) -> PgTestHarnessError {
	PgTestHarnessError::with_source(phase, reason, error)
}

/// How the harness removes a container that it created.
#[derive(Clone, Copy)]
pub(crate) enum ContainerRemoval {
	/// The runtime stops the container within the stop timeout so that its server shuts down
	/// before removal.
	StopThenRemove,
	/// The runtime removes the container at once and kills its process.
	Remove,
}

pub(crate) struct ContainerCleanup<'a> {
	pub(crate) docker: &'a Docker,
	pub(crate) run_id: &'a str,
	/// The reservation of the PostgreSQL port, which records the containers.
	pub(crate) record: &'a PortReservation,
	pub(crate) stop_timeout_secs: i32,
	pub(crate) end: Deadline,
	pub(crate) removal: ContainerRemoval,
}

/// This constant bounds how long cleanup of an interrupted create request in the same harness
/// polls for the container runtime to materialize the container before leaving the creation
/// uncertain.
pub(crate) const UNCERTAIN_CREATION_SETTLE_BUDGET: Duration = Duration::from_secs(2);

/// This constant bounds how long leftover recovery for an earlier harness polls for an
/// in-flight create request to materialize before counting a missing container as absent.
pub(crate) const RECORDED_CREATION_SETTLE_BUDGET: Duration = Duration::from_millis(200);

const UNCERTAIN_CREATION_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// This function interprets the container runtime's response to stopping a container. HTTP 304
/// indicates that the container's main process has already exited, whereas HTTP 404 and other
/// errors remain failures.
fn interpret_stop_container_result(result: Result<(), DockerError>) -> Result<(), DockerError> {
	match result {
		Ok(())
		| Err(DockerError::DockerResponseServerError {
			status_code: 304, ..
		}) => Ok(()),
		Err(error) => Err(error),
	}
}

/// Removes the container of `target` according to its lifecycle and returns the failures. A
/// created container is removed by its identifier. A container with an uncertain creation is
/// removed by its name, polling briefly in case an interrupted create request finishes in the
/// runtime after cancellation. When the runtime still has no container of that name, the
/// creation stays uncertain, and the next harness that starts settles it.
pub(crate) async fn remove_container(
	cleanup: &ContainerCleanup<'_>,
	lifecycle: &mut ContainerLifecycle,
	target: CreationTarget,
) -> Vec<PgTestHarnessError> {
	let phase = stop_phase(target);
	let label = container_label(target);
	let mut failures = Vec::new();
	let removed = match &*lifecycle {
		ContainerLifecycle::NotCreated | ContainerLifecycle::Removed => return failures,
		ContainerLifecycle::Created(container) => {
			if let ContainerRemoval::StopThenRemove = cleanup.removal {
				let stopped = until(cleanup.end, phase, async {
					interpret_stop_container_result(
						cleanup
							.docker
							.stop_container(
								&container.id,
								Some(
									StopContainerOptionsBuilder::new()
										.t(cleanup.stop_timeout_secs)
										.build(),
								),
							)
							.await,
					)
					.map_err(|error| {
						PgTestHarnessError::with_source(
							phase,
							format!("could not stop the {label} container"),
							error,
						)
					})
				})
				.await;
				failures.extend(stopped.err());
			}
			until(cleanup.end, phase, async {
				cleanup
					.docker
					.remove_container(
						&container.id,
						Some(
							RemoveContainerOptionsBuilder::new()
								.force(true)
								.v(true)
								.build(),
						),
					)
					.await
					.map_err(|error| {
						PgTestHarnessError::with_source(
							phase,
							format!("could not remove the {label} container"),
							error,
						)
					})
			})
			.await
		}
		ContainerLifecycle::CreationUncertain => {
			let removal = until(cleanup.end, phase, async {
				remove_uncertain_container_by_name(
					cleanup.docker,
					cleanup.run_id,
					target,
					UNCERTAIN_CREATION_SETTLE_BUDGET,
					cleanup.end,
				)
				.await
				.map_err(|error| {
					PgTestHarnessError::with_source(
						phase,
						format!("could not remove the {label} container by its name"),
						error,
					)
				})
			})
			.await;
			match removal {
				Ok(NameRemoval::Removed) => Ok(()),
				Ok(NameRemoval::NoContainer) => Err(PgTestHarnessError::new(
					phase,
					format!(
						"the {label} creation outcome remains uncertain because the runtime has no container with its name; the next harness that starts settles it"
					),
				)),
				Err(error) => Err(error),
			}
		}
	};
	match removed {
		Ok(()) => {
			*lifecycle = ContainerLifecycle::Removed;
			if let Err(error) = cleanup
				.record
				.record_container(target, RecordedContainer::Absent)
			{
				failures.push(PgTestHarnessError::with_source(
					phase,
					format!(
						"could not record the removal of the {label} container in the reservation of the PostgreSQL port"
					),
					error,
				));
			}
		}
		Err(error) => failures.push(error),
	}
	failures
}

/// The outcome of a removal by name.
pub(crate) enum NameRemoval {
	Removed,
	/// The runtime has no container with the name.
	NoContainer,
}

/// This function removes a container that has an uncertain creation by its name, and it polls
/// within `settle_budget` and `deadline` so that it also removes a container that the container
/// runtime creates shortly after an initial `404` response.
pub(crate) async fn remove_uncertain_container_by_name(
	docker: &Docker,
	run_id: &str,
	target: CreationTarget,
	settle_budget: Duration,
	deadline: Deadline,
) -> Result<NameRemoval, DockerError> {
	let settle_end = Instant::now()
		.checked_add(settle_budget)
		.filter(|&candidate| candidate < deadline.end);
	loop {
		match remove_container_by_name(docker, run_id, target).await? {
			NameRemoval::Removed => return Ok(NameRemoval::Removed),
			NameRemoval::NoContainer => {
				let remaining = match settle_end {
					Some(candidate) => candidate.checked_duration_since(Instant::now()),
					None => deadline.remaining(),
				};
				let Some(remaining) = remaining.filter(|duration| !duration.is_zero()) else {
					return Ok(NameRemoval::NoContainer);
				};
				tokio::time::sleep(remaining.min(UNCERTAIN_CREATION_POLL_INTERVAL)).await;
			}
		}
	}
}

/// Removes the container of `target` of the harness with the identifier `run_id` by its name.
async fn remove_container_by_name(
	docker: &Docker,
	run_id: &str,
	target: CreationTarget,
) -> Result<NameRemoval, DockerError> {
	match docker
		.remove_container(
			&container_name(run_id, target),
			Some(
				RemoveContainerOptionsBuilder::new()
					.force(true)
					.v(true)
					.build(),
			),
		)
		.await
	{
		Ok(()) => Ok(NameRemoval::Removed),
		Err(DockerError::DockerResponseServerError {
			status_code: 404, ..
		}) => Ok(NameRemoval::NoContainer),
		Err(error) => Err(error),
	}
}

/// The configuration of one PgBouncer server. Its fallback entry forwards a connection to the
/// PostgreSQL database that the connection names, so the server needs no change when a
/// database is created. Its only listener is its TCP port: when PgBouncer cannot bind that port,
/// it has no listener.
fn create_pgbouncer_config(
	config: &ParsedConfig,
	ports: PgBouncerPorts,
	pooler: PgTestHarnessPooler,
) -> String {
	let PgBouncerPorts {
		pg_port,
		pgbouncer_port,
	} = ports;
	format!(
		"[databases]
* = host=127.0.0.1 port={pg_port}

[pgbouncer]
listen_addr = 127.0.0.1
listen_port = {pgbouncer_port}
unix_socket_dir =
auth_type = plain
auth_file = {PGBOUNCER_USERLIST_PATH}
admin_users = {WRITER_ROLE_NAME}
pool_mode = {}
max_prepared_statements = {}
ignore_startup_parameters = extra_float_digits
max_client_conn = {}
default_pool_size = {}
",
		pooler.pool_mode(),
		config.pgbouncer_max_prepared_statements,
		config.pgbouncer_max_clients,
		config.pgbouncer_pool_size,
	)
}

/// The user list of PgBouncer. The password consists of hexadecimal digits, which need no
/// quoting there or in SQL, URLs, and environment variables.
fn create_pgbouncer_userlist(password: &str) -> String {
	PGBOUNCER_ROLE_NAMES
		.iter()
		.map(|role| format!("\"{role}\" \"{password}\"\n"))
		.collect()
}

/// The settings that keep PostgreSQL from writing any statement text of the session that
/// creates the roles to its log: the text of a failing statement (`log_min_error_statement`),
/// every statement (`log_statement`), and statements that run long or that a sample selects
/// (`log_min_duration_statement`, `log_min_duration_sample`, and
/// `log_transaction_sample_rate`). Only a superuser can change them. The session sends them in
/// a message of their own before the message that contains the role password because
/// PostgreSQL parses a whole message before it runs any statement of it, and logs the whole
/// message when the parse fails.
pub(crate) const UNLOGGED_SESSION_SQL: &str = "SET log_min_error_statement = panic; SET log_statement = none; SET log_min_duration_statement = -1; SET log_min_duration_sample = -1; SET log_transaction_sample_rate = 0";

/// The statements, run as the superuser in the database `test`, that create the writer role,
/// make it the owner of `test`, and then, as the writer, create the three scenario roles so
/// that the writer administers them.
pub(crate) fn create_role_provisioning_sql(
	statement_timeout_millis: u32,
	password: &str,
) -> String {
	format!(
		"CREATE ROLE {WRITER_ROLE_NAME} LOGIN CREATEROLE BYPASSRLS PASSWORD '{password}';
ALTER DATABASE {DATABASE_NAME} OWNER TO {WRITER_ROLE_NAME};
SET ROLE {WRITER_ROLE_NAME};
CREATE ROLE {SUBJECT_TO_ROW_SECURITY_ROLE_NAME} LOGIN PASSWORD '{password}';
CREATE ROLE {READ_ONLY_ROLE_NAME} LOGIN PASSWORD '{password}';
ALTER ROLE {READ_ONLY_ROLE_NAME} SET default_transaction_read_only = on;
CREATE ROLE {STATEMENT_TIMEOUT_ROLE_NAME} LOGIN PASSWORD '{password}';
ALTER ROLE {STATEMENT_TIMEOUT_ROLE_NAME} SET statement_timeout = '{statement_timeout_millis}ms';
"
	)
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::Duration;

	const TEST_PORTS: PgBouncerPorts = PgBouncerPorts {
		pg_port: 15432,
		pgbouncer_port: 25432,
	};

	fn parsed_config() -> ParsedConfig {
		crate::PgTestHarnessConfig {
			pg_image: "docker.io/library/postgres:16".to_owned(),
			pgbouncer_image: "docker.io/edoburu/pgbouncer:latest".to_owned(),
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
		.parse()
		.unwrap()
	}

	#[test]
	fn every_role_except_the_superuser_is_quoted_in_the_pgbouncer_user_list() {
		assert_eq!(
			create_pgbouncer_userlist("0a1b")
				.split('\n')
				.collect::<Vec<_>>(),
			[
				"\"test\" \"0a1b\"",
				"\"paranoid_nobypass\" \"0a1b\"",
				"\"paranoid_read_only\" \"0a1b\"",
				"\"paranoid_statement_timeout\" \"0a1b\"",
				"",
			]
		);
	}

	#[test]
	fn the_pgbouncer_config_names_its_pooling_mode_every_bound_and_the_prepared_statement_limit() {
		for (pooler, pool_mode) in [
			(PgTestHarnessPooler::Transaction, "transaction"),
			(PgTestHarnessPooler::Statement, "statement"),
		] {
			let text = create_pgbouncer_config(&parsed_config(), TEST_PORTS, pooler);
			assert!(
				text.starts_with("[databases]\n* = host=127.0.0.1 port=15432\n\n[pgbouncer]\n")
			);
			assert!(
				text.contains("listen_addr = 127.0.0.1\nlisten_port = 25432\nunix_socket_dir =\n")
			);
			assert!(text.contains(&format!("\npool_mode = {pool_mode}\n")));
			assert_eq!(text.matches("pool_mode").count(), 1);
			assert!(text.contains("max_client_conn = 500"));
			assert!(text.contains("default_pool_size = 20"));
			assert!(text.contains("admin_users = test"));
			assert!(text.contains("\nmax_prepared_statements = 200\n"));
			assert!(!text.contains("server_reset_query"));
		}
	}

	#[test]
	fn the_pgbouncer_config_carries_the_configured_prepared_statement_limit() {
		let mut parsed = parsed_config();
		parsed.pgbouncer_max_prepared_statements = 0;
		assert!(
			create_pgbouncer_config(&parsed, TEST_PORTS, PgTestHarnessPooler::Transaction)
				.contains("\nmax_prepared_statements = 0\n")
		);
	}

	#[test]
	fn role_provisioning_gives_each_scenario_role_only_its_defining_setting() {
		let sql = create_role_provisioning_sql(50, "0a1b");
		assert!(sql.starts_with(
			"CREATE ROLE test LOGIN CREATEROLE BYPASSRLS PASSWORD '0a1b';\nALTER DATABASE test OWNER TO test;\nSET ROLE test;\n"
		));
		assert!(
			sql.contains("ALTER ROLE paranoid_read_only SET default_transaction_read_only = on;")
		);
		assert!(
			sql.contains("ALTER ROLE paranoid_statement_timeout SET statement_timeout = '50ms';")
		);
		assert!(sql.contains("CREATE ROLE paranoid_nobypass LOGIN PASSWORD '0a1b';"));
		assert_eq!(sql.matches("BYPASSRLS").count(), 1);
		assert!(!sql.contains("SUPERUSER"));
		assert!(!sql.contains("GRANT"));
	}

	#[test]
	fn stopping_an_already_exited_container_succeeds_while_missing_containers_still_fail() {
		assert!(interpret_stop_container_result(Ok(())).is_ok());
		assert!(
			interpret_stop_container_result(Err(DockerError::DockerResponseServerError {
				status_code: 304,
				message: "container already stopped".to_owned(),
			}))
			.is_ok()
		);
		assert!(
			interpret_stop_container_result(Err(DockerError::DockerResponseServerError {
				status_code: 404,
				message: "no such container".to_owned(),
			}))
			.is_err()
		);
		assert!(
			interpret_stop_container_result(Err(DockerError::DockerResponseServerError {
				status_code: 500,
				message: "internal server error".to_owned(),
			}))
			.is_err()
		);
	}
}
