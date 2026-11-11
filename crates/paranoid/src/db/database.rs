//! The handles that an operation runs against.

use super::{
	DatabaseError, Pool, TransactionFailure, Tx, WritePool, WriteTx,
	finish_pool_owned_rollback_only_transaction_and_preserve_rollback_error,
	finish_pool_owned_write_transaction_and_preserve_rollback_error,
};

/// A handle that a read operation runs against: [`&Pool`](Pool), [`&WritePool`](WritePool), [`&mut
/// Tx`](Tx), or [`&mut WriteTx`](WriteTx).
///
/// With a pool, the operation runs in its own `READ ONLY` transaction, which it ends with a
/// rollback because it writes nothing; PostgreSQL rejects a write there. With a transaction, the
/// operation joins it, and the caller ends the transaction.
///
/// Applications pass these handles to Paranoid's operations and cannot implement this trait.
pub trait Database: sealed::Database {}

/// A handle that a write operation runs against: [`&WritePool`](WritePool) or
/// [`&mut WriteTx`](WriteTx).
///
/// With a pool, the operation runs in its own transaction, which it commits when the operation
/// succeeds and rolls back when it fails. With a transaction, the operation joins it, and the
/// caller commits or rolls back the transaction.
///
/// Applications pass these handles to Paranoid's operations and cannot implement this trait.
pub trait WriteDatabase: sealed::WriteDatabase {}

impl Database for &Pool {}
impl Database for &WritePool {}
impl Database for &mut Tx<'_> {}
impl Database for &mut WriteTx<'_> {}
impl WriteDatabase for &WritePool {}
impl WriteDatabase for &mut WriteTx<'_> {}

pub(crate) mod sealed {
	use super::{DatabaseError, TransactionFailure, Tx, WriteTx};

	/// Runs `work_fn` against a handle. When the handle is a pool, `work_fn` runs in a transaction
	/// that this method begins and ends: `build_database_error` converts a failure to begin,
	/// commit, or roll back, and `build_rollback_error` describes a failure of `work_fn` followed
	/// by a failure to roll back. When the handle is a transaction, `work_fn` joins it, and both
	/// builders go unused.
	pub trait Database: Sized + Send {
		fn run_read<R, E, W>(
			self,
			operation_label: &'static str,
			build_database_error: fn(DatabaseError) -> E,
			build_rollback_error: fn(&'static str, E, DatabaseError) -> E,
			work_fn: W,
		) -> impl Future<Output = Result<R, E>>
		where
			E: TransactionFailure,
			W: AsyncFnOnce(&mut Tx<'_>) -> Result<R, E>;
	}

	/// The write counterpart of [`Database`].
	pub trait WriteDatabase: Sized + Send {
		fn run_write<R, E, W>(
			self,
			operation_label: &'static str,
			build_database_error: fn(DatabaseError) -> E,
			build_rollback_error: fn(&'static str, E, DatabaseError) -> E,
			work_fn: W,
		) -> impl Future<Output = Result<R, E>>
		where
			E: TransactionFailure,
			W: AsyncFnOnce(&mut WriteTx<'_>) -> Result<R, E>;
	}
}

impl sealed::Database for &Pool {
	async fn run_read<R, E, W>(
		self,
		operation_label: &'static str,
		build_database_error: fn(DatabaseError) -> E,
		build_rollback_error: fn(&'static str, E, DatabaseError) -> E,
		work_fn: W,
	) -> Result<R, E>
	where
		E: TransactionFailure,
		W: AsyncFnOnce(&mut Tx<'_>) -> Result<R, E>,
	{
		let mut tx = self
			.begin_labeled_transaction(operation_label)
			.await
			.map_err(|error| build_database_error(error.with_operation_label(operation_label)))?;
		let result = work_fn(&mut tx).await;
		finish_pool_owned_rollback_only_transaction_and_preserve_rollback_error(
			operation_label,
			tx,
			result,
			build_database_error,
			build_rollback_error,
		)
		.await
	}
}

impl sealed::Database for &WritePool {
	async fn run_read<R, E, W>(
		self,
		operation_label: &'static str,
		build_database_error: fn(DatabaseError) -> E,
		build_rollback_error: fn(&'static str, E, DatabaseError) -> E,
		work_fn: W,
	) -> Result<R, E>
	where
		E: TransactionFailure,
		W: AsyncFnOnce(&mut Tx<'_>) -> Result<R, E>,
	{
		sealed::Database::run_read(
			&self.pool,
			operation_label,
			build_database_error,
			build_rollback_error,
			work_fn,
		)
		.await
	}
}

impl sealed::Database for &mut Tx<'_> {
	async fn run_read<R, E, W>(
		self,
		_operation: &'static str,
		_build_database_error: fn(DatabaseError) -> E,
		_build_rollback_error: fn(&'static str, E, DatabaseError) -> E,
		work_fn: W,
	) -> Result<R, E>
	where
		E: TransactionFailure,
		W: AsyncFnOnce(&mut Tx<'_>) -> Result<R, E>,
	{
		let result = work_fn(&mut *self).await;
		if let Err(error) = &result
			&& error.involves_database_failure()
		{
			self.note_statement_failure();
		}
		result
	}
}

impl sealed::Database for &mut WriteTx<'_> {
	async fn run_read<R, E, W>(
		self,
		_operation: &'static str,
		_build_database_error: fn(DatabaseError) -> E,
		_build_rollback_error: fn(&'static str, E, DatabaseError) -> E,
		work_fn: W,
	) -> Result<R, E>
	where
		E: TransactionFailure,
		W: AsyncFnOnce(&mut Tx<'_>) -> Result<R, E>,
	{
		let result = work_fn(&mut self.tx).await;
		if let Err(error) = &result
			&& error.involves_database_failure()
		{
			self.tx.note_statement_failure();
		}
		result
	}
}

impl sealed::WriteDatabase for &WritePool {
	async fn run_write<R, E, W>(
		self,
		operation_label: &'static str,
		build_database_error: fn(DatabaseError) -> E,
		build_rollback_error: fn(&'static str, E, DatabaseError) -> E,
		work_fn: W,
	) -> Result<R, E>
	where
		E: TransactionFailure,
		W: AsyncFnOnce(&mut WriteTx<'_>) -> Result<R, E>,
	{
		let mut tx = self
			.begin_labeled_transaction(operation_label)
			.await
			.map_err(|error| build_database_error(error.with_operation_label(operation_label)))?;
		let result = work_fn(&mut tx).await;
		finish_pool_owned_write_transaction_and_preserve_rollback_error(
			operation_label,
			tx,
			result,
			build_database_error,
			build_rollback_error,
		)
		.await
	}
}

impl sealed::WriteDatabase for &mut WriteTx<'_> {
	async fn run_write<R, E, W>(
		self,
		_operation: &'static str,
		_build_database_error: fn(DatabaseError) -> E,
		_build_rollback_error: fn(&'static str, E, DatabaseError) -> E,
		work_fn: W,
	) -> Result<R, E>
	where
		E: TransactionFailure,
		W: AsyncFnOnce(&mut WriteTx<'_>) -> Result<R, E>,
	{
		let result = work_fn(&mut *self).await;
		if let Err(error) = &result
			&& error.involves_database_failure()
		{
			self.note_statement_failure();
		}
		result
	}
}
