use super::PgSqlState;
use super::operation_observer::OperationFaultError;
use sqlx::postgres::PgDatabaseError;
use std::error::Error as StdError;
use std::fmt;
use std::sync::Arc;

/// The protocol state of a published v10 installation does not admit this release to import
/// it or to work beside it.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
	/// The active epoch or its immutable definition is unsupported by this release.
	#[error(
		"Paranoid protocol epoch {active_epoch} is incompatible with supported epoch {supported_epoch}: {reason}"
	)]
	Incompatible {
		/// Active database compatibility epoch.
		active_epoch: i32,
		/// Compatibility epoch implemented by this release.
		supported_epoch: i32,
		/// Exact compatibility mismatch.
		reason: &'static str,
	},
	/// The protocol relation does not exist. Either the v10 location names no published v10
	/// installation, for example because its schema name is not the one that v10 used, or the
	/// v10 `protocol` table of an installation that imported v10 was dropped. Every opening of
	/// such an installation reads that table, including after v10 data retirement.
	#[error("Paranoid protocol relation {relation} does not exist")]
	RelationAbsent {
		/// The quoted, schema-qualified name of the protocol relation.
		relation: String,
	},
	/// Required protocol metadata or transaction semantics are invalid.
	#[error("Paranoid protocol state is invalid: {reason}")]
	State {
		/// Reason why admission cannot safely proceed.
		reason: &'static str,
	},
	/// An installation or an upgrade definition is configured inconsistently.
	#[error("Paranoid protocol configuration is invalid: {reason}")]
	Config {
		/// Reason why the configuration is rejected.
		reason: &'static str,
	},
}

/// The phase of a failed transaction-control statement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionPhase {
	/// Beginning a transaction.
	Begin,
	/// Creating a savepoint.
	Savepoint,
	/// Committing a transaction.
	Commit,
	/// Rolling a transaction back.
	Rollback,
	/// Rolling back to a savepoint.
	RollbackToSavepoint,
	/// Releasing a savepoint.
	ReleaseSavepoint,
}

impl TransactionPhase {
	const fn description(self) -> &'static str {
		match self {
			Self::Begin => "beginning a transaction",
			Self::Savepoint => "creating a savepoint",
			Self::Commit => "committing a transaction",
			Self::Rollback => "rolling back a transaction",
			Self::RollbackToSavepoint => "rolling back to a savepoint",
			Self::ReleaseSavepoint => "releasing a savepoint",
		}
	}
}

/// Whether and why repeating the failed database work might succeed.
///
/// The classification comes from the SQLSTATE that PostgreSQL reported or, when it reported
/// none, from the kind of `sqlx` failure. It describes the failure and does not decide a retry
/// policy: whether to repeat work depends on whether the work is safe to repeat.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryClassification {
	/// Repeating the work without a change cannot succeed, or its outcome is unknown.
	NotRetryable,
	/// A concurrent transaction conflicted with this one (SQLSTATE `40001`).
	SerializationFailure,
	/// PostgreSQL chose this transaction to end a deadlock (SQLSTATE `40P01`).
	Deadlock,
	/// A lock was not available and the statement did not wait (SQLSTATE `55P03`).
	LockNotAvailable,
	/// The connection failed or could not be established (SQLSTATE class `08`, or an I/O, TLS,
	/// or pool failure).
	ConnectionFailure,
	/// The statement was canceled (SQLSTATE `57014`).
	QueryCanceled,
	/// The server is shutting down or not yet accepting connections (SQLSTATE `57P01`,
	/// `57P02`, or `57P03`).
	ServerUnavailable,
	/// The server ran out of a resource (SQLSTATE class `53`).
	InsufficientResources,
	/// The server or its operating system failed (SQLSTATE class `58`).
	SystemError,
}

impl RetryClassification {
	fn from_sql_state_code(code: &str) -> Self {
		match code {
			"40001" => Self::SerializationFailure,
			"40P01" => Self::Deadlock,
			"55P03" => Self::LockNotAvailable,
			"57014" => Self::QueryCanceled,
			"57P01" | "57P02" | "57P03" => Self::ServerUnavailable,
			_ if code.starts_with("08") => Self::ConnectionFailure,
			_ if code.starts_with("53") => Self::InsufficientResources,
			_ if code.starts_with("58") => Self::SystemError,
			_ => Self::NotRetryable,
		}
	}

