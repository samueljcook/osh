use super::{DatabaseError, RetryClassification};
use std::error::Error as StdError;
use std::fmt;
use std::sync::{Arc, Mutex};

/// Kind of database operation observed by a [`DatabaseOperationObserver`].
///
/// This instrumentation lets Paranoid's tests and an application's tests assert exact
/// minimum-query behavior in Postgres-backed test suites.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatabaseOperationKind {
	/// A transaction was begun.
	BeginTransaction,
	/// A transaction was committed.
	CommitTransaction,
	/// A transaction was rolled back.
	RollbackTransaction,
	/// A statement was executed for its side effect.
	Execute,
	/// A query fetched all matching rows.
	FetchAll,
	/// A query fetched exactly one row.
	FetchOne,
	/// A query fetched at most one row.
	FetchOptional,
	/// Statements streamed their affected-row counts.
	ExecuteMany,
	/// A query streamed its rows.
	Fetch,
	/// Statements streamed their rows and affected-row counts.
	FetchMany,
	/// A statement was explicitly prepared with optional parameter type information.
	Prepare,
	/// A statement's parameter and result types were described.
	Describe,
}

/// One observed database operation, as recorded by a [`DatabaseOperationObserver`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DatabaseOperationRecord {
	/// The kind of operation observed.
	pub kind: DatabaseOperationKind,
	/// The call-site label attached to the operation.
	pub label: &'static str,
	/// The rendered SQL statement, when the operation carried one.
	pub statement: Option<String>,
	/// The label of the Paranoid operation that began the operation's transaction, such as
	/// `fleet.native_lease.owned.renew` for the renewal of a lease. It is `None` in a
	/// transaction that the application began.
	pub transaction_label: Option<&'static str>,
}

/// The failure that a fault hook injects in place of one database operation, which Paranoid
/// then does not send. See
/// [`DatabaseOperationObserver::with_operation_fault_hook`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatabaseOperationFault {
	/// The [`retry`](super::DatabaseError::retry) classification of the operation's
	/// [`DatabaseError`](super::DatabaseError). [`NotRetryable`](RetryClassification::NotRetryable)
	/// makes the failure one that Paranoid's own retries do not repeat, and another
	/// classification, such as [`ConnectionFailure`](RetryClassification::ConnectionFailure),
	/// makes it one that they repeat as they repeat a failure in the database with that
	/// classification, for example a lease renewal within the lease's
	/// `max_consecutive_renewal_failures`.
	pub retry: RetryClassification,
}

type BeforeDatabaseOperationHook = Arc<dyn Fn(DatabaseOperationRecord) + Send + Sync + 'static>;
type DatabaseOperationFaultHook =
	Arc<dyn Fn(&DatabaseOperationRecord) -> Option<DatabaseOperationFault> + Send + Sync + 'static>;

