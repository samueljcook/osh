use crate::client::{
	ConnectionTarget, HarnessClient, Login, PGOPTIONS_REJECTION, is_pgoptions_set,
};
use crate::config::{ParsedConfig, PgTestHarnessConfig};
use crate::containers::{
	ContainerCleanup, ContainerRemoval, ContainerRuntime, ContainerStartup, DATABASE_NAME,
	Deadline, HarnessContainers, PgBouncerPorts, READ_ONLY_ROLE_NAME,
	RECORDED_CREATION_SETTLE_BUDGET, STATEMENT_TIMEOUT_ROLE_NAME,
	SUBJECT_TO_ROW_SECURITY_ROLE_NAME, UNLOGGED_SESSION_SQL, WRITER_ROLE_NAME,
	create_role_provisioning_sql, remove_container, remove_uncertain_container_by_name, start_pg,
	start_pgbouncer, until,
};
use crate::database::PgTestHarnessDatabase;
use crate::error::{PgTestHarnessError, PgTestHarnessPhase, PgTestHarnessShutdownError};
use crate::port::{
	CreationTarget, HarnessRecord, IdentifierPorts, PortReservation, RecordedContainer,
	RecordedContainers, RecordedRuntime, UnheldReservation, create_harness_identifier,
};
use bollard::Docker;
use paranoid::db::{DatabaseTls, PgIdentifier, PoolConfig};
use paranoid::sqlx::postgres::PgRow;
use paranoid::sqlx::{AssertSqlSafe, PgConnection, Row, raw_sql};
use primitives_rs::encoding::binary_to_text::ByteEncoding;
use primitives_rs::random::secure_bytes::generate_random_byte_array;
use secrecy::{ExposeSecret, SecretString};
use std::sync::Arc;
use std::time::Duration;

/// A server that the harness exposes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PgTestHarnessEndpoint {
	/// PostgreSQL itself, with no pooler.
	Pg,
	/// PgBouncer in transaction pooling.
	PgBouncerTransaction,
	/// PgBouncer in statement pooling.
	PgBouncerStatement,
}

/// One of the harness's two PgBouncer servers, by its pooling mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PgTestHarnessPooler {
	/// The PgBouncer in transaction pooling.
	Transaction,
	/// The PgBouncer in statement pooling.
	Statement,
}

impl PgTestHarnessPooler {
	pub(crate) const ALL: [Self; 2] = [Self::Transaction, Self::Statement];

	/// The value of PgBouncer's `pool_mode` setting.
	pub(crate) fn pool_mode(self) -> &'static str {
		match self {
			Self::Transaction => "transaction",
			Self::Statement => "statement",
		}
	}
}

/// A database role that the harness provisions. Every role of one harness has the same
/// password, which the harness generates at its start and which the connection URLs contain.
/// No role is a superuser.
///
/// A scenario role has no privileges on relations. A test grants the privileges that its
/// scenario needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PgTestHarnessRole {
	/// The owner role, named `test`: the owner of the database `test` and of every database
	/// that [`PgTestHarnessDatabase::create`] makes, and the admin of each PgBouncer. Its role
	/// attributes are `LOGIN`, `CREATEROLE`, and `BYPASSRLS`. It creates the three scenario
	/// roles, so it can change their settings also in PostgreSQL 16 and later, where a role
	/// with `CREATEROLE` changes only the roles that it administers.
	Writer,
	/// A login role that is subject to row security.
	SubjectToRowSecurity,
	/// A login role with transactions that are read-only by default.
	ReadOnly,
	/// A login role with a statement timeout, from
	/// [`statement_timeout`](PgTestHarnessConfig::statement_timeout).
	StatementTimeout,
}

impl PgTestHarnessRole {
	/// The role's name in the database.
	pub fn name(self) -> &'static str {
		match self {
			Self::Writer => WRITER_ROLE_NAME,
			Self::SubjectToRowSecurity => SUBJECT_TO_ROW_SECURITY_ROLE_NAME,
			Self::ReadOnly => READ_ONLY_ROLE_NAME,
			Self::StatementTimeout => STATEMENT_TIMEOUT_ROLE_NAME,
		}
	}
}

