use super::operation_observer::{OperationFaultError, PendingStatementFault};
use super::transaction_ending::{
	CommitAttemptMarker, PendingTransactionEnding, RequestedTransactionEnding,
	TransactionEndingDisposition, TransactionEndingPublisher, TransactionEndingReceipt,
	TransactionEndingResult, TransactionEndingSnapshot,
};
use super::transaction_permits::{
	LeaseCallerPermitHandle, LeaseCallerTransactionAssociationError, TransactionPermitHandle,
};
use super::{
	DatabaseError, DatabaseOperationKind, DatabaseOperationObserver, TransactionPhase,
	unparameterized_simple_query,
};
use futures_core::future::BoxFuture;
use futures_core::stream::BoxStream;
use primitives_rs::id::time_sortable::SortableId;
use secrecy::{ExposeSecret, SecretString};
use sqlx::pool::PoolConnection;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::postgres::{PgQueryResult, PgRow, PgStatement, PgTypeInfo};
use sqlx::{
	ConnectOptions, Database, Describe, Either, Execute, Executor, PgConnection, PgPool, Postgres,
	SqlSafeStr, SqlStr,
};
use sqlx_core::transaction::TransactionManager;
use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::num::{NonZeroU64, NonZeroUsize};
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::{Notify, oneshot};

mod statement_state;
use statement_state::{StatementFailureTracker, StatementInterruption};

const POOLER_SAFE_STATEMENT_CACHE_CAPACITY: usize = 0;

/// Matches sqlx's close-on-drop bound while the transaction retains its connection permit.
const DISCARDED_CONNECTION_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// Linear local work ownership, transferred before an owned BEGIN task starts.
/// Its receipt observes disposal and ending without proving server-side drainage. A registration
/// that drops before its transfer publishes the drop through its publisher, before it releases
/// its permits.
pub(crate) struct OwnedTransactionRegistration {
	publisher: TransactionEndingPublisher,
	permit: Box<dyn TransactionPermitHandle>,
	lease_caller: Option<Box<dyn Send + Sync>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum LeaseCallerRetentionError {
	#[error("owned transaction already retains a Lease caller")]
	AlreadyRetained,
	#[error("Lease caller does not match the transaction membership")]
	Association(#[source] LeaseCallerTransactionAssociationError),
}

/// A rejected attachment retains the supplied caller without changing its count.
pub(crate) struct RetainLeaseCallerError<C> {
	pub(crate) caller: C,
	pub(crate) source: LeaseCallerRetentionError,
}

impl<C> fmt::Debug for RetainLeaseCallerError<C> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("RetainLeaseCallerError")
			.field("source", &self.source)
			.finish_non_exhaustive()
	}
}

impl<C> fmt::Display for RetainLeaseCallerError<C> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		fmt::Display::fmt(&self.source, f)
	}
}

impl<C> std::error::Error for RetainLeaseCallerError<C> {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(&self.source)
	}
}

impl OwnedTransactionRegistration {
	pub(crate) fn new(permit: impl TransactionPermitHandle) -> Self {
		Self {
			publisher: TransactionEndingPublisher::new(),
			permit: Box::new(permit),
			lease_caller: None,
		}
	}

	/// Transfers an existing caller lifetime without incrementing or replacing its count.
	pub(crate) fn retain_lease_caller<C: LeaseCallerPermitHandle>(
		&mut self,
		caller: C,
	) -> Result<(), RetainLeaseCallerError<C>> {
		let source = if self.lease_caller.is_some() {
			Some(LeaseCallerRetentionError::AlreadyRetained)
		} else {
			caller
				.check_transaction_association(self.permit.as_ref())
				.err()
				.map(LeaseCallerRetentionError::Association)
		};
		if let Some(source) = source {
			return Err(RetainLeaseCallerError { caller, source });
		}
		self.lease_caller = Some(Box::new(caller));
		Ok(())
	}

	pub(crate) fn receipt(&self) -> TransactionEndingReceipt {
		self.publisher.receipt()
	}

	fn publish_failure(&self, error: &DatabaseError) {
		self.publisher.complete_unconfirmed(error);
	}

	fn transfer_to(self, transaction: &mut OwnedTransaction) {
		let Self {
			publisher,
			permit,
			lease_caller,
		} = self;
		transaction.runtime_permits.push(permit);
		transaction.lease_caller = lease_caller;
		assert!(
			transaction.ending.set(publisher).is_ok(),
			"registration transfers into an unobserved transaction"
		);
	}
}

const CONNECT_LABEL: &str = "db.connect";
const BEGIN_TRANSACTION_LABEL: &str = "db.begin_transaction";
const COMMIT_LABEL: &str = "db.tx.commit";
const COMMIT_PROBE_LABEL: &str = "db.tx.commit_probe";
const ROLLBACK_LABEL: &str = "db.tx.rollback";
const DISCARD_INTERRUPTED_STATEMENT_LABEL: &str = "db.tx.discard_interrupted_statement";
const COMMIT_ABANDONED_STATE_MUTATION_LABEL: &str = "db.tx.commit_abandoned_state_mutation";
const ROLLBACK_ABANDONED_STATE_MUTATION_LABEL: &str = "db.tx.rollback_abandoned_state_mutation";
const BEGIN_READ_ONLY_SQL: &str = "BEGIN ISOLATION LEVEL READ COMMITTED READ ONLY";
const BEGIN_WRITE_POOL_SQL: &str = "BEGIN ISOLATION LEVEL READ COMMITTED";
/// The statement that a transaction sends in place of a statement that the fault hook of a
/// [`DatabaseOperationObserver`] failed. PostgreSQL rejects it under every setting because the
/// division of an integer by zero is an error, and the rejection puts PostgreSQL's transaction
/// into the state that a failed statement leaves.
const STATEMENT_THAT_POSTGRES_REJECTS: &str = "/* Paranoid: the fault hook of a database operation observer failed an operation of this transaction */ SELECT 1 OPERATOR(pg_catalog./) 0";

/// The only URL query parameters that [`PoolConfig::database_url`] may carry. Each of them
/// names where the connection goes or who it authenticates as. Every other setting belongs to
/// [`PoolConfig`], so each setting has exactly one source.
const IDENTITY_URL_QUERY_PARAMETERS: [&str; 6] =
	["host", "hostaddr", "port", "dbname", "user", "password"];

/// Pool configuration for Paranoid-owned Postgres connections.
///
/// Every field is a choice that the application makes. `database_url` carries only the
/// connection's identity and credentials: its host, port, database, user, and password,
/// including the `host`, `hostaddr`, `port`, `dbname`, `user`, and `password` query
/// parameters. A URL that carries any other query parameter, such as `sslmode`,
/// `sslrootcert`, `application_name`, `connect_timeout`, or `options`, is rejected with
/// [`PoolConfigError::InvalidConfig`]. The rejection never shows
/// the URL.
///
/// The driver also reads connection settings from `PG*` environment variables. Paranoid
/// replaces each of them with the value of the URL or of the configuration, except
/// `PGOPTIONS`, `PGSSLCERT`, `PGSSLKEY`, `PGSSLROOTCERT`, and, when the URL has no password,
/// `PGPASSWORD`, which the driver offers no way to replace. The application must leave these
/// variables unset: connecting fails with [`PoolConfigError::InvalidConfig`] when the process
/// environment sets one of them.
///
/// The driver's statement logging is off for every connection of the pool, also for the
/// application's own SQL on it. SQLx otherwise logs every statement at the debug level and
/// every statement that takes one second or longer at the warning level, with the statement's
/// text, and the text of a portable query holds its bound values as literals. An application
/// observes Paranoid's operations with a
/// [`DatabaseOperationObserver`](super::DatabaseOperationObserver).
#[derive(Clone, Debug)]
pub struct PoolConfig {
	/// Postgres connection URL: identity and credentials only.
	pub database_url: SecretString,
	/// How the connection uses TLS.
	pub tls: DatabaseTls,
	/// Postgres application name. It must be non-empty and must not contain null bytes.
	pub application_name: String,
	/// Maximum number of open pool connections.
	///
	/// A transaction that the application holds keeps one connection until it ends. An
	/// operation that runs independently of that transaction, such as a guard or its heartbeat,
	/// a worker, a cron, or an operation that takes the pool itself, needs a connection of its
	/// own from the same pool. The application must choose a value that leaves a connection for
	/// that work while its transactions hold theirs. When no connection is free, the work waits
	/// for `acquire_timeout` and then fails.
	pub max_connections: u32,
	/// Minimum number of idle pool connections to maintain.
	pub min_connections: u32,
	/// Maximum time to wait when acquiring or opening a connection.
	pub acquire_timeout: Duration,
	/// Maximum time an idle connection may remain in the pool; `None` keeps idle connections
	/// until the pool closes or they reach their maximum lifetime.
	pub idle_timeout: Option<Duration>,
	/// Maximum lifetime for a pooled connection; `None` places no limit on it.
	pub max_lifetime: Option<Duration>,
	/// How often PostgreSQL checks, while a statement of a Paranoid-owned transaction runs or
	/// waits, that the client's connection is still open. Paranoid sets
	/// `client_connection_check_interval` with `SET LOCAL` in the same round trip as the
	/// `BEGIN` of every transaction that it owns, read-only and read-write.
	///
	/// An operation that the caller abandons closes its connection (see [`Tx`]). PostgreSQL
	/// then ends that connection's backend, including a backend that waits for a lock, within
	/// this interval, so the backend does not outlive the operation and no second connection
	/// is needed to cancel it. A statement of an abandoned operation therefore ends within the
	/// interval, not at once. A shorter interval ends it sooner and makes PostgreSQL check the
	/// connection more often.
	///
	/// The value must be a whole number of milliseconds from 1 millisecond to
	/// 2,147,483,647 milliseconds. The setting does not cover application SQL that runs on a
	/// bare `&Pool` outside a transaction: that statement runs without a Paranoid-owned
	/// transaction, so it keeps the connection's own setting.
	pub client_connection_check_interval: Duration,
}

/// The pool configuration cannot produce a connection.
///
/// No variant shows the database URL or any part of it.
#[derive(Debug, thiserror::Error)]
pub enum PoolConfigError {
	/// A field of [`PoolConfig`], or its URL, is outside what a connection accepts.
	#[error("invalid Postgres pool configuration: {reason}")]
	InvalidConfig {
		/// Human-readable reason.
		reason: &'static str,
	},
	/// The configured database URL could not be parsed.
	#[error("invalid Postgres database URL")]
	InvalidDatabaseUrl {
		/// Underlying parse error.
		#[source]
		source: Box<dyn std::error::Error + Send + Sync>,
	},
}

impl PoolConfigError {
	pub(crate) fn invalid_database_url(
		error: impl std::error::Error + Send + Sync + 'static,
	) -> Self {
		Self::InvalidDatabaseUrl {
			source: Box::new(error),
		}
	}
}

