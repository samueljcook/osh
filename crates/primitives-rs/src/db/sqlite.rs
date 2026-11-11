//! Owned synchronous SQLite transactions with explicit completion outcomes.
//!
//! A connection runs one callback at a time. Write callbacks begin with
//! `BEGIN IMMEDIATE`; contention therefore occurs before application work.
//! Callbacks run once, and successful return commits their statements together.
//! A failed operation prevents subsequent statements and commit, even when the
//! callback ignores that operation's error. Callback errors trigger rollback.
//!
//! Application statements cannot control transactions, attach databases, or
//! change connection settings. SQLite's authorizer enforces those restrictions
//! during preparation, and `query_only` prevents read callbacks from writing.
//! No underlying connection, prepared statement, or row iterator escapes the
//! transaction's ownership. Application SQL can create and use its own tables.
//! [`maintenance`] provides explicit auto-vacuum configuration, snapshot page
//! statistics, and bounded incremental reclamation in an owned write transaction.
//!
//! WAL connections use `synchronous=FULL`; rollback-journal connections use
//! `synchronous=EXTRA`. Apple targets also enable `fullfsync`, requesting
//! `F_FULLFSYNC` where SQLite's filesystem implementation supports it.
//! These settings request durable commits, subject to the filesystem and
//! hardware honoring SQLite's synchronization requests. The
//! library verifies its settings and uses bundled SQLite. A path selects a
//! trusted local database location; these APIs do not provide a filesystem
//! sandbox or encryption of database pages.
//!
//! Calls are synchronous. An async application can submit an entire operation to
//! a blocking worker. Abandoning that worker's result does not cancel a started
//! transaction or imply rollback. Unwinding through a callback attempts rollback
//! and preserves the panic; process abort does not execute Rust destructors.
//!
//! This facility owns no application schema, migration policy, record format,
//! synchronization protocol, or remote durability guarantee.
//!
//! [`Connection::open_read_only`] requires an existing file and preserves its
//! journal mode and contents. SQLite retains normal locking and change detection.
//! A WAL reader may create or update the `-shm` WAL-index sidecar; this is not a
//! promise of zero filesystem writes. Recovery that requires modifying the
//! database fails on a read-only connection.
//!
//! ```no_run
//! use primitives_rs::db::sqlite::{
//!     Connection, JournalMode, OpenMode, OpenOptions, OperationError, QueryOptions,
//! };
//! use std::time::Duration;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mut db = Connection::open("./app.db", OpenOptions {
//!     mode: OpenMode::CreateIfMissing,
//!     journal_mode: JournalMode::Wal,
//!     auto_vacuum: None,
//!     foreign_keys: true,
//!     busy_timeout: Duration::from_secs(2),
//! })?;
//! db.write(|tx| {
//!     tx.execute("CREATE TABLE counters (value INTEGER NOT NULL)", [])?;
//!     tx.execute("INSERT INTO counters VALUES (?1)", [42])?;
//!     Ok::<_, OperationError>(())
//! })?;
//! let values = db.read(|tx| tx.query(
//!     "SELECT value FROM counters", [], QueryOptions { max_rows: 1 },
//!     |row| row.get::<_, i64>(0),
//! ))?;
//! assert_eq!(values, [42]);
//! # Ok(())
//! # }
//! ```

#[cfg(all(feature = "sqlite-backup", any(unix, windows)))]
pub mod backup;
mod error;
pub mod maintenance;
#[cfg(feature = "sqlite-records")]
pub mod records;
pub use error::{
	BeginError, FailureCompletion, FailureOutcome, OpenError, OperationError, TransactionError,
};

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::{OpenFlags, Params, Row};
use std::convert::Infallible;
use std::ops::ControlFlow;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

/// Whether opening a database may create its file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenMode {
	/// Requires an existing database file.
	Existing,
	/// Creates the database file if it is absent. Parent directories must exist.
	CreateIfMissing,
}

/// SQLite's journaling strategy. Both variants request durable commits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalMode {
	/// Uses a write-ahead log with FULL synchronization.
	Wal,
	/// Uses a DELETE rollback journal with EXTRA synchronization.
	Rollback,
}

