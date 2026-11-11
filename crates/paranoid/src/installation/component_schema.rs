use super::{
	ComponentSchemaMigrationPlan, ComponentSchemaMigrationStep, ComponentSchemaVersion,
	validate_component_schema_version,
};
use crate::db::pg_lexer::{PgLexError, PgLexeme, PgLexer, PgLexerOptions, PgLiteralMode};
use crate::db::{AuditedSql, DatabaseError, SchemaError, Tx, unparameterized_simple_query};
use std::borrow::Cow;

use sqlx::Row;

pub const COMPONENT_SCHEMA_EXECUTE_FRESH_INSTALL_STATEMENT_LABEL: &str =
	"db.component_schema.execute_fresh_install_statement";
pub const COMPONENT_SCHEMA_EXECUTE_UPGRADE_STATEMENT_LABEL: &str =
	"db.component_schema.execute_upgrade_statement";
pub const COMPONENT_SCHEMA_EXECUTE_VALIDATION_CHECK_LABEL: &str =
	"db.component_schema.execute_validation_check";

/// One unparameterized SQL statement owned by a component schema migration.
///
/// Statements run through Postgres simple-query protocol as one nonempty SQL
/// unit. This constructor rejects comment-only input and top-level semicolons;
/// semicolons inside terminated PostgreSQL strings, quoted identifiers,
/// dollar-quoted bodies, and comments are permitted. It also rejects a statement
/// that begins, ends, or splits the transaction because the migration and its
/// ledger entry must commit together. It rejects a statement when the first word
/// of its SQL is `ABORT`, `BEGIN`, `COMMIT`, `END`, `PREPARE`, `RELEASE`,
/// `ROLLBACK`, `SAVEPOINT`, or `START`, in any letter case. It deliberately does not
/// parse or prove PostgreSQL grammar: the database remains authoritative for
/// whether the unit is one valid statement. Ordinary string literals containing
/// backslashes are rejected so validation cannot change with
/// `standard_conforming_strings`; use an explicit `E'...'` escape string instead.
/// Build DDL or migration SQL with dynamic identifiers through
/// [`audited_sql!`](crate::audited_sql), which quotes them, and pass it to
/// [`ComponentSchemaStatement::from_audited_sql`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComponentSchemaStatement<'a> {
	sql: Cow<'a, str>,
}

/// One physical schema validation check.
///
/// A validation check is a single SQL boolean expression. Paranoid wraps it as
/// `SELECT (<expression>)` and requires the result to decode as exactly one
/// boolean `true`. A false, null, non-boolean, non-scalar, or failing expression
/// makes schema validation fail. Build an expression with dynamic identifiers through
/// [`audited_sql!`](crate::audited_sql), which quotes them.
/// Its SQL lexical restrictions match [`ComponentSchemaStatement`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComponentSchemaValidationCheck<'a> {
	boolean_expression: Cow<'a, str>,
}

/// One ordered physical migration step for a component schema instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ComponentSchemaMigration<'a> {
	step: ComponentSchemaMigrationStep<'a>,
	statements: &'a [ComponentSchemaStatement<'a>],
}

/// An additional physical SQL layout that this application release can use.
///
/// The version and fingerprint identify one exact layout. Checks must prove that
/// layout, and the application must support its SQL and data semantics for both
/// reads and writes. Declaring compatibility does not install this version.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ComponentSchemaCompatibility<'a> {
	version: super::ComponentSchemaMigrationVersion<'a>,
	validation_checks: &'a [ComponentSchemaValidationCheck<'a>],
}