/// A Postgres pool could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
	/// The pool configuration cannot produce a connection.
	#[error(transparent)]
	Config(#[from] PoolConfigError),
	/// Connecting to Postgres failed.
	#[error(transparent)]
	Database(#[from] DatabaseError),
}

/// How a Paranoid-owned Postgres connection uses TLS.
///
/// The TLS-enabled variants require encryption and never retry without it.
/// `Disabled` explicitly selects plaintext. Both verifying variants trust the
/// public root certificates that `sqlx` includes, and
/// [`VerifyFull`](Self::VerifyFull) also trusts the root certificates that the
/// application supplies as PEM bytes. Neither reads a file named in the URL, and both check
/// the server's host name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DatabaseTls {
	/// Plaintext. Use it only where the transport is otherwise protected, such as a local
	/// socket in development.
	Disabled,
	/// TLS that encrypts the connection without authenticating the server. An attacker who
	/// can intercept the connection can impersonate the server, so this variant protects
	/// against passive observers only.
	Required,
	/// TLS that verifies that the server's certificate chains to one of the supplied root
	/// certificates or to one of the public root certificates that `sqlx` includes, and that
	/// the certificate matches the host name of the URL.
	VerifyFull {
		/// The trusted root certificates, as PEM bytes. They are trusted in addition to the
		/// public root certificates that `sqlx` includes.
		root_certificates: Vec<u8>,
	},
	/// TLS that verifies that the server's certificate chains to one of the public root
	/// certificates that `sqlx` includes, and that the certificate matches the host name of
	/// the URL. Use it for a server with a certificate from a public certificate authority.
	VerifyFullWithPublicRoots,
}

#[derive(Clone, Debug)]
enum ConnectionReserve {
	Unrestricted(Arc<tokio::sync::Semaphore>),
	Reserved(Arc<tokio::sync::Semaphore>),
}

impl ConnectionReserve {
	fn clone_reserved(&self) -> Self {
		match self {
			Self::Unrestricted(semaphore) | Self::Reserved(semaphore) => {
				Self::Reserved(Arc::clone(semaphore))
			}
		}
	}

	fn reserved_semaphore(&self) -> Option<&Arc<tokio::sync::Semaphore>> {
		match self {
			Self::Unrestricted(_) => None,
			Self::Reserved(semaphore) => Some(semaphore),
		}
	}
}

async fn within_pool_deadline<F: Future>(
	deadline: Option<tokio::time::Instant>,
	future: F,
) -> Result<F::Output, sqlx::Error> {
	match deadline {
		Some(deadline) => tokio::time::timeout_at(deadline, future)
			.await
			.map_err(|_| sqlx::Error::PoolTimedOut),
		None => Ok(future.await),
	}
}

async fn acquire_pool_reserve_permit(
	pool: &PgPool,
	connection_reserve: Option<&Arc<tokio::sync::Semaphore>>,
) -> Result<Option<tokio::sync::OwnedSemaphorePermit>, sqlx::Error> {
	let Some(connection_reserve) = connection_reserve else {
		return Ok(None);
	};
	let deadline = tokio::time::Instant::now().checked_add(pool.options().get_acquire_timeout());
	let permit = within_pool_deadline(deadline, Arc::clone(connection_reserve).acquire_owned())
		.await?
		.expect("the connection permits of a pool are never closed");
	Ok(Some(permit))
}

async fn acquire_pool_connection(
	pool: &PgPool,
	connection_reserve: Option<&Arc<tokio::sync::Semaphore>>,
) -> Result<
	(
		Option<tokio::sync::OwnedSemaphorePermit>,
		PoolConnection<Postgres>,
	),
	sqlx::Error,
> {
	let Some(connection_reserve) = connection_reserve else {
		return Ok((None, pool.acquire().await?));
	};
	let deadline = tokio::time::Instant::now().checked_add(pool.options().get_acquire_timeout());
	let permit = within_pool_deadline(deadline, Arc::clone(connection_reserve).acquire_owned())
		.await?
		.expect("the connection permits of a pool are never closed");
	let pooled = within_pool_deadline(deadline, pool.acquire()).await??;
	Ok((Some(permit), pooled))
}

/// Paranoid-owned, SQLx-backed Postgres pool.
///
/// Paranoid constructs this pool through [`Pool::connect`] so Paranoid-owned DB
/// primitives share one conservative connection configuration. A `&Pool` is a
/// [`sqlx::Executor`], so applications run app-owned SQL on the same pool, one
/// statement per pooled connection, and may use Paranoid's portable query
/// constructors or [`unparameterized_simple_query`](crate::db::unparameterized_simple_query).
/// Extended-protocol queries and metadata operations require an owned transaction; a pool
/// rejects them before acquiring a connection.
///
/// Application SQL on a pool must not start or end a transaction, and it must not run a
/// prepared statement with `EXECUTE`, as in a [`Tx`]: a transaction that the SQL left open
/// would stay on the pooled connection for the next user of that connection. Such a request
/// is rejected before any of its statements execute.
/// The check reads an ordinary string constant that contains a backslash both with and without
/// backslash escapes because the `standard_conforming_strings` setting decides where such a
/// constant ends, and it rejects the request when either reading starts or ends a transaction.
/// Write such text as an `E'…'` constant, or bind it as a value.
///
/// Paranoid's operations run in transactions, so they need a direct connection to PostgreSQL
/// or a pooler in transaction pooling mode. A pooler in statement pooling mode does not
/// support them; see the [`db`](crate::db) module documentation.
///
/// `Pool` does not imply any particular database privileges. It is the neutral
/// Paranoid DB handle type.
#[derive(Clone)]
pub struct Pool {
	inner: PgPool,
	client_connection_check_interval_millis: u32,
	transaction_control: Arc<TransactionControlTracker>,
	operation_observer: Option<DatabaseOperationObserver>,
	/// Shared connection reserve for views created through [`WritePool::clone_leaving_one_connection_free`].
	connection_reserve: ConnectionReserve,
}

/// Paranoid-owned Postgres pool for APIs that require write authority.
///
/// `WritePool` is a Rust API marker over the credentials used to connect. It
/// does not inspect, reduce, or enforce Postgres privileges. Construct it with a
/// connection URL for a database role with the privileges required by the write
/// APIs that you intend to call.
///
/// A `WritePool` can be passed to APIs that take [`Pool`] because a
/// write-marked handle is also a valid neutral DB handle. A plain [`Pool`]
/// cannot be passed to APIs that require `WritePool`. A `&WritePool` is a
/// [`sqlx::Executor`] that behaves like a `&Pool`.
#[derive(Clone)]
pub struct WritePool {
	pub(super) pool: Pool,
}

/// Paranoid-owned, SQLx-backed Postgres transaction.
///
/// A `&mut Tx` is a [`sqlx::Executor`], so applications run app-owned SQL in the
/// transaction, where it commits or rolls back with Paranoid-owned operations.
/// Paranoid's portable query constructors are optional helpers for
/// app-owned SQL that should stay portable to transaction-mode connection
/// poolers.
///
/// A transaction that drops before [`commit`](Tx::commit) or [`rollback`](Tx::rollback)
/// closes its connection instead of returning it to the pool because a statement of the
/// transaction can still be running on that connection. The pool opens a replacement
/// connection for later operations. Closing the connection does not cancel a statement that
/// the database is running; the statement ends when PostgreSQL next checks the connection,
/// which it does between statements and, when the `client_connection_check_interval` setting
/// is positive, while a statement runs or waits for a lock. Every transaction that Paranoid
/// begins sets that setting to [`PoolConfig::client_connection_check_interval`] with
/// `SET LOCAL`, so a statement of an abandoned transaction ends within that interval, not at
/// once. Application SQL that runs on a bare `&Pool` outside a transaction is not covered.
///
/// The `COMMIT` or `ROLLBACK` that [`commit`](Tx::commit) or [`rollback`](Tx::rollback) sends
/// is the exception: once Paranoid has sent it, it runs to its end even when the caller drops
/// the future, and only then does the connection return to the pool. A pooler between
/// Paranoid and PostgreSQL therefore never sees the client disconnect while the statement is
/// in flight. The caller of a dropped future learns nothing about the outcome.
///
/// Use this handle's consuming `commit` and `rollback` methods for whole-transaction
/// completion. Application SQL may use savepoints but must not start, end, chain, or prepare
/// the owned transaction for two-phase completion, and it must not run a prepared statement,
/// with `EXECUTE` or with a prepared statement handle because such a statement's text is
/// not visible to Paranoid. Such requests are rejected before any of their statements
/// execute. Starting another transaction requires another Rust handle.
/// The check reads an ordinary string constant that contains a backslash both with and without
/// backslash escapes because the `standard_conforming_strings` setting decides where such a
/// constant ends, and it rejects the request when either reading starts or ends a transaction.
/// Write such text as an `E'…'` constant, or bind it as a value.
///
/// `Tx` does not imply any particular database privileges. It is the neutral
/// Paranoid transaction handle type.
pub struct Tx<'tx> {
	pub(crate) inner: OwnedTransaction,
	pool: PhantomData<&'tx Pool>,
	operation_observer: Option<DatabaseOperationObserver>,
	state_mutations: Arc<StateMutationTracker>,
	interrupted_statement: Arc<StatementInterruption>,
	/// Retained across application SQL: invalidating a pin must not permit mixed generations.
	native_generations: HashMap<SortableId, NonZeroU64>,
}

#[derive(Default)]
enum StateMutationState {
	#[default]
	Ready,
	InFlight {
		unfinished: NonZeroUsize,
	},
	RollbackRequired {
		unfinished: usize,
	},
}

#[derive(Default)]
struct StateMutationTracker {
	state: Mutex<StateMutationState>,
}

impl StateMutationTracker {
	fn start(self: &Arc<Self>) -> StateMutationCompletion {
		let mut state = self
			.state
			.lock()
			.expect("mutation transitions contain no caller code");
		*state = match *state {
			StateMutationState::Ready => StateMutationState::InFlight {
				unfinished: NonZeroUsize::new(1).expect("one is nonzero"),
			},
			StateMutationState::InFlight { unfinished } => StateMutationState::InFlight {
				unfinished: unfinished
					.checked_add(1)
					.expect("mutation count fits usize"),
			},
			StateMutationState::RollbackRequired { unfinished } => {
				StateMutationState::RollbackRequired {
					unfinished: unfinished
						.checked_add(1)
						.expect("mutation count fits usize"),
				}
			}
		};
		StateMutationCompletion {
			state: StateMutationCompletionState::Pending(Arc::clone(self)),
		}
	}

	fn complete(&self) {
		let mut state = self
			.state
			.lock()
			.expect("mutation transitions contain no caller code");
		*state = match *state {
			StateMutationState::Ready => unreachable!("only pending mutations complete"),
			StateMutationState::InFlight { unfinished } => {
				match NonZeroUsize::new(unfinished.get() - 1) {
					Some(unfinished) => StateMutationState::InFlight { unfinished },
					None => StateMutationState::Ready,
				}
			}
			StateMutationState::RollbackRequired { unfinished } => {
				StateMutationState::RollbackRequired {
					unfinished: unfinished
						.checked_sub(1)
						.expect("only pending mutations complete"),
				}
			}
		};
	}

	fn abandon(&self) {
		let mut state = self
			.state
			.lock()
			.expect("mutation transitions contain no caller code");
		let unfinished = match *state {
			StateMutationState::Ready => unreachable!("only pending mutations are abandoned"),
			StateMutationState::InFlight { unfinished } => unfinished.get() - 1,
			StateMutationState::RollbackRequired { unfinished } => unfinished
				.checked_sub(1)
				.expect("only pending mutations are abandoned"),
		};
		*state = StateMutationState::RollbackRequired { unfinished };
	}

