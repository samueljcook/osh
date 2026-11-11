//! Postgres query constructors for app-owned SQL that stay safe under **every**
//! connection-pooler mode, including statement-mode pooling.
//!
//! [`portable_query`], [`portable_query_as`], and [`portable_query_scalar`] accept the same
//! `$1`, `$2`, ... placeholder SQL and fluent `.bind(value)` calling convention as SQLx's own
//! query constructors, but never use the Postgres extended protocol. The constructor parses
//! the template and fixes its parameter count, which is its highest placeholder index. Each
//! `.bind()` renders its value immediately as a safely-escaped Postgres literal (reusing the
//! same encoder as [`SimpleQuery`](super::SimpleQuery)); executing the query then substitutes
//! those literals into the SQL template and sends the result as a single simple-protocol
//! `Query` with no bind parameters at all. Neither the constructor nor `.bind()` can fail: a
//! malformed template, a value that cannot be encoded, or a wrong number of values makes the
//! query fail when it runs, before any statement reaches the database. There is no `Parse`/`Bind` split for a connection pooler to land
//! on two different backends, so this is safe to execute directly on a [`Pool`](super::Pool)
//! or [`WritePool`](super::WritePool) as well as inside a [`Tx`](super::Tx)/
//! [`WriteTx`](super::WriteTx), under every pooler mode.
//! Template strings with backslash escapes must use explicit `E'...'` syntax; a backslash
//! in an ordinary string is rejected so session `standard_conforming_strings` settings
//! cannot change the scanner's lexical boundaries.
//!
//! # Injection safety
//!
//! See [`SimpleQuery`](super::SimpleQuery)'s module documentation for the literal-encoding
//! guarantees this shares. SQL structure comes only from the template text passed to
//! [`portable_query`]/[`portable_query_as`]/[`portable_query_scalar`]. The template is a
//! string literal or an [`AuditedSql`](super::AuditedSql) built by [`audited_sql!`](crate::audited_sql); bound
//! values are always rendered as quoted/typed literals and can never widen structure.

use std::marker::PhantomData;

use sqlx::postgres::{PgQueryResult, PgRow};
use sqlx::{Decode, Executor, FromRow, Postgres, RawSql, Row, SqlSafeStr, Type};

use super::portable_query_literal::PortableQueryArgument;
use super::portable_query_template::PortableQueryTemplate;
use super::{DatabaseError, PortableQueryBindError, PortableQueryTemplateError};

/// The operation label of a portable query that fails. An operation of Paranoid that runs a
/// portable query replaces the label with its own.
const PORTABLE_QUERY_LABEL: &str = "db.portable_query";

/// Constructs an unparameterized SQLx query using Postgres simple-query protocol.
///
/// Use this for DDL and administration statements that cannot bind values. Because simple-query
/// protocol has no bind parameters, pass dynamic SQL through [`AuditedSql`](super::AuditedSql),
/// which [`audited_sql!`](crate::audited_sql) builds from a literal template and quoted
/// identifiers.
///
/// To bind *values* while keeping the single-`Query`, pooler-independent simple protocol, use
/// [`SimpleQuery`](crate::db::SimpleQuery), which renders bound values as safely-escaped literals,
/// or use [`portable_query`], which does the same thing while keeping the `$n` + `.bind()` calling
/// convention.
pub fn unparameterized_simple_query(sql: impl SqlSafeStr) -> RawSql {
	sqlx::raw_sql(sql)
}

/// Why a portable query cannot run, which the query reports when it runs.
#[derive(Clone, Debug)]
enum PortableQueryBuildFailure {
	Template(PortableQueryTemplateError),
	Bind(PortableQueryBindError),
}

impl PortableQueryBuildFailure {
	fn into_driver_error(self) -> sqlx::Error {
		match self {
			Self::Template(error) => sqlx::Error::Encode(Box::new(error)),
			Self::Bind(error) => sqlx::Error::Encode(Box::new(error)),
		}
	}
}