/// Explicit configuration for an owned SQLite connection.
#[derive(Clone, Copy, Debug)]
pub struct OpenOptions {
	/// Whether the database file must already exist.
	pub mode: OpenMode,
	/// The journaling strategy to apply and verify.
	pub journal_mode: JournalMode,
	/// An explicitly requested persisted auto-vacuum mode. `None` preserves the
	/// database's mode; a new database then uses SQLite's default mode.
	/// A change that requires rebuilding the database fails without a rewrite.
	/// Select incremental mode before creating tables to permit bounded reclamation.
	pub auto_vacuum: Option<maintenance::AutoVacuumMode>,
	/// Whether SQLite enforces the application's declared foreign-key constraints.
	pub foreign_keys: bool,
	/// SQLite's maximum busy-handler wait before reporting contention.
	/// Zero requests immediate failure. This does not replay application callbacks.
	/// SQLite uses whole milliseconds; fractional milliseconds are truncated.
	/// The truncated millisecond value must fit in a signed 32-bit integer.
	pub busy_timeout: Duration,
}

/// Configuration for a read-only connection to an existing SQLite file.
#[derive(Clone, Copy, Debug)]
pub struct ReadOnlyOpenOptions {
	/// SQLite's maximum busy-handler wait before reporting contention.
	/// Zero requests immediate failure. Whole milliseconds must fit in `i32`;
	/// fractional milliseconds are truncated.
	pub busy_timeout: Duration,
}

/// Bounds for the rows that a query delivers to its callback.
#[derive(Clone, Copy, Debug)]
pub struct QueryOptions {
	/// Maximum rows to decode or visit. An additional row fails the operation.
	/// This is not a bound on SQL execution work or individual values; the caller's
	/// SQL and decoder must impose those bounds where required.
	pub max_rows: usize,
}

/// One exclusively owned SQLite connection. A cleanup failure or an uncertain
/// commit outcome discards the connection; later transactions report that state.
pub struct Connection {
	ownership: ConnectionOwnership,
	read_only: bool,
	library_control: Arc<SqlAuthorization>,
	#[cfg(feature = "sqlite-records")]
	record_authorization: Arc<records::TableAuthorization>,
	/// The database file path that SQLite opened, with a canonical parent dir.
	#[cfg(all(feature = "sqlite-backup", any(unix, windows)))]
	path: std::path::PathBuf,
}

enum ConnectionOwnership {
	Open(rusqlite::Connection),
	Discarded,
}

#[derive(Clone, Copy)]
enum RollbackCertainty {
	NotCommitted,
	CommitUncertain,
}

impl Connection {
	/// Opens a local database, configures durable journaling, and installs
	/// transaction-control restrictions. Empty paths and `:memory:` are rejected,
	/// and the final path component cannot be a symlink. Parent directory symlinks
	/// are resolved before opening; this does not confine access against concurrent
	/// parent directory changes. The filename is interpreted as a filesystem path,
	/// never as a SQLite URI.
	pub fn open(path: impl AsRef<Path>, options: OpenOptions) -> Result<Self, OpenError> {
		let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE;
		if options.mode == OpenMode::CreateIfMissing {
			flags |= OpenFlags::SQLITE_OPEN_CREATE;
		}
		let (connection, path) = open_database(path.as_ref(), flags, options.busy_timeout)?;
		if let Some(requested) = options.auto_vacuum {
			connection.pragma_update(None, "auto_vacuum", requested.as_sqlite_value())?;
			let actual = maintenance::read_auto_vacuum_mode(&connection)?;
			if actual != requested {
				return Err(OpenError::AutoVacuumChangeRequiresRewrite { requested, actual });
			}
		}
		let (journal_mode, synchronization) = match options.journal_mode {
			JournalMode::Wal => ("wal", 2i32),
			JournalMode::Rollback => ("delete", 3i32),
		};
		connection.pragma_update(None, "journal_mode", journal_mode)?;
		connection.pragma_update(None, "synchronous", synchronization)?;
		connection.pragma_update(None, "foreign_keys", options.foreign_keys)?;
		#[cfg(target_vendor = "apple")]
		{
			connection.pragma_update(None, "fullfsync", true)?;
			let fullfsync: bool =
				connection.pragma_query_value(None, "fullfsync", |row| row.get(0))?;
			if !fullfsync {
				return Err(OpenError::SettingRejected {
					setting: "fullfsync",
				});
			}
		}
		let actual_journal: String =
			connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
		let actual_synchronization: i32 =
			connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
		let actual_foreign_keys: bool =
			connection.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
		if actual_journal != journal_mode {
			return Err(OpenError::SettingRejected {
				setting: "journal_mode",
			});
		}
		if actual_synchronization != synchronization {
			return Err(OpenError::SettingRejected {
				setting: "synchronous",
			});
		}
		if actual_foreign_keys != options.foreign_keys {
			return Err(OpenError::SettingRejected {
				setting: "foreign_keys",
			});
		}
		Self::with_transaction_control(connection, path, false)
	}