/// Records database operations performed through a cloned pool or transaction
/// for minimum-query assertions in Postgres-backed test suites.
///
/// Attach one to a pool via `clone_with_database_operation_observer`, then
/// inspect the accumulated `records()` after driving the operation under test.
/// Application SQL uses labels such as `db.executor.fetch_all`, according to the
/// executor method. Observation starts when the future or stream is first polled;
/// dropping an unpolled operation records nothing. One executor call produces one
/// record, including a call that submits multiple SQL statements. Internal SQLx
/// protocol messages, such as implicit preparation, are not separate records.
/// Explicit preparation and description calls each produce their own record.
/// The kind identifies the executor method: SQLx typed queries can implement
/// `fetch_all` by collecting `fetch`, which produces a `Fetch` record.
/// Bound arguments remain parameters in SQLx query text; portable query records
/// contain rendered literals. Treat collected SQL as potentially sensitive data.
///
/// # Failing an operation in a test
///
/// An application never names or changes a Paranoid table, so a test must not make an
/// operation fail by changing one. Instead, a test makes chosen operations fail with a fault
/// hook, which [`with_operation_fault_hook`](Self::with_operation_fault_hook) adds. The fault
/// hook receives the record of each operation before Paranoid sends the operation, and it
/// returns a [`DatabaseOperationFault`] for an operation that must fail and `None` for the
/// others. Paranoid does not send an operation that must fail. Paranoid operations
/// and portable queries return a [`DatabaseError`](super::DatabaseError) with a
/// [`faulted_operation_label`](super::DatabaseError::faulted_operation_label)
/// that identifies the operation. SQLx query methods return `sqlx::Error` with the
/// injected fault as their source. The fault hook is for tests; Paranoid never
/// injects a failure on its own.
///
/// The labels of a Paranoid operation's database operations are the labels that `records()`
/// returns for it. This example fails the statement that stores a Raw value:
///
/// ```rust,no_run
/// # async fn example(
/// #     runtime: &paranoid::runtime::PgRuntime,
/// #     pool: paranoid::db::WritePool,
/// # ) -> Result<(), Box<dyn std::error::Error>> {
/// use paranoid::db::{DatabaseOperationFault, DatabaseOperationObserver, RetryClassification};
/// use paranoid::kv::{Key, Ttl};
/// use paranoid::kv::RecordKey;
///
/// let observer = DatabaseOperationObserver::default().with_operation_fault_hook(|operation| {
///     (operation.label == "db.state_storage.write_point").then_some(DatabaseOperationFault {
///         retry: RetryClassification::NotRetryable,
///     })
/// });
/// let failing_pool = pool.clone_with_database_operation_observer(observer);
///
/// let key = RecordKey::from(&Key::from_parts(["example"])?);
/// let failure = runtime
///     .core_raw()
///     .set_bytes(&failing_pool, &key, b"value".to_vec(), Ttl::NO_EXPIRATION)
///     .await;
/// let Err(error) = failure else {
///     panic!("the fault hook fails the write");
/// };
/// let database_error = error.database_error().expect("the write failed in the database");
/// assert_eq!(
///     database_error.faulted_operation_label(),
///     Some("db.state_storage.write_point")
/// );
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Default)]
pub struct DatabaseOperationObserver {
	records: Arc<Mutex<Vec<DatabaseOperationRecord>>>,
	before_operation_hook: Option<BeforeDatabaseOperationHook>,
	operation_fault_hook: Option<DatabaseOperationFaultHook>,
	holder: ObserverHolder,
}

/// What holds one copy of an observer.
#[derive(Clone, Default)]
enum ObserverHolder {
	/// A pool, which returns an injected fault to the caller of the failed operation.
	#[default]
	Pool,
	/// One transaction, which fails the statement that it runs after the fault hook failed
	/// that statement's record.
	Transaction {
		/// The label of the Paranoid operation that began the transaction.
		label: Option<&'static str>,
		/// The fault hook's decision for the statement that the transaction recorded last.
		statement_fault: PendingStatementFault,
	},
}

impl fmt::Debug for DatabaseOperationObserver {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("DatabaseOperationObserver")
			.field("records", &self.records)
			.field(
				"has_before_operation_hook",
				&self.before_operation_hook.is_some(),
			)
			.field(
				"has_operation_fault_hook",
				&self.operation_fault_hook.is_some(),
			)
			.finish()
	}
}

impl DatabaseOperationObserver {
	/// Returns this observer with `hook` as its before-operation hook, which Paranoid invokes
	/// synchronously as each operation is recorded. The returned observer keeps the records and
	/// the fault hook of this observer, and `hook` replaces an earlier before-operation hook.
	///
	/// This lets a test inject a side effect (such as a concurrent racing write) at an
	/// exact point in a multi-step database operation.
	pub fn with_before_operation_hook(
		self,
		hook: impl Fn(DatabaseOperationRecord) + Send + Sync + 'static,
	) -> Self {
		Self {
			before_operation_hook: Some(Arc::new(hook)),
			..self
		}
	}