/// Constructs a pooler-safe Postgres query.
///
/// This helper is optional for application code. Use it when app-owned SQL should follow
/// Paranoid's portable Postgres execution style: `$1`, `$2`, ... placeholders and a fluent
/// `.bind(value)` call for each, exactly like a SQLx query. Unlike a raw SQLx query, every
/// bound value is rendered as a safely-escaped Postgres literal at bind time and substituted
/// into the SQL before it is ever sent, so the finished statement carries no bind parameters
/// and is safe to execute directly on a [`Pool`](super::Pool)/[`WritePool`](super::WritePool)
/// or inside a transaction, under every connection-pooler mode. The template is a string
/// literal, or an [`AuditedSql`](super::AuditedSql) when it needs identifiers that
/// [`audited_sql!`](crate::audited_sql) quotes. Each `$n` placeholder must appear in an SQL
/// expression position, where `portable_query` substitutes a parenthesized literal expression.
/// PostgreSQL rejects parenthesized expressions in utility-statement clauses such as `SET`, so
/// use `SELECT pg_catalog.set_config(...)` to change a session or transaction setting from a
/// bound value.
///
/// The template's parameter count is its highest placeholder index; lower-numbered
/// parameters may be unused, just as they may be in PostgreSQL's extended protocol. Neither
/// this constructor nor [`PortableQuery::bind`] can fail. The query fails when it runs, before
/// any statement reaches the database, when the template is malformed, such as an
/// unterminated string literal or a dollar-quoted string, when a bound value cannot be
/// encoded, or when the number of values differs from the number of parameters; the failure's
/// [`driver_error`](DatabaseError::driver_error) is a [`sqlx::Error::Encode`] that holds the
/// [`PortableQueryTemplateError`] or [`PortableQueryBindError`]. Backslash escapes in template
/// strings must use explicit `E'...'` syntax; ordinary-string backslashes are rejected so
/// parsing cannot vary with `standard_conforming_strings`.
pub fn portable_query(sql: impl SqlSafeStr) -> PortableQuery {
	let sql = sql.into_sql_str();
	match PortableQueryTemplate::parse(sql.as_str()) {
		Ok(template) => PortableQuery {
			statement: template.to_statement(),
			build: Ok(PortableQueryBuild {
				template,
				fragments: Vec::new(),
			}),
		},
		Err(error) => PortableQuery {
			statement: sql.as_str().to_owned(),
			build: Err(PortableQueryBuildFailure::Template(error)),
		},
	}
}

/// Constructs a typed pooler-safe Postgres query.
///
/// This is the [`portable_query`] equivalent for SQLx row mapping. See [`portable_query`].
pub fn portable_query_as<O>(sql: impl SqlSafeStr) -> PortableQueryAs<O>
where
	O: for<'r> FromRow<'r, PgRow>,
{
	PortableQueryAs {
		inner: portable_query(sql),
		row_type: PhantomData,
	}
}

/// Constructs a scalar pooler-safe Postgres query.
///
/// This is the [`portable_query`] equivalent for SQLx scalar row mapping. See
/// [`portable_query`].
pub fn portable_query_scalar<O>(sql: impl SqlSafeStr) -> PortableQueryScalar<O>
where
	O: for<'r> Decode<'r, Postgres> + Type<Postgres>,
{
	PortableQueryScalar {
		inner: portable_query(sql),
		scalar_type: PhantomData,
	}
}

/// Constructs a portable query from a template that Paranoid owns. A template that does not
/// parse is a defect in Paranoid, so this panics instead of returning the error.
pub(crate) fn owned_portable_query(sql: impl SqlSafeStr) -> PortableQuery {
	let query = portable_query(sql);
	assert!(
		query.build.is_ok(),
		"a template that Paranoid owns must parse as a portable query"
	);
	query
}

/// The typed form of [`owned_portable_query`].
pub(crate) fn owned_portable_query_as<O>(sql: impl SqlSafeStr) -> PortableQueryAs<O>
where
	O: for<'r> FromRow<'r, PgRow>,
{
	PortableQueryAs {
		inner: owned_portable_query(sql),
		row_type: PhantomData,
	}
}

/// The scalar form of [`owned_portable_query`].
pub(crate) fn owned_portable_query_scalar<O>(sql: impl SqlSafeStr) -> PortableQueryScalar<O>
where
	O: for<'r> Decode<'r, Postgres> + Type<Postgres>,
{
	PortableQueryScalar {
		inner: owned_portable_query(sql),
		scalar_type: PhantomData,
	}
}