/// The pool settings that the harness does not choose, for
/// [`PgTestHarness::pool_config`]. These are the application's own settings.
#[derive(Clone, Debug)]
pub struct PgTestHarnessPoolSettings {
	/// See [`PoolConfig::application_name`].
	pub application_name: String,
	/// See [`PoolConfig::max_connections`].
	pub max_connections: u32,
	/// See [`PoolConfig::min_connections`].
	pub min_connections: u32,
	/// See [`PoolConfig::acquire_timeout`].
	pub acquire_timeout: Duration,
	/// See [`PoolConfig::idle_timeout`].
	pub idle_timeout: Option<Duration>,
	/// See [`PoolConfig::max_lifetime`].
	pub max_lifetime: Option<Duration>,
	/// See [`PoolConfig::client_connection_check_interval`].
	pub client_connection_check_interval: Duration,
}

/// The pool configuration of the harness for a URL, with TLS disabled.
pub(crate) fn create_pool_config(
	database_url: SecretString,
	settings: PgTestHarnessPoolSettings,
) -> PoolConfig {
	PoolConfig {
		database_url,
		tls: DatabaseTls::Disabled,
		application_name: settings.application_name,
		max_connections: settings.max_connections,
		min_connections: settings.min_connections,
		acquire_timeout: settings.acquire_timeout,
		idle_timeout: settings.idle_timeout,
		max_lifetime: settings.max_lifetime,
		client_connection_check_interval: settings.client_connection_check_interval,
	}
}

/// A running PostgreSQL container and two running PgBouncer containers, one in transaction
/// pooling and one in statement pooling.
///
/// Each PgBouncer forwards a connection to the PostgreSQL database that the connection names,
/// so the harness's own database, `test`, and every database that
/// [`PgTestHarnessDatabase::create`] makes are reachable on every endpoint under their own
/// names. The containers share the host's network and listen on loopback ports.
///
/// The cluster's bootstrap superuser, `paranoid_superuser`, has a password of its own, which
/// differs from the password of the harness's roles, and serves the harness's own
/// administration: it provisions the roles and creates and removes databases. The only URL for
/// it is [`PgTestHarnessDatabase::superuser_connection_url`], for test setup that only a
/// superuser can perform, and PgBouncer does not accept it. The harness copies the superuser's
/// password into the PostgreSQL container as a file and the role password into each PgBouncer
/// container as a file, so neither password is in a container's environment or in a request
/// body that the runtime client logs.
///
/// Call [`shutdown`](Self::shutdown) to stop and remove the containers and learn about any
/// failure within the configured cleanup budget. Dropping the harness starts bounded container
/// removal within [`cleanup_timeout`](PgTestHarnessConfig::cleanup_timeout) on a separate thread
/// with an async runtime of its own without blocking the caller, so container removal also runs
/// when the async runtime where the harness ran is unwinding or has ended, as the runtime of a
/// `#[tokio::test]` does when its test returns or panics.
///
/// Containers can remain while background cleanup after drop is still running, when cleanup on
/// drop fails or exceeds `cleanup_timeout`, after a shutdown that could not remove a container,
/// and when the test process ends without running destructors or before the cleanup thread
/// finishes, for example after a kill signal, an abort, or a call to `std::process::exit`.
/// The harness installs no signal handler and runs no separate cleanup process.
/// In each of these cases all three port reservations stay in the system's temporary
/// directory. The PostgreSQL reservation names the harness and records which of its containers
/// can exist, and the PgBouncer reservations refer to it. The ports stay reserved. The next
/// harness that starts removes the containers that the file names and then the file, before it
/// starts its own containers. It does so only when all of these hold:
///
/// - No live process holds the file locked. A harness holds the lock from its start until its
///   shutdown or the end of the removal that its drop started, and the operating system
///   releases the lock when the process ends. A process that the test process started with
///   `fork` and without `exec` holds the lock too, for as long as it lives.
/// - The starting harness runs on Unix, shares the temporary directory, and runs as the user
///   who owns the file.
/// - Both harnesses reach the container runtime at the same address; see
///   [`PgTestHarnessConfig`] for how the harness finds the address. A file of an earlier
///   version of the harness with no random part in its identifier names the runtime by the
///   value of `DOCKER_HOST`, or by an empty text when that process had none; it names the
///   runtime of a starting harness when that harness has the same `DOCKER_HOST`, or none for an
///   empty text. The starting harness removes all three containers of such a file by name.
/// - The runtime removes the containers. When it does not, the file stays, and the next
///   harness that starts tries again.
///
/// A create request that the harness sent without receiving an answer, for example because the
/// startup deadline elapsed or the start was cancelled, leaves the creation uncertain: the
/// runtime can still create the container. The harness's own cleanup removes such a container
/// by its name when the runtime has one, and otherwise leaves the reservations to the next
/// harness that starts. That harness removes the container by its name, or, when the runtime
/// has no container of that name, counts the creation as settled. A container that the runtime
/// creates after that from the abandoned request is never started, so it holds no port, and it
/// keeps its label and name until someone removes it.
///
/// The removal reports no failure. Each container has the label `paranoid-testing.run`, with
/// its harness identifier as the value, and the name
/// `paranoid-testing-<identifier>-postgres`,
/// `paranoid-testing-<identifier>-pgbouncer-transaction`, or
/// `paranoid-testing-<identifier>-pgbouncer-statement`. The identifier is the ID of the test
/// process, the PostgreSQL port, the first PgBouncer port, and 16 random hexadecimal digits,
/// joined by `-`.
pub struct PgTestHarness {
	config: ParsedConfig,
	client: HarnessClient,
	ownership: HarnessOwnership,
}