	fn requires_rollback(&self) -> bool {
		!matches!(
			*self
				.state
				.lock()
				.expect("mutation transitions contain no caller code"),
			StateMutationState::Ready
		)
	}
}

/// Completion of one state mutation; abandoning it requires transaction rollback.
pub(crate) struct StateMutationCompletion {
	state: StateMutationCompletionState,
}

enum StateMutationCompletionState {
	Pending(Arc<StateMutationTracker>),
	Completed,
}

impl StateMutationCompletion {
	/// Confirms that mutation or rejection cleanup has completed.
	pub(crate) fn complete(&mut self) {
		if let StateMutationCompletionState::Pending(tracker) = &self.state {
			tracker.complete();
			self.state = StateMutationCompletionState::Completed;
		}
	}
}

impl Drop for StateMutationCompletion {
	fn drop(&mut self) {
		if let StateMutationCompletionState::Pending(tracker) = &self.state {
			tracker.abandon();
		}
	}
}

#[derive(Debug, thiserror::Error)]
#[error("transaction cannot commit because a state mutation was abandoned")]
struct AbandonedStateMutationError;

/// A timeout holder can poison a transaction without retaining its mutable query borrow.
#[derive(Clone)]
pub(crate) struct StatementInterruptionMarker {
	interrupted: Arc<StatementInterruption>,
}

impl fmt::Debug for StatementInterruptionMarker {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("StatementInterruptionMarker")
			.finish_non_exhaustive()
	}
}

impl StatementInterruptionMarker {
	pub(crate) fn mark_interrupted(&self) {
		self.interrupted.mark_interrupted();
	}

	pub(crate) fn arm(self) -> StatementInterruptionGuard {
		StatementInterruptionGuard {
			state: StatementInterruptionGuardState::Armed(self),
		}
	}
}

/// Marks an interrupted borrowed-transaction operation before its query borrow is released.
pub(crate) struct StatementInterruptionGuard {
	state: StatementInterruptionGuardState,
}

enum StatementInterruptionGuardState {
	Armed(StatementInterruptionMarker),
	Completed,
}

impl StatementInterruptionGuard {
	pub(crate) fn complete(&mut self) {
		self.state = StatementInterruptionGuardState::Completed;
	}
}

impl Drop for StatementInterruptionGuard {
	fn drop(&mut self) {
		if let StatementInterruptionGuardState::Armed(marker) = &self.state {
			marker.mark_interrupted();
		}
	}
}

#[derive(Debug, thiserror::Error)]
#[error(
	"transaction connection was discarded after an interrupted statement; server rollback is unconfirmed"
)]
struct InterruptedStatementDiscardError;

/// Paranoid-owned Postgres transaction for APIs that require write authority.
///
/// Like [`WritePool`], this is a Rust API marker over the connection
/// credentials that created the transaction. It does not inspect or enforce
/// Postgres privileges. A `&mut WriteTx` is a [`sqlx::Executor`] that behaves
/// like a `&mut Tx`.
pub struct WriteTx<'tx> {
	pub(super) tx: Tx<'tx>,
}

/// A transaction that owns its pooled connection.
///
/// A transaction that ends through [`commit`](Self::commit) or [`rollback`](Self::rollback)
/// returns its connection to the pool. A transaction that drops before it ends, such as when
/// the caller drops the future of an operation while a statement of the application's or of an
/// operation's work runs, closes its connection instead: the connection may still be running
/// that statement, so it never returns to the pool as usable, and the pool opens a replacement
/// for later operations. PostgreSQL rolls back a transaction after its connection closes.
///
/// A transaction-control statement that Paranoid has started (`BEGIN`, `COMMIT`, `ROLLBACK`)
/// is the exception to that interruptibility. It runs in a task of its own, so a caller that
/// drops its future after Paranoid sent the statement does not stop it: the task reads the
/// statement's answer and then returns the connection to the pool, or closes it when the
/// answer shows that the connection is not safe to reuse. A pooler between Paranoid and
/// PostgreSQL then sees the statement end instead of a client that disconnects while the
/// statement is in flight, so the transaction's fate does not depend on whether a pooler is
/// present. A dropped future yields no result; a retained receipt can still observe local
/// disposal or transaction ending.
pub(crate) struct OwnedTransaction {
	connection: TransactionConnection,
	transaction_control: Arc<TransactionControlTracker>,
	/// Retained by the background ending task, including when the commit caller cancels.
	runtime_permits: Vec<Box<dyn TransactionPermitHandle>>,
	retained_resources: Vec<Box<dyn Send + Sync>>,
	/// The same caller lifetime follows background BEGIN and ending ownership.
	lease_caller: Option<Box<dyn Send + Sync>>,
	ending: OnceLock<TransactionEndingPublisher>,
}

/// BEGIN, active work, and ending own the connection until control completes.
/// Only Returning permits SQLx to return the connection or run its queued cleanup.
#[derive(Debug)]
enum TransactionConnectionLifecycle {
	Beginning { statement: &'static str },
	Active,
	Ending,
	Returning,
}

#[derive(Debug)]
enum TransactionConnectionOwnership {
	Attached {
		pooled: PoolConnection<Postgres>,
		permit: Option<tokio::sync::OwnedSemaphorePermit>,
		lifecycle: TransactionConnectionLifecycle,
	},
	Released,
}

/// The connection of a transaction, as the executor of the transaction's statements.
///
/// It runs each statement on the connection, except a statement that the fault hook of a
/// [`DatabaseOperationObserver`] failed. In place of that statement it sends
/// [`STATEMENT_THAT_POSTGRES_REJECTS`], and it returns the fault as the statement's failure.
/// PostgreSQL's transaction is then in the state that a failed statement leaves, so the
/// transaction's later statements, its savepoints, and its commit behave as they do after a
/// statement that failed in the database.
#[derive(Debug)]
pub(crate) struct TransactionConnection {
	ownership: TransactionConnectionOwnership,
	/// The fault hook's decision for the statement that the transaction recorded last. `None`
	/// when the transaction has no observer.
	pending_statement_fault: Option<PendingStatementFault>,
	/// Whether a statement on this connection failed, whichever error its caller returned. Such
	/// a failure can leave PostgreSQL's transaction aborted, and PostgreSQL ends an aborted
	/// transaction with a rollback when it commits, and reports no error.
	statement_failed: StatementFailureTracker,
}

impl TransactionConnection {
	fn connection_mut(&mut self) -> &mut PgConnection {
		self.connection_and_failure_tracker().0
	}

	/// The connection, and the tracker that records the failures of its statements.
	fn connection_and_failure_tracker(&mut self) -> (&mut PgConnection, &StatementFailureTracker) {
		let connection = match &mut self.ownership {
			TransactionConnectionOwnership::Attached { pooled, .. } => &mut **pooled,
			TransactionConnectionOwnership::Released => {
				unreachable!("only dropping releases the connection")
			}
		};
		(connection, &self.statement_failed)
	}

	fn note_statement_failure(&self) {
		self.statement_failed.note_failure();
	}

	fn has_observed_statement_failure(&self) -> bool {
		self.statement_failed.has_observed_failure()
	}

	fn lifecycle(&self) -> &TransactionConnectionLifecycle {
		match &self.ownership {
			TransactionConnectionOwnership::Attached { lifecycle, .. } => lifecycle,
			TransactionConnectionOwnership::Released => {
				unreachable!("only dropping releases the connection")
			}
		}
	}

	fn lifecycle_mut(&mut self) -> &mut TransactionConnectionLifecycle {
		match &mut self.ownership {
			TransactionConnectionOwnership::Attached { lifecycle, .. } => lifecycle,
			TransactionConnectionOwnership::Released => {
				unreachable!("only dropping releases the connection")
			}
		}
	}

	fn confirm_begin(&mut self) {
		let lifecycle = self.lifecycle_mut();
		assert!(
			matches!(lifecycle, TransactionConnectionLifecycle::Beginning { .. }),
			"only BEGIN can activate a transaction"
		);
		*lifecycle = TransactionConnectionLifecycle::Active;
	}

	fn start_ending(&mut self) {
		let lifecycle = self.lifecycle_mut();
		assert!(
			matches!(lifecycle, TransactionConnectionLifecycle::Active),
			"only an active transaction can start ending"
		);
		*lifecycle = TransactionConnectionLifecycle::Ending;
	}

	fn return_after_control(&mut self) {
		let lifecycle = self.lifecycle_mut();
		assert!(
			matches!(
				lifecycle,
				TransactionConnectionLifecycle::Active | TransactionConnectionLifecycle::Ending
			),
			"only a begun transaction returns its connection, once"
		);
		*lifecycle = TransactionConnectionLifecycle::Returning;
	}

	fn take_pending_statement_fault(&self) -> Option<OperationFaultError> {
		self.pending_statement_fault
			.as_ref()
			.and_then(PendingStatementFault::take)
	}

	/// Sends the statement that PostgreSQL rejects, and returns `fault` as the failure of the
	/// statement that the fault hook failed.
	async fn fail_statement(&mut self, fault: OperationFaultError) -> sqlx::Error {
		self.note_statement_failure();
		let result = unparameterized_simple_query(STATEMENT_THAT_POSTGRES_REJECTS)
			.execute(self.connection_mut())
			.await;
		let fault = match result {
			Err(error)
				if !error.as_database_error().is_some_and(|database_error| {
					matches!(database_error.code().as_deref(), Some("22012" | "25P02"))
				}) =>
			{
				fault.with_abort_statement_error(DatabaseError::for_statement(
					"db.tx.fault_abort",
					STATEMENT_THAT_POSTGRES_REJECTS,
					error,
				))
			}
			_ => fault,
		};
		fault.into_driver_error()
	}
}

impl Drop for TransactionConnection {
	fn drop(&mut self) {
		let TransactionConnectionOwnership::Attached {
			mut pooled,
			permit,
			lifecycle,
		} = std::mem::replace(
			&mut self.ownership,
			TransactionConnectionOwnership::Released,
		)
		else {
			unreachable!("the connection is released once at drop");
		};
		let close = !matches!(lifecycle, TransactionConnectionLifecycle::Returning);
		if close {
			pooled.close_on_drop();
		}
		let Some(permit) = permit else {
			return;
		};
		// sqlx returns and closes connections asynchronously. The runtime's permit must
		// cover that work too, or another transaction could take the renewal connection.
		tokio::spawn(async move {
			if close {
				let _ =
					tokio::time::timeout(DISCARDED_CONNECTION_CLOSE_TIMEOUT, pooled.close()).await;
			} else {
				pooled.return_to_pool().await;
			}
			drop(permit);
		});
	}
}

/// The results of a statement that a fault hook failed: the failure, and then the end.
struct FailedStatementStream<'e> {
	state: FailedStatementStreamState<'e>,
}

enum FailedStatementStreamState<'e> {
	Pending(BoxFuture<'e, sqlx::Error>),
	Exhausted,
}

