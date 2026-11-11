use crate::client::{ConnectionTarget, HarnessClient, Login};
use crate::containers::{DATABASE_NAME, PGBOUNCER_ADMIN_DATABASE_NAME, WRITER_ROLE_NAME};
use crate::error::{PgTestHarnessError, PgTestHarnessPhase};
use crate::harness::{
	PgTestHarnessEndpoint, PgTestHarnessPoolSettings, PgTestHarnessRole, create_pool_config,
};
use paranoid::db::{PgIdentifier, PoolConfig};
use paranoid::sqlx::{AssertSqlSafe, PgConnection, raw_sql};
use secrecy::SecretString;
use std::sync::Arc;

/// What [`PgTestHarnessDatabase::create`] copies to make a database.
#[derive(Clone, Copy, Debug)]
pub enum PgTestHarnessDatabaseTemplate<'a> {
	/// PostgreSQL's `template0`, with the encoding UTF8 and the locale `locale`: the new
	/// database is empty.
	Empty {
		/// The locale of the new database.
		locale: PgTestHarnessDatabaseLocale,
	},
	/// Another database of the same harness: the new database is a copy of it, with its
	/// schemas, tables, rows, and locale.
	Database(&'a PgTestHarnessDatabase),
}

/// The locale of an empty database, which sets the database's default collation and character
/// classification. A test suite that creates its database once with each locale checks that its
/// results do not depend on the locale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PgTestHarnessDatabaseLocale {
	/// The C locale of the libc provider: text sorts and compares by its bytes. The harness's
	/// own database, `test`, has this locale.
	C,
	/// The ICU locale `en-US`, which the ICU library of the official PostgreSQL images provides:
	/// text sorts by the rules of American English so that, for example, `a` sorts before
	/// `B`. The database's `LC_COLLATE` and `LC_CTYPE` are C, but PostgreSQL takes case mapping,
	/// the character classes of regular expressions, and `ILIKE` from ICU as well: `lower('ÉA')`
	/// is `éa`, `'é' ~ '^[[:alpha:]]$'` is true, and `'É' ILIKE 'é'` is true, while in a C
	/// database each of them is the other way. PostgreSQL has ICU databases from version 15, so
	/// with an earlier version the creation fails.
	IcuEnUs,
}

impl PgTestHarnessDatabaseLocale {
	/// The locale clauses of `CREATE DATABASE`.
	fn create_database_clauses(self) -> &'static str {
		match self {
			Self::C => "ENCODING 'UTF8' LOCALE 'C'",
			Self::IcuEnUs => "ENCODING 'UTF8' LOCALE 'C' LOCALE_PROVIDER icu ICU_LOCALE 'en-US'",
		}
	}
}

/// One database in a harness's PostgreSQL server, by its name, from
/// [`PgTestHarness::database`](crate::PgTestHarness::database).
///
/// A test suite that needs fixed schema names in every test gives each test a database of
/// its own in one running harness: it prepares one database with its schemas, and each test
/// creates a copy of it with [`create`](Self::create), uses it, and removes it with
/// [`remove`](Self::remove). PostgreSQL copies a database's files, so a copy takes much less
/// time than the start of a harness.
///
/// Each PgBouncer forwards a connection to the PostgreSQL database that the connection names,
/// so the database is reachable on every endpoint as soon as it exists.
///
/// The value stores the harness's ports, passwords, and timeouts and does not borrow the harness,
/// so it can be stored beside a shared harness. Its methods fail when the harness has shut down:
/// a connection then finds no server, or, when another harness has reserved the same port, fails
/// authentication because each harness has passwords of its own.
///
/// The application chooses each name and must use a name for one database only: PgBouncer
/// remembers a failed login to a database for its `server_login_retry` time, so a database
/// that is created again under the name of a removed one can be unreachable through PgBouncer
/// for that time. PgBouncer accepts a client that names a database that does not exist, and
/// the client's first statement fails.
#[derive(Clone, Debug)]
pub struct PgTestHarnessDatabase {
	pub(crate) client: HarnessClient,
	pub(crate) name: PgIdentifier,
}