	/// Returns this observer with `hook` as its fault hook, which makes chosen operations fail
	/// in a test. The returned observer keeps the records and the before-operation hook of
	/// this observer, and `hook` replaces an earlier fault hook.
	///
	/// Paranoid records each operation, then invokes the before-operation hook, and then
	/// invokes `hook` synchronously with the record. When `hook` returns a
	/// [`DatabaseOperationFault`], Paranoid does not send the operation to the database.
	/// Paranoid operations and portable queries return a [`DatabaseError`](super::DatabaseError)
	/// that has no SQLSTATE and has the fault's [`retry`](DatabaseOperationFault::retry)
	/// classification. SQLx query methods return `sqlx::Error` with the injected fault as
	/// their source.
	/// [`faulted_operation_label`](super::DatabaseError::faulted_operation_label)
	/// returns the operation's label for that error. The failure takes the path that a failure
	/// of the same operation in the database takes, so the Paranoid operation that was running
	/// fails and cleans up as it does after a failure in the database. A failed operation
	/// stays in `records()`, so a test can read what Paranoid attempted.
	///
	/// The state after a failed operation depends on the operation's kind:
	///
	/// - [`BeginTransaction`](DatabaseOperationKind::BeginTransaction): Paranoid acquires no
	///   connection, and no transaction exists.
	/// - Query, execution, preparation, and description operations: inside a transaction,
	///   Paranoid sends a statement that PostgreSQL rejects in place of the requested
	///   operation. PostgreSQL rejects later statements, and
	///   [`commit`](super::Tx::commit) rolls back and returns an error, unless the caller
	///   first rolls back to a savepoint that preceded the failure. A Paranoid operation
	///   that owns its transaction rolls that transaction back. Application SQL directly
	///   on a pool fails before acquiring a connection and sends no statement.
	/// - [`CommitTransaction`](DatabaseOperationKind::CommitTransaction): Paranoid sends a
	///   `ROLLBACK` in place of the `COMMIT`, so the transaction's work is not stored. The
	///   failure has the [`Commit`](super::TransactionPhase::Commit) phase, as the failure of
	///   every commit has.
	/// - [`RollbackTransaction`](DatabaseOperationKind::RollbackTransaction): Paranoid sends no
	///   `ROLLBACK` and returns the transaction's connection to the pool, as it does after a
	///   rollback that fails in the database. The driver rolls back what is left of the
	///   transaction before the connection serves another operation. The call that failed
	///   does not wait for that rollback, so the transaction can still hold its locks for a
	///   short time after the call returns.
	///
	/// Paranoid does not record the injected statement that PostgreSQL rejects, and it invokes
	/// no hook for that statement. Implicit rollback attempts are recorded and use the rollback
	/// hook, including rollback after a rejected commit or failed `db.tx.commit_probe`. A
	/// rollback hook rejection sends no rollback and leaves server completion unconfirmed.
	///
	/// `hook` can choose operations by [`DatabaseOperationRecord::label`], by
	/// [`DatabaseOperationRecord::kind`], and by [`DatabaseOperationRecord::transaction_label`]. For
	/// example, a hook that returns a fault when
	/// `operation.transaction_label == Some("fleet.native_lease.owned.renew")` fails the
	/// renewals of the leases that use the pool, and no other operation, while the runtime keeps
	/// renewing its own participation. With the classification `NotRetryable`, a lease ends at
	/// its first failed renewal; with `ConnectionFailure`, the lease retries the renewal and ends
	/// only after more consecutive failed renewals than its `max_consecutive_renewal_failures`
	/// allows. To fail only one of several operations with the same labels, count the matching
	/// operations in `hook`.
	pub fn with_operation_fault_hook(
		self,
		hook: impl Fn(&DatabaseOperationRecord) -> Option<DatabaseOperationFault>
		+ Send
		+ Sync
		+ 'static,
	) -> Self {
		Self {
			operation_fault_hook: Some(Arc::new(hook)),
			..self
		}
	}

	/// Returns the copy of this observer that one transaction holds, together with that
	/// transaction's pending statement fault. `transaction_label` is the label of the Paranoid
	/// operation that begins the transaction.
	pub(crate) fn for_transaction(
		&self,
		transaction_label: Option<&'static str>,
	) -> (Self, PendingStatementFault) {
		let statement_fault = PendingStatementFault::default();
		let observer = Self {
			holder: ObserverHolder::Transaction {
				label: transaction_label,
				statement_fault: statement_fault.clone(),
			},
			..self.clone()
		};
		(observer, statement_fault)
	}

	/// Records an operation, invokes the before-operation hook, and returns the fault when the
	/// fault hook fails the operation.
	fn observe(
		&self,
		kind: DatabaseOperationKind,
		label: &'static str,
		statement: Option<&str>,
	) -> Option<OperationFaultError> {
		let transaction_label = match &self.holder {
			ObserverHolder::Pool => None,
			ObserverHolder::Transaction { label, .. } => *label,
		};
		let record = DatabaseOperationRecord {
			kind,
			label,
			statement: statement.map(ToOwned::to_owned),
			transaction_label,
		};
		self.records
			.lock()
			.expect("database operation observer lock poisoned")
			.push(record.clone());
		let Some(operation_fault_hook) = &self.operation_fault_hook else {
			if let Some(before_operation_hook) = &self.before_operation_hook {
				before_operation_hook(record);
			}
			return None;
		};
		if let Some(before_operation_hook) = &self.before_operation_hook {
			before_operation_hook(record.clone());
		}
		operation_fault_hook(&record).map(|fault| OperationFaultError {
			label,
			retry: fault.retry,
			abort_statement_error: None,
		})
	}