	/// Opens an existing file with SQLite's read-only flag and no journal changes.
	///
	/// This operation does not create a database, apply a schema, or run migrations.
	/// It uses the same filesystem-path and symlink rules as [`open`](Self::open).
	/// [`write`](Self::write) fails before its callback runs. Read transactions
	/// retain the ordinary SQL restrictions and SQLite snapshot behavior.
	/// WAL readers may create or update SQLite's coordination sidecars; this API
	/// does not mark the database immutable or disable concurrent-change detection.
	///
	/// ```no_run
	/// use primitives_rs::db::sqlite::{Connection, ReadOnlyOpenOptions, QueryOptions};
	/// use std::time::Duration;
	/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
	/// let mut db = Connection::open_read_only("app.db", ReadOnlyOpenOptions {
	///     busy_timeout: Duration::from_secs(1),
	/// })?;
	/// let names = db.read(|tx| tx.query(
	///     "SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name",
	///     [], QueryOptions { max_rows: 100 }, |row| row.get::<_, String>(0),
	/// ))?;
	/// # let _ = names;
	/// # Ok(())
	/// # }
	/// ```
	pub fn open_read_only(
		path: impl AsRef<Path>,
		options: ReadOnlyOpenOptions,
	) -> Result<Self, OpenError> {
		let (connection, path) = open_database(
			path.as_ref(),
			OpenFlags::SQLITE_OPEN_READ_ONLY,
			options.busy_timeout,
		)?;
		Self::with_transaction_control(connection, path, true)
	}