impl futures_core::Stream for FailedStatementStream<'_> {
	type Item = Result<Either<PgQueryResult, PgRow>, sqlx::Error>;

	fn poll_next(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		let FailedStatementStreamState::Pending(failure) = &mut self.state else {
			return Poll::Ready(None);
		};
		let error = std::task::ready!(failure.as_mut().poll(ctx));
		self.state = FailedStatementStreamState::Exhausted;
		Poll::Ready(Some(Err(error)))
	}
}

/// Records every failure of a statement on a transaction's connection in `statement_failed`,
/// whatever its kind: an error that PostgreSQL reports, an error of an earlier statement that the
/// driver reads before this one, and a failure of the connection. A query that finds no row
/// returns no error at this level; `sqlx::Error::RowNotFound` arises above it, in `fetch_one`.
fn note_statement_result<T>(
	statement_failed: &StatementFailureTracker,
	result: Result<T, sqlx::Error>,
) -> Result<T, sqlx::Error> {
	if result.is_err() {
		statement_failed.note_failure();
	}
	result
}

/// The results of a statement on a transaction's connection, which records the statement's
/// failure when the stream yields one.
struct FailureNotingStream<'a, T> {
	statement_failed: &'a StatementFailureTracker,
	stream: BoxStream<'a, Result<T, sqlx::Error>>,
}

impl<T> futures_core::Stream for FailureNotingStream<'_, T> {
	type Item = Result<T, sqlx::Error>;
	fn poll_next(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		let poll = self.stream.as_mut().poll_next(ctx);
		if let Poll::Ready(Some(Err(_))) = &poll {
			self.statement_failed.note_failure();
		}
		poll
	}
}

/// Runs a statement of the transaction on the connection, or fails the statement when a fault
/// hook failed it. Either way, the connection records the statement's failure so that
/// [`Tx::commit`] learns of it whatever error the caller of the statement returns.
impl<'c> Executor<'c> for &'c mut TransactionConnection {
	type Database = Postgres;

	fn fetch_many<'e, 'q: 'e, E>(
		self,
		query: E,
	) -> BoxStream<'e, Result<Either<PgQueryResult, PgRow>, sqlx::Error>>
	where
		'c: 'e,
		E: 'q + Execute<'q, Postgres>,
	{
		match self.take_pending_statement_fault() {
			None => {
				let (connection, statement_failed) = self.connection_and_failure_tracker();
				Box::pin(FailureNotingStream {
					statement_failed,
					stream: connection.fetch_many(query),
				})
			}
			Some(fault) => Box::pin(FailedStatementStream {
				state: FailedStatementStreamState::Pending(Box::pin(self.fail_statement(fault))),
			}),
		}
	}

	fn fetch_optional<'e, 'q: 'e, E>(
		self,
		query: E,
	) -> BoxFuture<'e, Result<Option<PgRow>, sqlx::Error>>
	where
		'c: 'e,
		E: 'q + Execute<'q, Postgres>,
	{
		match self.take_pending_statement_fault() {
			None => {
				let (connection, statement_failed) = self.connection_and_failure_tracker();
				let fetched = connection.fetch_optional(query);
				Box::pin(async move { note_statement_result(statement_failed, fetched.await) })
			}
			Some(fault) => Box::pin(async move { Err(self.fail_statement(fault).await) }),
		}
	}

	fn prepare_with<'e>(
		self,
		sql: SqlStr,
		parameters: &'e [PgTypeInfo],
	) -> BoxFuture<'e, Result<PgStatement, sqlx::Error>>
	where
		'c: 'e,
	{
		match self.take_pending_statement_fault() {
			None => {
				let (connection, statement_failed) = self.connection_and_failure_tracker();
				let prepared = connection.prepare_with(sql, parameters);
				Box::pin(async move { note_statement_result(statement_failed, prepared.await) })
			}
			Some(fault) => Box::pin(async move { Err(self.fail_statement(fault).await) }),
		}
	}

	fn describe<'e>(self, sql: SqlStr) -> BoxFuture<'e, Result<Describe<Postgres>, sqlx::Error>>
	where
		'c: 'e,
	{
		match self.take_pending_statement_fault() {
			None => {
				let (connection, statement_failed) = self.connection_and_failure_tracker();
				let described = connection.describe(sql);
				Box::pin(async move { note_statement_result(statement_failed, described.await) })
			}
			Some(fault) => Box::pin(async move { Err(self.fail_statement(fault).await) }),
		}
	}
}

/// The statement that ends a transaction, with what that statement needs. The requested ending
/// and the failure that replaced a requested commit are a [`PendingTransactionEnding`].
enum EndingStatement {
	Commit {
		after_failed_statement: bool,
		observer: Option<DatabaseOperationObserver>,
		attempted: CommitAttemptMarker,
	},
	Rollback {
		recorded: Result<(), sqlx::Error>,
	},
}

/// The failure that an ending reports when its own statement failed. A rollback in place of a
/// requested commit reports the failure that prevented the commit, with the rollback's failure
/// as its rollback error.
fn ending_error(
	request: &PendingTransactionEnding,
	failure: TransactionEndingFailure,
) -> DatabaseError {
	let error = match request.primary() {
		Some(primary) => primary.shared().with_rollback_error(
			failure.into_database_error(ROLLBACK_LABEL, TransactionPhase::Rollback),
		),
		None => failure.into_database_error(request.operation_label(), request.requested().phase()),
	};
	requested_ending_error(request, error)
}

/// `error` as the failure of `request`: the failure of a requested commit records whether
/// Paranoid asked the driver to send the `COMMIT`.
fn requested_ending_error(
	request: &PendingTransactionEnding,
	error: DatabaseError,
) -> DatabaseError {
	match request.requested() {
		RequestedTransactionEnding::Commit => {
			error.with_commit_attempted(request.commit_attempted())
		}
		RequestedTransactionEnding::Rollback => error,
	}
}

/// Driver causes retained until the consuming transaction publishes its typed failure.
struct TransactionEndingFailure {
	primary: Arc<sqlx::Error>,
	rollback: Option<Box<Self>>,
}
impl From<sqlx::Error> for TransactionEndingFailure {
	fn from(primary: sqlx::Error) -> Self {
		Self {
			primary: Arc::new(primary),
			rollback: None,
		}
	}
}
impl TransactionEndingFailure {
	fn with_rollback(primary: Arc<sqlx::Error>, rollback: Result<(), Self>) -> Self {
		Self {
			primary,
			rollback: rollback.err().map(Box::new),
		}
	}
	fn into_database_error(
		self,
		operation_label: &'static str,
		phase: TransactionPhase,
	) -> DatabaseError {
		let error =
			DatabaseError::for_transaction_shared_driver(operation_label, phase, self.primary);
		match self.rollback {
			Some(rollback) => error.with_rollback_error(
				rollback.into_database_error(ROLLBACK_LABEL, TransactionPhase::Rollback),
			),
			None => error,
		}
	}
}

impl OwnedTransaction {
	/// Acquires a connection from `pool`. When `transaction_connections` limits the connections
	/// that the transactions hold at once, the transaction first takes one of its permits, and
	/// the pool's `acquire_timeout` bounds both waits together, as it bounds an acquisition from
	/// the pool alone.
	async fn acquire_connection(
		pool: &PgPool,
		transaction_connections: Option<&Arc<tokio::sync::Semaphore>>,
	) -> Result<
		(
			Option<tokio::sync::OwnedSemaphorePermit>,
			PoolConnection<Postgres>,
		),
		sqlx::Error,
	> {
		acquire_pool_connection(pool, transaction_connections).await
	}

	/// Begins the transaction with `begin_statement` and sets `client_connection_check_interval`
	/// locally in the same simple query, so the setting applies before any statement that can
	/// wait.
	///
	/// Acquiring the connection is a wait that the caller can interrupt. The `BEGIN` is not: when
	/// the caller drops the future after `BEGIN` was sent, the task reads its answer and queues
	/// rollback before returning the connection. Queued cleanup confirms no server rollback. A
	/// `BEGIN` that fails closes the connection instead of returning it.
	async fn begin(
		pool: &PgPool,
		transaction_connections: Option<&Arc<tokio::sync::Semaphore>>,
		transaction_control: &Arc<TransactionControlTracker>,
		begin_statement: &'static str,
		client_connection_check_interval_millis: u32,
		pending_statement_fault: Option<PendingStatementFault>,
		registration: Option<OwnedTransactionRegistration>,
	) -> Result<Self, DatabaseError> {
		let (connection_permit, pooled) = Self::acquire_connection(pool, transaction_connections)
			.await
			.map_err(|driver_error| {
				let error = Self::begin_error(begin_statement, driver_error);
				if let Some(registration) = &registration {
					registration.publish_failure(&error);
				}
				error
			})?;
		// The transaction exists before BEGIN is sent, so a transaction that the task cannot
		// finish closes the connection.
		let mut transaction = OwnedTransaction {
			connection: TransactionConnection {
				ownership: TransactionConnectionOwnership::Attached {
					pooled,
					permit: connection_permit,
					lifecycle: TransactionConnectionLifecycle::Beginning {
						statement: begin_statement,
					},
				},
				pending_statement_fault,
				statement_failed: StatementFailureTracker::new(),
			},
			transaction_control: Arc::clone(transaction_control),
			runtime_permits: Vec::new(),
			retained_resources: Vec::new(),
			lease_caller: None,
			ending: OnceLock::new(),
		};
		if let Some(registration) = registration {
			registration.transfer_to(&mut transaction);
		}
		let receipt = transaction
			.ending
			.get()
			.map(TransactionEndingPublisher::receipt);
		let in_flight = transaction_control.register_statement();
		run_to_end_in_background(
			in_flight,
			async move {
				transaction
					.run_begin(begin_statement, client_connection_check_interval_millis)
					.await
			},
			|begun: Result<Self, DatabaseError>| {
				if let Ok(transaction) = begun {
					transaction.abandon_begun_transaction();
				}
			},
		)
		.await
		.map_err(|driver_error| {
			if let Some(receipt) = &receipt
				&& let TransactionEndingSnapshot::Terminal(outcome) = receipt.snapshot()
				&& let Some(error) = outcome.error()
			{
				return error.shared();
			}
			Self::begin_error(begin_statement, driver_error)
		})?
	}

	fn begin_error(begin_statement: &'static str, driver_error: sqlx::Error) -> DatabaseError {
		DatabaseError::for_transaction(
			BEGIN_TRANSACTION_LABEL,
			TransactionPhase::Begin,
			Some(begin_statement),
			driver_error,
		)
	}

	fn publish_disposal_failure(&self, error: &DatabaseError) {
		if let Some(publisher) = self.ending.get() {
			publisher.complete_unconfirmed(error);
		}
	}

	async fn run_begin(
		mut self,
		begin_statement: &'static str,
		client_connection_check_interval_millis: u32,
	) -> Result<Self, DatabaseError> {
		let begun = <Postgres as Database>::TransactionManager::begin(
			self.connection.connection_mut(),
			Some(
				sqlx::AssertSqlSafe(format!(
					"{begin_statement}; SET LOCAL client_connection_check_interval = {client_connection_check_interval_millis}"
				))
				.into_sql_str(),
			),
		)
		.await;
		if let Err(driver_error) = begun {
			// The driver counts no transaction after a failed `BEGIN`, so it would queue no
			// rollback, but the session can still be inside the transaction block that the
			// `BEGIN` opened. The connection stays `Beginning`, and dropping the transaction
			// closes it.
			let error = Self::begin_error(begin_statement, driver_error);
			self.publish_disposal_failure(&error);
			return Err(error);
		}
		self.connection.confirm_begin();
		Ok(self)
	}

