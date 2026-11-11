//! Postgres-only, SQLx-backed database foundation primitives.
//!
//! This module provides the small Postgres substrate that Paranoid-owned storage primitives build on.
//!
//! Paranoid constructs its own Postgres pools through [`Pool::connect`] and
//! [`WritePool::connect`] so its storage primitives keep their conservative
//! connection configuration. [`Pool`] and [`Tx`] are neutral DB handles; they do
//! not imply any particular database privileges. [`WritePool`] and [`WriteTx`]
//! are marker wrappers for APIs that require write authority from the
//! credentials used to connect. They do not inspect, reduce, or enforce
//! Postgres privileges.
//!
//! # Pools and transactions
//!
//! Each Paranoid operation is one method. Its database argument is a handle
//! that decides where the operation runs: [`Database`] accepts `&Pool`,
//! `&WritePool`, `&mut Tx`, and `&mut WriteTx`, and [`WriteDatabase`] accepts
//! `&WritePool` and `&mut WriteTx`. Read operations take a [`Database`], and
//! write operations take a [`WriteDatabase`]. With a pool, the operation runs
//! in a transaction of its own: it commits when a write succeeds, rolls back
//! when it fails, and always rolls back when it only reads. With a
//! transaction, the operation joins it, and the caller commits or rolls back
//! the transaction. Applications cannot implement either trait.
//!
//! Paranoid's transactions use the `READ COMMITTED` isolation level, so each statement sees the
//! data that was committed when that statement started. Two reads in one transaction can see
//! different data, and the pages of a scan that the application reads with several calls do not
//! form one snapshot, also when all of the calls run in one transaction.
//!
//! # App-owned SQL
//!
//! [`Pool`], [`WritePool`], [`Tx`], and [`WriteTx`] implement [`sqlx::Executor`]
//! (`&Pool`, `&WritePool`, `&mut Tx`, and `&mut WriteTx`), so app-owned SQL
//! uses the same handles as Paranoid's operations. A statement that runs on a
//! pool uses one pooled connection and is outside any transaction. A statement
//! that runs on a transaction commits or rolls back with the Paranoid
//! operations that run in that transaction. The `paranoid::sqlx`
//! re-export is the `sqlx` version that these implementations use.
//!
//! For app-owned SQL, use [`portable_query`], [`portable_query_as`], and
//! [`portable_query_scalar`], or raw SQLx queries.
//!
//! The portable query constructors keep bound values out of the SQL structure, but they cannot
//! check the SQL text that the application supplies. SQL text that the application builds at
//! run time and passes through [`sqlx::AssertSqlSafe`], and any raw SQLx query, is outside that
//! protection: [`sqlx::AssertSqlSafe`] makes SQLx accept the text without a check, so the
//! application must ensure that the text contains no untrusted input and must send values as
//! bound parameters instead.
//!
//! The constructors also do not check that the SQL text is portable. SQL that depends on the
//! state of one server session, such as temporary tables, a session-level `SET`, and
//! session-level advisory locks, needs the same server connection for all of its statements.
//! The application must run such SQL inside a transaction and must use the forms that end with
//! the transaction: `SET LOCAL`, transaction-level advisory locks, and temporary tables with
//! `ON COMMIT DROP`. Consecutive statements that run on a pool can use different connections,
//! and behind a pooler they can use different server sessions.
//!
//! The portable query constructors, [`SimpleQuery`], and [`unparameterized_simple_query`]
//! send their statements in the simple query protocol, and PostgreSQL sends the results of such
//! statements as text. When the session's `extra_float_digits` setting is positive, PostgreSQL
//! prints a `real` or `double precision` value as the shortest text that reads back as the same
//! value. Paranoid's connections send no `extra_float_digits` startup parameter, so a session
//! has the value that the server, the database, or the role configures, or the value of a
//! session-level `SET extra_float_digits` that remains on the connection: application SQL on a
//! bare pool leaves such a setting on the pooled connection, and through a pooler in
//! transaction pooling mode another client's setting can remain on the server connection.
//! PostgreSQL's default value is 1. When the value n is zero or negative, PostgreSQL prints
//! 15 + n significant digits for `double precision` and 6 + n for `real`, and at least 1, so
//! the decoded value can differ from the stored value. An application that reads `real` or
//! `double precision` values with these statements where the value can be zero or negative must
//! make the setting positive for those statements, for example with `SET LOCAL` inside a
//! transaction, or must read the exact bytes of each value with `float4send` or `float8send`.
//!
//! PostgreSQL prints a `timestamptz` value in the session's time zone, and without a fraction
//! when the value has a whole number of seconds. An application that reads a `timestamptz` with
//! these statements must read it as [`PgTimestamptz`], which decodes every such text to the
//! correct instant, and not as `time::OffsetDateTime`.
//!
//! # Pooler independence
//!
//! [`portable_query`], [`portable_query_as`], and [`portable_query_scalar`] never use the
//! Postgres extended protocol (`Parse` then `Bind`/`Execute`): every bound value is rendered
//! as a safely-escaped Postgres literal and substituted into the SQL before it is sent, as a
//! single simple-protocol `Query` with no bind parameters at all. There is no `Parse`/`Bind`
//! split for a connection pooler to land on two different backends, so these constructors
//! are safe to execute directly on a [`Pool`]/[`WritePool`], as well as inside a
//! [`Tx`]/[`WriteTx`], under **every** connection-pooler mode, including statement-mode
//! pooling. Raw SQLx queries use the extended protocol and do not carry this guarantee.
//!
//! A [`Pool`] or [`WritePool`] rejects extended-protocol queries and metadata operations
//! before acquiring a connection. This includes `sqlx::query`, even without parameters,
//! prepared-statement handles, `Executor::prepare`, and `Executor::describe`. Use portable
//! queries or [`unparameterized_simple_query`] on a pool. Run extended-protocol SQL inside an
//! owned transaction so that its Parse and Bind requests reach the same server connection.
//!
//! In a [`Tx`] or [`WriteTx`], Paranoid sends every raw SQLx query with the unnamed statement,
//! and a pooler in transaction pooling mode keeps one server connection for the whole
//! transaction. A raw SQLx query in a transaction therefore leaves no named statement on the
//! server and has neither outcome of the two round trips, whatever the pooler's
//! `max_prepared_statements` value is. Run raw SQLx queries inside an explicit
//! [`Tx`]/[`WriteTx`].
//!
//! Explicit SQLx metadata operations retain SQLx's connection requirements.
//! In particular, `Executor::describe` uses SQL-level `EXPLAIN EXECUTE`, which cannot
//! address prepared statements that PgBouncer renames. Use a direct PostgreSQL
//! connection for that metadata operation; an explicit transaction does not solve
//! the name mismatch.
//!
//! Paranoid's own operations do not have this independence. Each of them runs in a transaction,
//! which must stay on one server connection from its `BEGIN` to its `COMMIT` or `ROLLBACK`. A pooler in transaction
//! pooling mode keeps a connection for a whole transaction, so Paranoid's operations work through
//! it, but a pooler in statement pooling mode does not support a transaction that spans
//! statements. The application must connect Paranoid's operations to PostgreSQL directly or
//! through a pooler in transaction pooling mode. In statement pooling mode, only the portable
//! query constructors, [`SimpleQuery`], and [`unparameterized_simple_query`] work, and
//! they must run on a pool, outside a transaction.
//!
//! # Database-time discipline
//!
//! Time comparisons in Paranoid database protocols use the PostgreSQL clock rather than the client
//! process clock, so clock skew between application processes does not shift liveness or expiry
//! outcomes.
//!
//! Use [`unparameterized_simple_query`] for unparameterized DDL or
//! administration statements that must run through the PostgreSQL simple-query
//! protocol. Use [`AuditedSql`], which [`audited_sql!`](crate::audited_sql) builds from a
//! string literal template with quoted identifiers and typed literals, for SQL text that
//! needs dynamic identifiers.
//!
//! # Observing and failing operations in tests
//!
//! A [`DatabaseOperationObserver`] that a test attaches to a pool with
//! [`Pool::clone_with_database_operation_observer`] or
//! [`WritePool::clone_with_database_operation_observer`] records every database
//! operation that Paranoid performs through that pool, including application
//! executor calls and transaction begin, commit, and rollback. Each [`DatabaseOperationRecord`]
//! contains the operation's kind, its label, and its SQL text. An executor call that sends
//! several statements produces one record; implicit SQLx protocol messages are
//! not separate records. See [`DatabaseOperationObserver`] for hook timing and
//! operation labels.
//!
//! To test how an application handles a failed database statement, give the observer a fault hook
//! with [`DatabaseOperationObserver::with_operation_fault_hook`]. The fault hook receives each
//! record before Paranoid sends the operation, and it returns a [`DatabaseOperationFault`] for an
//! operation that must fail. Paranoid does not send that operation, and the operation fails with a
//! [`DatabaseError`] that [`DatabaseError::faulted_operation_label`] identifies. The fault hook is
//! for tests, and Paranoid never fails an operation in this way on its own.
//!
//! The `paranoid-testing` crate starts PostgreSQL and PgBouncer containers for an
//! application's own tests.
//!
//! ```rust,no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use paranoid::db::{
//!     portable_query_scalar, DatabaseTls, Pool, PoolConfig,
//! };
//! use secrecy::SecretString;
//! use std::time::Duration;
//!
//! // The URL carries only the connection's identity and credentials. The configuration
//! // states every other setting.
//! let pool = Pool::connect(PoolConfig {
//!     database_url: SecretString::from("postgres://app:secret@localhost/app"),
//!     tls: DatabaseTls::Disabled,
//!     application_name: "example-api".to_owned(),
//!     max_connections: 20,
//!     min_connections: 0,
//!     acquire_timeout: Duration::from_secs(5),
//!     idle_timeout: None,
//!     max_lifetime: Some(Duration::from_secs(1800)),
//!     client_connection_check_interval: Duration::from_millis(500),
//! })
//! .await?;
//!
//! let mut tx = pool.begin_transaction().await?;
//! // Just as safe directly on `pool` itself, without an explicit transaction: there is no
//! // extended-protocol Parse/Bind split for a pooler to land on two different backends.
//! let _count = portable_query_scalar::<i64>("SELECT $1")
//!     .bind(1_i64)
//!     .fetch_one(&mut tx)
//!     .await?;
//! tx.commit().await?;
//! pool.close().await;
//! # Ok(())
//! # }
//! ```