/// Complete schema description for one schema-family instance that an application registers
/// in its installation's schema ledger.
///
/// `ComponentSchema` describes the installation target of a schema family plus the physical
/// work needed to fresh-install or upgrade to that target. It does not choose the schema ledger
/// table or transaction boundary; callers run it through
/// [`PgRuntime::migrate_component_schema`](crate::runtime::PgRuntime::migrate_component_schema) or
/// [`PgRuntime::validate_component_schema`](crate::runtime::PgRuntime::validate_component_schema).
///
/// Additional compatible layouts are explicit version/fingerprint pairs with
/// their own physical checks. Before advancing a shared SQL schema during a
/// rolling deployment, deploy a preparation release that supports both layouts
/// on every instance. That release can be redeployed while the newer layout
/// remains installed. The application author must establish that its reads,
/// writes, constraints, defaults, views, and data semantics support each declared
/// layout. Paranoid does not infer those properties or discover the SQL layouts
/// that other processes support. Retire incompatible application code before an
/// incompatible physical or data change. Redeploying compatible code does not
/// reverse SQL migrations or converted data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ComponentSchema<'a> {
	current_version: ComponentSchemaVersion<'a>,
	fresh_install_statements: &'a [ComponentSchemaStatement<'a>],
	migrations: &'a [ComponentSchemaMigration<'a>],
	validation_checks: &'a [ComponentSchemaValidationCheck<'a>],
	compatible_versions: &'a [ComponentSchemaCompatibility<'a>],
}

/// Result of applying or validating a component schema migration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ComponentSchemaMigrationResult {
	/// The ledger had no row for this component instance, so fresh-install
	/// statements and validation ran before recording the current version.
	FreshInstall {
		/// Recorded schema version.
		version: i32,
	},
	/// The ledger row already matched the requested version and fingerprint,
	/// and physical validation succeeded.
	AlreadyCurrent {
		/// Validated schema version.
		version: i32,
	},
	/// An explicitly compatible layout was installed. Its checks passed, and
	/// Paranoid preserved its version and physical schema without running migrations.
	AlreadyCompatible {
		/// The installed schema version.
		version: i32,
	},
	/// One or more ordered upgrade steps ran, physical validation succeeded,
	/// and the ledger row was advanced.
	Upgraded {
		/// Previously recorded schema version.
		from_version: i32,
		/// Newly recorded schema version.
		to_version: i32,
		/// Number of migration steps applied.
		steps_applied: usize,
	},
}

/// A component schema definition is invalid. The definition is wrong before any database work.
#[derive(Debug, thiserror::Error)]
#[error("invalid component schema: {reason}")]
pub struct ComponentSchemaError {
	reason: String,
}

impl ComponentSchemaError {
	fn new(reason: impl Into<String>) -> Self {
		Self {
			reason: reason.into(),
		}
	}

	/// Why the definition is invalid.
	pub fn reason(&self) -> &str {
		&self.reason
	}
}

impl ComponentSchemaStatement<'static> {
	/// Creates a component schema statement from static SQL.
	pub fn from_static_sql(
		sql: &'static str,
	) -> Result<ComponentSchemaStatement<'static>, ComponentSchemaError> {
		Self::from_cow(Cow::Borrowed(sql))
	}

	/// Creates a component schema statement from [`AuditedSql`], which
	/// [`audited_sql!`](crate::audited_sql) builds from a literal template.
	pub fn from_audited_sql(
		sql: AuditedSql,
	) -> Result<ComponentSchemaStatement<'static>, ComponentSchemaError> {
		Self::from_cow(Cow::Owned(sql.into_string()))
	}
}

impl<'a> ComponentSchemaStatement<'a> {
	/// Returns the SQL statement text.
	pub fn as_str(&self) -> &str {
		self.sql.as_ref()
	}

	fn from_cow(sql: Cow<'a, str>) -> Result<Self, ComponentSchemaError> {
		validate_component_schema_sql(sql.as_ref(), "component schema SQL statement")?;
		validate_component_schema_sql_unit(sql.as_ref(), "component schema SQL statement")?;
		Ok(Self { sql })
	}
}