	fn with_transaction_control(
		connection: rusqlite::Connection,
		path: std::path::PathBuf,
		read_only: bool,
	) -> Result<Self, OpenError> {
		#[cfg(not(all(feature = "sqlite-backup", any(unix, windows))))]
		let _ = path;
		let library_control = Arc::new(SqlAuthorization::new());
		let authorized_control = Arc::clone(&library_control);
		#[cfg(feature = "sqlite-records")]
		let record_authorization = Arc::new(records::TableAuthorization::new());
		#[cfg(feature = "sqlite-records")]
		let authorized_records = Arc::clone(&record_authorization);
		connection.authorizer(Some(move |context: AuthContext<'_>| {
			#[cfg(feature = "sqlite-records")]
			if !authorized_records.allows(context) {
				return Authorization::Deny;
			}
			match context.action {
				// ALTER TABLE validates CHECK, generated NOT NULL, and STRICT
				// constraints through this read-only pragma. Its argument selects
				// a table or result bound; it never changes connection settings.
				AuthAction::Pragma { pragma_name, .. }
					if pragma_name.eq_ignore_ascii_case("quick_check") =>
				{
					Authorization::Allow
				}
				AuthAction::Transaction { .. }
				| AuthAction::Savepoint { .. }
				| AuthAction::Pragma { .. }
				| AuthAction::Attach { .. }
				| AuthAction::Detach { .. }
					if !authorized_control.allows_library_control() =>
				{
					Authorization::Deny
				}
				_ => Authorization::Allow,
			}
		}))?;
		Ok(Self {
			ownership: ConnectionOwnership::Open(connection),
			read_only,
			library_control,
			#[cfg(feature = "sqlite-records")]
			record_authorization,
			#[cfg(all(feature = "sqlite-backup", any(unix, windows)))]
			path,
		})
	}

	/// Returns whether this handle still owns its database connection.
	pub fn is_open(&self) -> bool {
		matches!(self.ownership, ConnectionOwnership::Open(_))
	}

	/// Returns whether this handle was opened with read-only access.
	pub fn is_read_only(&self) -> bool {
		self.read_only
	}

	/// Runs a callback in a read-only transaction and completes the transaction
	/// before returning its result. SQLite rejects writes even through query methods.
	/// Its database snapshot is established by its first read, not by callback entry.
	pub fn read<T, E>(
		&mut self,
		read_fn: impl FnOnce(&mut Transaction<'_>) -> Result<T, E>,
	) -> Result<T, TransactionError<E>> {
		self.transact(false, read_fn)
	}

	/// Acquires write authority, runs the callback once, and commits its successful
	/// result. The callback may combine application and library statements.
	pub fn write<T, E>(
		&mut self,
		write_fn: impl FnOnce(&mut Transaction<'_>) -> Result<T, E>,
	) -> Result<T, TransactionError<E>> {
		self.transact(true, write_fn)
	}

	fn transact<T, E>(
		&mut self,
		writable: bool,
		callback: impl FnOnce(&mut Transaction<'_>) -> Result<T, E>,
	) -> Result<T, TransactionError<E>> {
		let ConnectionOwnership::Open(connection) = &self.ownership else {
			return Err(TransactionError::Begin {
				source: BeginError::ConnectionDiscarded,
				completion: FailureCompletion {
					outcome: FailureOutcome::NotStarted,
					cleanup_error: None,
				},
			});
		};
		if writable && self.read_only {
			return Err(TransactionError::Begin {
				source: BeginError::ReadOnlyConnection,
				completion: FailureCompletion {
					outcome: FailureOutcome::NotStarted,
					cleanup_error: None,
				},
			});
		}
		if !connection.is_autocommit() {
			self.ownership = ConnectionOwnership::Discarded;
			return Err(TransactionError::Begin {
				source: BeginError::UnexpectedTransaction,
				completion: FailureCompletion {
					outcome: FailureOutcome::Unknown,
					cleanup_error: None,
				},
			});
		}
		#[cfg(feature = "sqlite-records")]
		self.record_authorization.reset();
		let begin = self.with_library_control(|connection| {
			connection.pragma_update(None, "query_only", !writable)?;
			connection.execute_batch(if writable {
				"BEGIN IMMEDIATE"
			} else {
				"BEGIN DEFERRED"
			})
		});
		if let Err(source) = begin {
			let completion = if self.raw().is_autocommit() {
				FailureCompletion {
					outcome: FailureOutcome::NotStarted,
					cleanup_error: None,
				}
			} else {
				self.rollback(RollbackCertainty::NotCommitted)
			};
			return Err(TransactionError::Begin {
				source: BeginError::Sqlite(source),
				completion,
			});
		}
		let mut transaction = Transaction {
			owner: self,
			state: TransactionState::Active,
			writable,
		};
		let value = match callback(&mut transaction) {
			Ok(value) => value,
			Err(source) => {
				let operation_error = transaction.operation_error();
				let completion = transaction.rollback();
				return Err(TransactionError::Callback {
					source,
					operation_error,
					completion,
				});
			}
		};
		if let Err(source) = transaction.require_active() {
			let completion = transaction.rollback();
			return Err(TransactionError::Operation { source, completion });
		}
		transaction.state = TransactionState::Committing;
		let commit = transaction
			.owner
			.with_library_control(|connection| connection.execute_batch("COMMIT"));
		match commit {
			Ok(()) => {
				transaction.state = TransactionState::Finished;
				Ok(value)
			}
			Err(source) => {
				let completion = transaction.rollback();
				Err(TransactionError::Commit { source, completion })
			}
		}
	}

	fn raw(&self) -> &rusqlite::Connection {
		match &self.ownership {
			ConnectionOwnership::Open(connection) => connection,
			ConnectionOwnership::Discarded => {
				unreachable!("an active transaction owns its connection")
			}
		}
	}

	fn with_library_control<T>(
		&self,
		operation_fn: impl FnOnce(&rusqlite::Connection) -> rusqlite::Result<T>,
	) -> rusqlite::Result<T> {
		let _guard = LibraryControlGuard::new(&self.library_control);
		operation_fn(self.raw())
	}

	fn rollback(&mut self, certainty: RollbackCertainty) -> FailureCompletion {
		if self.raw().is_autocommit() {
			let outcome = match certainty {
				RollbackCertainty::NotCommitted => FailureOutcome::RolledBack,
				RollbackCertainty::CommitUncertain => {
					self.ownership = ConnectionOwnership::Discarded;
					FailureOutcome::Unknown
				}
			};
			return FailureCompletion {
				outcome,
				cleanup_error: None,
			};
		}
		let result = self.with_library_control(|connection| connection.execute_batch("ROLLBACK"));
		let ended = self.raw().is_autocommit();
		if result.is_err() || !ended {
			self.ownership = ConnectionOwnership::Discarded;
		}
		FailureCompletion {
			outcome: if ended {
				FailureOutcome::RolledBack
			} else {
				FailureOutcome::Unknown
			},
			cleanup_error: result.err().map(Box::new),
		}
	}
}

fn open_database(
	path: &Path,
	flags: OpenFlags,
	busy_timeout: Duration,
) -> Result<(rusqlite::Connection, std::path::PathBuf), OpenError> {
	if path.as_os_str().is_empty() || path == Path::new(":memory:") {
		return Err(OpenError::NonPersistentPath);
	}
	if busy_timeout.as_millis() > i32::MAX as u128 {
		return Err(OpenError::BusyTimeoutTooLong);
	}
	// An absolute filesystem path cannot be interpreted as a SQLite file: URI.
	let path = resolve_database_path(path).map_err(OpenError::Path)?;
	let connection = rusqlite::Connection::open_with_flags(
		&path,
		flags | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW,
	)?;
	connection.busy_timeout(busy_timeout)?;
	Ok((connection, path))
}

fn resolve_database_path(path: &Path) -> std::io::Result<std::path::PathBuf> {
	let path = std::path::absolute(path)?;
	let parent = path.parent().ok_or_else(|| {
		std::io::Error::new(
			std::io::ErrorKind::InvalidInput,
			"database path must name a file",
		)
	})?;
	let name = path.file_name().ok_or_else(|| {
		std::io::Error::new(
			std::io::ErrorKind::InvalidInput,
			"database path must name a file",
		)
	})?;
	// SQLite's NOFOLLOW rejects symlinks in every component. Resolve only the
	// parent so ordinary paths such as macOS /var work while the final filename
	// remains subject to SQLite's symlink rejection.
	Ok(parent.canonicalize()?.join(name))
}

#[derive(Clone, Copy)]
#[repr(u8)]
enum SqlAuthorizationState {
	ApplicationSql,
	LibraryControl,
}

struct SqlAuthorization(AtomicU8);

impl SqlAuthorization {
	fn new() -> Self {
		Self(AtomicU8::new(SqlAuthorizationState::ApplicationSql as u8))
	}

	fn allows_library_control(&self) -> bool {
		match self.0.load(Ordering::Relaxed) {
			value if value == SqlAuthorizationState::ApplicationSql as u8 => false,
			value if value == SqlAuthorizationState::LibraryControl as u8 => true,
			_ => unreachable!("SQL authorization states are ApplicationSql and LibraryControl"),
		}
	}
}

struct LibraryControlGuard<'a> {
	authorized: &'a SqlAuthorization,
}
impl<'a> LibraryControlGuard<'a> {
	fn new(authorized: &'a SqlAuthorization) -> Self {
		assert!(
			authorized
				.0
				.compare_exchange(
					SqlAuthorizationState::ApplicationSql as u8,
					SqlAuthorizationState::LibraryControl as u8,
					Ordering::Relaxed,
					Ordering::Relaxed
				)
				.is_ok(),
			"library SQL control must not nest"
		);
		Self { authorized }
	}
}
impl Drop for LibraryControlGuard<'_> {
	fn drop(&mut self) {
		self.authorized.0.store(
			SqlAuthorizationState::ApplicationSql as u8,
			Ordering::Relaxed,
		);
	}
}