/// What the harness owns until it has removed its containers: the client of the container
/// runtime, the reservations of its three ports, and the lifecycle of each container. Dropping
/// it ends the reservations as [`end_reservations`](Self::end_reservations) does.
struct HarnessResources {
	run_id: String,
	runtime: ContainerRuntime,
	/// The reservation of the PostgreSQL port, which records the harness and its containers.
	pg_reservation: PortReservation,
	pgbouncer_reservations: [PortReservation; 2],
	containers: HarnessContainers,
	cleanup_timeout: Duration,
	container_stop_timeout_secs: i32,
}

impl HarnessResources {
	/// Removes every container that can exist, the two PgBouncer containers at the same time
	/// and then the PostgreSQL container, within `cleanup_timeout`, and returns the failures.
	async fn remove_containers(&mut self, removal: ContainerRemoval) -> Vec<PgTestHarnessError> {
		let end = match Deadline::after(self.cleanup_timeout, "cleanup_timeout") {
			Ok(end) => end,
			Err(error) => return vec![error],
		};
		let Self {
			run_id,
			runtime,
			pg_reservation,
			containers,
			container_stop_timeout_secs,
			..
		} = self;
		let cleanup = ContainerCleanup {
			docker: &runtime.docker,
			run_id,
			record: pg_reservation,
			stop_timeout_secs: *container_stop_timeout_secs,
			end,
			removal,
		};
		let HarnessContainers {
			pg,
			pgbouncer_transaction,
			pgbouncer_statement,
		} = containers;
		let (mut failures, statement) = tokio::join!(
			remove_container(
				&cleanup,
				pgbouncer_transaction,
				CreationTarget::PgBouncerTransaction
			),
			remove_container(
				&cleanup,
				pgbouncer_statement,
				CreationTarget::PgBouncerStatement
			),
		);
		failures.extend(statement);
		failures.extend(remove_container(&cleanup, pg, CreationTarget::Postgres).await);
		failures
	}