impl ComponentSchemaValidationCheck<'static> {
	/// Creates a component schema validation check from a static SQL boolean expression.
	pub fn from_static_boolean_expression(
		boolean_expression: &'static str,
	) -> Result<ComponentSchemaValidationCheck<'static>, ComponentSchemaError> {
		Self::from_cow(Cow::Borrowed(boolean_expression))
	}

	/// Creates a component schema validation check from an [`AuditedSql`] boolean expression,
	/// which [`audited_sql!`](crate::audited_sql) builds from a literal template.
	pub fn from_audited_boolean_expression(
		boolean_expression: AuditedSql,
	) -> Result<ComponentSchemaValidationCheck<'static>, ComponentSchemaError> {
		Self::from_cow(Cow::Owned(boolean_expression.into_string()))
	}
}

impl<'a> ComponentSchemaValidationCheck<'a> {
	/// Returns the SQL boolean expression text.
	pub fn boolean_expression(&self) -> &str {
		self.boolean_expression.as_ref()
	}

	fn from_cow(boolean_expression: Cow<'a, str>) -> Result<Self, ComponentSchemaError> {
		validate_component_schema_sql(
			boolean_expression.as_ref(),
			"component schema validation boolean expression",
		)?;
		validate_component_schema_sql_unit(
			boolean_expression.as_ref(),
			"component schema validation boolean expression",
		)?;
		Ok(Self { boolean_expression })
	}
}

impl<'a> ComponentSchemaMigration<'a> {
	/// Creates a physical migration for one supported version/fingerprint step.
	pub const fn new(
		step: ComponentSchemaMigrationStep<'a>,
		statements: &'a [ComponentSchemaStatement<'a>],
	) -> Self {
		Self { step, statements }
	}

	/// Returns the version/fingerprint step that this migration implements.
	pub const fn step(&self) -> ComponentSchemaMigrationStep<'a> {
		self.step
	}

	/// Returns the SQL statements for this physical migration.
	pub const fn statements(&self) -> &'a [ComponentSchemaStatement<'a>] {
		self.statements
	}
}