impl PgTestHarnessDatabase {
	/// The database's name.
	pub fn name(&self) -> &PgIdentifier {
		&self.name
	}

	/// Creates the database as a copy of `template`, owned by the writer role. Every role of the
	/// harness can connect to it because PostgreSQL gives every role the right to connect to a
	/// new database. It fails when a database with this name exists. A template from another
	/// harness and the name `pgbouncer`, which is the database of PgBouncer's admin console and
	/// which PgBouncer therefore does not forward to PostgreSQL, are rejected before any
	/// connection is opened or statement is sent. The harness's superuser runs the statements.
	///
	/// PostgreSQL copies a database only while no other session is connected to it. Before a
	/// copy of [`PgTestHarnessDatabaseTemplate::Database`], this ends every session that is
	/// connected to the template database, including the idle server connections that the
	/// PgBouncer servers keep. The application must not use the template database while a copy is
	/// created, and must not create two copies of one template at the same time from outside
	/// this method.
	///
	/// The statement runs under the configured query timeout.
	pub async fn create(
		&self,
		template: PgTestHarnessDatabaseTemplate<'_>,
	) -> Result<(), PgTestHarnessError> {
		if let PgTestHarnessDatabaseTemplate::Database(template) = template
			&& !Arc::ptr_eq(
				&self.client.harness_identity,
				&template.client.harness_identity,
			) {
			return Err(PgTestHarnessError::new(
				PgTestHarnessPhase::DatabaseCreate,
				"the template database belongs to another harness",
			));
		}
		if self.name.as_str() == PGBOUNCER_ADMIN_DATABASE_NAME {
			return Err(PgTestHarnessError::new(
				PgTestHarnessPhase::DatabaseCreate,
				"the name pgbouncer belongs to the database of PgBouncer's admin console, so PgBouncer does not forward it to PostgreSQL",
			));
		}
		let mut connection = self
			.client
			.connect(
				self.client.superuser_target(DATABASE_NAME),
				"could not connect to PostgreSQL",
			)
			.await?;
		let created = self.run_create(&mut connection, template).await;
		self.client.close(connection, created).await
	}

	async fn run_create(
		&self,
		connection: &mut PgConnection,
		template: PgTestHarnessDatabaseTemplate<'_>,
	) -> Result<(), PgTestHarnessError> {
		let phase = PgTestHarnessPhase::DatabaseCreate;
		let name = self.name.quoted();
		let template_clauses = match template {
			PgTestHarnessDatabaseTemplate::Empty { locale } => {
				format!("template0 {}", locale.create_database_clauses())
			}
			PgTestHarnessDatabaseTemplate::Database(template) => {
				// The name is a validated identifier, so it is also a valid string literal.
				let disconnect = format!(
					"SELECT pg_catalog.pg_terminate_backend(pid) FROM pg_catalog.pg_stat_activity \
					 WHERE datname OPERATOR(pg_catalog.=) '{}' AND pid OPERATOR(pg_catalog.<>) pg_catalog.pg_backend_pid()",
					template.name.as_str()
				);
				self.client
					.run_query(
						phase,
						"could not end the sessions of the template database",
						async {
							raw_sql(AssertSqlSafe(disconnect))
								.execute(&mut *connection)
								.await
						},
					)
					.await?;
				template.name.quoted().to_string()
			}
		};
		// A database cannot be created inside a transaction, so the statement is sent alone.
		let statement =
			format!("CREATE DATABASE {name} OWNER {WRITER_ROLE_NAME} TEMPLATE {template_clauses}");
		self.client
			.run_query(phase, "could not create the database", async {
				raw_sql(AssertSqlSafe(statement))
					.execute(&mut *connection)
					.await
			})
			.await?;
		Ok(())
	}