/// A pooler-safe Postgres query built by [`portable_query`].
///
/// Bind values with [`PortableQuery::bind`], then finish with [`PortableQuery::execute`],
/// [`PortableQuery::fetch_one`], [`PortableQuery::fetch_all`], or
/// [`PortableQuery::fetch_optional`].
#[derive(Debug)]
pub struct PortableQuery {
	/// The template as the statement that a failure names, which holds no values.
	statement: String,
	build: Result<PortableQueryBuild, PortableQueryBuildFailure>,
}

#[derive(Debug)]
struct PortableQueryBuild {
	template: PortableQueryTemplate,
	fragments: Vec<String>,
}

impl PortableQuery {
	/// Binds a value in place of its `$n` placeholder, rendering it immediately as a
	/// safely-escaped, parenthesized Postgres expression. The parentheses keep the
	/// value separate from neighboring tokens and string constants in the template.
	///
	/// Binding cannot fail. The query fails when it runs if this value cannot be encoded or if
	/// every parameter of the template already had a value. A value cannot be encoded when it is
	/// text with a NUL byte, which Postgres text cannot store; a `time::OffsetDateTime` that RFC
	/// 3339 cannot represent, such as one with an offset that includes seconds; a `time::Date` that ISO 8601
	/// cannot represent, such as one with a negative year; or a [`serde_json::Value`] that nests
	/// more than 100 arrays or objects along a path or that cannot be serialized. The failure
	/// names the first such value; see [`portable_query`].
	pub fn bind<T>(mut self, value: T) -> Self
	where
		T: PortableQueryArgument,
	{
		let Ok(build) = &mut self.build else {
			return self;
		};
		if build.fragments.len() >= build.template.param_count() {
			self.build = Err(PortableQueryBuildFailure::Bind(
				PortableQueryBindError::TooManyValues {
					param_count: build.template.param_count(),
				},
			));
			return self;
		}
		let mut fragment = String::from("(");
		match value.encode_portable_query_literal(&mut fragment) {
			Ok(()) => {
				fragment.push(')');
				build.fragments.push(fragment);
			}
			Err(source) => {
				self.build = Err(PortableQueryBuildFailure::Bind(
					PortableQueryBindError::Encoding {
						position: build.fragments.len() + 1,
						source,
					},
				));
			}
		}
		self
	}

	/// Binds a value that Paranoid has already validated, such as a parsed key. Encoding such a
	/// value cannot fail, and a template that Paranoid owns takes as many values as it binds, so
	/// a failure is a defect in Paranoid and this panics instead of reporting it when the query
	/// runs.
	pub(crate) fn bind_validated<T>(self, value: T) -> Self
	where
		T: PortableQueryArgument,
	{
		let query = self.bind(value);
		assert!(
			query.build.is_ok(),
			"Paranoid binds only validated values to templates that it owns"
		);
		query
	}

	/// Finishes the query into the unparameterized [`RawSql`] it always executes as.
	///
	/// Every finishing method (`execute`, `fetch_one`, `fetch_all`, `fetch_optional`) sends
	/// this statement, so the query carries no bind parameters and is never eligible for a
	/// persistent server-side prepared statement.
	#[cfg(any(test, feature = "__internal"))]
	#[doc(hidden)]
	pub fn into_raw_sql(self) -> Result<RawSql, sqlx::Error> {
		self.render()
	}

	fn render(&self) -> Result<RawSql, sqlx::Error> {
		let build = self
			.build
			.as_ref()
			.map_err(|failure| failure.clone().into_driver_error())?;
		let sql = build
			.template
			.render(&build.fragments)
			.map_err(|error| PortableQueryBuildFailure::Template(error).into_driver_error())?;
		Ok(unparameterized_simple_query(sqlx::AssertSqlSafe(sql)))
	}

	/// The failure of this query: the driver's error behind a [`DatabaseError`] that names the
	/// template, which holds no values, as its statement.
	fn to_database_error(&self, driver_error: sqlx::Error) -> DatabaseError {
		DatabaseError::for_statement(PORTABLE_QUERY_LABEL, &self.statement, driver_error)
	}