impl<'a> ComponentSchema<'a> {
	/// Creates a component schema description.
	///
	/// Validation checks are required because the schema ledger must never
	/// become the only proof that physical tables match the claimed version.
	/// Fresh-install statements may be empty for schemas that are only valid as
	/// upgrades from earlier versions, but validation checks must always prove
	/// the final physical shape.
	pub fn new(
		current_version: ComponentSchemaVersion<'a>,
		fresh_install_statements: &'a [ComponentSchemaStatement<'a>],
		migrations: &'a [ComponentSchemaMigration<'a>],
		validation_checks: &'a [ComponentSchemaValidationCheck<'a>],
	) -> Result<Self, ComponentSchemaError> {
		validate_component_schema_version(current_version)
			.map_err(|error| ComponentSchemaError::new(error.to_string()))?;
		if validation_checks.is_empty() {
			return Err(ComponentSchemaError::new(
				"component schema validation checks must not be empty",
			));
		}
		Ok(Self {
			current_version,
			fresh_install_statements,
			migrations,
			validation_checks,
			compatible_versions: &[],
		})
	}

	/// Declares additional exact layouts that this release can use.
	///
	/// A preparation release can retain its installation target and declare a
	/// later compatible layout here. Once another release installs that layout,
	/// both migration and validation check the installed layout and preserve it.
	/// Validation also accepts declared older layouts. Migration upgrades an older
	/// layout to the installation target through the declared migration chain.
	/// Unknown versions and fingerprints still fail. An incompatible SQL change
	/// requires the application to retire code that cannot use the new layout;
	/// Paranoid cannot infer compatibility from arbitrary application SQL.
	///
	/// ```
	/// use paranoid::installation::{
	///     ComponentSchema, ComponentSchemaCompatibility, ComponentSchemaMigrationVersion,
	///     ComponentSchemaStatement, ComponentSchemaValidationCheck, ComponentSchemaVersion,
	/// };
	/// let fresh = [ComponentSchemaStatement::from_static_sql(
	///     "CREATE TABLE app.widgets (id bigint PRIMARY KEY)",
	/// )?];
	/// let base_checks = [ComponentSchemaValidationCheck::from_static_boolean_expression(
	///     "pg_catalog.to_regclass('app.widgets') IS NOT NULL",
	/// )?];
	/// let expanded_checks = [ComponentSchemaValidationCheck::from_static_boolean_expression(
	///     "EXISTS (SELECT 1 FROM pg_catalog.pg_attribute WHERE attrelid = pg_catalog.to_regclass('app.widgets') AND attname = 'label' AND NOT attisdropped)",
	/// )?];
	/// let compatible = [ComponentSchemaCompatibility::new(
	///     ComponentSchemaMigrationVersion::new(2, "widgets-v2"), &expanded_checks,
	/// )];
	/// let schema = ComponentSchema::new(
	///     ComponentSchemaVersion {
	///         component: "app.widgets", instance_key: "main", version: 1, fingerprint: "widgets-v1",
	///     },
	///     &fresh, &[], &base_checks,
	/// )?.with_compatible_versions(&compatible)?;
	/// assert_eq!(schema.current_version().version, 1);
	/// # Ok::<(), Box<dyn std::error::Error>>(())
	/// ```
	pub fn with_compatible_versions(
		mut self,
		versions: &'a [ComponentSchemaCompatibility<'a>],
	) -> Result<Self, ComponentSchemaError> {
		for (index, compatible) in versions.iter().enumerate() {
			validate_component_schema_version(ComponentSchemaVersion {
				version: compatible.version.version(),
				fingerprint: compatible.version.fingerprint(),
				..self.current_version
			})
			.map_err(|error| ComponentSchemaError::new(error.to_string()))?;
			if compatible.validation_checks.is_empty() {
				return Err(ComponentSchemaError::new(
					"compatible schema checks must not be empty",
				));
			}
			if compatible.version.version() == self.current_version.version
				|| versions[..index]
					.iter()
					.any(|prior| prior.version.version() == compatible.version.version())
			{
				return Err(ComponentSchemaError::new(
					"each compatible schema version must be distinct from the installation target and other compatible versions",
				));
			}
		}
		self.compatible_versions = versions;
		Ok(self)
	}

	/// Returns the installation target's version identity.
	pub const fn current_version(&self) -> ComponentSchemaVersion<'a> {
		self.current_version
	}

	/// Returns the fresh-install SQL statements.
	pub const fn fresh_install_statements(&self) -> &'a [ComponentSchemaStatement<'a>] {
		self.fresh_install_statements
	}

	/// Returns the ordered migration definitions.
	pub const fn migrations(&self) -> &'a [ComponentSchemaMigration<'a>] {
		self.migrations
	}

	/// Returns the physical validation checks.
	pub const fn validation_checks(&self) -> &'a [ComponentSchemaValidationCheck<'a>] {
		self.validation_checks
	}

	/// Returns additional layouts that this release explicitly supports.
	pub const fn compatible_versions(&self) -> &'a [ComponentSchemaCompatibility<'a>] {
		self.compatible_versions
	}
}

impl<'a> ComponentSchemaCompatibility<'a> {
	/// Describes an exact compatible layout and its physical checks.
	/// [`ComponentSchema::with_compatible_versions`] checks the version, fingerprint,
	/// and nonempty checks before accepting this declaration.
	pub const fn new(
		version: super::ComponentSchemaMigrationVersion<'a>,
		validation_checks: &'a [ComponentSchemaValidationCheck<'a>],
	) -> Self {
		Self {
			version,
			validation_checks,
		}
	}

	/// Returns the supported version and fingerprint.
	pub const fn version(&self) -> super::ComponentSchemaMigrationVersion<'a> {
		self.version
	}

	/// Returns the physical checks for this supported layout.
	pub const fn validation_checks(&self) -> &'a [ComponentSchemaValidationCheck<'a>] {
		self.validation_checks
	}
}