	async fn commit(
		self,
		after_failed_statement: bool,
		observer: Option<DatabaseOperationObserver>,
	) -> Result<(), DatabaseError> {
		let attempted = CommitAttemptMarker::new();
		self.end(
			EndingStatement::Commit {
				after_failed_statement,
				observer,
				attempted: attempted.clone(),
			},
			PendingTransactionEnding::Commit {
				attempted,
				operation_label: COMMIT_LABEL,
			},
		)
		.await
	}

	async fn rollback(
		self,
		request: PendingTransactionEnding,
		recorded: Result<(), sqlx::Error>,
	) -> Result<(), DatabaseError> {
		self.end(EndingStatement::Rollback { recorded }, request)
			.await
	}

	/// Publishes from owned ending work before delivery to its possibly cancelled waiter.
	async fn end(
		mut self,
		statement: EndingStatement,
		request: PendingTransactionEnding,
	) -> Result<(), DatabaseError> {
		self.connection.start_ending();
		let receipt = self.ending.get().map(TransactionEndingPublisher::receipt);
		let pending = request.shared();
		if let Some(publisher) = self.ending.get() {
			publisher.mark_pending(pending.shared());
		}
		let in_flight = self.transaction_control.register_statement();
		match run_to_end_in_background(
			in_flight,
			self.run_ending_statement(statement, request),
			|_| {},
		)
		.await
		{
			Ok(result) => result,
			Err(error) => {
				if let Some(receipt) = receipt {
					let outcome = receipt.wait_until_terminal().await;
					if let Some(error) = outcome.error() {
						return Err(error.shared());
					}
					if matches!(
						outcome.disposition(),
						TransactionEndingDisposition::KnownCommit
							| TransactionEndingDisposition::KnownRollback
					) {
						return Ok(());
					}
				}
				Err(pending.abandoned_error(error).1)
			}
		}
	}

	async fn run_ending_statement(
		mut self,
		statement: EndingStatement,
		request: PendingTransactionEnding,
	) -> Result<(), DatabaseError> {
		let (result, ending) = match statement {
			EndingStatement::Commit {
				after_failed_statement,
				observer,
				attempted,
			} => {
				let probe = if after_failed_statement {
					if let Some(observer) = &observer {
						observer.record_statement(
							DatabaseOperationKind::Execute,
							COMMIT_PROBE_LABEL,
							Some("SELECT 1"),
						);
					}
					unparameterized_simple_query("SELECT 1")
						.execute(self.as_mut())
						.await
						.map(|_| ())
				} else {
					Ok(())
				};
				if let Err(primary) = probe {
					let primary = Arc::new(primary);
					if let Some(publisher) = self.ending.get() {
						publisher.record_primary_before_rollback(
							DatabaseError::for_transaction_shared_driver(
								request.operation_label(),
								request.requested().phase(),
								Arc::clone(&primary),
							)
							.with_commit_attempted(false),
						);
					}
					let recorded = record_transaction_control(
						observer.as_ref(),
						DatabaseOperationKind::RollbackTransaction,
						ROLLBACK_LABEL,
						None,
					);
					let rollback = self.perform_rollback(recorded).await;
					let rolled_back = rollback.is_ok();
					let error = ending_error(
						&request,
						TransactionEndingFailure::with_rollback(primary, rollback),
					);
					let ending = if rolled_back {
						TransactionEndingResult::KnownRollback {
							error: Some(error.shared()),
						}
					} else {
						TransactionEndingResult::UnconfirmedDisposal {
							error: Some(error.shared()),
						}
					};
					(Err(error), ending)
				} else {
					attempted.mark_attempted();
					let committed = <Postgres as Database>::TransactionManager::commit(
						self.connection.connection_mut(),
					)
					.await;
					match self.finish_connection(committed) {
						Ok(()) => (Ok(()), TransactionEndingResult::KnownCommit),
						Err(failure) => {
							let error = ending_error(&request, failure.into());
							let ending = TransactionEndingResult::Unknown {
								error: error.shared(),
							};
							(Err(error), ending)
						}
					}
				}
			}
			EndingStatement::Rollback { recorded } => {
				match (self.perform_rollback(recorded).await, request.primary()) {
					(Ok(()), None) => (
						Ok(()),
						TransactionEndingResult::KnownRollback { error: None },
					),
					(Ok(()), Some(primary)) => {
						let error = requested_ending_error(&request, primary.shared());
						let ending = TransactionEndingResult::KnownRollback {
							error: Some(error.shared()),
						};
						(Err(error), ending)
					}
					(Err(failure), _) => {
						let error = ending_error(&request, failure);
						let ending = TransactionEndingResult::UnconfirmedDisposal {
							error: Some(error.shared()),
						};
						(Err(error), ending)
					}
				}
			}
		};
		if let Some(publisher) = self.ending.get() {
			publisher.complete(ending);
		}
		result
	}

	async fn perform_rollback(
		&mut self,
		recorded: Result<(), sqlx::Error>,
	) -> Result<(), TransactionEndingFailure> {
		let rolled_back = match recorded {
			Ok(()) => {
				<Postgres as Database>::TransactionManager::rollback(
					self.connection.connection_mut(),
				)
				.await
			}
			Err(error) => Err(error),
		};
		self.finish_connection(rolled_back)
			.map_err(TransactionEndingFailure::from)
	}

	/// Queued driver cleanup after failure is not a confirmed server rollback.
	fn finish_connection(&mut self, completed: Result<(), sqlx::Error>) -> Result<(), sqlx::Error> {
		if completed.is_err() {
			<Postgres as Database>::TransactionManager::start_rollback(
				self.connection.connection_mut(),
			);
		}
		self.connection.return_after_control();
		completed
	}

	fn discard(self, error: DatabaseError) -> Result<(), DatabaseError> {
		self.publish_disposal_failure(&error);
		Err(error)
	}

	/// Returns the connection of a transaction that began after its caller stopped waiting. The
	/// connection's next use rolls the transaction back.
	fn abandon_begun_transaction(mut self) {
		<Postgres as Database>::TransactionManager::start_rollback(
			self.connection.connection_mut(),
		);
		self.connection.return_after_control();
	}
}

/// Runs `work` in a task, so it runs to its end when the caller drops the returned future, and
/// returns its output while the caller waits. `abandon_fn` receives the output when the caller no
/// longer waits for it. `in_flight` is held until `work` ends.
async fn run_to_end_in_background<T, Work>(
	in_flight: TransactionControlInFlight,
	work: Work,
	abandon_fn: fn(T),
) -> Result<T, sqlx::Error>
where
	T: Send + 'static,
	Work: Future<Output = T> + Send + 'static,
{
	let (sender, receiver) = oneshot::channel();
	tokio::spawn(async move {
		let _in_flight = in_flight;
		if let Err(output) = sender.send(work.await) {
			abandon_fn(output);
		}
	});
	// The sender is gone without an output only when the task ended abnormally, such as when
	// the runtime shuts down.
	receiver.await.map_err(|_| sqlx::Error::WorkerCrashed)
}

/// The transaction-control statements of a pool that are running. Work that must follow a
/// statement that its caller stopped waiting for uses them to wait for the statement to end.
#[derive(Default)]
struct TransactionControlTracker {
	state: Mutex<TrackedStatements>,
	ended: Notify,
}

#[derive(Default)]
struct TrackedStatements {
	next_statement_number: u64,
	running: BTreeSet<u64>,
}

/// A running transaction-control statement; the tracker forgets it when this drops.
struct TransactionControlInFlight {
	tracker: Arc<TransactionControlTracker>,
	statement_number: u64,
}

impl TransactionControlTracker {
	fn lock_statements(&self) -> std::sync::MutexGuard<'_, TrackedStatements> {
		self.state
			.lock()
			.expect("no code panics while it holds the tracker's lock")
	}

	fn register_statement(self: &Arc<Self>) -> TransactionControlInFlight {
		let mut statements = self.lock_statements();
		let statement_number = statements.next_statement_number;
		statements.next_statement_number += 1;
		statements.running.insert(statement_number);
		TransactionControlInFlight {
			tracker: Arc::clone(self),
			statement_number,
		}
	}

	/// The statements that are running now.
	fn running_statements(self: &Arc<Self>) -> RunningTransactionControl {
		RunningTransactionControl {
			tracker: Arc::clone(self),
			first_later_statement_number: self.lock_statements().next_statement_number,
		}
	}
}

/// The transaction-control statements of a pool that were running at one moment, including
/// statements that their callers stopped awaiting.
pub(crate) struct RunningTransactionControl {
	tracker: Arc<TransactionControlTracker>,
	first_later_statement_number: u64,
}

impl RunningTransactionControl {
	fn any_still_running(&self) -> bool {
		self.tracker
			.lock_statements()
			.running
			.range(..self.first_later_statement_number)
			.next()
			.is_some()
	}

	/// Runs `run_fn`, and again after every statement of this set has ended when any was running
	/// at the start.
	///
	/// Cleanup that depends on the outcome of such a statement, such as the release of a claim
	/// that the statement may commit, needs both runs: the first finds what the statement
	/// already made durable even when the database never answers the statement, and the second
	/// finds what a statement still in flight makes durable later. `run_fn` must change nothing
	/// when it finds nothing to clean up.
	pub(crate) async fn run_now_and_after_they_end<RunFn, RunFuture>(self, run_fn: RunFn)
	where
		RunFn: Fn() -> RunFuture,
		RunFuture: Future<Output = ()>,
	{
		run_fn().await;
		if !self.any_still_running() {
			return;
		}
		loop {
			let ended = self.tracker.ended.notified();
			tokio::pin!(ended);
			ended.as_mut().enable();
			if !self.any_still_running() {
				break;
			}
			ended.await;
		}
		run_fn().await;
	}
}

impl Drop for TransactionControlInFlight {
	fn drop(&mut self) {
		self.tracker
			.lock_statements()
			.running
			.remove(&self.statement_number);
		self.tracker.ended.notify_waiters();
	}
}

impl Drop for OwnedTransaction {
	fn drop(&mut self) {
		if let TransactionConnectionLifecycle::Beginning { statement } = self.connection.lifecycle()
		{
			self.publish_disposal_failure(&Self::begin_error(
				statement,
				sqlx::Error::WorkerCrashed,
			));
		}
		if let Some(publisher) = self.ending.get() {
			publisher.mark_owner_dropped();
		}
	}
}

/// The executor of the statements of Paranoid's operations in the transaction.
impl AsMut<TransactionConnection> for OwnedTransaction {
	fn as_mut(&mut self) -> &mut TransactionConnection {
		&mut self.connection
	}
}

impl fmt::Debug for OwnedTransaction {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("OwnedTransaction")
			.field("lifecycle", self.connection.lifecycle())
			.finish_non_exhaustive()
	}
}