	/// Releases the three reservations when no container of the harness can exist, and
	/// otherwise leaves them to the next harness that starts, which removes the containers that
	/// the record names. Returns the release failures.
	fn end_reservations(&mut self) -> Vec<PgTestHarnessError> {
		let settled = self.containers.are_settled();
		let reservations =
			std::iter::once(&mut self.pg_reservation).chain(self.pgbouncer_reservations.iter_mut());
		if settled {
			reservations
				.filter_map(|reservation| reservation.release().err())
				.collect()
		} else {
			for reservation in reservations {
				reservation.leave_to_the_next_harness();
			}
			Vec::new()
		}
	}
}

impl Drop for HarnessResources {
	fn drop(&mut self) {
		let _ = self.end_reservations();
	}
}

/// This struct owns the harness's resources until [`finish`](Self::finish) takes them, and it
/// stores `None` afterward. When a caller drops the harness while this struct still owns those
/// resources, `Drop` starts container removal on a separate cleanup thread without waiting for
/// network operations.
struct HarnessOwnership {
	resources: Option<HarnessResources>,
}

impl HarnessOwnership {
	fn resources_mut(&mut self) -> &mut HarnessResources {
		self.resources
			.as_mut()
			.expect("the harness owns its resources until it finishes")
	}

	/// Removes the containers, ends the reservations, and returns every failure. When the
	/// caller drops this future before it finishes, the drop of `self` removes what remains on
	/// its cleanup thread.
	async fn finish(mut self, removal: ContainerRemoval) -> Vec<PgTestHarnessError> {
		let resources = self.resources_mut();
		let mut failures = resources.remove_containers(removal).await;
		failures.extend(resources.end_reservations());
		self.resources.take();
		failures
	}
}

impl Drop for HarnessOwnership {
	fn drop(&mut self) {
		let Some(mut resources) = self.resources.take() else {
			return;
		};
		if resources.containers.are_settled() {
			// No container can exist, so dropping the resources releases the reservations.
			return;
		}
		// The async runtime where the harness ran can be unwinding or shutting down, and a
		// current-thread runtime cannot advance background tasks while `Drop` blocks its thread,
		// so container removal runs on a separate thread with an async runtime of its own without
		// blocking the caller. If the thread cannot start or its async runtime cannot be built,
		// dropping `resources` leaves the port reservations for the next harness to recover.
		let _ = std::thread::Builder::new()
			.name("paranoid-testing-cleanup".to_owned())
			.spawn(move || {
				if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
					.enable_all()
					.build()
				{
					runtime.block_on(async {
						if let Ok(docker) = resources.runtime.reconnect() {
							resources.runtime.docker = docker;
							resources.remove_containers(ContainerRemoval::Remove).await;
						}
					});
					drop(resources);
					runtime.shutdown_timeout(Duration::ZERO);
				}
			});
	}
}

/// How many random bytes each password of a harness has. The harness writes them as lowercase
/// hexadecimal digits, which need no quoting in a URL, in SQL, in PgBouncer's user list, or in
/// a file.
const PASSWORD_RANDOM_BYTES: usize = 16;

fn create_password() -> Result<SecretString, PgTestHarnessError> {
	let bytes: [u8; PASSWORD_RANDOM_BYTES] = generate_random_byte_array().map_err(|error| {
		PgTestHarnessError::with_source(
			PgTestHarnessPhase::Environment,
			"the platform's random source failed",
			error,
		)
	})?;
	Ok(SecretString::from(ByteEncoding::HexLower.encode(&bytes)))
}

/// Runs file and socket work on a thread of Tokio's blocking pool, or returns `None` when the
/// async runtime shut down before the work ran.
async fn run_blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
	match tokio::task::spawn_blocking(work).await {
		Ok(value) => Some(value),
		Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
		Err(_) => None,
	}
}

/// Runs reservation work, which reads, locks, and writes files and binds sockets, on a thread
/// of Tokio's blocking pool.
async fn run_reservation<T: Send + 'static>(
	work: impl FnOnce() -> Result<T, PgTestHarnessError> + Send + 'static,
) -> Result<T, PgTestHarnessError> {
	run_blocking(work).await.unwrap_or_else(|| {
		Err(PgTestHarnessError::new(
			PgTestHarnessPhase::PortReserve,
			"the async runtime shut down before the port reservation ran",
		))
	})
}