	fn from_sqlx_error(error: &sqlx::Error) -> Self {
		match error {
			sqlx::Error::Io(_)
			| sqlx::Error::Tls(_)
			| sqlx::Error::PoolTimedOut
			| sqlx::Error::PoolClosed
			| sqlx::Error::WorkerCrashed => Self::ConnectionFailure,
			_ => Self::NotRetryable,
		}
	}
}

/// A failure of database work: connecting, acquiring a connection, running a transaction
/// control statement, or running a query. The queries include an application's own
/// [`portable_query`](super::portable_query).
///
/// The error names the Paranoid operation that was running and, when a statement failed, the
/// statement as its template with `$n` placeholders. The template never contains values, so
/// the error is safe to log. When a transaction control statement failed, the error names its
/// phase. When PostgreSQL reported a SQLSTATE, the error carries it, together with a
/// [`RetryClassification`]. When PostgreSQL reported the names of the constraint, table,
/// column, schema, or data type that the failure concerns, the error carries them too.
///
/// PostgreSQL's messages and details can contain row values, and Paranoid's portable queries
/// carry values as literals in the statement text. The error therefore does not print or
/// expose the driver's error by default: [`source`](StdError::source) returns `None`, and
/// neither `Display` nor `Debug` shows the driver's message or detail. The identifiers that
/// PostgreSQL reports are names and never values. [`driver_error`](Self::driver_error)
/// returns the driver's error.
///
/// In a test, the fault hook of a
/// [`DatabaseOperationObserver`](super::DatabaseOperationObserver) can fail an operation that
/// Paranoid then does not send.
/// [`faulted_operation_label`](Self::faulted_operation_label) identifies such a
/// failure, which has no SQLSTATE and the retry classification that the fault hook chose in
/// its [`DatabaseOperationFault`](super::DatabaseOperationFault).
pub struct DatabaseError(Arc<DatabaseErrorDetails>);

#[derive(Clone)]
struct DatabaseErrorDetails {
	operation_label: &'static str,
	statement: Option<Box<str>>,
	phase: Option<FailedPhase>,
	sql_state: Option<PgSqlState>,
	retry: RetryClassification,
	identifiers: ReportedIdentifiers,
	driver_error: Arc<sqlx::Error>,
	rollback_error: Option<Arc<DatabaseError>>,
}

/// The transaction-control phase of a failure. A commit-phase failure records whether Paranoid
/// asked the driver to send the `COMMIT`, or ended the transaction with a rollback in place of
/// the commit.
#[derive(Clone, Copy, Debug)]
enum FailedPhase {
	Begin,
	Savepoint,
	Commit { attempted: bool },
	Rollback,
	RollbackToSavepoint,
	ReleaseSavepoint,
}

impl FailedPhase {
	/// A commit-phase failure without a recorded attempt follows an attempted `COMMIT`.
	fn new(phase: TransactionPhase) -> Self {
		match phase {
			TransactionPhase::Begin => Self::Begin,
			TransactionPhase::Savepoint => Self::Savepoint,
			TransactionPhase::Commit => Self::Commit { attempted: true },
			TransactionPhase::Rollback => Self::Rollback,
			TransactionPhase::RollbackToSavepoint => Self::RollbackToSavepoint,
			TransactionPhase::ReleaseSavepoint => Self::ReleaseSavepoint,
		}
	}

	fn phase(self) -> TransactionPhase {
		match self {
			Self::Begin => TransactionPhase::Begin,
			Self::Savepoint => TransactionPhase::Savepoint,
			Self::Commit { .. } => TransactionPhase::Commit,
			Self::Rollback => TransactionPhase::Rollback,
			Self::RollbackToSavepoint => TransactionPhase::RollbackToSavepoint,
			Self::ReleaseSavepoint => TransactionPhase::ReleaseSavepoint,
		}
	}
}

/// The names that PostgreSQL reports for a failure.
#[derive(Clone, Default)]
struct ReportedIdentifiers {
	constraint: Option<Box<str>>,
	table: Option<Box<str>>,
	column: Option<Box<str>>,
	schema: Option<Box<str>>,
	data_type: Option<Box<str>>,
}

