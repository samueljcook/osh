//! Observes application SQL without recording Paranoid's internally labeled statements twice.

use super::{
	AbandonedStateMutationError, BoxFuture, BoxStream, ConnectionReserve, Database,
	DatabaseOperationKind, DatabaseOperationObserver, Describe, Either, Execute, Executor,
	InterruptedStatementDiscardError, PgPool, PgQueryResult, PgRow, PgStatement, PgTypeInfo, Pool,
	Postgres, SqlStr, StatementInterruptionGuard, StatementInterruptionMarker,
	TransactionConnection, Tx, WritePool, WriteTx, acquire_pool_connection,
	acquire_pool_reserve_permit,
};
use futures_util::{StreamExt, TryStreamExt, stream};

/// Owns the query fields because SQLx consumes a query when extracting its SQL text.
struct ApplicationStatement {
	sql: SqlStr,
	prepared: Option<PgStatement>,
	arguments: Result<Option<<Postgres as Database>::Arguments>, sqlx::error::BoxDynError>,
	persistent: bool,
}

impl ApplicationStatement {
	fn new<'q>(mut query: impl Execute<'q, Postgres>) -> Self {
		Self {
			prepared: query.statement().cloned(),
			persistent: query.persistent(),
			arguments: query.take_arguments(),
			sql: query.sql(),
		}
	}
}

impl Execute<'_, Postgres> for ApplicationStatement {
	fn sql(self) -> SqlStr {
		self.sql
	}
	fn statement(&self) -> Option<&PgStatement> {
		self.prepared.as_ref()
	}
	fn take_arguments(
		&mut self,
	) -> Result<Option<<Postgres as Database>::Arguments>, sqlx::error::BoxDynError> {
		std::mem::replace(&mut self.arguments, Ok(None))
	}
	fn persistent(&self) -> bool {
		self.persistent
	}
}

#[derive(Debug, thiserror::Error)]
#[error("an owned transaction runs queries from SQL text, not from a prepared statement handle")]
struct PreparedStatementHandleError;

#[derive(Debug, thiserror::Error)]
#[error(
	"extended-protocol SQL requires an owned transaction; use portable queries or unparameterized_simple_query directly on a pool"
)]
struct ExtendedProtocolRequiresTransactionError;

fn extended_protocol_requires_transaction() -> sqlx::Error {
	sqlx::Error::Configuration(Box::new(ExtendedProtocolRequiresTransactionError))
}

#[derive(Debug)]
enum ApplicationTransactionState<'a> {
	Ready {
		connection: &'a mut TransactionConnection,
		interruption: StatementInterruptionMarker,
	},
	InterruptedStatement,
	AbandonedStateMutation,
}

impl<'a> ApplicationTransactionState<'a> {
	fn expect_ready_statement(self) -> (&'a mut TransactionConnection, StatementInterruptionGuard) {
		match self {
			Self::Ready {
				connection,
				interruption,
			} => (connection, interruption.arm()),
			Self::InterruptedStatement | Self::AbandonedStateMutation => {
				unreachable!("prepare_query rejects non-ready transaction states before execution")
			}
		}
	}

	fn expect_ready_stream(self) -> &'a mut TransactionConnection {
		match self {
			Self::Ready { connection, .. } => connection,
			Self::InterruptedStatement | Self::AbandonedStateMutation => {
				unreachable!("prepare_query rejects non-ready transaction states before execution")
			}
		}
	}
}

#[derive(Debug)]
enum ApplicationConnection<'a> {
	Pool {
		pool: PgPool,
		connection_reserve: ConnectionReserve,
	},
	Transaction(ApplicationTransactionState<'a>),
}

#[derive(Debug)]
pub(crate) struct ApplicationExecutor<'a> {
	connection: ApplicationConnection<'a>,
	observer: Option<DatabaseOperationObserver>,
	label: Option<&'static str>,
}

impl Pool {
	fn application_executor(&self) -> ApplicationExecutor<'static> {
		ApplicationExecutor {
			connection: ApplicationConnection::Pool {
				pool: self.inner.clone(),
				connection_reserve: self.connection_reserve.clone(),
			},
			observer: self.operation_observer.clone(),
			label: None,
		}
	}
}

impl Tx<'_> {
	fn application_executor(&mut self) -> ApplicationExecutor<'_> {
		let state = if self.statement_was_interrupted() {
			ApplicationTransactionState::InterruptedStatement
		} else if self.state_mutations.requires_rollback() {
			ApplicationTransactionState::AbandonedStateMutation
		} else {
			let interruption = self.statement_interruption_marker();
			ApplicationTransactionState::Ready {
				connection: self.inner.as_mut(),
				interruption,
			}
		};
		ApplicationExecutor {
			connection: ApplicationConnection::Transaction(state),
			observer: self.operation_observer.clone(),
			label: None,
		}
	}

	/// Component migrations use application SQL checks and their own operation labels.
	pub(crate) fn labeled_application_executor(
		&mut self,
		label: &'static str,
	) -> ApplicationExecutor<'_> {
		ApplicationExecutor {
			label: Some(label),
			..self.application_executor()
		}
	}
}