impl Pool {
	/// Opens a Paranoid-owned Postgres pool.
	pub async fn connect(config: PoolConfig) -> Result<Self, ConnectError> {
		let connect_options = build_pooler_safe_pg_connect_options(&config)?;
		let pool_options = build_pg_pool_options(&config)?;
		let client_connection_check_interval_millis =
			client_connection_check_interval_millis(&config);
		let inner = pool_options
			.connect_with(connect_options)
			.await
			.map_err(|error| DatabaseError::for_connection(CONNECT_LABEL, error))?;
		let work_connection_count = usize::try_from(
			inner
				.options()
				.get_max_connections()
				.saturating_sub(1)
				.max(1),
		)
		.expect("a connection count fits in usize");
		Ok(Self {
			inner,
			client_connection_check_interval_millis,
			transaction_control: Arc::default(),
			operation_observer: None,
			connection_reserve: ConnectionReserve::Unrestricted(Arc::new(
				tokio::sync::Semaphore::new(work_connection_count),
			)),
		})
	}

	/// Closes the underlying connection pool, as `sqlx`'s `close` does: it stops the pool from
	/// handing out connections, waits for every checked-out connection to be returned, and then
	/// closes every connection. Clones of this pool share the same underlying pool and are
	/// closed with it.
	///
	/// A `BEGIN`, `COMMIT`, or `ROLLBACK` that Paranoid has sent runs to its end even when its
	/// caller dropped the future, so `close` also waits for those statements' answers. A
	/// connection with an unanswered statement keeps `close` waiting until the operating system
	/// ends that connection.
	pub async fn close(&self) {
		self.inner.close().await;
	}

	/// The transaction-control statements of this pool that are running now.
	pub(crate) fn running_transaction_control(&self) -> RunningTransactionControl {
		self.transaction_control.running_statements()
	}

	/// Starts an explicit READ COMMITTED, READ ONLY Postgres transaction. PostgreSQL rejects a
	/// write in it with SQLSTATE 25006. Application executor calls check transaction-control
	/// SQL and reject prepared statement handles before sending an application query.
	pub async fn begin_transaction(&self) -> Result<Tx<'_>, DatabaseError> {
		self.begin_transaction_with(BEGIN_READ_ONLY_SQL, None, None)
			.await
	}

	/// Begins a READ ONLY transaction, as [`begin_transaction`](Self::begin_transaction) does,
	/// for the Paranoid operation `transaction_label`, which names the transaction in an
	/// observer's records.
	pub(crate) async fn begin_labeled_transaction(
		&self,
		transaction_label: &'static str,
	) -> Result<Tx<'_>, DatabaseError> {
		self.begin_transaction_with(BEGIN_READ_ONLY_SQL, None, Some(transaction_label))
			.await
	}

	async fn begin_transaction_with(
		&self,
		begin_statement: &'static str,
		registration: Option<OwnedTransactionRegistration>,
		transaction_label: Option<&'static str>,
	) -> Result<Tx<'_>, DatabaseError> {
		// The transaction holds a copy of the observer that carries the transaction's pending
		// statement fault and label.
		let (operation_observer, pending_statement_fault) = match &self.operation_observer {
			Some(operation_observer) => {
				let (operation_observer, pending_statement_fault) =
					operation_observer.for_transaction(transaction_label);
				(Some(operation_observer), Some(pending_statement_fault))
			}
			None => (None, None),
		};
		let recorded = record_transaction_control(
			operation_observer.as_ref(),
			DatabaseOperationKind::BeginTransaction,
			BEGIN_TRANSACTION_LABEL,
			Some(begin_statement),
		);
		let begun = match recorded {
			Ok(()) => {
				OwnedTransaction::begin(
					&self.inner,
					self.connection_reserve.reserved_semaphore(),
					&self.transaction_control,
					begin_statement,
					self.client_connection_check_interval_millis,
					pending_statement_fault,
					registration,
				)
				.await
			}
			// The fault hook failed the begin, so Paranoid acquires no connection and no
			// transaction exists.
			Err(fault) => {
				let error = OwnedTransaction::begin_error(begin_statement, fault);
				if let Some(registration) = &registration {
					registration.publish_failure(&error);
				}
				Err(error)
			}
		};
		let inner = begun?;
		Ok(Tx {
			inner,
			pool: PhantomData,
			native_generations: HashMap::new(),
			operation_observer,
			state_mutations: Arc::default(),
			interrupted_statement: Arc::new(StatementInterruption::new()),
		})
	}

	/// Returns a clone of this pool that records its application executor calls and
	/// Paranoid database operations in `operation_observer`.
	///
	/// Tests use this to assert how many Postgres operations a Paranoid operation performs
	/// and, with [`DatabaseOperationObserver::with_operation_fault_hook`], to make chosen
	/// operations fail.
	pub fn clone_with_database_operation_observer(
		&self,
		operation_observer: DatabaseOperationObserver,
	) -> Self {
		Self {
			inner: self.inner.clone(),
			client_connection_check_interval_millis: self.client_connection_check_interval_millis,
			transaction_control: Arc::clone(&self.transaction_control),
			operation_observer: Some(operation_observer),
			connection_reserve: self.connection_reserve.clone(),
		}
	}
}

/// Records the begin, commit, or rollback of a transaction when an observer is attached, and
/// returns the failure when the observer's fault hook fails the operation. The failure stands
/// in the place of the driver's error of the operation.
fn record_transaction_control(
	operation_observer: Option<&DatabaseOperationObserver>,
	kind: DatabaseOperationKind,
	label: &'static str,
	statement: Option<&str>,
) -> Result<(), sqlx::Error> {
	match operation_observer {
		Some(operation_observer) => operation_observer.record_operation(kind, label, statement),
		None => Ok(()),
	}
}

impl WritePool {
	/// Pool clones share acquisition capacity; independently connected pools do not.
	pub fn shares_connection_capacity(&self, other: &Self) -> bool {
		Arc::ptr_eq(
			&self.pool.transaction_control,
			&other.pool.transaction_control,
		)
	}

	/// Returns a clone of this pool that shares one limit with every clone obtained through this
	/// method on the same pool. Their transactions hold at most all but one of the pool's
	/// connections at once, leaving one for transactions through unrestricted clones. A transaction
	/// of the clone waits for a permit before acquiring a connection, and the pool's
	/// `acquire_timeout` bounds both waits together. A pool of one connection
	/// keeps none free: the clone's transactions may hold that connection.
	pub(crate) fn clone_leaving_one_connection_free(&self) -> Self {
		Self {
			pool: Pool {
				connection_reserve: self.pool.connection_reserve.clone_reserved(),
				..self.pool.clone()
			},
		}
	}

	/// Opens a Paranoid-owned Postgres pool for APIs that require write authority.
	pub async fn connect(config: PoolConfig) -> Result<Self, ConnectError> {
		Ok(Self {
			pool: Pool::connect(config).await?,
		})
	}

	/// Closes the underlying connection pool. See [`Pool::close`].
	pub async fn close(&self) {
		self.pool.close().await;
	}

	/// Starts an explicit READ COMMITTED Postgres transaction for write-requiring APIs. The
	/// statement names no access mode, so the transaction inherits the role's
	/// `default_transaction_read_only`: a role that sets it to `on` keeps that safeguard, and a
	/// write fails with SQLSTATE 25006. Application SQL follows the same transaction-control
	/// and prepared-handle restrictions as [`Pool::begin_transaction`].
	pub async fn begin_transaction(&self) -> Result<WriteTx<'_>, DatabaseError> {
		Ok(WriteTx {
			tx: self
				.pool
				.begin_transaction_with(BEGIN_WRITE_POOL_SQL, None, None)
				.await?,
		})
	}

	/// Begins a transaction, as [`begin_transaction`](Self::begin_transaction) does, for the
	/// Paranoid operation `transaction_label`, which names the transaction in an observer's
	/// records.
	pub(crate) async fn begin_labeled_transaction(
		&self,
		transaction_label: &'static str,
	) -> Result<WriteTx<'_>, DatabaseError> {
		Ok(WriteTx {
			tx: self
				.pool
				.begin_transaction_with(BEGIN_WRITE_POOL_SQL, None, Some(transaction_label))
				.await?,
		})
	}

	/// Retains fixed local work ownership before acquisition and background BEGIN.
	/// `transaction_label` names the transaction in an observer's records.
	pub(crate) async fn begin_registered_transaction(
		&self,
		registration: OwnedTransactionRegistration,
		transaction_label: &'static str,
	) -> Result<WriteTx<'_>, DatabaseError> {
		Ok(WriteTx {
			tx: self
				.pool
				.begin_transaction_with(
					BEGIN_WRITE_POOL_SQL,
					Some(registration),
					Some(transaction_label),
				)
				.await?,
		})
	}

	/// Retains fixed local work ownership before acquisition and background READ ONLY BEGIN.
	/// `transaction_label` names the transaction in an observer's records.
	pub(crate) async fn begin_registered_read_transaction(
		&self,
		registration: OwnedTransactionRegistration,
		transaction_label: &'static str,
	) -> Result<Tx<'_>, DatabaseError> {
		self.pool
			.begin_transaction_with(
				BEGIN_READ_ONLY_SQL,
				Some(registration),
				Some(transaction_label),
			)
			.await
	}

	/// Returns a clone of this pool that records its application executor calls and
	/// Paranoid database operations in `operation_observer`.
	///
	/// Tests use this to assert how many Postgres operations a Paranoid operation performs
	/// and, with [`DatabaseOperationObserver::with_operation_fault_hook`], to make chosen
	/// operations fail.
	pub fn clone_with_database_operation_observer(
		&self,
		operation_observer: DatabaseOperationObserver,
	) -> Self {
		Self {
			pool: self
				.pool
				.clone_with_database_operation_observer(operation_observer),
		}
	}
}

impl Deref for WritePool {
	type Target = Pool;

	fn deref(&self) -> &Self::Target {
		&self.pool
	}
}

impl<'tx> Tx<'tx> {
	/// Identifies this actual transaction before protected work; it grants no execution authority.
	pub(crate) fn ending_receipt(&mut self) -> TransactionEndingReceipt {
		self.inner
			.ending
			.get_or_init(TransactionEndingPublisher::new)
			.receipt()
	}

	/// Retains an owned resource until transaction control finishes or the connection is
	/// discarded. This includes background commit/rollback after the awaiting caller cancels.
	/// Savepoint rollback does not release retained resources. Retention does not confirm
	/// server-side rollback after a connection failure. Resources are never exposed in Debug.
	pub fn retain_resource(&mut self, resource: impl Send + Sync + 'static) {
		self.inner.retained_resources.push(Box::new(resource));
	}

	pub(crate) fn retain_runtime_permit(&mut self, permit: impl TransactionPermitHandle) {
		let runtime_identity = permit.runtime_identity();
		if !self
			.inner
			.runtime_permits
			.iter()
			.any(|held| held.runtime_identity() == runtime_identity)
		{
			self.inner.runtime_permits.push(Box::new(permit));
		}
	}
	/// Selects one sticky generation per installation for this whole transaction.
	/// A savepoint rollback can release the physical pin but cannot clear this selection.
	pub(crate) fn select_native_generation(
		&mut self,
		installation_id: SortableId,
		generation: NonZeroU64,
	) -> bool {
		match self.native_generations.entry(installation_id) {
			std::collections::hash_map::Entry::Occupied(selected) => *selected.get() == generation,
			std::collections::hash_map::Entry::Vacant(selected) => {
				selected.insert(generation);
				true
			}
		}
	}