impl ReportedIdentifiers {
	fn from_driver_error(driver_error: &sqlx::Error) -> Self {
		let Some(pg_error) = driver_error
			.as_database_error()
			.and_then(|database_error| database_error.try_downcast_ref::<PgDatabaseError>())
		else {
			return Self::default();
		};
		Self {
			constraint: pg_error.constraint().map(Box::from),
			table: pg_error.table().map(Box::from),
			column: pg_error.column().map(Box::from),
			schema: pg_error.schema().map(Box::from),
			data_type: pg_error.data_type().map(Box::from),
		}
	}
}

impl DatabaseError {
	/// Shares immutable causes; metadata rewrites use copy-on-write.
	pub(crate) fn shared(&self) -> Self {
		Self(Arc::clone(&self.0))
	}

	fn new(
		operation_label: &'static str,
		statement: Option<&str>,
		phase: Option<TransactionPhase>,
		driver_error: Arc<sqlx::Error>,
	) -> Self {
		let sql_state = driver_error
			.as_database_error()
			.and_then(|database_error| database_error.code())
			.map(PgSqlState::from_code);
		let retry = match &sql_state {
			Some(sql_state) => RetryClassification::from_sql_state_code(sql_state.as_str()),
			None => OperationFaultError::retry_in_driver_error(&driver_error)
				.unwrap_or_else(|| RetryClassification::from_sqlx_error(&driver_error)),
		};
		Self(Arc::new(DatabaseErrorDetails {
			operation_label,
			statement: statement.map(Box::from),
			phase: phase.map(FailedPhase::new),
			sql_state,
			retry,
			identifiers: ReportedIdentifiers::from_driver_error(&driver_error),
			driver_error,
			rollback_error: None,
		}))
	}

	/// A statement of the operation that `operation_label` names failed. `statement` is its
	/// template without values.
	pub(crate) fn for_statement(
		operation_label: &'static str,
		statement: &str,
		driver_error: sqlx::Error,
	) -> Self {
		Self::new(
			operation_label,
			Some(statement),
			None,
			Arc::new(driver_error),
		)
	}

	/// A transaction control statement failed. A commit-phase failure follows an attempted
	/// `COMMIT` until [`with_commit_attempted`](Self::with_commit_attempted) records otherwise.
	pub(super) fn for_transaction(
		operation_label: &'static str,
		phase: TransactionPhase,
		statement: Option<&str>,
		driver_error: sqlx::Error,
	) -> Self {
		Self::new(
			operation_label,
			statement,
			Some(phase),
			Arc::new(driver_error),
		)
	}

	pub(super) fn for_transaction_shared_driver(
		operation_label: &'static str,
		phase: TransactionPhase,
		driver_error: Arc<sqlx::Error>,
	) -> Self {
		Self::new(operation_label, None, Some(phase), driver_error)
	}

	/// Connecting or acquiring a connection failed.
	pub(super) fn for_connection(operation_label: &'static str, driver_error: sqlx::Error) -> Self {
		Self::new(operation_label, None, None, Arc::new(driver_error))
	}

	/// Names `operation_label` as the label of the failed Paranoid operation.
	pub(crate) fn with_operation_label(mut self, operation_label: &'static str) -> Self {
		Arc::make_mut(&mut self.0).operation_label = operation_label;
		self
	}

	/// Names `phase` as the transaction control phase that failed. A commit-phase failure keeps
	/// its recorded commit attempt, and a failure that gets the commit phase follows an
	/// attempted `COMMIT`.
	pub(crate) fn with_phase(mut self, phase: TransactionPhase) -> Self {
		let details = Arc::make_mut(&mut self.0);
		if !matches!(
			(phase, details.phase),
			(TransactionPhase::Commit, Some(FailedPhase::Commit { .. }))
		) {
			details.phase = Some(FailedPhase::new(phase));
		}
		self
	}

	/// Records whether Paranoid asked the driver to send the `COMMIT` of this commit-phase
	/// failure, from the actual owned transaction path and not from operation labels. The
	/// failure must have the commit phase.
	pub(super) fn with_commit_attempted(mut self, attempted: bool) -> Self {
		let Some(FailedPhase::Commit {
			attempted: recorded,
		}) = &mut Arc::make_mut(&mut self.0).phase
		else {
			unreachable!("only a commit-phase failure records a commit attempt");
		};
		*recorded = attempted;
		self
	}

	pub(crate) fn with_rollback_error(mut self, rollback_error: DatabaseError) -> Self {
		Arc::make_mut(&mut self.0).rollback_error = Some(Arc::new(rollback_error));
		self
	}