	async fn fetch_one_row<'e, E>(&self, executor: E) -> Result<PgRow, DatabaseError>
	where
		E: Executor<'e, Database = Postgres>,
	{
		let raw_sql = self
			.render()
			.map_err(|error| self.to_database_error(error))?;
		raw_sql
			.fetch_one(executor)
			.await
			.map_err(|error| self.to_database_error(error))
	}

	async fn fetch_all_rows<'e, E>(&self, executor: E) -> Result<Vec<PgRow>, DatabaseError>
	where
		E: Executor<'e, Database = Postgres>,
	{
		let raw_sql = self
			.render()
			.map_err(|error| self.to_database_error(error))?;
		raw_sql
			.fetch_all(executor)
			.await
			.map_err(|error| self.to_database_error(error))
	}

	async fn fetch_optional_row<'e, E>(&self, executor: E) -> Result<Option<PgRow>, DatabaseError>
	where
		E: Executor<'e, Database = Postgres>,
	{
		let raw_sql = self
			.render()
			.map_err(|error| self.to_database_error(error))?;
		raw_sql
			.fetch_optional(executor)
			.await
			.map_err(|error| self.to_database_error(error))
	}

	/// Executes the query, returning the number of rows affected.
	///
	/// A failure is a [`DatabaseError`] with the operation label `db.portable_query` and the
	/// template as its statement. PostgreSQL's message and detail can contain the bound values,
	/// so the error prints neither. [`DatabaseError::driver_error`] gives the driver's error.
	pub async fn execute<'e, E>(self, executor: E) -> Result<PgQueryResult, DatabaseError>
	where
		E: Executor<'e, Database = Postgres>,
	{
		let raw_sql = self
			.render()
			.map_err(|error| self.to_database_error(error))?;
		raw_sql
			.execute(executor)
			.await
			.map_err(|error| self.to_database_error(error))
	}

	/// Fetches the first row. When the query returned no row, the failure's driver error is
	/// [`sqlx::Error::RowNotFound`]. See [`PortableQuery::execute`] for the failure.
	pub async fn fetch_one<'e, E>(self, executor: E) -> Result<PgRow, DatabaseError>
	where
		E: Executor<'e, Database = Postgres>,
	{
		self.fetch_one_row(executor).await
	}

	/// Fetches every row. See [`PortableQuery::execute`] for the failure.
	pub async fn fetch_all<'e, E>(self, executor: E) -> Result<Vec<PgRow>, DatabaseError>
	where
		E: Executor<'e, Database = Postgres>,
	{
		self.fetch_all_rows(executor).await
	}

	/// Fetches at most one row. See [`PortableQuery::execute`] for the failure.
	pub async fn fetch_optional<'e, E>(self, executor: E) -> Result<Option<PgRow>, DatabaseError>
	where
		E: Executor<'e, Database = Postgres>,
	{
		self.fetch_optional_row(executor).await
	}
}

/// A typed pooler-safe Postgres query built by [`portable_query_as`].
#[derive(Debug)]
pub struct PortableQueryAs<O> {
	inner: PortableQuery,
	row_type: PhantomData<fn() -> O>,
}

