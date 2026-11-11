//! PostgreSQL and PgBouncer containers for testing applications that use `paranoid`.
//!
//! [`PgTestHarness::start`] runs a PostgreSQL container and two PgBouncer containers, one in
//! transaction pooling and one in statement pooling, through the local Docker or Podman
//! runtime at the address that `DOCKER_HOST` or a default socket gives (see
//! [`PgTestHarnessConfig`]). The harness downloads no database binaries and builds nothing. The
//! containers use the host's network and listen on reserved loopback ports. On Linux the harness
//! draws those ports from outside the range that the kernel assigns automatically
//! (`net.ipv4.ip_local_port_range`) so that only a socket that names such a port can take it
//! before the server listens. Harnesses of different processes exclude each other's ports only
//! when the processes share the system's temporary directory, where the reservations are files.
//! The container runtime must support host networking, and its containers must share the
//! network of the process that runs the tests because the harness connects to them on
//! `127.0.0.1`. On a runtime that does not meet this requirement, startup fails.
//!
//! The harness offers three endpoints ([`PgTestHarnessEndpoint`]: PostgreSQL itself,
//! PgBouncer in transaction pooling, and PgBouncer in statement pooling) and four roles
//! ([`PgTestHarnessRole`]). For each combination it gives the connection URL and a complete
//! [`paranoid::db::PoolConfig`]. It also opens direct connections for tests that inspect
//! PostgreSQL and connections to the admin console of each PgBouncer ([`PgTestHarnessPooler`]),
//! and it gives the URL of each console to a process that runs beside the harness.
//!
//! The harness sets each PgBouncer's `max_prepared_statements` to the configured value, which
//! can be 0, and verifies the setting at startup. It rejects an expected PgBouncer version below 1.21.0
//! because PgBouncer has the setting from that version.
//!
//! Every setting is a field of [`PgTestHarnessConfig`]; the harness has no defaults. The
//! environment variables that the harness reads are listed there too. Failures name the phase
//! that failed ([`PgTestHarnessPhase`]).
//!
//! # A database for each test
//!
//! One running harness can contain a database for each test, for a test suite with schema names
//! that are the same in every test. [`PgTestHarness::database`] names a database,
//! [`PgTestHarnessDatabase::create`] creates it empty or as a copy of another database, and
//! [`PgTestHarnessDatabase::remove`] removes it. A suite prepares one database with its
//! schemas once, and each test creates a copy of it, which takes much less time than the
//! start of a harness. Such a database is reachable on every endpoint, for every role. An empty
//! database has the locale that the test chooses ([`PgTestHarnessDatabaseLocale`]) so that a
//! suite can check that its results do not depend on the locale.
//!
//! Explicit shutdown awaits cleanup within its configured budget and reports cleanup failures.
//! Dropping the harness starts bounded cleanup within [`PgTestHarnessConfig::cleanup_timeout`]
//! on a separate thread without blocking the caller. A value in a `static` is never dropped, so
//! a suite that shares one harness among the tests of a process must store that harness in a way
//! that lets the last test shut it down, such as a reference count.
//!
//! The total startup timeout includes image pulling and every runtime request. Container
//! readiness has narrower timeouts within that total deadline. Failed startup gets a
//! separate cleanup timeout and reports unresolved cleanup. A cancelled or timed-out creation
//! can still complete in the runtime; [`PgTestHarness`] describes how the harness settles it.
//!
//! ```rust,no_run
//! # async fn example(
//! #     harness: &paranoid_testing::PgTestHarness,
//! #     settings: paranoid_testing::PgTestHarnessPoolSettings,
//! # ) -> Result<(), Box<dyn std::error::Error>> {
//! use paranoid::db::{PgIdentifier, WritePool};
//! use paranoid_testing::{
//!     PgTestHarnessDatabaseLocale, PgTestHarnessDatabaseTemplate, PgTestHarnessEndpoint,
//!     PgTestHarnessRole,
//! };
//!
//! // Once: a database that contains the application's schemas.
//! let template = harness.database(PgIdentifier::new("app_template")?);
//! template
//!     .create(PgTestHarnessDatabaseTemplate::Empty {
//!         locale: PgTestHarnessDatabaseLocale::IcuEnUs,
//!     })
//!     .await?;
//! // ... create the schemas in `template`, then close every pool for it ...
//!
//! // In each test: a copy under a name that no other test uses.
//! let database = harness.database(PgIdentifier::new("orders_are_listed_by_date")?);
//! database
//!     .create(PgTestHarnessDatabaseTemplate::Database(&template))
//!     .await?;
//! let pool = WritePool::connect(database.pool_config(
//!     PgTestHarnessEndpoint::PgBouncerTransaction,
//!     PgTestHarnessRole::Writer,
//!     settings,
//! ))
//! .await?;
//! // ... run the test ...
//! pool.close().await;
//! database.remove().await?;
//! # Ok(())
//! # }
//! ```
//!
//! ```rust,no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use paranoid::db::Pool;
//! use paranoid_testing::{
//!     PgTestHarness, PgTestHarnessConfig, PgTestHarnessEndpoint,
//!     PgTestHarnessPoolSettings, PgTestHarnessRole,
//! };
//! use std::time::Duration;
//!
//! let harness = PgTestHarness::start(PgTestHarnessConfig {
//!     pg_image: "docker.io/library/postgres:16".to_owned(),
//!     pgbouncer_image: "docker.io/hyperized/pgbouncer@sha256:7948b561791a7888f69e6d94b763c11847f63a0092ab5bd446108d5ce2c4b192".to_owned(),
//!     expected_pgbouncer_version: "1.25.2".to_owned(),
//!     startup_timeout: Duration::from_secs(300),
//!     cleanup_timeout: Duration::from_secs(60),
//!     pg_startup_timeout: Duration::from_secs(60),
//!     pgbouncer_startup_timeout: Duration::from_secs(30),
//!     container_stop_timeout: Duration::from_secs(10),
//!     client_connect_timeout: Duration::from_secs(10),
//!     client_query_timeout: Duration::from_secs(10),
//!     statement_timeout: Duration::from_millis(50),
//!     pg_max_connections: 300,
//!     pgbouncer_max_clients: 300,
//!     pgbouncer_pool_size: 20,
//!     pgbouncer_max_prepared_statements: 200,
//!     port_reservation_attempts: 100,
//! })
//! .await?;
//!
//! let pool = Pool::connect(harness.pool_config(
//!     PgTestHarnessEndpoint::PgBouncerTransaction,
//!     PgTestHarnessRole::Writer,
//!     PgTestHarnessPoolSettings {
//!         application_name: "my-tests".to_owned(),
//!         max_connections: 10,
//!         min_connections: 0,
//!         acquire_timeout: Duration::from_secs(10),
//!         idle_timeout: None,
//!         max_lifetime: None,
//!         client_connection_check_interval: Duration::from_millis(500),
//!     },
//! ))
//! .await?;
//! // ... run the test ...
//! pool.close().await;
//! harness.shutdown().await?;
//! # Ok(())
//! # }
//! ```
#![deny(missing_docs)]

mod client;
mod config;
mod containers;
mod database;
mod error;
mod harness;
mod port;
mod registry;

pub use config::PgTestHarnessConfig;
pub use database::{
	PgTestHarnessDatabase, PgTestHarnessDatabaseLocale, PgTestHarnessDatabaseTemplate,
};
pub use error::{PgTestHarnessError, PgTestHarnessPhase, PgTestHarnessShutdownError};
pub use harness::{
	PgTestHarness, PgTestHarnessEndpoint, PgTestHarnessPoolSettings, PgTestHarnessPooler,
	PgTestHarnessRole,
};