	/// The separate rollback failure after this primary transaction-ending failure.
	/// Its operation, phase, classification, and driver details remain independently available.
	pub fn rollback_error(&self) -> Option<&DatabaseError> {
		self.0.rollback_error.as_deref()
	}

	#[cfg(test)]
	pub(crate) fn query_for_tests(sql_state: Option<PgSqlState>) -> Self {
		let mut error = Self::for_statement(
			"test.query",
			"SELECT 1",
			sqlx::Error::Io(std::io::Error::other("test")),
		);
		if let Some(sql_state) = sql_state {
			Arc::make_mut(&mut error.0).retry =
				RetryClassification::from_sql_state_code(sql_state.as_str());
			Arc::make_mut(&mut error.0).sql_state = Some(sql_state);
		}
		error
	}

	#[cfg(test)]
	pub(crate) fn transaction_for_tests() -> Self {
		Self::for_transaction(
			"test.transaction",
			TransactionPhase::Commit,
			None,
			sqlx::Error::Io(std::io::Error::other("test")),
		)
	}

	/// The label of the Paranoid operation that was running, such as `kv.get_bytes`.
	///
	/// These are the labels that [`DatabaseOperationObserver`](super::DatabaseOperationObserver)
	/// records. A portable query that the application runs itself has the label
	/// `db.portable_query`, and the observer records it with the label of the executor method
	/// that runs it, such as `db.executor.fetch_all`.
	pub fn operation_label(&self) -> &'static str {
		self.0.operation_label
	}

	/// The statement that failed, as its template with `$n` placeholders and without values.
	/// `None` when no statement failed, such as when a connection could not be established.
	pub fn statement(&self) -> Option<&str> {
		self.0.statement.as_deref()
	}

	/// The transaction phase, when a transaction control statement failed.
	pub fn phase(&self) -> Option<TransactionPhase> {
		self.0.phase.map(FailedPhase::phase)
	}

	/// Whether a failed commit attempt could have persisted the transaction's work: Paranoid
	/// asked the driver to send the `COMMIT`, and that call failed, so the transaction may or
	/// may not have committed. The rollback paths after a fault-hook rejection, an abandoned
	/// state mutation, and a failed commit probe return false, even though the caller requested
	/// commit and `phase` reports the commit phase.
	pub fn commit_outcome_is_unknown(&self) -> bool {
		matches!(self.0.phase, Some(FailedPhase::Commit { attempted: true }))
	}

	/// The SQLSTATE that PostgreSQL reported, when it reported one.
	pub fn sql_state(&self) -> Option<&PgSqlState> {
		self.0.sql_state.as_ref()
	}

	/// The retry classification of the failure.
	pub fn retry(&self) -> RetryClassification {
		self.0.retry
	}

	/// The label of the database operation that a fault hook failed, when the fault hook of a
	/// [`DatabaseOperationObserver`](super::DatabaseOperationObserver) caused this failure. `None`
	/// for every failure that the database or the driver reported.
	///
	/// The label is the [`label`](super::DatabaseOperationRecord::label) of the record that the
	/// fault hook received, such as `kv.set_bytes` or `db.tx.commit`. It names the one database
	/// operation that Paranoid did not send, and [`operation_label`](Self::operation_label)
	/// names the Paranoid operation that was running. See
	/// [`DatabaseOperationObserver::with_operation_fault_hook`](super::DatabaseOperationObserver::with_operation_fault_hook).
	pub fn faulted_operation_label(&self) -> Option<&'static str> {
		OperationFaultError::label_in_driver_error(&self.0.driver_error)
	}

	/// An unexpected failure of the statement that aborts a fault-injected transaction.
	/// The original fault remains primary. Expected divide-by-zero and already-aborted
	/// responses return `None`. This cause differs from a rollback failure
	/// and does not establish that COMMIT was attempted.
	pub fn abort_statement_error(&self) -> Option<&DatabaseError> {
		OperationFaultError::abort_statement_error_in_driver_error(&self.0.driver_error)
	}

	/// The name of the constraint that PostgreSQL reported, when it reported one.
	pub fn constraint(&self) -> Option<&str> {
		self.0.identifiers.constraint.as_deref()
	}

	/// The name of the table that PostgreSQL reported, when it reported one.
	pub fn table(&self) -> Option<&str> {
		self.0.identifiers.table.as_deref()
	}

	/// The name of the column that PostgreSQL reported, when it reported one.
	pub fn column(&self) -> Option<&str> {
		self.0.identifiers.column.as_deref()
	}

	/// The name of the schema that PostgreSQL reported, when it reported one.
	pub fn schema(&self) -> Option<&str> {
		self.0.identifiers.schema.as_deref()
	}

	/// The name of the data type that PostgreSQL reported, when it reported one.
	pub fn data_type(&self) -> Option<&str> {
		self.0.identifiers.data_type.as_deref()
	}

	/// The error that the `sqlx` driver returned.
	///
	/// The driver's message and detail can contain the values of the rows that the statement
	/// read or wrote, and the statement text of a portable query contains its values as
	/// literals. Do not write this error to logs that are not allowed to hold such values.
	///
	/// The driver returned no error for a failure that a fault hook caused (see
	/// [`faulted_operation_label`](Self::faulted_operation_label)). For such a
	/// failure this method returns an [`sqlx::Error::Configuration`] with a message that
	/// names the fault hook and the operation.
	pub fn driver_error(&self) -> &sqlx::Error {
		&self.0.driver_error
	}
}