impl<O> PortableQueryAs<O>
where
	O: for<'r> FromRow<'r, PgRow>,
{
	/// Binds a value in place of its `$n` placeholder. See [`PortableQuery::bind`].
	pub fn bind<T>(mut self, value: T) -> Self
	where
		T: PortableQueryArgument,
	{
		self.inner = self.inner.bind(value);
		self
	}

	/// See [`PortableQuery::bind_validated`].
	pub(crate) fn bind_validated<T>(mut self, value: T) -> Self
	where
		T: PortableQueryArgument,
	{
		self.inner = self.inner.bind_validated(value);
		self
	}

	/// Finishes the query into the unparameterized [`RawSql`] it always executes as. See
	/// [`PortableQuery::into_raw_sql`].
	#[cfg(feature = "__internal")]
	#[doc(hidden)]
	pub fn into_raw_sql(self) -> Result<RawSql, sqlx::Error> {
		self.inner.into_raw_sql()
	}

	/// Fetches the first row, decoded as `O`. When the query returned no row, the failure's
	/// driver error is [`sqlx::Error::RowNotFound`]. A row that does not decode as `O` is a
	/// failure too. See [`PortableQuery::execute`] for the failure.
	pub async fn fetch_one<'e, E>(self, executor: E) -> Result<O, DatabaseError>
	where
		E: Executor<'e, Database = Postgres>,
	{
		let row = self.inner.fetch_one_row(executor).await?;
		O::from_row(&row).map_err(|error| self.inner.to_database_error(error))
	}

	/// Fetches every row, each decoded as `O`. See [`PortableQueryAs::fetch_one`] for the
	/// failure.
	pub async fn fetch_all<'e, E>(self, executor: E) -> Result<Vec<O>, DatabaseError>
	where
		E: Executor<'e, Database = Postgres>,
	{
		let rows = self.inner.fetch_all_rows(executor).await?;
		rows.iter()
			.map(O::from_row)
			.collect::<Result<_, _>>()
			.map_err(|error| self.inner.to_database_error(error))
	}

	/// Fetches at most one row, decoded as `O`. See [`PortableQueryAs::fetch_one`] for the
	/// failure.
	pub async fn fetch_optional<'e, E>(self, executor: E) -> Result<Option<O>, DatabaseError>
	where
		E: Executor<'e, Database = Postgres>,
	{
		self.inner
			.fetch_optional_row(executor)
			.await?
			.map(|row| O::from_row(&row))
			.transpose()
			.map_err(|error| self.inner.to_database_error(error))
	}
}

/// A scalar pooler-safe Postgres query built by [`portable_query_scalar`].
#[derive(Debug)]
pub struct PortableQueryScalar<O> {
	inner: PortableQuery,
	scalar_type: PhantomData<fn() -> O>,
}

impl<O> PortableQueryScalar<O>
where
	O: for<'r> Decode<'r, Postgres> + Type<Postgres>,
{
	/// Binds a value in place of its `$n` placeholder. See [`PortableQuery::bind`].
	pub fn bind<T>(mut self, value: T) -> Self
	where
		T: PortableQueryArgument,
	{
		self.inner = self.inner.bind(value);
		self
	}

	/// See [`PortableQuery::bind_validated`].
	pub(crate) fn bind_validated<T>(mut self, value: T) -> Self
	where
		T: PortableQueryArgument,
	{
		self.inner = self.inner.bind_validated(value);
		self
	}

	/// Finishes the query into the unparameterized [`RawSql`] it always executes as. See
	/// [`PortableQuery::into_raw_sql`].
	#[cfg(feature = "__internal")]
	#[doc(hidden)]
	pub fn into_raw_sql(self) -> Result<RawSql, sqlx::Error> {
		self.inner.into_raw_sql()
	}

	/// Fetches the first row's first column, decoded as `O`. When the query returned no row,
	/// the failure's driver error is [`sqlx::Error::RowNotFound`]. A column that does not
	/// decode as `O` is a failure too. See [`PortableQuery::execute`] for the failure.
	pub async fn fetch_one<'e, E>(self, executor: E) -> Result<O, DatabaseError>
	where
		E: Executor<'e, Database = Postgres>,
	{
		let row = self.inner.fetch_one_row(executor).await?;
		row.try_get(0)
			.map_err(|error| self.inner.to_database_error(error))
	}

	/// Fetches every row's first column, each decoded as `O`. See
	/// [`PortableQueryScalar::fetch_one`] for the failure.
	pub async fn fetch_all<'e, E>(self, executor: E) -> Result<Vec<O>, DatabaseError>
	where
		E: Executor<'e, Database = Postgres>,
	{
		let rows = self.inner.fetch_all_rows(executor).await?;
		rows.iter()
			.map(|row| row.try_get(0))
			.collect::<Result<_, _>>()
			.map_err(|error| self.inner.to_database_error(error))
	}

	/// Fetches at most one row's first column, decoded as `O`. See
	/// [`PortableQueryScalar::fetch_one`] for the failure.
	pub async fn fetch_optional<'e, E>(self, executor: E) -> Result<Option<O>, DatabaseError>
	where
		E: Executor<'e, Database = Postgres>,
	{
		self.inner
			.fetch_optional_row(executor)
			.await?
			.map(|row| row.try_get(0))
			.transpose()
			.map_err(|error| self.inner.to_database_error(error))
	}
}