	/// Removes the database with `DROP DATABASE ... WITH (FORCE)`, which PostgreSQL has from
	/// version 13. PostgreSQL ends every session that is connected to the database, including
	/// the server connections that the PgBouncer servers keep, so a pool that the application
	/// still holds for the database fails afterward. It fails when no database with this name
	/// exists.
	///
	/// A database that is not removed stays until the harness shuts down, with its files and
	/// with the idle server connections that the PgBouncer servers keep for it. The harness's
	/// superuser runs the statement.
	pub async fn remove(&self) -> Result<(), PgTestHarnessError> {
		let mut connection = self
			.client
			.connect(
				self.client.superuser_target(DATABASE_NAME),
				"could not connect to PostgreSQL",
			)
			.await?;
		let statement = format!("DROP DATABASE {} WITH (FORCE)", self.name.quoted());
		let removed = self
			.client
			.run_query(
				PgTestHarnessPhase::DatabaseRemove,
				"could not remove the database",
				async {
					raw_sql(AssertSqlSafe(statement))
						.execute(&mut connection)
						.await
						.map(|_| ())
				},
			)
			.await;
		self.client.close(connection, removed).await
	}

	/// The connection URL for a role on an endpoint. It contains the role's password.
	pub fn connection_url(
		&self,
		endpoint: PgTestHarnessEndpoint,
		role: PgTestHarnessRole,
	) -> SecretString {
		self.client.connection_url(ConnectionTarget {
			port: self.client.port(endpoint),
			login: Login::Role(role),
			database: self.name.as_str(),
		})
	}

	/// The URL of a direct connection to the database as the cluster's superuser,
	/// `paranoid_superuser`. It contains the superuser's password, which differs from the
	/// password of the harness's roles, so this is the only URL that reaches the superuser.
	///
	/// It serves test setup that only a superuser can perform, such as the creation of an
	/// operator class or a role default of a parameter that only a superuser can set so that a
	/// test can check an application's behavior under such a configuration. A superuser has every
	/// privilege, so the code that a test examines must run as one of the harness's roles: as the
	/// superuser, a test cannot detect a privilege that the application's own roles lack.
	/// PgBouncer does not accept the superuser, so the URL names PostgreSQL itself.
	pub fn superuser_connection_url(&self) -> SecretString {
		self.client
			.connection_url(self.client.superuser_target(self.name.as_str()))
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

	/// Opens a connection to the database in PostgreSQL itself, for tests that inspect the
	/// server. The caller owns the connection and its queries; the query timeout of the
	/// configuration does not apply to it.
	pub async fn connect_direct(
		&self,
		role: PgTestHarnessRole,
	) -> Result<PgConnection, PgTestHarnessError> {
		self.client.connect_direct(role, self.name.as_str()).await
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;
	use std::time::Duration;

	use paranoid::db::PgIdentifier;

	use super::{PgTestHarnessDatabase, PgTestHarnessDatabaseTemplate};
	use crate::PgTestHarnessPhase;
	use crate::client::HarnessClient;

	fn database(identity: &Arc<()>, name: &str) -> PgTestHarnessDatabase {
		PgTestHarnessDatabase {
			client: HarnessClient {
				harness_identity: Arc::clone(identity),
				role_password: "0a1b".into(),
				superuser_password: "2c3d".into(),
				// No server listens on port 1, so any connection attempt would fail with a
				// connect error instead of the rejection.
				pg_port: 1,
				pgbouncer_ports: [1, 1],
				connect_timeout: Duration::from_secs(1),
				query_timeout: Duration::from_secs(1),
			},
			name: PgIdentifier::new(name).expect("a valid name"),
		}
	}

	#[tokio::test]
	async fn a_template_of_another_harness_is_rejected_before_any_connection() {
		let template = database(&Arc::new(()), "template_of_another_harness");
		let copy = database(&Arc::new(()), "copy");
		let error = copy
			.create(PgTestHarnessDatabaseTemplate::Database(&template))
			.await
			.expect_err("a template of another harness must be rejected");
		assert_eq!(error.phase(), PgTestHarnessPhase::DatabaseCreate);
		assert!(
			error.to_string().contains("belongs to another harness"),
			"{error}"
		);
	}
}