impl fmt::Debug for DatabaseError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("DatabaseError")
			.field("operation_label", &self.0.operation_label)
			.field("statement", &self.0.statement)
			.field("phase", &self.phase())
			.field("sql_state", &self.0.sql_state)
			.field("retry", &self.0.retry)
			.field("faulted_operation_label", &self.faulted_operation_label())
			.field("constraint", &self.constraint())
			.field("table", &self.table())
			.field("column", &self.column())
			.field("schema", &self.schema())
			.field("data_type", &self.data_type())
			.field("rollback_error", &self.rollback_error())
			.field("abort_statement_error", &self.abort_statement_error())
			.finish_non_exhaustive()
	}
}

impl fmt::Display for DatabaseError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(
			formatter,
			"Postgres operation {} failed",
			self.0.operation_label
		)?;
		if let Some(phase) = self.phase() {
			write!(formatter, " while {}", phase.description())?;
		}
		if let Some(sql_state) = &self.0.sql_state {
			write!(formatter, " (SQLSTATE {sql_state})")?;
		}
		if let Some(constraint) = self.constraint() {
			write!(formatter, " (constraint {constraint})")?;
		}
		if let Some(label) = self.faulted_operation_label() {
			write!(
				formatter,
				" (the fault hook of a database operation observer failed the operation {label}, which Paranoid did not send)"
			)?;
		}
		if let Some(error) = self.rollback_error() {
			write!(formatter, "; rollback also failed: {error}")?;
		}
		if let Some(error) = self.abort_statement_error() {
			write!(formatter, "; fault abort statement also failed: {error}")?;
		}
		Ok(())
	}
}

impl StdError for DatabaseError {}

/// Admission of a transaction under the installation's protocol failed. Every operation that
/// admits converts this into its own error type.
#[derive(Debug)]
pub(crate) enum AdmissionError {
	/// The installation's protocol state cannot admit this release.
	Protocol(ProtocolError),
	/// Reading the protocol relation failed.
	Database(DatabaseError),
}

impl From<ProtocolError> for AdmissionError {
	fn from(error: ProtocolError) -> Self {
		Self::Protocol(error)
	}
}

impl From<DatabaseError> for AdmissionError {
	fn from(error: DatabaseError) -> Self {
		Self::Database(error)
	}
}

/// The failures of an operation that owns its transaction, as the operation's error type
/// builds them. Implemented through [`impl_transaction_failure!`].
pub trait TransactionFailure: Sized {
	/// The installation's protocol state cannot admit this release.
	fn from_protocol_error(error: ProtocolError) -> Self;

	/// Beginning, committing, or rolling back the operation's own transaction failed.
	fn from_database_error(error: DatabaseError) -> Self;

	/// The operation failed, and then rolling back its own transaction also failed.
	fn from_rollback_failure(operation_error: Self, rollback_error: DatabaseError) -> Self;

	/// Whether a statement of the operation failed in the database, which leaves the
	/// operation's transaction unable to commit.
	fn involves_database_failure(&self) -> bool;

	/// Whether the operation's own `COMMIT` failed, which means that the transaction may or may
	/// not have committed.
	fn commit_outcome_is_unknown(&self) -> bool;
}