impl ApplicationExecutor<'_> {
	fn check_and_observe(&self, kind: DatabaseOperationKind, sql: &str) -> Result<(), sqlx::Error> {
		match &self.connection {
			ApplicationConnection::Transaction(
				ApplicationTransactionState::InterruptedStatement,
			) => {
				return Err(sqlx::Error::Configuration(Box::new(
					InterruptedStatementDiscardError,
				)));
			}
			ApplicationConnection::Transaction(
				ApplicationTransactionState::AbandonedStateMutation,
			) => {
				return Err(sqlx::Error::Configuration(Box::new(
					AbandonedStateMutationError,
				)));
			}
			ApplicationConnection::Pool { .. }
			| ApplicationConnection::Transaction(ApplicationTransactionState::Ready { .. }) => {}
		}
		// A transaction handle owns its transaction, and on a pool a transaction that the SQL
		// left open would stay on the pooled connection for the next user of that connection.
		super::super::transaction_sql::check_transaction_lifetime(sql)
			.map_err(|error| sqlx::Error::Configuration(Box::new(error)))?;
		if let Some(observer) = &self.observer {
			let label = self.label.unwrap_or(match kind {
				DatabaseOperationKind::Execute => "db.executor.execute",
				DatabaseOperationKind::ExecuteMany => "db.executor.execute_many",
				DatabaseOperationKind::Fetch => "db.executor.fetch",
				DatabaseOperationKind::FetchMany => "db.executor.fetch_many",
				DatabaseOperationKind::FetchAll => "db.executor.fetch_all",
				DatabaseOperationKind::FetchOne => "db.executor.fetch_one",
				DatabaseOperationKind::FetchOptional => "db.executor.fetch_optional",
				DatabaseOperationKind::Prepare => "db.executor.prepare",
				DatabaseOperationKind::Describe => "db.executor.describe",
				_ => unreachable!("application executors do not control transaction lifetimes"),
			});
			match self.connection {
				ApplicationConnection::Pool { .. } => {
					observer.record_operation(kind, label, Some(sql))?
				}
				ApplicationConnection::Transaction(_) => {
					observer.record_statement(kind, label, Some(sql))
				}
			}
		}
		Ok(())
	}

	fn prepare_query<'q>(
		&self,
		query: impl Execute<'q, Postgres>,
		kind: DatabaseOperationKind,
	) -> Result<ApplicationStatement, sqlx::Error> {
		let mut query = ApplicationStatement::new(query);
		if matches!(self.connection, ApplicationConnection::Pool { .. })
			&& (query.prepared.is_some() || !matches!(&query.arguments, Ok(None)))
		{
			return Err(extended_protocol_requires_transaction());
		}
		if matches!(self.connection, ApplicationConnection::Transaction(_)) {
			if query.prepared.is_some() {
				return Err(sqlx::Error::Configuration(Box::new(
					PreparedStatementHandleError,
				)));
			}
			// An unnamed statement cannot outlive the transaction or collide with another
			// client's named statement when a pooler reassigns the server connection.
			query.persistent = false;
		}
		self.check_and_observe(kind, query.sql.as_str())?;
		Ok(query)
	}
}

// Override each query method so SQLx's default delegation does not change its observed kind.
macro_rules! observed_query_future {
	($method:ident, $kind:ident, $output:ty) => {
		fn $method<'e, 'q: 'e, E>(self, query: E) -> BoxFuture<'e, Result<$output, sqlx::Error>>
		where
			'c: 'e,
			E: 'q + Execute<'q, Postgres>,
		{
			Box::pin(async move {
				let query = self.prepare_query(query, DatabaseOperationKind::$kind)?;
				match self.connection {
					ApplicationConnection::Pool {
						pool,
						connection_reserve,
					} => {
						let (_permit, mut pooled) =
							acquire_pool_connection(&pool, connection_reserve.reserved_semaphore())
								.await?;
						(&mut *pooled).$method(query).await
					}
					ApplicationConnection::Transaction(state) => {
						let (connection, mut interruption) = state.expect_ready_statement();
						let result = connection.$method(query).await;
						interruption.complete();
						result
					}
				}
			})
		}
	};
}

macro_rules! observed_query_stream {
	($method:ident, $kind:ident, $output:ty) => {
		fn $method<'e, 'q: 'e, E>(self, query: E) -> BoxStream<'e, Result<$output, sqlx::Error>>
		where
			'c: 'e,
			E: 'q + Execute<'q, Postgres>,
		{
			stream::once(async move {
				let query = self.prepare_query(query, DatabaseOperationKind::$kind)?;
				let stream: BoxStream<'e, Result<$output, sqlx::Error>> = match self.connection {
					ApplicationConnection::Pool {
						pool,
						connection_reserve,
					} => {
						let permit = acquire_pool_reserve_permit(
							&pool,
							connection_reserve.reserved_semaphore(),
						)
						.await?;
						(&pool)
							.$method(query)
							.inspect(move |_| {
								let _keep_alive = &permit;
							})
							.boxed()
					}
					ApplicationConnection::Transaction(state) => {
						state.expect_ready_stream().$method(query)
					}
				};
				Ok::<_, sqlx::Error>(stream)
			})
			.try_flatten()
			.boxed()
		}
	};
}