	/// Records a statement of a transaction. When the fault hook fails the statement, the
	/// transaction's connection fails the next statement that it runs, which is this
	/// statement because a call site records a statement directly before it runs the
	/// statement on the same transaction.
	pub(crate) fn record_statement(
		&self,
		kind: DatabaseOperationKind,
		label: &'static str,
		statement: Option<&str>,
	) {
		let ObserverHolder::Transaction {
			statement_fault, ..
		} = &self.holder
		else {
			unreachable!("only a transaction's copy of an observer records statements");
		};
		statement_fault.replace(self.observe(kind, label, statement));
	}

	/// Records an operation and returns an injected fault directly to its caller.
	/// Transaction statements use `record_statement` to fail their database transaction.
	pub(crate) fn record_operation(
		&self,
		kind: DatabaseOperationKind,
		label: &'static str,
		statement: Option<&str>,
	) -> Result<(), sqlx::Error> {
		match self.observe(kind, label, statement) {
			Some(fault) => Err(fault.into_driver_error()),
			None => Ok(()),
		}
	}

	/// Returns every operation recorded so far, in recorded order.
	pub fn records(&self) -> Vec<DatabaseOperationRecord> {
		self.records
			.lock()
			.expect("database operation observer lock poisoned")
			.clone()
	}

	/// Discards every operation recorded so far.
	pub fn clear(&self) {
		self.records
			.lock()
			.expect("database operation observer lock poisoned")
			.clear();
	}
}

/// The failure of an operation that an observer's fault hook failed.
///
/// It travels as the source of an `sqlx::Error`, in the place where the driver's error of the
/// operation would be, which means that the failure takes the path of a failure in the database.
/// [`DatabaseError`](super::DatabaseError) recognizes it there.
#[derive(Debug)]
pub(crate) struct OperationFaultError {
	label: &'static str,
	retry: RetryClassification,
	abort_statement_error: Option<DatabaseError>,
}

impl OperationFaultError {
	pub(crate) fn with_abort_statement_error(mut self, error: DatabaseError) -> Self {
		self.abort_statement_error = Some(error);
		self
	}

	fn in_driver_error(driver_error: &sqlx::Error) -> Option<&Self> {
		match driver_error {
			sqlx::Error::Configuration(source) => source.downcast_ref::<Self>(),
			_ => None,
		}
	}

	pub(crate) fn abort_statement_error_in_driver_error(
		driver_error: &sqlx::Error,
	) -> Option<&DatabaseError> {
		Self::in_driver_error(driver_error)?
			.abort_statement_error
			.as_ref()
	}

	pub(crate) fn into_driver_error(self) -> sqlx::Error {
		sqlx::Error::Configuration(Box::new(self))
	}

	/// The label of the operation that a fault hook failed, when `driver_error` is such a
	/// failure.
	pub(crate) fn label_in_driver_error(driver_error: &sqlx::Error) -> Option<&'static str> {
		Self::in_driver_error(driver_error).map(|fault| fault.label)
	}

	/// The retry classification that the fault hook chose, when `driver_error` is a failure
	/// that a fault hook injected.
	pub(crate) fn retry_in_driver_error(driver_error: &sqlx::Error) -> Option<RetryClassification> {
		Self::in_driver_error(driver_error).map(|fault| fault.retry)
	}
}

impl fmt::Display for OperationFaultError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(
			formatter,
			"the fault hook of a database operation observer failed the operation {}, which Paranoid did not send",
			self.label
		)
	}
}

impl StdError for OperationFaultError {}

/// The fault hook's decision for the statement that one transaction recorded last, until the
/// transaction's connection runs that statement.
#[derive(Clone, Debug, Default)]
pub(crate) struct PendingStatementFault(Arc<Mutex<StatementFaultState>>);

#[derive(Debug, Default)]
enum StatementFaultState {
	#[default]
	Clear,
	FailNextStatement(OperationFaultError),
}

impl PendingStatementFault {
	fn replace(&self, fault: Option<OperationFaultError>) {
		*self
			.0
			.lock()
			.expect("no code panics while it holds the pending statement fault's lock") = match fault {
			Some(fault) => StatementFaultState::FailNextStatement(fault),
			None => StatementFaultState::Clear,
		};
	}