/// Operations inside a connection-owned callback. Each method finishes or closes
/// its statement before returning. Statements and row references cannot escape.
pub struct Transaction<'a> {
	owner: &'a mut Connection,
	state: TransactionState,
	writable: bool,
}

enum TransactionState {
	Active,
	ExecutingStatement,
	Failed(OperationError),
	Committing,
	Finished,
}

impl Transaction<'_> {
	/// Returns whether this callback was opened through `Connection::read`.
	pub fn is_read_only(&self) -> bool {
		!self.writable
	}

	/// Runs one composed operation that may contain SQL and application validation.
	///
	/// Returning an error prevents later statements and commit, even when the
	/// enclosing transaction callback ignores that error. The returned error keeps
	/// its original type. If no SQL operation failed, later use of the transaction
	/// reports `ComposedOperationFailed`; the transaction does not retain a copy of
	/// the operation's error. Propagate that error to preserve its full diagnostic.
	///
	/// A panic that unwinds out of the operation marks the transaction as failed
	/// before continuing the unwind. Catching that panic outside this method cannot
	/// permit commit. An operation that ignores a failed nested operation also fails.
	/// Represent expected alternatives, such as a condition mismatch, with `Ok`.
	pub fn run_operation<T, E>(
		&mut self,
		operation_fn: impl FnOnce(&mut Self) -> Result<T, E>,
	) -> Result<T, E>
	where
		E: From<OperationError>,
	{
		self.require_active().map_err(E::from)?;
		// After an unwind, the transaction is unusable except for rollback. The
		// panic resumes after that state is recorded, so this boundary does not
		// claim that application state is safe to reuse after a panic.
		match catch_unwind(AssertUnwindSafe(|| operation_fn(self))) {
			Ok(Ok(value)) => {
				self.require_active().map_err(E::from)?;
				Ok(value)
			}
			Ok(Err(error)) => {
				self.note_operation_failure(OperationError::ComposedOperationFailed);
				Err(error)
			}
			Err(panic) => {
				self.note_operation_failure(OperationError::OperationInterrupted);
				resume_unwind(panic)
			}
		}
	}

	/// Executes one statement that returns no rows and reports its affected row count.
	/// Transaction control, savepoints, PRAGMA, ATTACH, and DETACH are rejected.
	pub fn execute(&mut self, sql: &str, params: impl Params) -> Result<usize, OperationError> {
		self.require_active()?;
		self.state = TransactionState::ExecutingStatement;
		let result = self
			.owner
			.raw()
			.execute(sql, params)
			.map_err(OperationError::from);
		self.finish_operation(result)
	}

	/// Decodes and collects a bounded number of rows from one statement.
	/// Decode errors poison the transaction just like SQL errors. Rows borrow the
	/// statement only during `decode_fn`; decoded output must own any retained data.
	pub fn query<T>(
		&mut self,
		sql: &str,
		params: impl Params,
		options: QueryOptions,
		mut decode_fn: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
	) -> Result<Vec<T>, OperationError> {
		let mut values = Vec::new();
		let completion = self.visit_statement_rows(sql, params, options, false, |row| {
			values
				.try_reserve(1)
				.map_err(|_| OperationError::AllocationFailed)?;
			values.push(decode_fn(row)?);
			Ok(ControlFlow::<Infallible>::Continue(()))
		})?;
		match completion {
			ControlFlow::Continue(()) => Ok(values),
			ControlFlow::Break(impossible) => match impossible {},
		}
	}

	/// Visits rows from a read-only statement without collecting them.
	///
	/// `ControlFlow::Break(value)` successfully stops visitation and returns that
	/// value. `Continue(())` as the final result means that the statement exhausted
	/// its rows. A stopped scan makes no assertion about whether more rows exist.
	/// SQLite must classify the statement as read-only before it can execute.
	/// Use `query` for writes with `RETURNING`, which must drain all their rows.
	///
	/// Visitor errors and panics prevent commit even when the caller catches them.
	/// Row references cannot escape the visitor; retained output must own its data.
	pub fn visit_rows<B>(
		&mut self,
		sql: &str,
		params: impl Params,
		options: QueryOptions,
		mut visitor: impl FnMut(&Row<'_>) -> rusqlite::Result<ControlFlow<B>>,
	) -> Result<ControlFlow<B>, OperationError> {
		self.visit_statement_rows(sql, params, options, true, |row| {
			visitor(row).map_err(OperationError::from)
		})
	}

	fn visit_statement_rows<B>(
		&mut self,
		sql: &str,
		params: impl Params,
		options: QueryOptions,
		read_only: bool,
		mut visitor: impl FnMut(&Row<'_>) -> Result<ControlFlow<B>, OperationError>,
	) -> Result<ControlFlow<B>, OperationError> {
		self.require_active()?;
		self.state = TransactionState::ExecutingStatement;
		let result = (|| {
			let mut statement = self.owner.raw().prepare(sql)?;
			if read_only && !statement.readonly() {
				return Err(OperationError::ReadOnlyStatementRequired);
			}
			let mut rows = statement.query(params)?;
			let mut visited_rows = 0;
			while let Some(row) = rows.next()? {
				if visited_rows == options.max_rows {
					return Err(OperationError::RowLimitExceeded {
						max_rows: options.max_rows,
					});
				}
				visited_rows += 1;
				if let ControlFlow::Break(value) = visitor(row)? {
					return Ok(ControlFlow::Break(value));
				}
			}
			Ok(ControlFlow::Continue(()))
		})();
		self.finish_operation(result)
	}

	fn require_active(&mut self) -> Result<(), OperationError> {
		match &self.state {
			TransactionState::Active => {}
			TransactionState::ExecutingStatement => {
				return Err(OperationError::OperationInterrupted);
			}
			TransactionState::Failed(error) => return Err(error.clone()),
			TransactionState::Committing | TransactionState::Finished => {
				unreachable!("callbacks operate only before transaction completion")
			}
		}
		if self.owner.raw().is_autocommit() {
			self.state = TransactionState::Failed(OperationError::TransactionEnded);
			return Err(OperationError::TransactionEnded);
		}
		Ok(())
	}

	fn finish_operation<T>(
		&mut self,
		result: Result<T, OperationError>,
	) -> Result<T, OperationError> {
		assert!(
			matches!(self.state, TransactionState::ExecutingStatement),
			"only an executing statement can complete"
		);
		match result {
			Ok(value) => {
				self.state = TransactionState::Active;
				self.require_active()?;
				Ok(value)
			}
			Err(error) => {
				self.state = TransactionState::Failed(error.clone());
				Err(error)
			}
		}
	}

	fn operation_error(&self) -> Option<OperationError> {
		match &self.state {
			TransactionState::Active => None,
			TransactionState::ExecutingStatement => Some(OperationError::OperationInterrupted),
			TransactionState::Failed(error) => Some(error.clone()),
			TransactionState::Committing | TransactionState::Finished => {
				unreachable!("callbacks finish before commit begins")
			}
		}
	}

	fn note_operation_failure(&mut self, error: OperationError) {
		self.state = match &self.state {
			TransactionState::Active => TransactionState::Failed(error),
			TransactionState::ExecutingStatement => {
				TransactionState::Failed(OperationError::OperationInterrupted)
			}
			TransactionState::Failed(_) => return,
			TransactionState::Committing | TransactionState::Finished => {
				unreachable!("callbacks operate only before transaction completion")
			}
		};
	}

	fn rollback(&mut self) -> FailureCompletion {
		let certainty = match self.state {
			TransactionState::Active
			| TransactionState::ExecutingStatement
			| TransactionState::Failed(_) => RollbackCertainty::NotCommitted,
			TransactionState::Committing => RollbackCertainty::CommitUncertain,
			TransactionState::Finished => {
				unreachable!("a finished transaction cannot roll back again")
			}
		};
		let completion = self.owner.rollback(certainty);
		self.state = TransactionState::Finished;
		completion
	}
}

impl Drop for Transaction<'_> {
	fn drop(&mut self) {
		if !matches!(self.state, TransactionState::Finished) {
			self.rollback();
		}
	}
}