	pub(crate) fn database_operation_observer(&self) -> Option<&DatabaseOperationObserver> {
		self.operation_observer.as_ref()
	}

	/// Records a statement of this transaction, directly before the caller runs the statement
	/// on this transaction.
	pub(crate) fn record_database_operation(
		&self,
		kind: DatabaseOperationKind,
		label: &'static str,
		statement: Option<&str>,
	) {
		if let Some(operation_observer) = &self.operation_observer {
			operation_observer.record_statement(kind, label, statement);
		}
	}

	/// Records that a statement of this transaction failed in the database.
	pub(crate) fn note_statement_failure(&self) {
		self.inner.connection.note_statement_failure();
	}

	/// Tracks mutation completion without borrowing the transaction across guard construction.
	pub(crate) fn track_state_mutation(&mut self) -> StateMutationCompletion {
		self.state_mutations.start()
	}

	/// Used only when a bounded wait can drop a statement before the driver reads its answer.
	pub(crate) fn statement_interruption_marker(&mut self) -> StatementInterruptionMarker {
		StatementInterruptionMarker {
			interrupted: Arc::clone(&self.interrupted_statement),
		}
	}

	pub(crate) fn track_statement_interruption(&mut self) -> StatementInterruptionGuard {
		StatementInterruptionGuard {
			state: StatementInterruptionGuardState::Armed(self.statement_interruption_marker()),
		}
	}

	/// Whether Paranoid interrupted a statement of this transaction through its
	/// [`StatementInterruptionMarker`], which means that ending the transaction discards its
	/// connection.
	pub(crate) fn statement_was_interrupted(&self) -> bool {
		self.interrupted_statement.was_interrupted()
	}

	fn interrupted_statement_error(phase: TransactionPhase) -> DatabaseError {
		let error = DatabaseError::for_transaction(
			DISCARD_INTERRUPTED_STATEMENT_LABEL,
			phase,
			None,
			sqlx::Error::Configuration(Box::new(InterruptedStatementDiscardError)),
		);
		match phase {
			// The discard sends no `COMMIT`.
			TransactionPhase::Commit => error.with_commit_attempted(false),
			_ => error,
		}
	}

	/// Commits this transaction.
	///
	/// PostgreSQL aborts a transaction when one of its statements fails with an error that the
	/// server reports, in any message of the simple or the extended query protocol. This
	/// includes a statement that a cancellation or `statement_timeout` ends (SQLSTATE 57014), a
	/// lock timeout (55P03), and a deadlock (40P01). Inside a savepoint, the failure aborts the
	/// savepoint's subtransaction, and a `ROLLBACK TO SAVEPOINT` to that savepoint or to an
	/// earlier one restores the transaction. An error that a PL/pgSQL `EXCEPTION` clause catches
	/// aborts nothing, and neither does an error that only the driver detects, such as a missing
	/// row or a value that cannot be decoded. A `COMMIT` of an aborted transaction rolls the
	/// transaction back, and PostgreSQL reports no error. After a failure of the connection,
	/// PostgreSQL rolls the transaction back when it notices the closed connection, and no later
	/// statement of the transaction succeeds.
	///
	/// The transaction's connection records every failure of a statement that runs on it,
	/// whether the statement belongs to the application or to a Paranoid operation, and
	/// whichever error the statement's caller then returns: every error that PostgreSQL
	/// reports, also an error of an earlier statement that the driver reads first, every
	/// failure of the connection, and every failure that a fault hook injects. A query that
	/// finds no row has not failed. After a recorded failure, this method runs an observed
	/// `db.tx.commit_probe` statement before it sends `COMMIT`. The probe succeeds when a
	/// savepoint rollback restored the transaction, and the transaction then commits. A failed
	/// probe requests an observed rollback and returns the probe failure in the
	/// [`TransactionPhase::Commit`] phase, preserving any rollback failure, so this method does
	/// not report a commit of a transaction that a failed statement aborted.
	///
	/// When a state mutation or its rejection cleanup was abandoned, this method sends a
	/// rollback instead of a commit and returns an error in the commit phase. The error has
	/// the operation label `db.tx.commit_abandoned_state_mutation`. A rollback failure retains
	/// its database classification. The observer records the rollback that was attempted.
	///
	/// When the fault hook of the transaction's [`DatabaseOperationObserver`] fails the commit,
	/// this method sends a `ROLLBACK` in place of the `COMMIT` and returns the failure with
	/// the [`TransactionPhase::Commit`] phase.
	pub async fn commit(self) -> Result<(), DatabaseError> {
		if self.statement_was_interrupted() {
			return self
				.inner
				.discard(Self::interrupted_statement_error(TransactionPhase::Commit));
		}
		if self.state_mutations.requires_rollback() {
			let recorded = record_transaction_control(
				self.operation_observer.as_ref(),
				DatabaseOperationKind::RollbackTransaction,
				ROLLBACK_ABANDONED_STATE_MUTATION_LABEL,
				None,
			);
			return self
				.inner
				.rollback(
					PendingTransactionEnding::rejected_commit(
						COMMIT_ABANDONED_STATE_MUTATION_LABEL,
						sqlx::Error::Configuration(Box::new(AbandonedStateMutationError)),
					),
					recorded,
				)
				.await;
		}
		let recorded = record_transaction_control(
			self.operation_observer.as_ref(),
			DatabaseOperationKind::CommitTransaction,
			COMMIT_LABEL,
			None,
		);
		match recorded {
			Ok(()) => {
				let after_failed_statement = self.inner.connection.has_observed_statement_failure();
				self.inner
					.commit(after_failed_statement, self.operation_observer)
					.await
			}
			Err(fault) => {
				let rollback = record_transaction_control(
					self.operation_observer.as_ref(),
					DatabaseOperationKind::RollbackTransaction,
					ROLLBACK_LABEL,
					None,
				);
				self.inner
					.rollback(
						PendingTransactionEnding::rejected_commit(COMMIT_LABEL, fault),
						rollback,
					)
					.await
			}
		}
	}

	/// Runs `sql` on the transaction's connection as Paranoid's own operations run their
	/// statements, without the checks of the application executor, and returns its failure.
	/// Tests use it to fail a statement of the transaction that no application executor runs.
	#[cfg(feature = "__internal")]
	#[doc(hidden)]
	pub async fn execute_on_transaction_connection_for_tests(
		&mut self,
		sql: &'static str,
	) -> Result<(), DatabaseError> {
		super::owned_portable_query(sql)
			.execute(self.inner.as_mut())
			.await
			.map(drop)
	}

	/// Rolls this transaction back. A rejected or failed rollback queues driver cleanup,
	/// without confirming that PostgreSQL has released the transaction's locks.
	pub async fn rollback(self) -> Result<(), DatabaseError> {
		if self.statement_was_interrupted() {
			return self.inner.discard(Self::interrupted_statement_error(
				TransactionPhase::Rollback,
			));
		}
		let recorded = record_transaction_control(
			self.operation_observer.as_ref(),
			DatabaseOperationKind::RollbackTransaction,
			ROLLBACK_LABEL,
			None,
		);
		self.inner
			.rollback(
				PendingTransactionEnding::Rollback {
					operation_label: ROLLBACK_LABEL,
				},
				recorded,
			)
			.await
	}
}

impl<'tx> WriteTx<'tx> {
	pub(crate) fn database_operation_observer(&self) -> Option<&DatabaseOperationObserver> {
		self.tx.database_operation_observer()
	}

	pub(crate) fn record_database_operation(
		&self,
		kind: DatabaseOperationKind,
		label: &'static str,
		statement: Option<&str>,
	) {
		self.tx.record_database_operation(kind, label, statement);
	}

	/// Commits this transaction.
	pub async fn commit(self) -> Result<(), DatabaseError> {
		self.tx.commit().await
	}

	/// Rolls this transaction back.
	pub async fn rollback(self) -> Result<(), DatabaseError> {
		self.tx.rollback().await
	}
}

impl<'tx> Deref for WriteTx<'tx> {
	type Target = Tx<'tx>;

	fn deref(&self) -> &Self::Target {
		&self.tx
	}
}

impl<'tx> DerefMut for WriteTx<'tx> {
	fn deref_mut(&mut self) -> &mut Self::Target {
		&mut self.tx
	}
}

impl DatabaseTls {
	fn configure(&self, options: PgConnectOptions) -> PgConnectOptions {
		match self {
			Self::Disabled => options.ssl_mode(PgSslMode::Disable),
			Self::Required => options.ssl_mode(PgSslMode::Require),
			Self::VerifyFull { root_certificates } => options
				.ssl_mode(PgSslMode::VerifyFull)
				.ssl_root_cert_from_pem(root_certificates.clone()),
			Self::VerifyFullWithPublicRoots => options.ssl_mode(PgSslMode::VerifyFull),
		}
	}
}

/// Builds connection options without connecting to PostgreSQL.
///
/// Checks the complete [`PoolConfig`], disables statement caching and SQL logging, sends no
/// `extra_float_digits` startup parameter, and applies the configured identity, application
/// name, and TLS policy. The URL must name its host, user, and database. Ambient `PGOPTIONS`,
/// `PGSSLCERT`, `PGSSLKEY`, and `PGSSLROOTCERT` are rejected; `PGPASSWORD` is rejected when
/// the URL has no password. These settings cannot be cleared through the driver's options API.
///
/// Returns [`PoolConfigError`] for invalid configuration or URL input. To retain
/// Paranoid's transaction lifecycle and pooler guarantees, use [`Pool::connect`]
/// or [`WritePool::connect`] rather than creating a raw SQLx pool from these options.
pub fn build_pooler_safe_pg_connect_options(
	config: &PoolConfig,
) -> Result<PgConnectOptions, PoolConfigError> {
	validate_pg_pool_config(config)?;

	let url = parse_database_url_identity(config.database_url.expose_secret())?;
	// A statement's text can contain values, and a portable query's text always contains its bound
	// values as literals. The driver would log every statement at the debug level and every
	// statement that takes one second or longer at the warning level, with that text.
	// A pooler can reject a startup parameter that it does not recognize: PgBouncer rejects
	// `extra_float_digits` unless its `ignore_startup_parameters` setting names it.
	let options = connect_options_from_identity_url(&url)?
		.statement_cache_capacity(POOLER_SAFE_STATEMENT_CACHE_CAPACITY)
		.application_name(&config.application_name)
		.extra_float_digits(None)
		.disable_statement_logging();

	Ok(config.tls.configure(options))
}

/// Environment variables that `sqlx` reads when it creates connect options and that it offers
/// no way to clear. Setting any of them would add a second source for a setting that the
/// URL or [`PoolConfig`] owns.
const UNCLEARABLE_CONNECTION_ENVIRONMENT_VARIABLES: [(&str, &str); 4] = [
	(
		"PGOPTIONS",
		"the PGOPTIONS environment variable is set; sqlx would send it as startup options, so unset it",
	),
	(
		"PGSSLCERT",
		"the PGSSLCERT environment variable is set; sqlx would use it as the client certificate, so unset it",
	),
	(
		"PGSSLKEY",
		"the PGSSLKEY environment variable is set; sqlx would use it as the client key, so unset it",
	),
	(
		"PGSSLROOTCERT",
		"the PGSSLROOTCERT environment variable is set; sqlx would trust its certificates as root certificates, so unset it",
	),
];

