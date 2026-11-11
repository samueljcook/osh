//! Runs Paranoid's work in a transaction that a handle supplies or that a pool begins, and ends
//! a transaction that a pool began. The registered runners also retain local work ownership
//! through acquisition, `BEGIN`, and ending.

use super::pool::OwnedTransactionRegistration;
use super::{Database, DatabaseError, TransactionFailure, Tx, WriteDatabase, WritePool, WriteTx};

/// Runs `work_fn` in a write transaction that the handle supplies or joins. The error type
/// builds its own failures for the transaction control statements.
pub(crate) async fn run_transaction_write<R, E: TransactionFailure>(
	database: impl WriteDatabase,
	operation_label: &'static str,
	work_fn: impl AsyncFnOnce(&mut WriteTx<'_>) -> Result<R, E>,
) -> Result<R, E> {
	database
		.run_write(
			operation_label,
			E::from_database_error,
			|_operation_label, error, rollback_error| {
				E::from_rollback_failure(error, rollback_error)
			},
			work_fn,
		)
		.await
}

/// The read counterpart of [`run_transaction_write`].
pub(crate) async fn run_transaction_read<R, E: TransactionFailure>(
	database: impl Database,
	operation_label: &'static str,
	work_fn: impl AsyncFnOnce(&mut Tx<'_>) -> Result<R, E>,
) -> Result<R, E> {
	database
		.run_read(
			operation_label,
			E::from_database_error,
			|_operation_label, error, rollback_error| {
				E::from_rollback_failure(error, rollback_error)
			},
			work_fn,
		)
		.await
}

/// Finishes the pool-owned transaction of an operation that failed with `error`: rolls the
/// transaction back and returns the failure that the operation reports.
///
/// When Paranoid interrupted one of the transaction's statements, the rollback discards the
/// connection instead of sending `ROLLBACK` because the connection's state is unknown, and
/// PostgreSQL ends the transaction when it notices that the connection closed. That discard is
/// the expected end of the interrupted operation, not a failure, so the operation reports its
/// own failure. Any other failed rollback is reported with `build_rollback_error`.
async fn finish_pool_owned_failed_transaction<E>(
	operation_label: &'static str,
	tx: Tx<'_>,
	error: E,
	build_rollback_error: impl FnOnce(&'static str, E, DatabaseError) -> E,
) -> E {
	let statement_was_interrupted = tx.statement_was_interrupted();
	match tx.rollback().await {
		Ok(()) => error,
		Err(_) if statement_was_interrupted => error,
		Err(rollback_error) => build_rollback_error(
			operation_label,
			error,
			rollback_error.with_operation_label(operation_label),
		),
	}
}

pub(crate) async fn finish_pool_owned_write_transaction_and_preserve_rollback_error<T, E>(
	operation_label: &'static str,
	tx: WriteTx<'_>,
	result: Result<T, E>,
	build_database_error: impl FnOnce(DatabaseError) -> E,
	build_rollback_error: impl FnOnce(&'static str, E, DatabaseError) -> E,
) -> Result<T, E> {
	match result {
		Ok(value) => {
			tx.commit().await.map_err(|error| {
				build_database_error(error.with_operation_label(operation_label))
			})?;
			Ok(value)
		}
		Err(error) => Err(finish_pool_owned_failed_transaction(
			operation_label,
			tx.tx,
			error,
			build_rollback_error,
		)
		.await),
	}
}

pub(super) async fn finish_pool_owned_rollback_only_transaction_and_preserve_rollback_error<
	T,
	E,
>(
	operation_label: &'static str,
	tx: Tx<'_>,
	result: Result<T, E>,
	build_database_error: impl FnOnce(DatabaseError) -> E,
	build_rollback_error: impl FnOnce(&'static str, E, DatabaseError) -> E,
) -> Result<T, E> {
	match result {
		Ok(value) => {
			tx.rollback().await.map_err(|error| {
				build_database_error(error.with_operation_label(operation_label))
			})?;
			Ok(value)
		}
		Err(error) => Err(finish_pool_owned_failed_transaction(
			operation_label,
			tx,
			error,
			build_rollback_error,
		)
		.await),
	}
}

/// The caller publishes the registration's actual receipt before awaiting this runner.
/// Its callback receives the same transaction but supplies no lifetime registration.
pub(crate) async fn run_registered_transaction_write<R, E: TransactionFailure>(
	pool: &WritePool,
	operation_label: &'static str,
	registration: OwnedTransactionRegistration,
	work_fn: impl AsyncFnOnce(&mut WriteTx<'_>) -> Result<R, E>,
) -> Result<R, E> {
	let mut tx = pool
		.begin_registered_transaction(registration, operation_label)
		.await
		.map_err(|error| E::from_database_error(error.with_operation_label(operation_label)))?;
	let result = work_fn(&mut tx).await;
	finish_pool_owned_write_transaction_and_preserve_rollback_error(
		operation_label,
		tx,
		result,
		E::from_database_error,
		|_operation, error, rollback_error| E::from_rollback_failure(error, rollback_error),
	)
	.await
}

/// Executes a fixed READ ONLY observation and publishes it only after known rollback.
/// Its registration and receipt cover acquisition, BEGIN and the actual ending.
pub(crate) async fn run_registered_transaction_read<R, E: TransactionFailure>(
	pool: &WritePool,
	operation_label: &'static str,
	registration: OwnedTransactionRegistration,
	work_fn: impl AsyncFnOnce(&mut Tx<'_>) -> Result<R, E>,
) -> Result<R, E> {
	let mut tx = pool
		.begin_registered_read_transaction(registration, operation_label)
		.await
		.map_err(|error| E::from_database_error(error.with_operation_label(operation_label)))?;
	let result = work_fn(&mut tx).await;
	finish_pool_owned_rollback_only_transaction_and_preserve_rollback_error(
		operation_label,
		tx,
		result,
		E::from_database_error,
		|_operation, error, rollback_error| E::from_rollback_failure(error, rollback_error),
	)
	.await
}
