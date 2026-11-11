use crate::containers::{PGBOUNCER_ADMIN_DATABASE_NAME, SUPERUSER_ROLE_NAME};
use crate::error::{PgTestHarnessError, PgTestHarnessPhase};
use crate::harness::{PgTestHarnessEndpoint, PgTestHarnessPooler, PgTestHarnessRole};
use paranoid::sqlx::postgres::{PgConnectOptions, PgSslMode};
use paranoid::sqlx::{ConnectOptions, Connection, PgConnection};
use secrecy::{ExposeSecret, SecretString};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

const CLIENT_APPLICATION_NAME: &str = "paranoid-testing";

/// The reason that the harness gives when `PGOPTIONS` is set. `sqlx` reads the variable when
/// it creates connection options and offers no way to clear it, so every connection would send
/// its startup options. Paranoid's pool rejects the variable with the same reason.
pub(crate) const PGOPTIONS_REJECTION: &str =
	"the PGOPTIONS environment variable is set; sqlx would send it as startup options, so unset it";

/// Whether `PGOPTIONS` is set.
pub(crate) fn is_pgoptions_set() -> bool {
	std::env::var_os("PGOPTIONS").is_some()
}

/// Who a connection logs in as.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Login {
	Role(PgTestHarnessRole),
	/// The cluster's superuser, which PgBouncer does not accept.
	Superuser,
}

impl Login {
	fn name(self) -> &'static str {
		match self {
			Self::Role(role) => role.name(),
			Self::Superuser => SUPERUSER_ROLE_NAME,
		}
	}
}

/// A database on one of the harness's servers and who logs in to it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ConnectionTarget<'a> {
	pub(crate) port: u16,
	pub(crate) login: Login,
	pub(crate) database: &'a str,
}

/// What the harness needs to reach its servers: their loopback ports, the passwords of its
/// logins, and the timeouts of the harness's own connections and statements.
#[derive(Clone, Debug)]
pub(crate) struct HarnessClient {
	pub(crate) harness_identity: Arc<()>,
	/// The password of every role of the harness, which the harness generated at its start.
	pub(crate) role_password: SecretString,
	/// The password of the cluster's superuser, which the harness generated at its start and
	/// which differs from the password of the roles.
	pub(crate) superuser_password: SecretString,
	pub(crate) pg_port: u16,
	/// The ports of the two PgBouncer servers, in the order of [`PgTestHarnessPooler::ALL`].
	pub(crate) pgbouncer_ports: [u16; 2],
	pub(crate) connect_timeout: Duration,
	pub(crate) query_timeout: Duration,
}

impl HarnessClient {
	/// The port of one of the two PgBouncer servers.
	pub(crate) fn pgbouncer_port(&self, pooler: PgTestHarnessPooler) -> u16 {
		match pooler {
			PgTestHarnessPooler::Transaction => self.pgbouncer_ports[0],
			PgTestHarnessPooler::Statement => self.pgbouncer_ports[1],
		}
	}

	/// The port of the server behind an endpoint.
	pub(crate) fn port(&self, endpoint: PgTestHarnessEndpoint) -> u16 {
		match endpoint {
			PgTestHarnessEndpoint::Pg => self.pg_port,
			PgTestHarnessEndpoint::PgBouncerTransaction => {
				self.pgbouncer_port(PgTestHarnessPooler::Transaction)
			}
			PgTestHarnessEndpoint::PgBouncerStatement => {
				self.pgbouncer_port(PgTestHarnessPooler::Statement)
			}
		}
	}

	fn password(&self, login: Login) -> &str {
		match login {
			Login::Role(_) => self.role_password.expose_secret(),
			Login::Superuser => self.superuser_password.expose_secret(),
		}
	}

	/// The connection URL of a target. It contains the login's password.
	pub(crate) fn connection_url(&self, target: ConnectionTarget<'_>) -> SecretString {
		SecretString::from(format!(
			"postgresql://{}:{}@127.0.0.1:{}/{}",
			target.login.name(),
			self.password(target.login),
			target.port,
			target.database
		))
	}