impl PgTestHarness {
	/// Starts PostgreSQL, creates the harness's roles, starts both PgBouncer servers, and
	/// checks that each one reports the configured version, its pooling mode, the bounds, and
	/// the prepared statement limit.
	///
	/// When a step fails, the harness removes whatever had started, and the error lists any
	/// failure of that cleanup. When the caller drops the returned future before it finishes,
	/// the harness removes what had started on its cleanup thread, as a dropped harness does.
	///
	/// Before it starts its own containers, the harness removes the containers and the port
	/// reservations that an earlier harness left; see [`PgTestHarness`].
	///
	/// The total startup budget includes the runtime check, image pulls, runtime requests,
	/// leftover recovery, readiness, and database provisioning and verification. Each readiness
	/// limit is also bounded by that shared deadline. Failed startup receives a separate cleanup
	/// budget. A creation that the deadline interrupts is uncertain, and its reservations stay
	/// until a harness settles it, as [`PgTestHarness`] describes.
	pub async fn start(config: PgTestHarnessConfig) -> Result<Self, PgTestHarnessError> {
		let config = config.parse()?;
		let end = Deadline::after(config.startup_timeout, "startup_timeout")?;
		if is_pgoptions_set() {
			return Err(PgTestHarnessError::new(
				PgTestHarnessPhase::Environment,
				PGOPTIONS_REJECTION,
			));
		}
		let role_password = create_password()?;
		let superuser_password = create_password()?;
		let runtime =
			ContainerRuntime::connect(end, config.startup_timeout.max(config.cleanup_timeout))
				.await?;
		let attempts = config.port_reservation_attempts;
		let (mut pg_port, earlier) = run_reservation(move || {
			let pg_port = PortReservation::reserve(attempts)?;
			let earlier = pg_port.take_unheld_reservations();
			Ok((pg_port, earlier))
		})
		.await?;
		until(end, PgTestHarnessPhase::Environment, async {
			remove_what_earlier_harnesses_left(earlier, &runtime, end).await;
			if end.has_passed() {
				return Err(PgTestHarnessError::new(
					PgTestHarnessPhase::Environment,
					"the startup_timeout elapsed during leftover recovery",
				));
			}
			Ok(())
		})
		.await?;
		let runtime_url = runtime.address.url().to_owned();
		// The reservations, the identifier, and the records are file and socket work.
		let (pg_port, [transaction_port, statement_port], run_id) = run_reservation(move || {
			let mut pgbouncer_ports = [
				PortReservation::reserve(attempts)?,
				PortReservation::reserve(attempts)?,
			];
			let run_id = create_harness_identifier(IdentifierPorts {
				pg_port: pg_port.port(),
				first_pgbouncer_port: pgbouncer_ports[0].port(),
			})
			.map_err(|error| {
				PgTestHarnessError::with_source(
					PgTestHarnessPhase::Environment,
					"the platform's random source failed",
					error,
				)
			})?;
			let record = HarnessRecord {
				identifier: run_id.clone(),
				runtime: RecordedRuntime::Address(runtime_url),
				containers: RecordedContainers::absent(),
			};
			pg_port.record_harness(&record)?;
			for port in &mut pgbouncer_ports {
				port.associate_with(&record)?;
			}
			Ok((pg_port, pgbouncer_ports, run_id))
		})
		.await?;
		let client = HarnessClient {
			harness_identity: Arc::new(()),
			role_password,
			superuser_password,
			pg_port: pg_port.port(),
			pgbouncer_ports: [transaction_port.port(), statement_port.port()],
			connect_timeout: config.client_connect_timeout,
			query_timeout: config.client_query_timeout,
		};
		let mut harness = Self {
			client,
			ownership: HarnessOwnership {
				resources: Some(HarnessResources {
					run_id,
					runtime,
					pg_reservation: pg_port,
					pgbouncer_reservations: [transaction_port, statement_port],
					containers: HarnessContainers::not_created(),
					cleanup_timeout: config.cleanup_timeout,
					container_stop_timeout_secs: config.container_stop_timeout_secs,
				}),
			},
			config,
		};
		match harness.start_servers(end).await {
			Ok(()) => Ok(harness),
			Err(error) => {
				let failures = harness.ownership.finish(ContainerRemoval::Remove).await;
				Err(error.with_cleanup_failures(failures))
			}
		}
	}