/// Implements `From<AdmissionError>` and [`TransactionFailure`] for an error enum with the
/// variants `Protocol(ProtocolError)`, `Database(DatabaseError)`, and
/// `RollbackFailed { operation_error: Box<Self>, rollback_error: DatabaseError }`. An optional
/// second argument is a pattern for a further variant that also holds a database failure, such
/// as `Self::CleanupFailed { .. }`.
macro_rules! impl_transaction_failure {
	($error:ty; cleanup $cleanup_failed:ident) => {
		$crate::db::impl_transaction_failure!(@generate $error; [] [$cleanup_failed]);
	};
	($error:ty $(, $extra_failure:pat)?) => {
		$crate::db::impl_transaction_failure!(@generate $error; [$($extra_failure)?] []);
	};
	(@generate $error:ty; [$($extra_failure:pat)?] [$($cleanup_failed:ident)?]) => {
		impl From<$crate::db::AdmissionError> for $error {
			fn from(error: $crate::db::AdmissionError) -> Self {
				match error {
					$crate::db::AdmissionError::Protocol(error) => Self::Protocol(error),
					$crate::db::AdmissionError::Database(error) => Self::Database(error),
				}
			}
		}

		impl $crate::db::TransactionFailure for $error {
			fn from_protocol_error(error: $crate::db::ProtocolError) -> Self {
				Self::Protocol(error)
			}

			fn from_database_error(error: $crate::db::DatabaseError) -> Self {
				Self::Database(error)
			}

			fn from_rollback_failure(
				operation_error: Self,
				rollback_error: $crate::db::DatabaseError,
			) -> Self {
				Self::RollbackFailed {
					operation_error: Box::new(operation_error),
					rollback_error,
				}
			}

			fn involves_database_failure(&self) -> bool {
				matches!(
					self,
					Self::Database(_) | Self::RollbackFailed { .. } $(| $extra_failure)? $(| Self::$cleanup_failed { .. })?
				)
			}

			fn commit_outcome_is_unknown(&self) -> bool {
				match self {
					Self::Database(error) => error.commit_outcome_is_unknown(),
					Self::RollbackFailed { operation_error, .. } => operation_error.commit_outcome_is_unknown(),
					$(Self::$cleanup_failed { operation_error, .. } => operation_error.commit_outcome_is_unknown(),)?
					_ => false,
				}
			}
		}
	};
}
pub(crate) use impl_transaction_failure;

/// A portable-query SQL template cannot be used.
///
/// The template is malformed, such as an unterminated string literal or a dollar-quoted
/// string, or the number of bound values does not match the template's parameters.
#[derive(Clone, Debug, thiserror::Error)]
#[error("portable_query SQL template error: {reason}")]
pub struct PortableQueryTemplateError {
	reason: String,
}

impl PortableQueryTemplateError {
	pub(crate) fn new(reason: impl Into<String>) -> Self {
		Self {
			reason: reason.into(),
		}
	}

	/// Why the template cannot be used.
	pub fn reason(&self) -> &str {
		&self.reason
	}
}

/// A value cannot be rendered as a safe Postgres literal.
#[derive(Clone, Debug, thiserror::Error)]
#[error("Postgres literal encoding failed: {reason}")]
pub struct LiteralEncodingError {
	reason: &'static str,
}

impl LiteralEncodingError {
	pub(crate) fn new(reason: &'static str) -> Self {
		Self { reason }
	}

	/// Why the value cannot be rendered.
	pub fn reason(&self) -> &'static str {
		self.reason
	}
}

/// A value cannot be bound to a portable query.
#[derive(Clone, Debug, thiserror::Error)]
pub enum PortableQueryBindError {
	/// The value bound in place of `$position` cannot be rendered as a Postgres literal.
	#[error("the value bound in place of ${position} cannot be rendered as a Postgres literal")]
	Encoding {
		/// The placeholder index that the value was bound for, starting at 1.
		position: usize,
		/// Why the value cannot be rendered.
		source: LiteralEncodingError,
	},
	/// Every parameter of the template already has a value.
	#[error(
		"portable_query SQL template takes {param_count} bind value(s), but a further `.bind()` call was supplied"
	)]
	TooManyValues {
		/// The number of parameters that the template has.
		param_count: usize,
	},
}