	/// Takes the fault of the statement that the transaction is about to run.
	pub(crate) fn take(&self) -> Option<OperationFaultError> {
		let mut state = self
			.0
			.lock()
			.expect("no code panics while it holds the pending statement fault's lock");
		match std::mem::replace(&mut *state, StatementFaultState::Clear) {
			StatementFaultState::Clear => None,
			StatementFaultState::FailNextStatement(fault) => Some(fault),
		}
	}
}

pub(crate) fn record_database_operation(
	observer: Option<&DatabaseOperationObserver>,
	kind: DatabaseOperationKind,
	label: &'static str,
	statement: Option<&str>,
) {
	if let Some(observer) = observer {
		observer.record_statement(kind, label, statement);
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::sync::atomic::{AtomicUsize, Ordering};

	#[test]
	fn a_record_is_visible_to_the_hook_that_runs_for_it() {
		let observer = DatabaseOperationObserver::default();
		let hook_observer = observer.clone();
		let visible_in_hook = Arc::new(AtomicUsize::new(0));
		let hook_count = Arc::clone(&visible_in_hook);
		let observer = DatabaseOperationObserver {
			before_operation_hook: Some(Arc::new(move |_record| {
				hook_count.store(hook_observer.records().len(), Ordering::SeqCst);
			})),
			..observer
		};
		let (observer, _) = observer.for_transaction(None);
		// The hook reads the observer that it was cloned from, which shares the record buffer.
		observer.record_statement(
			DatabaseOperationKind::Execute,
			"queue.enqueue",
			Some("SELECT 1"),
		);

		assert_eq!(visible_in_hook.load(Ordering::SeqCst), 1);
		assert_eq!(
			observer.records(),
			vec![DatabaseOperationRecord {
				kind: DatabaseOperationKind::Execute,
				label: "queue.enqueue",
				statement: Some("SELECT 1".to_owned()),
				transaction_label: None,
			}]
		);
	}

	#[test]
	fn a_transaction_s_records_carry_the_label_of_the_operation_that_began_it() {
		let observer =
			DatabaseOperationObserver::default().with_operation_fault_hook(|operation| {
				(operation.transaction_label == Some("lease.renew")).then_some(
					DatabaseOperationFault {
						retry: RetryClassification::NotRetryable,
					},
				)
			});
		let (renewal, _) = observer.for_transaction(Some("lease.renew"));
		let (application, _) = observer.for_transaction(None);
		assert!(
			renewal
				.record_operation(
					DatabaseOperationKind::BeginTransaction,
					"db.begin_transaction",
					Some("BEGIN"),
				)
				.is_err()
		);
		application
			.record_operation(
				DatabaseOperationKind::BeginTransaction,
				"db.begin_transaction",
				Some("BEGIN"),
			)
			.expect("the hook fails only the renewal's operations");
		let transactions = observer
			.records()
			.iter()
			.map(|record| record.transaction_label)
			.collect::<Vec<_>>();
		assert_eq!(transactions, vec![Some("lease.renew"), None]);
	}

	#[test]
	fn an_injected_failure_has_the_fault_s_retry_classification() {
		let observer = DatabaseOperationObserver::default().with_operation_fault_hook(|_| {
			Some(DatabaseOperationFault {
				retry: RetryClassification::ConnectionFailure,
			})
		});
		let (transaction, _) = observer.for_transaction(None);
		let driver_error = transaction
			.record_operation(
				DatabaseOperationKind::BeginTransaction,
				"db.begin_transaction",
				Some("BEGIN"),
			)
			.expect_err("the hook fails the begin");
		let error = DatabaseError::for_connection("test.begin", driver_error);
		assert_eq!(error.retry(), RetryClassification::ConnectionFailure);
		assert_eq!(error.sql_state(), None);
		assert_eq!(
			error.faulted_operation_label(),
			Some("db.begin_transaction")
		);
	}

	#[test]
	fn clearing_discards_the_records() {
		let observer = DatabaseOperationObserver::default();
		observer
			.record_operation(
				DatabaseOperationKind::BeginTransaction,
				"db.begin_transaction",
				Some("BEGIN"),
			)
			.expect("an observer without a fault hook fails no operation");
		assert_eq!(observer.records().len(), 1);
		observer.clear();
		assert!(observer.records().is_empty());
	}

	#[test]
	fn an_operation_without_an_observer_records_nothing() {
		record_database_operation(None, DatabaseOperationKind::Execute, "kv.set", None);
	}
}