	/// Stops and removes the containers and releases the reserved ports. Every step runs even
	/// when an earlier one fails. When a container could not be removed, the three port
	/// reservations stay in place, and the next harness that starts tries the removal again
	/// and then removes the reservations. The complete operation is bounded by
	/// `cleanup_timeout`; a stop or removal beyond that deadline is reported as unresolved.
	pub async fn shutdown(self) -> Result<(), PgTestHarnessShutdownError> {
		let failures = self
			.ownership
			.finish(ContainerRemoval::StopThenRemove)
			.await;
		match PgTestHarnessShutdownError::from_failures(failures) {
			Some(error) => Err(error),
			None => Ok(()),
		}
	}

	/// The connection URL for a role on an endpoint. It contains the password.
	pub fn connection_url(
		&self,
		endpoint: PgTestHarnessEndpoint,
		role: PgTestHarnessRole,
	) -> SecretString {
		self.client.connection_url(ConnectionTarget {
			port: self.client.port(endpoint),
			login: Login::Role(role),
			database: DATABASE_NAME,
		})
	}

	/// The URL of a PgBouncer's admin console as the writer role, for a process that runs
	/// beside the harness and issues console commands such as `RECONNECT`. It contains the
	/// password.
	pub fn pgbouncer_admin_url(&self, pooler: PgTestHarnessPooler) -> SecretString {
		self.client
			.connection_url(self.client.pgbouncer_admin_target(pooler))
	}

	/// A complete pool configuration for a role on an endpoint, with TLS disabled. The harness
	/// chooses the URL and the application chooses `settings`.
	pub fn pool_config(
		&self,
		endpoint: PgTestHarnessEndpoint,
		role: PgTestHarnessRole,
		settings: PgTestHarnessPoolSettings,
	) -> PoolConfig {
		create_pool_config(self.connection_url(endpoint, role), settings)
	}

	/// Opens a connection to PostgreSQL itself, for tests that inspect the server. The caller
	/// owns the connection and its queries; the query timeout of the configuration does not
	/// apply to it.
	pub async fn connect_direct(
		&self,
		role: PgTestHarnessRole,
	) -> Result<PgConnection, PgTestHarnessError> {
		self.client.connect_direct(role, DATABASE_NAME).await
	}

	/// The database with this name in the harness's PostgreSQL server, whether or not it
	/// exists yet. [`PgTestHarnessDatabase::create`] creates it so that each test can have a
	/// database of its own in one running harness.
	pub fn database(&self, name: PgIdentifier) -> PgTestHarnessDatabase {
		PgTestHarnessDatabase {
			client: self.client.clone(),
			name,
		}
	}

	/// Opens a connection to a PgBouncer's admin console as the writer role, for tests that
	/// inspect the pooler. The caller owns the connection and its queries.
	pub async fn connect_pgbouncer_admin(
		&self,
		pooler: PgTestHarnessPooler,
	) -> Result<PgConnection, PgTestHarnessError> {
		self.client
			.connect(
				self.client.pgbouncer_admin_target(pooler),
				"could not connect to the PgBouncer admin console",
			)
			.await
	}

