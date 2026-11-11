use std::sync::Arc;

/// Opening or configuring a SQLite connection failed.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
	/// An empty path or `:memory:` would select a nonpersistent database.
	#[error("SQLite database path must name a persistent file")]
	NonPersistentPath,
	/// The filesystem path could not be resolved against the process's current dir.
	#[error("could not resolve SQLite database path: {0}")]
	Path(#[source] std::io::Error),
	/// SQLite's timeout parameter cannot represent the supplied duration.
	#[error("SQLite busy timeout exceeds its signed 32-bit millisecond range")]
	BusyTimeoutTooLong,
	/// The requested auto-vacuum mode cannot be applied without rebuilding the database.
	/// Opening never performs that rewrite. The persisted mode is unchanged.
	#[error(
		"changing SQLite auto-vacuum mode from {actual:?} to {requested:?} requires an explicit database rewrite"
	)]
	AutoVacuumChangeRequiresRewrite {
		/// The application's requested mode.
		requested: super::maintenance::AutoVacuumMode,
		/// The database's persisted mode.
		actual: super::maintenance::AutoVacuumMode,
	},
	/// SQLite rejected the open or configuration operation.
	#[error("SQLite connection setup failed: {0}")]
	Sqlite(#[from] rusqlite::Error),
	/// SQLite did not apply a required connection setting.
	#[error("SQLite did not apply the requested {setting} setting")]
	SettingRejected {
		/// The setting that did not match the requested value.
		setting: &'static str,
	},
}

/// A transaction operation failed. Later operations return the first failure
/// without executing more SQL, and the transaction cannot commit.
#[derive(Clone, Debug, thiserror::Error)]
pub enum OperationError {
	/// SQLite rejected a statement or row decoding failed.
	#[error("SQLite operation failed: {0}")]
	Sqlite(#[source] Arc<rusqlite::Error>),
	/// SQLite ended the transaction before the library requested completion.
	#[error("SQLite transaction ended before its owned completion")]
	TransactionEnded,
	/// An operation did not finish, for example because parameter conversion or
	/// row decoding panicked. Catching that panic does not permit commit.
	#[error("SQLite operation did not finish")]
	OperationInterrupted,
	/// A composed operation returned an error. Its caller received the original
	/// typed error; this marker prevents commit after that error was ignored.
	#[error("a composed SQLite operation failed")]
	ComposedOperationFailed,
	/// Early-stop row visitation requires a statement that SQLite classifies as read-only.
	#[error("SQLite row visitation requires a read-only statement")]
	ReadOnlyStatementRequired,
	/// A query returned more rows than the caller permitted.
	#[error("SQLite query exceeds its row limit of {max_rows}")]
	RowLimitExceeded {
		/// The caller's maximum row count.
		max_rows: usize,
	},
	/// The query result could not reserve memory for another row.
	#[error("SQLite query result allocation failed")]
	AllocationFailed,
}

impl From<rusqlite::Error> for OperationError {
	fn from(error: rusqlite::Error) -> Self {
		Self::Sqlite(Arc::new(error))
	}
}

/// Why an owned transaction could not begin.
#[derive(Debug, thiserror::Error)]
pub enum BeginError {
	/// A write transaction was requested on a connection opened read-only.
	#[error("SQLite connection is read-only")]
	ReadOnlyConnection,
	/// An earlier failure required discarding this connection.
	#[error("SQLite connection was discarded after an earlier failure")]
	ConnectionDiscarded,
	/// The connection already held a transaction outside library ownership.
	#[error("SQLite connection has an unexpected active transaction")]
	UnexpectedTransaction,
	/// SQLite rejected transaction setup or BEGIN.
	#[error("SQLite transaction could not begin: {0}")]
	Sqlite(#[from] rusqlite::Error),
}

/// What the library established about a failed transaction's writes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureOutcome {
	/// The transaction did not begin, and its callback did not run.
	NotStarted,
	/// The transaction's writes were rolled back.
	RolledBack,
	/// The library could not establish whether the transaction committed.
	/// This outcome must not be interpreted as a rollback or permission to retry.
	Unknown,
}

/// Transaction cleanup information, separate from the primary failure.
#[derive(Debug)]
pub struct FailureCompletion {
	/// The established outcome of the transaction's writes.
	pub outcome: FailureOutcome,
	/// A cleanup error, if one occurred. The connection is discarded on this error.
	pub cleanup_error: Option<Box<rusqlite::Error>>,
}

/// A transaction failed before its callback, in its callback, or during commit.
/// Every variant retains completion information without replacing the primary error.
#[derive(Debug, thiserror::Error)]
pub enum TransactionError<E> {
	/// The callback did not run.
	#[error("could not begin SQLite transaction: {source}")]
	Begin {
		/// The setup or BEGIN failure.
		#[source]
		source: BeginError,
		/// The established outcome and any separate cleanup failure.
		completion: FailureCompletion,
	},
	/// The callback returned an error.
	#[error("SQLite transaction callback failed: {source}")]
	Callback {
		/// The original application error.
		#[source]
		source: E,
		/// The first failed operation, including when the callback ignored its error.
		operation_error: Option<OperationError>,
		/// The established outcome and any separate cleanup failure.
		completion: FailureCompletion,
	},
	/// The callback returned success after a failed operation.
	#[error("SQLite transaction cannot commit after an operation failed: {source}")]
	Operation {
		/// The first operation failure.
		#[source]
		source: OperationError,
		/// The established outcome and any separate cleanup failure.
		completion: FailureCompletion,
	},
	/// SQLite rejected COMMIT. The completion information distinguishes a
	/// confirmed rollback from an uncertain commit outcome.
	#[error("SQLite transaction commit failed: {source}")]
	Commit {
		/// The original commit failure.
		#[source]
		source: rusqlite::Error,
		/// The established outcome and any separate cleanup failure.
		completion: FailureCompletion,
	},
}

impl<E> TransactionError<E> {
	/// Returns the established transaction outcome and any cleanup error.
	pub fn completion(&self) -> &FailureCompletion {
		match self {
			Self::Begin { completion, .. }
			| Self::Callback { completion, .. }
			| Self::Operation { completion, .. }
			| Self::Commit { completion, .. } => completion,
		}
	}
}