impl<'a> ComponentSchema<'a> {
	pub(crate) fn checks_for(
		&self,
		version: i32,
		fingerprint: &str,
	) -> Option<&'a [ComponentSchemaValidationCheck<'a>]> {
		if version == self.current_version.version
			&& fingerprint == self.current_version.fingerprint
		{
			Some(self.validation_checks)
		} else {
			self.compatible_versions
				.iter()
				.find(|compatible| {
					compatible.version.version() == version
						&& compatible.version.fingerprint() == fingerprint
				})
				.map(|compatible| compatible.validation_checks)
		}
	}

	pub(crate) fn plan<'s>(
		&self,
		recorded: Option<super::RecordedComponentSchemaVersion>,
		steps: &'s [super::ComponentSchemaMigrationStep<'s>],
	) -> Result<super::ComponentSchemaMigrationPlan<'s>, SchemaError> {
		// Check the declaration even when the installed layout needs no migration.
		super::plan_component_schema_migration(self.current_version, None, steps)?;
		if let Some(recorded) = &recorded
			&& recorded.version > self.current_version.version
			&& self
				.checks_for(recorded.version, &recorded.fingerprint)
				.is_some()
		{
			return Ok(super::ComponentSchemaMigrationPlan::AlreadyCompatible {
				recorded: recorded.clone(),
			});
		}
		super::plan_component_schema_migration(self.current_version, recorded, steps)
	}

	pub(crate) fn migration_steps(&self) -> Vec<ComponentSchemaMigrationStep<'a>> {
		self.migrations
			.iter()
			.map(ComponentSchemaMigration::step)
			.collect()
	}
}

/// Runs a planned migration's statements and then the validation checks in the caller's
/// transaction. The caller records the new version only after this returns success.
pub(crate) async fn execute_component_schema_plan(
	tx: &mut Tx<'_>,
	schema: &ComponentSchema<'_>,
	plan: &ComponentSchemaMigrationPlan<'_>,
) -> Result<ComponentSchemaMigrationResult, SchemaError> {
	let outcome = match plan {
		ComponentSchemaMigrationPlan::FreshInstall => {
			execute_component_schema_statements(
				tx,
				COMPONENT_SCHEMA_EXECUTE_FRESH_INSTALL_STATEMENT_LABEL,
				schema.fresh_install_statements,
			)
			.await?;
			ComponentSchemaMigrationResult::FreshInstall {
				version: schema.current_version.version,
			}
		}
		ComponentSchemaMigrationPlan::AlreadyCurrent => {
			ComponentSchemaMigrationResult::AlreadyCurrent {
				version: schema.current_version.version,
			}
		}
		ComponentSchemaMigrationPlan::AlreadyCompatible { recorded } => {
			execute_component_schema_validation_checks(
				tx,
				schema
					.checks_for(recorded.version, &recorded.fingerprint)
					.expect("the planner selected an explicitly compatible layout"),
			)
			.await?;
			return Ok(ComponentSchemaMigrationResult::AlreadyCompatible {
				version: recorded.version,
			});
		}
		ComponentSchemaMigrationPlan::Upgrade { from, steps } => {
			for step in steps {
				let migration = component_schema_migration_for_step(schema, *step)?;
				execute_component_schema_statements(
					tx,
					COMPONENT_SCHEMA_EXECUTE_UPGRADE_STATEMENT_LABEL,
					migration.statements,
				)
				.await?;
			}
			ComponentSchemaMigrationResult::Upgraded {
				from_version: from.version,
				to_version: schema.current_version.version,
				steps_applied: steps.len(),
			}
		}
	};
	execute_component_schema_validation_checks(tx, schema.validation_checks).await?;
	Ok(outcome)
}