	async fn start_servers(&mut self, end: Deadline) -> Result<(), PgTestHarnessError> {
		let Self {
			config,
			client,
			ownership,
		} = self;
		let HarnessResources {
			run_id,
			runtime,
			pg_reservation,
			containers,
			..
		} = ownership.resources_mut();
		let startup = ContainerStartup {
			docker: &runtime.docker,
			run_id,
			record: pg_reservation,
			role_password: client.role_password.expose_secret(),
			superuser_password: client.superuser_password.expose_secret(),
			end,
		};
		start_pg(config, client.pg_port, &startup, &mut containers.pg).await?;
		until(
			end,
			PgTestHarnessPhase::PgRoles,
			provision_roles(config, client),
		)
		.await?;
		start_pgbouncer(
			config,
			PgBouncerPorts {
				pg_port: client.pg_port,
				pgbouncer_port: client.pgbouncer_port(PgTestHarnessPooler::Transaction),
			},
			PgTestHarnessPooler::Transaction,
			&startup,
			&mut containers.pgbouncer_transaction,
		)
		.await?;
		start_pgbouncer(
			config,
			PgBouncerPorts {
				pg_port: client.pg_port,
				pgbouncer_port: client.pgbouncer_port(PgTestHarnessPooler::Statement),
			},
			PgTestHarnessPooler::Statement,
			&startup,
			&mut containers.pgbouncer_statement,
		)
		.await?;
		for pooler in PgTestHarnessPooler::ALL {
			until(
				end,
				PgTestHarnessPhase::PgBouncerVerify,
				verify_pgbouncer(config, client, pooler),
			)
			.await?;
		}
		Ok(())
	}
}

async fn provision_roles(
	config: &ParsedConfig,
	client: &HarnessClient,
) -> Result<(), PgTestHarnessError> {
	let mut connection = client
		.connect(
			client.superuser_target(DATABASE_NAME),
			"could not connect to PostgreSQL on 127.0.0.1 after its container reported readiness; the container runtime must run the containers in the network of this process (host networking)",
		)
		.await?;
	let provisioned = async {
		client
			.run_query(
				PgTestHarnessPhase::PgRoles,
				"could not turn off the server's statement logging for the session that creates the roles",
				raw_sql(UNLOGGED_SESSION_SQL).execute(&mut connection),
			)
			.await?;
		client
			.run_query(
				PgTestHarnessPhase::PgRoles,
				"could not configure the harness database roles",
				async {
					// The statements come from constants, one validated number, and the
					// hexadecimal role password.
					raw_sql(AssertSqlSafe(create_role_provisioning_sql(
						config.statement_timeout_millis,
						client.role_password.expose_secret(),
					)))
					.execute(&mut connection)
					.await
					.map(|_| ())
				},
			)
			.await
	}
	.await;
	client.close(connection, provisioned).await
}

async fn verify_pgbouncer(
	config: &ParsedConfig,
	client: &HarnessClient,
	pooler: PgTestHarnessPooler,
) -> Result<(), PgTestHarnessError> {
	let mut connection = client
		.connect(
			client.pgbouncer_admin_target(pooler),
			"could not connect to the PgBouncer admin console",
		)
		.await?;
	let verified = inspect_pgbouncer(config, client, &mut connection, pooler).await;
	client.close(connection, verified).await
}

async fn inspect_pgbouncer(
	config: &ParsedConfig,
	client: &HarnessClient,
	connection: &mut PgConnection,
	pooler: PgTestHarnessPooler,
) -> Result<(), PgTestHarnessError> {
	let phase = PgTestHarnessPhase::PgBouncerVerify;
	let version = client
		.run_query(phase, "could not inspect the PgBouncer version", async {
			raw_sql("SHOW VERSION").fetch_all(&mut *connection).await
		})
		.await?;
	let settings = client
		.run_query(
			phase,
			"could not inspect the PgBouncer configuration",
			async { raw_sql("SHOW CONFIG").fetch_all(&mut *connection).await },
		)
		.await?;
	let setting = |key: &str| {
		settings
			.iter()
			.find(|row: &&PgRow| {
				row.try_get::<String, _>("key")
					.is_ok_and(|name| name == key)
			})
			.and_then(|row| row.try_get::<String, _>("value").ok())
	};
	let reported_and_expected = [
		(
			"version",
			version
				.first()
				.and_then(|row| row.try_get::<String, _>("version").ok()),
			format!("PgBouncer {}", config.expected_pgbouncer_version),
		),
		(
			"pool_mode",
			setting("pool_mode"),
			pooler.pool_mode().to_owned(),
		),
		(
			"max_client_conn",
			setting("max_client_conn"),
			config.pgbouncer_max_clients.to_string(),
		),
		(
			"default_pool_size",
			setting("default_pool_size"),
			config.pgbouncer_pool_size.to_string(),
		),
		(
			"max_prepared_statements",
			setting("max_prepared_statements"),
			config.pgbouncer_max_prepared_statements.to_string(),
		),
	];
	let mismatches: Vec<String> = reported_and_expected
		.iter()
		.filter(|(_, reported, expected)| reported.as_deref() != Some(expected.as_str()))
		.map(|(name, reported, expected)| match reported {
			Some(reported) => format!("{name} {reported:?} instead of {expected:?}"),
			None => format!("no {name} instead of {expected:?}"),
		})
		.collect();
	if mismatches.is_empty() {
		Ok(())
	} else {
		Err(PgTestHarnessError::new(
			phase,
			format!(
				"PgBouncer did not report the configured version, pooling mode, pool bounds, and prepared statement limit: it reported {}",
				mismatches.join(", ")
			),
		))
	}
}