pub(crate) mod audited_sql;

pub(crate) mod bytea;
pub(crate) mod catalog;
mod database;
mod error;
pub(crate) mod identifier;
pub(crate) mod pool;
pub(crate) mod sql_state;
pub(crate) mod transaction;

mod operation_observer;
pub(crate) mod pg_lexer;
mod pg_timestamptz;
mod portable_query;
mod portable_query_literal;
mod portable_query_template;
pub(crate) mod transaction_ending;
pub(crate) mod transaction_permits;
mod transaction_sql;

mod simple_query;

pub(crate) use audited_sql::AuditedSqlBuilder;
pub use audited_sql::{AuditedSql, AuditedSqlArgument};

pub use database::{Database, WriteDatabase};
pub(crate) use error::is_transient_database_failure;
pub(crate) use error::{
	AdmissionError, TransactionFailure, database_work_error, impl_transaction_failure,
};
pub use error::{
	DatabaseError, LiteralEncodingError, PortableQueryBindError, PortableQueryTemplateError,
	ProtocolError, RetryClassification, SchemaError, TransactionPhase,
};
pub use identifier::{
	InvalidPgIdentifierError, MAX_PG_IDENTIFIER_LEN, PgIdentifier, PgQualifiedTableName,
	PgSchemaName, QuotedPgIdentifier, QuotedPgQualifiedTableName,
};
pub use pg_timestamptz::PgTimestamptz;
pub use pool::{
	ConnectError, DatabaseTls, Pool, PoolConfig, PoolConfigError, Tx, WritePool, WriteTx,
};
pub(crate) use pool::{
	LeaseCallerRetentionError, OwnedTransactionRegistration, StateMutationCompletion,
};
pub use portable_query::{
	PortableQuery, PortableQueryAs, PortableQueryScalar, portable_query, portable_query_as,
	portable_query_scalar, unparameterized_simple_query,
};
pub(crate) use portable_query::{
	owned_portable_query, owned_portable_query_as, owned_portable_query_scalar,
};
pub use portable_query_literal::PortableQueryArgument;
pub use simple_query::{PgLiteral, SimpleQuery};
pub use sql_state::PgSqlState;

pub(crate) use catalog::{
	PG_COLUMN_METADATA_SQL, PG_SINGLE_COLUMN_INDEX_EXISTS_SQL,
	PG_USABLE_SINGLE_COLUMN_UNIQUE_INDEX_EXISTS_SQL, TIMESTAMPTZ_OPERATOR_CLASS,
	builtin_btree_index_conditions, migration_object_identifier,
};
pub(crate) use identifier::pg_table_name_set_contains_duplicate;
#[cfg(any(test, feature = "__internal"))]
pub(crate) use identifier::table_name_for_tests;
pub(crate) use operation_observer::record_database_operation;
pub use operation_observer::{
	DatabaseOperationFault, DatabaseOperationKind, DatabaseOperationObserver,
	DatabaseOperationRecord,
};
use transaction::finish_pool_owned_rollback_only_transaction_and_preserve_rollback_error;
pub(crate) use transaction::{
	finish_pool_owned_write_transaction_and_preserve_rollback_error, run_transaction_read,
	run_transaction_write,
};