impl<'c> Executor<'c> for ApplicationExecutor<'c> {
	type Database = Postgres;
	observed_query_future!(execute, Execute, PgQueryResult);
	observed_query_future!(fetch_all, FetchAll, Vec<PgRow>);
	observed_query_future!(fetch_one, FetchOne, PgRow);
	observed_query_future!(fetch_optional, FetchOptional, Option<PgRow>);
	observed_query_stream!(execute_many, ExecuteMany, PgQueryResult);
	observed_query_stream!(fetch, Fetch, PgRow);
	observed_query_stream!(fetch_many, FetchMany, Either<PgQueryResult, PgRow>);

	fn prepare_with<'e>(
		self,
		sql: SqlStr,
		parameters: &'e [PgTypeInfo],
	) -> BoxFuture<'e, Result<PgStatement, sqlx::Error>>
	where
		'c: 'e,
	{
		Box::pin(async move {
			if matches!(self.connection, ApplicationConnection::Pool { .. }) {
				return Err(extended_protocol_requires_transaction());
			}
			self.check_and_observe(DatabaseOperationKind::Prepare, sql.as_str())?;
			match self.connection {
				ApplicationConnection::Pool { .. } => unreachable!(
					"extended-protocol statement preparation on a pool returns before execution"
				),
				ApplicationConnection::Transaction(state) => {
					let (connection, mut interruption) = state.expect_ready_statement();
					let result = connection.prepare_with(sql, parameters).await;
					interruption.complete();
					result
				}
			}
		})
	}

	fn describe<'e>(self, sql: SqlStr) -> BoxFuture<'e, Result<Describe<Postgres>, sqlx::Error>>
	where
		'c: 'e,
	{
		Box::pin(async move {
			if matches!(self.connection, ApplicationConnection::Pool { .. }) {
				return Err(extended_protocol_requires_transaction());
			}
			self.check_and_observe(DatabaseOperationKind::Describe, sql.as_str())?;
			match self.connection {
				ApplicationConnection::Pool { .. } => {
					unreachable!("extended-protocol describe on a pool returns before execution")
				}
				ApplicationConnection::Transaction(state) => {
					let (connection, mut interruption) = state.expect_ready_statement();
					let result = connection.describe(sql).await;
					interruption.complete();
					result
				}
			}
		})
	}
}

// All handles must forward the complete Executor API, including methods with SQLx defaults.
macro_rules! forward_query_future {
	($method:ident, $output:ty, $this:ident, $executor:expr) => {
		fn $method<'e, 'q: 'e, E>(self, query: E) -> BoxFuture<'e, Result<$output, sqlx::Error>>
		where
			'c: 'e,
			E: 'q + Execute<'q, Postgres>,
		{
			let $this = self;
			$executor.$method(query)
		}
	};
}
macro_rules! forward_query_stream {
	($method:ident, $output:ty, $this:ident, $executor:expr) => {
		fn $method<'e, 'q: 'e, E>(self, query: E) -> BoxStream<'e, Result<$output, sqlx::Error>>
		where
			'c: 'e,
			E: 'q + Execute<'q, Postgres>,
		{
			let $this = self;
			$executor.$method(query)
		}
	};
}
macro_rules! forward_executor {
	($this:ident, $executor:expr) => {
		type Database = Postgres;
		forward_query_future!(execute, PgQueryResult, $this, $executor);
		forward_query_future!(fetch_all, Vec<PgRow>, $this, $executor);
		forward_query_future!(fetch_one, PgRow, $this, $executor);
		forward_query_future!(fetch_optional, Option<PgRow>, $this, $executor);
		forward_query_stream!(execute_many, PgQueryResult, $this, $executor);
		forward_query_stream!(fetch, PgRow, $this, $executor);
		forward_query_stream!(fetch_many, Either<PgQueryResult, PgRow>, $this, $executor);
		fn prepare_with<'e>(self, sql: SqlStr, parameters: &'e [PgTypeInfo]) -> BoxFuture<'e, Result<PgStatement, sqlx::Error>>
		where 'c: 'e {
			let $this = self;
			$executor.prepare_with(sql, parameters)
		}
		fn describe<'e>(self, sql: SqlStr) -> BoxFuture<'e, Result<Describe<Postgres>, sqlx::Error>>
		where 'c: 'e {
			let $this = self;
			$executor.describe(sql)
		}
	};
}

impl<'c> Executor<'c> for &'_ Pool {
	forward_executor!(pool, pool.application_executor());
}
impl<'c> Executor<'c> for &'_ WritePool {
	forward_executor!(pool, pool.pool.application_executor());
}
impl<'c> Executor<'c> for &'c mut Tx<'_> {
	forward_executor!(tx, tx.application_executor());
}
impl<'c> Executor<'c> for &'c mut WriteTx<'_> {
	forward_executor!(tx, tx.tx.application_executor());
}