const PGPASSWORD_WITHOUT_URL_PASSWORD_REASON: &str = "the PGPASSWORD environment variable is set and database_url has no password; sqlx would use the variable as the password, so unset it or put the password in database_url";

/// Builds `sqlx` connect options from the identity that the URL names, without the `PG*`
/// environment variables and without the `.pgpass` file as sources of any setting.
///
/// Every setting that `sqlx` fills from the environment is set here from the URL
/// (`PGHOST`, `PGHOSTADDR`, `PGPORT`, `PGUSER`, `PGDATABASE`, and `PGPASSWORD` when the URL has
/// a password) or by the caller of this function (`PGSSLMODE` and `PGAPPNAME`). `PGOPTIONS`,
/// `PGSSLCERT`, `PGSSLKEY`, `PGSSLROOTCERT`, and `PGPASSWORD` without a URL password cannot be
/// cleared, so they are rejected. The URL is read as `sqlx` reads it, including the `host`,
/// `hostaddr`, `port`, `dbname`, `user`, and `password` query parameters.
fn connect_options_from_identity_url(url: &url::Url) -> Result<PgConnectOptions, PoolConfigError> {
	for (variable, reason) in UNCLEARABLE_CONNECTION_ENVIRONMENT_VARIABLES {
		if std::env::var_os(variable).is_some() {
			return Err(PoolConfigError::InvalidConfig { reason });
		}
	}
	let query_password = url
		.query_pairs()
		.any(|(name, _)| name.as_ref() == "password");
	if url.password().is_none() && !query_password && std::env::var_os("PGPASSWORD").is_some() {
		return Err(PoolConfigError::InvalidConfig {
			reason: PGPASSWORD_WITHOUT_URL_PASSWORD_REASON,
		});
	}

	// An empty host and the default port replace the environment's values; the URL's values
	// replace these.
	let mut options = PgConnectOptions::new_without_pgpass().host("").port(5432);
	let mut host_named = false;
	let mut user_named = false;
	let mut database_named = false;

	if let Some(host) = url.host_str().filter(|host| !host.is_empty()) {
		let decoded = percent_decode_utf8(host)?;
		options = if decoded.starts_with('/') {
			options.socket(decoded.as_ref())
		} else {
			options.host(host)
		};
		host_named = true;
	}
	if let Some(port) = url.port() {
		options = options.port(port);
	}
	if !url.username().is_empty() {
		options = options.username(&percent_decode_utf8(url.username())?);
		user_named = true;
	}
	if let Some(password) = url.password() {
		options = options.password(&percent_decode_utf8(password)?);
	}
	let path = url.path().trim_start_matches('/');
	if !path.is_empty() {
		options = options.database(&percent_decode_utf8(path)?);
		database_named = true;
	}
	for (name, value) in url.query_pairs() {
		match name.as_ref() {
			"host" => {
				options = if value.starts_with('/') {
					options.socket(value.as_ref())
				} else {
					options.host(&value)
				};
				host_named = true;
			}
			"hostaddr" => {
				value
					.parse::<std::net::IpAddr>()
					.map_err(PoolConfigError::invalid_database_url)?;
				options = options.host(&value);
				host_named = true;
			}
			"port" => {
				options = options.port(
					value
						.parse()
						.map_err(PoolConfigError::invalid_database_url)?,
				);
			}
			"dbname" => {
				options = options.database(&value);
				database_named = true;
			}
			"user" => {
				options = options.username(&value);
				user_named = true;
			}
			"password" => options = options.password(&value),
			_ => unreachable!("parse_database_url_identity admits only identity parameters"),
		}
	}

	if !host_named {
		return Err(PoolConfigError::InvalidConfig {
			reason: "database_url must name the host",
		});
	}
	if !user_named {
		return Err(PoolConfigError::InvalidConfig {
			reason: "database_url must name the user",
		});
	}
	if !database_named {
		return Err(PoolConfigError::InvalidConfig {
			reason: "database_url must name the database",
		});
	}
	Ok(options)
}

fn percent_decode_utf8(text: &str) -> Result<std::borrow::Cow<'_, str>, PoolConfigError> {
	percent_encoding::percent_decode_str(text)
		.decode_utf8()
		.map_err(PoolConfigError::invalid_database_url)
}

/// Builds SQLx pool limits without opening connections.
///
/// Checks the complete [`PoolConfig`] and copies its connection count, acquisition
/// timeout, idle timeout, and maximum lifetime. This function does not build the
/// connection identity or TLS options. Returns [`PoolConfigError`] for invalid
/// configuration or URL input.
pub fn build_pg_pool_options(config: &PoolConfig) -> Result<PgPoolOptions, PoolConfigError> {
	validate_pg_pool_config(config)?;

	Ok(PgPoolOptions::new()
		.max_connections(config.max_connections)
		.min_connections(config.min_connections)
		.acquire_timeout(config.acquire_timeout)
		.idle_timeout(config.idle_timeout)
		.max_lifetime(config.max_lifetime))
}

const MAX_CLIENT_CONNECTION_CHECK_INTERVAL_MILLIS: u32 = i32::MAX as u32;

/// The validated interval in milliseconds, which PostgreSQL's setting takes.
fn client_connection_check_interval_millis(config: &PoolConfig) -> u32 {
	u32::try_from(config.client_connection_check_interval.as_millis())
		.expect("PoolConfig validation bounds the interval")
}

fn validate_pg_pool_config(config: &PoolConfig) -> Result<(), PoolConfigError> {
	let interval = config.client_connection_check_interval;
	if !interval.subsec_nanos().is_multiple_of(1_000_000)
		|| interval.is_zero()
		|| interval.as_millis() > u128::from(MAX_CLIENT_CONNECTION_CHECK_INTERVAL_MILLIS)
	{
		return Err(PoolConfigError::InvalidConfig {
			reason: "client_connection_check_interval must be a whole number of milliseconds from 1 to 2147483647",
		});
	}
	if config.max_connections == 0 {
		return Err(PoolConfigError::InvalidConfig {
			reason: "max_connections must be at least 1",
		});
	}
	if config.min_connections > config.max_connections {
		return Err(PoolConfigError::InvalidConfig {
			reason: "min_connections must not exceed max_connections",
		});
	}
	if config.acquire_timeout.is_zero() {
		return Err(PoolConfigError::InvalidConfig {
			reason: "acquire_timeout must be non-zero",
		});
	}
	if config
		.idle_timeout
		.is_some_and(|duration| duration.is_zero())
	{
		return Err(PoolConfigError::InvalidConfig {
			reason: "idle_timeout must be non-zero when configured",
		});
	}
	if config
		.max_lifetime
		.is_some_and(|duration| duration.is_zero())
	{
		return Err(PoolConfigError::InvalidConfig {
			reason: "max_lifetime must be non-zero when configured",
		});
	}
	if config.database_url.expose_secret().trim().is_empty() {
		return Err(PoolConfigError::InvalidConfig {
			reason: "database_url must not be empty",
		});
	}
	parse_database_url_identity(config.database_url.expose_secret())?;
	if config.application_name.trim().is_empty() || config.application_name.as_bytes().contains(&0)
	{
		return Err(PoolConfigError::InvalidConfig {
			reason: "application_name must contain a visible character and must not contain null bytes",
		});
	}
	if let DatabaseTls::VerifyFull { root_certificates } = &config.tls
		&& root_certificates.is_empty()
	{
		return Err(PoolConfigError::InvalidConfig {
			reason: "root_certificates must not be empty",
		});
	}

	Ok(())
}

/// Parses the database URL and rejects any query parameter that names a setting other than
/// the connection's identity and credentials. No error contains the URL or any of its parts.
fn parse_database_url_identity(database_url: &str) -> Result<url::Url, PoolConfigError> {
	let url = url::Url::parse(database_url).map_err(PoolConfigError::invalid_database_url)?;
	if !matches!(url.scheme(), "postgres" | "postgresql") {
		return Err(PoolConfigError::InvalidConfig {
			reason: "database_url must use the postgres or postgresql scheme",
		});
	}
	let carries_other_setting = url
		.query_pairs()
		.any(|(name, _)| !IDENTITY_URL_QUERY_PARAMETERS.contains(&name.as_ref()));
	if carries_other_setting {
		return Err(PoolConfigError::InvalidConfig {
			reason: "database_url query parameters other than host, hostaddr, port, dbname, user, and password are not accepted; PoolConfig fields own TLS, application name, and every other setting",
		});
	}
	Ok(url)
}

impl fmt::Debug for Pool {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("Pool")
			.field("inner", &self.inner)
			.finish_non_exhaustive()
	}
}

impl fmt::Debug for WritePool {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("WritePool")
			.field("pool", &self.pool)
			.finish()
	}
}

impl fmt::Debug for Tx<'_> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("Tx")
			.field("inner", &self.inner)
			.finish_non_exhaustive()
	}
}

impl fmt::Debug for WriteTx<'_> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("WriteTx")
			.field("tx", &self.tx)
			.finish()
	}
}

mod application_executor;

#[cfg(test)]
mod tests {
	use super::*;
	use std::sync::atomic::{AtomicUsize, Ordering};
	use std::time::Duration;

	async fn run_counting(runs: &AtomicUsize) {
		runs.fetch_add(1, Ordering::SeqCst);
	}

	#[tokio::test]
	async fn cleanup_runs_once_when_no_statement_was_running() {
		let tracker = Arc::new(TransactionControlTracker::default());
		let runs = AtomicUsize::new(0);
		tracker
			.running_statements()
			.run_now_and_after_they_end(|| run_counting(&runs))
			.await;
		assert_eq!(runs.load(Ordering::SeqCst), 1);
	}

	#[tokio::test]
	async fn cleanup_runs_again_after_the_statement_that_was_running_ends() {
		let tracker = Arc::new(TransactionControlTracker::default());
		let in_flight = tracker.register_statement();
		let running = tracker.running_statements();
		// A statement that starts after the snapshot is not waited for.
		let later = tracker.register_statement();
		let runs = Arc::new(AtomicUsize::new(0));
		let cleanup = tokio::spawn({
			let runs = Arc::clone(&runs);
			async move {
				running
					.run_now_and_after_they_end(|| run_counting(&runs))
					.await;
			}
		});
		tokio::time::sleep(Duration::from_millis(50)).await;
		assert_eq!(
			runs.load(Ordering::SeqCst),
			1,
			"the first run does not wait for the statement"
		);
		assert!(
			!cleanup.is_finished(),
			"the second run waits for the statement"
		);
		drop(in_flight);
		tokio::time::timeout(Duration::from_secs(5), cleanup)
			.await
			.expect("the cleanup ends after the statement ends")
			.expect("the cleanup does not panic");
		assert_eq!(runs.load(Ordering::SeqCst), 2);
		drop(later);
	}
}