/// Validating a schema, or changing a component schema or the active protocol, failed.
#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
	/// The installation's protocol state cannot admit this release.
	#[error("Paranoid protocol rejected database work")]
	Protocol(#[from] ProtocolError),
	/// A database operation failed.
	#[error("database work failed")]
	Database(#[from] DatabaseError),
	/// The schema in the database does not match what Paranoid requires.
	#[error("Postgres schema mismatch: {reason}")]
	SchemaMismatch {
		/// Human-readable reason.
		reason: String,
	},
	/// The operation failed, and rolling back its transaction also failed.
	#[error("schema work failed, then transaction rollback failed")]
	RollbackFailed {
		/// The failure that required the rollback.
		#[source]
		operation_error: Box<SchemaError>,
		/// The failure of the rollback.
		rollback_error: DatabaseError,
	},
}

impl SchemaError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Database(source) => Some(source),
			Self::RollbackFailed {
				operation_error,
				rollback_error,
			} => operation_error.database_error().or(Some(rollback_error)),
			_ => None,
		}
	}
	pub(crate) fn mismatch(reason: impl Into<String>) -> Self {
		Self::SchemaMismatch {
			reason: reason.into(),
		}
	}
}

impl_transaction_failure!(SchemaError);

/// Defines an error enum for an operation that owns database work and can report its
/// failures. The macro appends the variants `Protocol`, `Database`, and `RollbackFailed` after the
/// operation's own variants, and implements `From<AdmissionError>` and [`TransactionFailure`].
macro_rules! database_work_error {
	(
		$(#[$meta:meta])*
		$(@also_database_failure [$cleanup_failed:ident])?
		pub enum $name:ident {
			$($variants:tt)*
		}
	) => {
		$(#[$meta])*
		#[derive(Debug, thiserror::Error)]
		pub enum $name {
			$($variants)*
			/// The installation's protocol state cannot admit this release.
			#[error("Paranoid protocol rejected database work")]
			Protocol(#[from] $crate::db::ProtocolError),
			/// A database operation failed.
			#[error("database work failed")]
			Database(#[from] $crate::db::DatabaseError),
			/// The operation failed, and rolling back its transaction also failed.
			#[error("database operation failed, then transaction rollback failed")]
			RollbackFailed {
				/// The failure that required the rollback.
				#[source]
				operation_error: Box<$name>,
				/// The failure of the rollback.
				rollback_error: $crate::db::DatabaseError,
			},
		}

		$crate::db::impl_transaction_failure!($name $(; cleanup $cleanup_failed)?);
	};
}
pub(crate) use database_work_error;

/// Whether a database failure in `error`'s source chain is one that repeating the attempt can
/// overcome.
pub(crate) fn is_transient_database_failure(error: &(dyn StdError + 'static)) -> bool {
	let mut current = Some(error);
	while let Some(error) = current {
		if let Some(database) = error.downcast_ref::<DatabaseError>() {
			return database.retry() != RetryClassification::NotRetryable;
		}
		current = error.source();
	}
	false
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn sqlstate_codes_keep_their_retry_classification() {
		for (code, expected) in [
			("23505", RetryClassification::NotRetryable),
			("23503", RetryClassification::NotRetryable),
			("23514", RetryClassification::NotRetryable),
			("23502", RetryClassification::NotRetryable),
			("40001", RetryClassification::SerializationFailure),
			("40P01", RetryClassification::Deadlock),
			("55P03", RetryClassification::LockNotAvailable),
			("57014", RetryClassification::QueryCanceled),
			("57P01", RetryClassification::ServerUnavailable),
			("08006", RetryClassification::ConnectionFailure),
			("53300", RetryClassification::InsufficientResources),
			("58030", RetryClassification::SystemError),
			("ZZ999", RetryClassification::NotRetryable),
		] {
			assert_eq!(
				RetryClassification::from_sql_state_code(code),
				expected,
				"SQLSTATE {code}"
			);
		}
	}

	#[test]
	fn failures_without_a_sqlstate_are_classified_by_their_kind() {
		assert_eq!(
			RetryClassification::from_sqlx_error(&sqlx::Error::PoolTimedOut),
			RetryClassification::ConnectionFailure
		);
		assert_eq!(
			RetryClassification::from_sqlx_error(&sqlx::Error::RowNotFound),
			RetryClassification::NotRetryable
		);
	}
}