	/// Opens a connection to a database of PostgreSQL itself.
	pub(crate) async fn connect_direct(
		&self,
		role: PgTestHarnessRole,
		database: &str,
	) -> Result<PgConnection, PgTestHarnessError> {
		self.connect(
			ConnectionTarget {
				port: self.pg_port,
				login: Login::Role(role),
				database,
			},
			"could not connect to PostgreSQL",
		)
		.await
	}

	/// The admin console of one of the two PgBouncer servers, as the writer role.
	pub(crate) fn pgbouncer_admin_target(
		&self,
		pooler: PgTestHarnessPooler,
	) -> ConnectionTarget<'static> {
		ConnectionTarget {
			port: self.pgbouncer_port(pooler),
			login: Login::Role(PgTestHarnessRole::Writer),
			database: PGBOUNCER_ADMIN_DATABASE_NAME,
		}
	}

	/// The superuser's target in a database of PostgreSQL itself, for the harness's own
	/// administration.
	pub(crate) fn superuser_target<'a>(&self, database: &'a str) -> ConnectionTarget<'a> {
		ConnectionTarget {
			port: self.pg_port,
			login: Login::Superuser,
			database,
		}
	}

	/// Opens a connection. The connection logs no statement because the harness's statements
	/// contain the role password.
	pub(crate) async fn connect(
		&self,
		target: ConnectionTarget<'_>,
		reason: &'static str,
	) -> Result<PgConnection, PgTestHarnessError> {
		let phase = PgTestHarnessPhase::ClientConnect;
		if is_pgoptions_set() {
			return Err(PgTestHarnessError::new(phase, PGOPTIONS_REJECTION));
		}
		let options = PgConnectOptions::new_without_pgpass()
			.host("127.0.0.1")
			.port(target.port)
			.username(target.login.name())
			.password(self.password(target.login))
			.database(target.database)
			.ssl_mode(PgSslMode::Disable)
			.application_name(CLIENT_APPLICATION_NAME)
			.disable_statement_logging();
		match tokio::time::timeout(self.connect_timeout, PgConnection::connect_with(&options)).await
		{
			Ok(Ok(connection)) => Ok(connection),
			Ok(Err(error)) => Err(PgTestHarnessError::with_source(phase, reason, error)),
			Err(_) => Err(PgTestHarnessError::new(
				phase,
				format!("{reason}: the client_connect_timeout elapsed"),
			)),
		}
	}

	/// Runs one statement of the harness's own under the query timeout.
	pub(crate) async fn run_query<T>(
		&self,
		phase: PgTestHarnessPhase,
		reason: &'static str,
		query: impl Future<Output = Result<T, paranoid::sqlx::Error>>,
	) -> Result<T, PgTestHarnessError> {
		match tokio::time::timeout(self.query_timeout, query).await {
			Ok(Ok(value)) => Ok(value),
			Ok(Err(error)) => Err(PgTestHarnessError::with_source(phase, reason, error)),
			Err(_) => Err(PgTestHarnessError::new(
				phase,
				format!("{reason}: the client_query_timeout elapsed"),
			)),
		}
	}

	/// Closes a connection that the harness opened. If both the work and closure fail,
	/// the work error includes the closure failure in its cleanup failures.
	pub(crate) async fn close<T>(
		&self,
		connection: PgConnection,
		outcome: Result<T, PgTestHarnessError>,
	) -> Result<T, PgTestHarnessError> {
		let closed = connection.close().await.map_err(|error| {
			PgTestHarnessError::with_source(
				PgTestHarnessPhase::ClientClose,
				"could not close a harness connection",
				error,
			)
		});
		match (outcome, closed) {
			(Ok(value), Ok(())) => Ok(value),
			(Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
			(Err(error), Err(close_error)) => Err(error.with_cleanup_failures(vec![close_error])),
		}
	}
}