/// Removes what earlier harnesses left: the containers and the port reservations of a harness
/// that ended without a complete shutdown, in this process or in a process that has ended.
///
/// `earlier` lists the reservation files that no process holds locked. When the containers of
/// an earlier harness cannot be removed, or another runtime holds them, the reservation that
/// names that harness stays, and the next harness that starts tries again. Failures are not
/// reported.
async fn remove_what_earlier_harnesses_left(
	earlier: Vec<UnheldReservation>,
	runtime: &ContainerRuntime,
	end: Deadline,
) {
	let mut associated = Vec::new();
	for reservation in earlier {
		if end.has_passed() {
			return;
		}
		if reservation.is_associated() {
			associated.push(reservation);
			continue;
		}
		match reservation.harness_record().cloned() {
			Some(record) if !runtime.address.is_recorded_as(&record.runtime) => {}
			Some(record) => {
				if let Some(reservation) =
					remove_recorded_containers(reservation, &record, &runtime.docker, end).await
				{
					run_blocking(move || reservation.remove()).await;
				}
			}
			None => remove_when_no_harness_names_it(reservation).await,
		}
	}
	for reservation in associated {
		remove_when_no_harness_names_it(reservation).await;
	}
}

/// Removes a reservation that names no harness of its own, when no live record names it
/// either. The check reads another reservation file, so it runs in a blocking task.
async fn remove_when_no_harness_names_it(reservation: UnheldReservation) {
	run_blocking(move || {
		if reservation.can_remove_without_harness() {
			reservation.remove();
		}
	})
	.await;
}

/// Removes by name each container that `record` names as possibly present, records each
/// removal in the reservation, and returns the reservation when no recorded container remains.
/// A container that the runtime does not have counts as removed: the runtime never created it,
/// or it creates it later from a request that the earlier harness abandoned, and then the
/// runtime never starts it. The record is written in a blocking task.
async fn remove_recorded_containers(
	mut reservation: UnheldReservation,
	record: &HarnessRecord,
	docker: &Docker,
	end: Deadline,
) -> Option<UnheldReservation> {
	for target in CreationTarget::ALL {
		if record.containers.get(target) == RecordedContainer::Absent {
			continue;
		}
		until(end, PgTestHarnessPhase::Environment, async {
			remove_uncertain_container_by_name(
				docker,
				&record.identifier,
				target,
				RECORDED_CREATION_SETTLE_BUDGET,
				end,
			)
			.await
			.map_err(|error| {
				PgTestHarnessError::with_source(
					PgTestHarnessPhase::Environment,
					"could not remove a container that an earlier harness left",
					error,
				)
			})
		})
		.await
		.ok()?;
		let (returned, recorded) = run_blocking(move || {
			let recorded = reservation.record_absent(target);
			(reservation, recorded)
		})
		.await?;
		recorded.ok()?;
		reservation = returned;
	}
	Some(reservation)
}