/// Runs only the validation checks in the caller's transaction.
pub(crate) async fn execute_component_schema_checks(
	tx: &mut Tx<'_>,
	checks: &[ComponentSchemaValidationCheck<'_>],
) -> Result<(), SchemaError> {
	execute_component_schema_validation_checks(tx, checks).await
}

async fn execute_component_schema_statements(
	tx: &mut Tx<'_>,
	operation_label: &'static str,
	statements: &[ComponentSchemaStatement<'_>],
) -> Result<(), SchemaError> {
	for statement in statements {
		unparameterized_simple_query(sqlx::AssertSqlSafe(statement.as_str()))
			.execute(tx.labeled_application_executor(operation_label))
			.await
			.map_err(|error| {
				DatabaseError::for_statement(operation_label, statement.as_str(), error)
			})?;
	}
	Ok(())
}

async fn execute_component_schema_validation_checks(
	tx: &mut Tx<'_>,
	validation_checks: &[ComponentSchemaValidationCheck<'_>],
) -> Result<(), SchemaError> {
	for validation_check in validation_checks {
		let statement = format!(
			"SELECT ({}) AS component_schema_validation_check",
			validation_check.boolean_expression()
		);
		let rows = unparameterized_simple_query(sqlx::AssertSqlSafe(statement.clone()))
			.fetch_all(
				tx.labeled_application_executor(COMPONENT_SCHEMA_EXECUTE_VALIDATION_CHECK_LABEL),
			)
			.await
			.map_err(|error| {
				DatabaseError::for_statement(
					COMPONENT_SCHEMA_EXECUTE_VALIDATION_CHECK_LABEL,
					&statement,
					error,
				)
			})?;
		if rows.len() != 1 {
			return Err(SchemaError::mismatch(format!(
				"component schema validation check returned {} rows instead of exactly one: {}",
				rows.len(),
				validation_check.boolean_expression()
			)));
		}
		let row = &rows[0];
		let passed = row.try_get::<Option<bool>, _>(0).map_err(|error| {
			DatabaseError::for_statement(
				COMPONENT_SCHEMA_EXECUTE_VALIDATION_CHECK_LABEL,
				&statement,
				error,
			)
		})?;
		match passed {
			Some(true) => {}
			Some(false) => {
				return Err(SchemaError::mismatch(format!(
					"component schema validation check returned false: {}",
					validation_check.boolean_expression()
				)));
			}
			None => {
				return Err(SchemaError::mismatch(format!(
					"component schema validation check returned null: {}",
					validation_check.boolean_expression()
				)));
			}
		}
	}
	Ok(())
}

fn component_schema_migration_for_step<'schema, 'a>(
	schema: &'schema ComponentSchema<'a>,
	step: ComponentSchemaMigrationStep<'a>,
) -> Result<&'schema ComponentSchemaMigration<'a>, SchemaError> {
	schema
		.migrations
		.iter()
		.find(|migration| migration.step == step)
		.ok_or_else(|| {
			SchemaError::mismatch(format!(
				"component schema migration for step {} fingerprint {:?} to {} fingerprint {:?} was not found",
				step.from.version(),
				step.from.fingerprint(),
				step.to.version(),
				step.to.fingerprint()
			))
		})
}

fn validate_component_schema_sql(
	sql: &str,
	label: &'static str,
) -> Result<(), ComponentSchemaError> {
	if sql.trim().is_empty() {
		return Err(ComponentSchemaError::new(format!(
			"{label} must not be empty"
		)));
	}
	if sql.as_bytes().contains(&0) {
		return Err(ComponentSchemaError::new(format!(
			"{label} must not contain null bytes"
		)));
	}
	Ok(())
}

fn validate_component_schema_sql_unit(
	sql: &str,
	label: &'static str,
) -> Result<(), ComponentSchemaError> {
	let lexer = PgLexer::new(
		sql,
		PgLexerOptions {
			literal_mode: PgLiteralMode::Portable,
		},
	);
	let mut tokens = lexer.filter_map(|lexeme| match lexeme {
		Ok(PgLexeme::Protected(text)) if text.starts_with("--") || text.starts_with("/*") => None,
		Ok(PgLexeme::Code(code)) if code.trim().is_empty() => None,
		lexeme => Some(lexeme),
	});
	let first = tokens
		.next()
		.ok_or_else(|| {
			ComponentSchemaError::new(format!("{label} must contain SQL, not only comments"))
		})?
		.map_err(|error| component_schema_lex_error(error, label))?;
	if let PgLexeme::Code(code) = first {
		require_no_statement_separator(code, label)?;
		// The migration and its ledger entry must retain their owning transaction.
		if controls_transaction(code) {
			return Err(ComponentSchemaError::new(format!(
				"{label} must not begin, end, or split the transaction"
			)));
		}
	}
	for token in tokens {
		if let PgLexeme::Code(code) =
			token.map_err(|error| component_schema_lex_error(error, label))?
		{
			require_no_statement_separator(code, label)?;
		}
	}
	Ok(())
}

fn require_no_statement_separator(
	code: &str,
	label: &'static str,
) -> Result<(), ComponentSchemaError> {
	if code.as_bytes().contains(&b';') {
		return Err(ComponentSchemaError::new(format!(
			"{label} must be one SQL unit with no top-level semicolons"
		)));
	}
	Ok(())
}

/// Whether the statement that begins with `code` is a transaction control statement.
fn controls_transaction(code: &str) -> bool {
	let first_word = code
		.trim_start()
		.split(|character: char| !character.is_ascii_alphabetic())
		.next()
		.unwrap_or_default();
	[
		"abort",
		"begin",
		"commit",
		"end",
		"prepare",
		"release",
		"rollback",
		"savepoint",
		"start",
	]
	.iter()
	.any(|keyword| first_word.eq_ignore_ascii_case(keyword))
}

fn component_schema_lex_error(error: PgLexError, label: &'static str) -> ComponentSchemaError {
	let detail = match error {
		PgLexError::UnterminatedStringLiteral => "contains an unterminated string literal",
		PgLexError::OrdinaryStringBackslash => {
			"contains a backslash in an ordinary string literal; use an explicit E-prefixed escape string so scanning is independent of standard_conforming_strings"
		}
		PgLexError::EscapeStringBackslashQuote => {
			"contains a backslash-escaped apostrophe in an escape string; write a doubled apostrophe because the backslash_quote setting decides whether PostgreSQL accepts the backslash form"
		}
		PgLexError::UnterminatedQuotedIdentifier => "contains an unterminated quoted identifier",
		PgLexError::TooManyNestedBlockComments => "contains too many nested block comments",
		PgLexError::UnterminatedBlockComment => "contains an unterminated block comment",
		PgLexError::UnterminatedDollarQuotedString => {
			"contains an unterminated dollar-quoted string"
		}
		PgLexError::AmbiguousDollarAfterNumber => {
			"contains a dollar sign immediately after a numeric literal; separate them with whitespace so scanning is unambiguous"
		}
	};
	ComponentSchemaError::new(format!("{label} {detail}"))
}

#[cfg(test)]
mod component_schema_statement_scanner_tests {
	use super::*;

	fn statement(
		sql: &'static str,
	) -> Result<ComponentSchemaStatement<'static>, ComponentSchemaError> {
		ComponentSchemaStatement::from_static_sql(sql)
	}

	#[test]
	fn rejects_top_level_statement_separators() {
		assert!(statement("CREATE TABLE one (id bigint); CREATE TABLE two (id bigint)").is_err());
		assert!(statement("SELECT 1;").is_err());

		// `$` is legal inside an unquoted PostgreSQL identifier. Neither a
		// tagged nor untagged identifier suffix may be mistaken for a pair of
		// dollar-quote delimiters that hides the statement separator.
		assert!(statement("SELECT 1 AS foo$tag$; SELECT 2 AS bar$tag$").is_err());
		assert!(statement("SELECT 1 AS foo$$; SELECT 2 AS bar$$").is_err());
		assert!(statement("SELECT 1 AS café$tag$; SELECT 2 AS thé$tag$").is_err());

		// PostgreSQL treats both LF and CR as line endings for `--` comments.
		assert!(statement("SELECT 1 -- comment\r; SELECT 2").is_err());

		// With `standard_conforming_strings=off`, these are two valid SELECTs
		// but a mode-naive scanner pairs the escaped quotes in the opposite
		// direction and can hide the separator. Ordinary string backslashes
		// are rejected so validation is independent of the session setting.
		assert!(statement(r"SELECT 'a\'b'; SELECT 'c\'d'").is_err());
		assert!(statement(r"SELECT 'ordinary\backslash'").is_err());
	}

	#[test]
	fn rejects_transaction_control() {
		assert!(statement("COMMIT").is_err());
		assert!(statement("  /* lead */ rollback").is_err());
		assert!(statement("SAVEPOINT before_migration").is_err());
		statement("CREATE TABLE commit_log (id bigint)").expect("a table named after a keyword");
		statement("SELECT 'COMMIT'").expect("a keyword inside a literal");
	}

	#[test]
	fn rejects_comment_only_units() {
		assert!(statement("-- no statement").is_err());
		assert!(statement("/* no statement */").is_err());
		assert!(statement(" \n /* outer /* nested */ comment */ \r -- tail").is_err());
	}

	#[test]
	fn allows_semicolons_inside_pg_quoted_constructs() {
		statement("SELECT ';' AS value").expect("single-quoted literal");
		statement("SELECT \"a;b\" FROM relation").expect("quoted identifier");
		statement("SELECT 1 /* ; /* nested ; */ ; */").expect("nested block comment");
		statement("SELECT 1 -- ;\n").expect("line comment");
		statement("CREATE FUNCTION f() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.id := 1; RETURN NEW; END; $$")
            .expect("dollar-quoted function body");
		statement("CREATE FUNCTION f() RETURNS text LANGUAGE sql AS $body$ SELECT ';'; $body$")
			.expect("tagged dollar-quoted function body");
		statement("SELECT E'doubled quote: ''; and semicolon: ;'").expect("escape string literal");
	}

	#[test]
	fn rejects_a_dollar_sign_after_an_identifier_or_digit_as_a_quote_opener() {
		assert!(statement("SELECT 1 AS prefix$tag$; SELECT 2").is_err());
		assert!(statement("SELECT 1 AS café$tag$; SELECT 2").is_err());
		assert!(statement("SELECT $1$; SELECT 2").is_err());
	}

	#[test]
	fn rejects_backslash_escaped_apostrophes_whose_meaning_depends_on_backslash_quote() {
		assert!(statement(r"SELECT E'can\'t'").is_err());
		assert!(statement(r"SELECT e'can\'t'").is_err());
	}

	#[test]
	fn a_schema_may_have_only_migrations_but_must_have_validation_checks() {
		let version = ComponentSchemaVersion {
			component: "test",
			instance_key: "table=test",
			version: 2,
			fingerprint: "v2",
		};
		let step = ComponentSchemaMigrationStep {
			from: super::super::ComponentSchemaMigrationVersion::new(1, "v1"),
			to: super::super::ComponentSchemaMigrationVersion::new(2, "v2"),
		};
		let statements = [statement("ALTER TABLE t ADD x int").expect("statement")];
		let migrations = [ComponentSchemaMigration::new(step, &statements)];
		let checks = [
			ComponentSchemaValidationCheck::from_static_boolean_expression("TRUE").expect("check"),
		];

		let schema = ComponentSchema::new(version, &[], &migrations, &checks).expect("schema");
		assert!(schema.fresh_install_statements().is_empty());
		assert_eq!(schema.migrations().len(), 1);
		assert!(ComponentSchema::new(version, &[], &migrations, &[]).is_err());
	}

	#[test]
	fn rejects_unterminated_quoted_constructs() {
		assert!(statement("SELECT 'unterminated").is_err());
		assert!(statement("SELECT \"unterminated").is_err());
		assert!(statement("SELECT /* unterminated").is_err());
		assert!(statement("CREATE FUNCTION f() RETURNS text AS $$ body").is_err());
	}
}
