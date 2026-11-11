use super::RecordedComponentSchemaVersion;
use crate::db::{
	DatabaseOperationKind, PG_COLUMN_METADATA_SQL, PgIdentifier, PgQualifiedTableName, PgSqlState,
	SchemaError, Tx, owned_portable_query, owned_portable_query_as, owned_portable_query_scalar,
};

mod create_attempt;
use create_attempt::SchemaLedgerCreateAttempt;
#[doc(hidden)]
pub const SCHEMA_LEDGER_CREATE_SAVEPOINT_LABEL: &str = "db.schema_ledger.create_savepoint";
#[doc(hidden)]
pub const SCHEMA_LEDGER_CREATE_TABLE_LABEL: &str = "db.schema_ledger.create_table";
#[doc(hidden)]
pub const SCHEMA_LEDGER_RELEASE_SAVEPOINT_LABEL: &str = "db.schema_ledger.release_savepoint";
pub(crate) const SCHEMA_LEDGER_ROLLBACK_SAVEPOINT_LABEL: &str =
	"db.schema_ledger.rollback_savepoint";
#[doc(hidden)]
pub const SCHEMA_LEDGER_VALIDATE_COLUMNS_LABEL: &str = "db.schema_ledger.validate_columns";
#[doc(hidden)]
pub const SCHEMA_LEDGER_VALIDATE_PRIMARY_KEY_LABEL: &str = "db.schema_ledger.validate_primary_key";
#[doc(hidden)]
pub const SCHEMA_LEDGER_VALIDATE_CHECK_CONSTRAINTS_LABEL: &str =
	"db.schema_ledger.validate_check_constraints";
#[doc(hidden)]
pub const SCHEMA_LEDGER_CLAIM_COMPONENT_VERSION_LABEL: &str =
	"db.schema_ledger.claim_component_version";
#[doc(hidden)]
pub const SCHEMA_LEDGER_LOCK_COMPONENT_VERSION_LABEL: &str =
	"db.schema_ledger.lock_component_version";
#[doc(hidden)]
pub const SCHEMA_LEDGER_RECORD_COMPONENT_VERSION_LABEL: &str =
	"db.schema_ledger.record_component_version";
#[doc(hidden)]
pub const SCHEMA_LEDGER_UPDATE_COMPONENT_VERSION_LABEL: &str =
	"db.schema_ledger.update_component_version";
#[doc(hidden)]
pub const SCHEMA_LEDGER_FETCH_COMPONENT_VERSION_LABEL: &str =
	"db.schema_ledger.fetch_component_version";

pub(crate) const MAX_SCHEMA_LEDGER_COMPONENT_LEN: usize = 128;
pub(crate) const MAX_SCHEMA_LEDGER_INSTANCE_KEY_LEN: usize = 1024;
pub(crate) const MAX_SCHEMA_LEDGER_FINGERPRINT_LEN: usize = 256;
const SCHEMA_LEDGER_C_COLLATION: &str = "C";
const SCHEMA_LEDGER_SCHEMA_COMPONENT: &str = "schema_ledger";
const SCHEMA_LEDGER_SCHEMA_VERSION: i32 = 1;
const SCHEMA_LEDGER_SCHEMA_FINGERPRINT: &str = "paranoid.schema_ledger.v1";

const SCHEMA_LEDGER_REQUIRED_COLUMNS: [SchemaLedgerColumn; 5] = [
	SchemaLedgerColumn::Component,
	SchemaLedgerColumn::InstanceKey,
	SchemaLedgerColumn::SchemaVersion,
	SchemaLedgerColumn::SchemaFingerprint,
	SchemaLedgerColumn::AppliedAt,
];
const SCHEMA_LEDGER_PRIMARY_KEY_COLUMNS: [SchemaLedgerColumn; 2] = [
	SchemaLedgerColumn::Component,
	SchemaLedgerColumn::InstanceKey,
];
const SCHEMA_LEDGER_CHECKED_COLUMNS: [SchemaLedgerColumn; 4] = [
	SchemaLedgerColumn::Component,
	SchemaLedgerColumn::InstanceKey,
	SchemaLedgerColumn::SchemaVersion,
	SchemaLedgerColumn::SchemaFingerprint,
];

/// Current schema version identity for one registered schema-family instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ComponentSchemaVersion<'a> {
	/// Stable schema-family name, such as `paranoid.kv` or `cook.md_pages`.
	pub component: &'a str,
	/// The stable key of this instance of the component's tables.
	///
	/// Use [`component_schema_instance_key_for_tables`] when the instance is
	/// defined by one or more table names.
	pub instance_key: &'a str,
	/// Current supported schema version.
	pub version: i32,
	/// Fingerprint of the canonical schema shape for this version.
	pub fingerprint: &'a str,
}

/// Recorded metadata for one component schema instance in an installation's ledger.
///
/// This status does not show that the component's tables are valid.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ComponentSchemaRegistration {
	/// The ledger has no row for this component and instance key. This does not show whether
	/// the component's tables exist.
	Absent,
	/// The ledger records this version and fingerprint, regardless of the supplied
	/// declaration's expected version or the validation checks.
	Recorded {
		/// Positive recorded schema version.
		version: i32,
		/// Recorded schema fingerprint.
		fingerprint: String,
	},
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SchemaLedgerColumn {
	Component,
	InstanceKey,
	SchemaVersion,
	SchemaFingerprint,
	AppliedAt,
}

impl SchemaLedgerColumn {
	const fn name(self) -> &'static str {
		match self {
			Self::Component => "component",
			Self::InstanceKey => "instance_key",
			Self::SchemaVersion => "schema_version",
			Self::SchemaFingerprint => "schema_fingerprint",
			Self::AppliedAt => "applied_at",
		}
	}

	const fn create_table_type(self) -> &'static str {
		match self {
			Self::Component | Self::InstanceKey | Self::SchemaFingerprint => "pg_catalog.text",
			Self::SchemaVersion => "INTEGER",
			Self::AppliedAt => "pg_catalog.timestamptz",
		}
	}

	const fn validation_type(self) -> &'static str {
		match self {
			Self::Component | Self::InstanceKey | Self::SchemaFingerprint => "text",
			Self::SchemaVersion => "integer",
			Self::AppliedAt => "timestamp with time zone",
		}
	}

	const fn required_collation(self) -> Option<&'static str> {
		match self {
			Self::Component | Self::InstanceKey | Self::SchemaFingerprint => {
				Some(SCHEMA_LEDGER_C_COLLATION)
			}
			Self::SchemaVersion | Self::AppliedAt => None,
		}
	}

	const fn max_octet_length(self) -> Option<usize> {
		match self {
			Self::Component => Some(MAX_SCHEMA_LEDGER_COMPONENT_LEN),
			Self::InstanceKey => Some(MAX_SCHEMA_LEDGER_INSTANCE_KEY_LEN),
			Self::SchemaFingerprint => Some(MAX_SCHEMA_LEDGER_FINGERPRINT_LEN),
			Self::SchemaVersion | Self::AppliedAt => None,
		}
	}

	fn normalized_check_constraint(self) -> Option<String> {
		let column = self.name();
		match self {
			Self::Component | Self::InstanceKey | Self::SchemaFingerprint => {
				let max_octet_length = self
					.max_octet_length()
					.expect("checked text column must have a max length");
				Some(format!(
					"(octet_length({column})>0)AND(octet_length({column})<={max_octet_length})"
				))
			}
			Self::SchemaVersion => Some(format!("{column}>0")),
			Self::AppliedAt => None,
		}
	}
}

struct SchemaLedgerCatalog<'a> {
	table_name: &'a PgQualifiedTableName,
}

impl<'a> SchemaLedgerCatalog<'a> {
	fn new(table_name: &'a PgQualifiedTableName) -> Self {
		Self { table_name }
	}

	fn quoted_table_name(&self) -> String {
		self.table_name.quoted().to_string()
	}

	fn create_table_statement(&self) -> String {
		let component = SchemaLedgerColumn::Component.name();
		let component_type = SchemaLedgerColumn::Component.create_table_type();
		let instance_key = SchemaLedgerColumn::InstanceKey.name();
		let instance_key_type = SchemaLedgerColumn::InstanceKey.create_table_type();
		let schema_version = SchemaLedgerColumn::SchemaVersion.name();
		let schema_version_type = SchemaLedgerColumn::SchemaVersion.create_table_type();
		let schema_fingerprint = SchemaLedgerColumn::SchemaFingerprint.name();
		let schema_fingerprint_type = SchemaLedgerColumn::SchemaFingerprint.create_table_type();
		let applied_at = SchemaLedgerColumn::AppliedAt.name();
		let applied_at_type = SchemaLedgerColumn::AppliedAt.create_table_type();
		let primary_key_columns = schema_ledger_column_list(&SCHEMA_LEDGER_PRIMARY_KEY_COLUMNS);

		format!(
			r#"
        CREATE TABLE IF NOT EXISTS {} (
            {component} {component_type} COLLATE pg_catalog."C" NOT NULL CHECK (
                pg_catalog.octet_length({component}) OPERATOR(pg_catalog.>) 0
                AND pg_catalog.octet_length({component}) OPERATOR(pg_catalog.<=) {MAX_SCHEMA_LEDGER_COMPONENT_LEN}
            ),
            {instance_key} {instance_key_type} COLLATE pg_catalog."C" NOT NULL CHECK (
                pg_catalog.octet_length({instance_key}) OPERATOR(pg_catalog.>) 0
                AND pg_catalog.octet_length({instance_key}) OPERATOR(pg_catalog.<=) {MAX_SCHEMA_LEDGER_INSTANCE_KEY_LEN}
            ),
            {schema_version} {schema_version_type} NOT NULL CHECK ({schema_version} OPERATOR(pg_catalog.>) 0),
            {schema_fingerprint} {schema_fingerprint_type} COLLATE pg_catalog."C" NOT NULL CHECK (
                pg_catalog.octet_length({schema_fingerprint}) OPERATOR(pg_catalog.>) 0
                AND pg_catalog.octet_length({schema_fingerprint}) OPERATOR(pg_catalog.<=) {MAX_SCHEMA_LEDGER_FINGERPRINT_LEN}
            ),
            {applied_at} {applied_at_type} NOT NULL,
            PRIMARY KEY ({primary_key_columns})
        )
        "#,
			self.table_name.quoted()
		)
	}

	fn claim_component_version_statement(&self) -> String {
		let component = SchemaLedgerColumn::Component.name();
		let instance_key = SchemaLedgerColumn::InstanceKey.name();
		let schema_version = SchemaLedgerColumn::SchemaVersion.name();
		let schema_fingerprint = SchemaLedgerColumn::SchemaFingerprint.name();
		let applied_at = SchemaLedgerColumn::AppliedAt.name();
		let primary_key_columns = schema_ledger_column_list(&SCHEMA_LEDGER_PRIMARY_KEY_COLUMNS);

		format!(
			r#"
        INSERT INTO {} (
            {component},
            {instance_key},
            {schema_version},
            {schema_fingerprint},
            {applied_at}
        )
        VALUES ($1, $2, $3, $4, pg_catalog.statement_timestamp())
        ON CONFLICT ({primary_key_columns})
        DO NOTHING
        "#,
			self.table_name.quoted()
		)
	}

	fn lock_component_version_statement(&self) -> String {
		let component = SchemaLedgerColumn::Component.name();
		let instance_key = SchemaLedgerColumn::InstanceKey.name();
		let schema_version = SchemaLedgerColumn::SchemaVersion.name();
		let schema_fingerprint = SchemaLedgerColumn::SchemaFingerprint.name();

		format!(
			r#"
        SELECT {schema_version}, {schema_fingerprint}
        FROM {}
        WHERE {component} OPERATOR(pg_catalog.=) $1
          AND {instance_key} OPERATOR(pg_catalog.=) $2
        FOR UPDATE
        "#,
			self.table_name.quoted()
		)
	}

	fn fetch_component_version_statement(&self) -> String {
		let component = SchemaLedgerColumn::Component.name();
		let instance_key = SchemaLedgerColumn::InstanceKey.name();
		let schema_version = SchemaLedgerColumn::SchemaVersion.name();
		let schema_fingerprint = SchemaLedgerColumn::SchemaFingerprint.name();

		format!(
			r#"
        SELECT {schema_version}, {schema_fingerprint}
        FROM {}
        WHERE {component} OPERATOR(pg_catalog.=) $1
          AND {instance_key} OPERATOR(pg_catalog.=) $2
        "#,
			self.table_name.quoted()
		)
	}

	fn update_component_version_statement(&self) -> String {
		let component = SchemaLedgerColumn::Component.name();
		let instance_key = SchemaLedgerColumn::InstanceKey.name();
		let schema_version = SchemaLedgerColumn::SchemaVersion.name();
		let schema_fingerprint = SchemaLedgerColumn::SchemaFingerprint.name();
		let applied_at = SchemaLedgerColumn::AppliedAt.name();

		format!(
			r#"
        UPDATE {}
        SET
            {schema_version} = $5,
            {schema_fingerprint} = $6,
            {applied_at} = pg_catalog.statement_timestamp()
        WHERE {component} OPERATOR(pg_catalog.=) $1
          AND {instance_key} OPERATOR(pg_catalog.=) $2
          AND {schema_version} OPERATOR(pg_catalog.=) $3
          AND {schema_fingerprint} OPERATOR(pg_catalog.=) $4
        "#,
			self.table_name.quoted()
		)
	}

	fn complete_fresh_component_version_statement(&self) -> String {
		let component = SchemaLedgerColumn::Component.name();
		let instance_key = SchemaLedgerColumn::InstanceKey.name();
		let schema_version = SchemaLedgerColumn::SchemaVersion.name();
		let schema_fingerprint = SchemaLedgerColumn::SchemaFingerprint.name();
		let applied_at = SchemaLedgerColumn::AppliedAt.name();

		format!(
			r#"
        UPDATE {}
        SET {applied_at} = pg_catalog.statement_timestamp()
        WHERE {component} OPERATOR(pg_catalog.=) $1
          AND {instance_key} OPERATOR(pg_catalog.=) $2
          AND {schema_version} OPERATOR(pg_catalog.=) $3
          AND {schema_fingerprint} OPERATOR(pg_catalog.=) $4
        "#,
			self.table_name.quoted()
		)
	}
}

fn schema_ledger_column_list(columns: &[SchemaLedgerColumn]) -> String {
	columns
		.iter()
		.map(|column| column.name())
		.collect::<Vec<_>>()
		.join(", ")
}

#[cfg(feature = "__internal")]
#[doc(hidden)]
pub fn schema_ledger_table_name_for_tests() -> PgQualifiedTableName {
	crate::db::table_name_for_tests("__paranoid_test_schema_ledger")
}

pub(crate) async fn record_component_schema_migration_completion_in_current_transaction(
	tx: &mut Tx<'_>,
	table_name: &PgQualifiedTableName,
	component_schema_version: ComponentSchemaVersion<'_>,
	prior_recorded_version: Option<&RecordedComponentSchemaVersion>,
) -> Result<(), SchemaError> {
	validate_component_schema_version(component_schema_version)?;
	let catalog = SchemaLedgerCatalog::new(table_name);

	if let Some(prior_recorded_version) = prior_recorded_version {
		let statement = catalog.update_component_version_statement();
		tx.record_database_operation(
			DatabaseOperationKind::Execute,
			SCHEMA_LEDGER_UPDATE_COMPONENT_VERSION_LABEL,
			Some(statement.as_str()),
		);
		let rows_affected = owned_portable_query(sqlx::AssertSqlSafe(statement.as_str()))
			.bind_validated(component_schema_version.component)
			.bind_validated(component_schema_version.instance_key)
			.bind_validated(prior_recorded_version.version)
			.bind_validated(prior_recorded_version.fingerprint.as_str())
			.bind_validated(component_schema_version.version)
			.bind_validated(component_schema_version.fingerprint)
			.execute(tx.inner.as_mut())
			.await
			.map_err(|error| {
				error.with_operation_label(SCHEMA_LEDGER_UPDATE_COMPONENT_VERSION_LABEL)
			})?
			.rows_affected();
		if rows_affected != 1 {
			return Err(SchemaError::mismatch(format!(
				"schema ledger row for component {:?} instance {:?} changed before migration completion",
				component_schema_version.component, component_schema_version.instance_key
			)));
		}
	} else {
		let statement = catalog.complete_fresh_component_version_statement();
		tx.record_database_operation(
			DatabaseOperationKind::Execute,
			SCHEMA_LEDGER_RECORD_COMPONENT_VERSION_LABEL,
			Some(statement.as_str()),
		);
		let rows_affected = owned_portable_query(sqlx::AssertSqlSafe(statement.as_str()))
			.bind_validated(component_schema_version.component)
			.bind_validated(component_schema_version.instance_key)
			.bind_validated(component_schema_version.version)
			.bind_validated(component_schema_version.fingerprint)
			.execute(tx.inner.as_mut())
			.await
			.map_err(|error| {
				error.with_operation_label(SCHEMA_LEDGER_RECORD_COMPONENT_VERSION_LABEL)
			})?
			.rows_affected();
		if rows_affected != 1 {
			return Err(SchemaError::mismatch(format!(
				"schema ledger row for component {:?} instance {:?} was not claimed before migration completion",
				component_schema_version.component, component_schema_version.instance_key
			)));
		}
	}

	validate_component_schema_version_row_in_current_transaction(
		tx,
		table_name,
		component_schema_version,
	)
	.await
}

pub(crate) async fn validate_component_schema_version_in_current_transaction(
	tx: &mut Tx<'_>,
	table_name: &PgQualifiedTableName,
	component_schema_version: ComponentSchemaVersion<'_>,
) -> Result<(), SchemaError> {
	validate_component_schema_version(component_schema_version)?;
	validate_schema_ledger_with_transaction_table(tx, table_name).await?;
	validate_component_schema_version_row_in_current_transaction(
		tx,
		table_name,
		component_schema_version,
	)
	.await
}

/// Checks the ledger table and its own registration without changing either.
pub(crate) async fn validate_schema_ledger_registration_in_current_transaction(
	tx: &mut Tx<'_>,
	table_name: &PgQualifiedTableName,
) -> Result<(), SchemaError> {
	let instance_key = schema_ledger_schema_instance_key(table_name);
	validate_component_schema_version_in_current_transaction(
		tx,
		table_name,
		schema_ledger_component_schema_version(&instance_key),
	)
	.await
}

/// Reads exact component metadata after validating the ledger and its own registration.
pub(crate) async fn fetch_component_registration_in_current_transaction(
	tx: &mut Tx<'_>,
	table_name: &PgQualifiedTableName,
	component_schema_version: ComponentSchemaVersion<'_>,
) -> Result<ComponentSchemaRegistration, SchemaError> {
	validate_component_schema_version(component_schema_version)?;
	validate_schema_ledger_registration_in_current_transaction(tx, table_name).await?;
	let recorded = fetch_component_schema_version_row_in_current_transaction(
		tx,
		table_name,
		component_schema_version,
	)
	.await?;
	let Some(recorded) = recorded else {
		return Ok(ComponentSchemaRegistration::Absent);
	};
	validate_component_schema_version(ComponentSchemaVersion {
		version: recorded.version,
		fingerprint: &recorded.fingerprint,
		..component_schema_version
	})?;
	Ok(ComponentSchemaRegistration::Recorded {
		version: recorded.version,
		fingerprint: recorded.fingerprint,
	})
}

async fn validate_component_schema_version_row_in_current_transaction(
	tx: &mut Tx<'_>,
	table_name: &PgQualifiedTableName,
	component_schema_version: ComponentSchemaVersion<'_>,
) -> Result<(), SchemaError> {
	let actual = fetch_component_schema_version_row_in_current_transaction(
		tx,
		table_name,
		component_schema_version,
	)
	.await?;

	let Some(actual) = actual else {
		return Err(SchemaError::mismatch(format!(
			"schema ledger row for component {:?} instance {:?} was not found",
			component_schema_version.component, component_schema_version.instance_key
		)));
	};

	if actual.version != component_schema_version.version {
		return Err(SchemaError::mismatch(format!(
			"schema ledger row for component {:?} instance {:?} recorded version {}, expected {}",
			component_schema_version.component,
			component_schema_version.instance_key,
			actual.version,
			component_schema_version.version
		)));
	}

	if actual.fingerprint != component_schema_version.fingerprint {
		return Err(SchemaError::mismatch(format!(
			"schema ledger row for component {:?} instance {:?} recorded fingerprint {:?}, expected {:?}",
			component_schema_version.component,
			component_schema_version.instance_key,
			actual.fingerprint,
			component_schema_version.fingerprint
		)));
	}

	Ok(())
}

pub(crate) async fn fetch_component_schema_version_row_in_current_transaction(
	tx: &mut Tx<'_>,
	table_name: &PgQualifiedTableName,
	component_schema_version: ComponentSchemaVersion<'_>,
) -> Result<Option<RecordedComponentSchemaVersion>, SchemaError> {
	let catalog = SchemaLedgerCatalog::new(table_name);

	let statement = catalog.fetch_component_version_statement();

	tx.record_database_operation(
		DatabaseOperationKind::FetchOptional,
		SCHEMA_LEDGER_FETCH_COMPONENT_VERSION_LABEL,
		Some(statement.as_str()),
	);
	let actual = owned_portable_query_as::<(i32, String)>(sqlx::AssertSqlSafe(statement.as_str()))
		.bind_validated(component_schema_version.component)
		.bind_validated(component_schema_version.instance_key)
		.fetch_optional(tx.inner.as_mut())
		.await
		.map_err(|error| error.with_operation_label(SCHEMA_LEDGER_FETCH_COMPONENT_VERSION_LABEL))?;

	Ok(
		actual.map(|(version, fingerprint)| RecordedComponentSchemaVersion {
			version,
			fingerprint,
		}),
	)
}

/// Claims one additive component row only in an already valid, registered ledger.
/// It creates no table and changes no existing component row; Some returns the locked occupant.
pub(crate) async fn claim_component_registration_in_existing_ledger(
	tx: &mut Tx<'_>,
	table_name: &PgQualifiedTableName,
	component_schema_version: ComponentSchemaVersion<'_>,
) -> Result<Option<RecordedComponentSchemaVersion>, SchemaError> {
	validate_component_schema_version(component_schema_version)?;
	validate_schema_ledger_registration_in_current_transaction(tx, table_name).await?;
	claim_or_lock_component_schema_version_row_in_current_transaction(
		tx,
		table_name,
		component_schema_version,
	)
	.await
}

async fn claim_or_lock_component_schema_version_row_in_current_transaction(
	tx: &mut Tx<'_>,
	table_name: &PgQualifiedTableName,
	component_schema_version: ComponentSchemaVersion<'_>,
) -> Result<Option<RecordedComponentSchemaVersion>, SchemaError> {
	let catalog = SchemaLedgerCatalog::new(table_name);
	let claim_statement = catalog.claim_component_version_statement();

	tx.record_database_operation(
		DatabaseOperationKind::Execute,
		SCHEMA_LEDGER_CLAIM_COMPONENT_VERSION_LABEL,
		Some(claim_statement.as_str()),
	);
	let rows_affected = owned_portable_query(sqlx::AssertSqlSafe(claim_statement.as_str()))
		.bind_validated(component_schema_version.component)
		.bind_validated(component_schema_version.instance_key)
		.bind_validated(component_schema_version.version)
		.bind_validated(component_schema_version.fingerprint)
		.execute(tx.inner.as_mut())
		.await
		.map_err(|error| error.with_operation_label(SCHEMA_LEDGER_CLAIM_COMPONENT_VERSION_LABEL))?
		.rows_affected();

	if rows_affected == 1 {
		return Ok(None);
	}

	let lock_statement = catalog.lock_component_version_statement();
	tx.record_database_operation(
		DatabaseOperationKind::FetchOptional,
		SCHEMA_LEDGER_LOCK_COMPONENT_VERSION_LABEL,
		Some(lock_statement.as_str()),
	);
	let actual =
		owned_portable_query_as::<(i32, String)>(sqlx::AssertSqlSafe(lock_statement.as_str()))
			.bind_validated(component_schema_version.component)
			.bind_validated(component_schema_version.instance_key)
			.fetch_optional(tx.inner.as_mut())
			.await
			.map_err(|error| {
				error.with_operation_label(SCHEMA_LEDGER_LOCK_COMPONENT_VERSION_LABEL)
			})?;

	actual
		.map(|(version, fingerprint)| RecordedComponentSchemaVersion {
			version,
			fingerprint,
		})
		.ok_or_else(|| {
			SchemaError::mismatch(format!(
				"schema ledger row for component {:?} instance {:?} disappeared before migration planning",
				component_schema_version.component, component_schema_version.instance_key
			))
		})
		.map(Some)
}

/// Builds a stable schema instance key from labeled qualified table names.
///
/// Labels are validated Postgres identifiers. The returned key includes quoted
/// schema and table names, so schemas with the same table names remain
/// distinct.
pub fn component_schema_instance_key_for_tables<'a, I>(parts: I) -> String
where
	I: IntoIterator<Item = (&'a PgIdentifier, &'a PgQualifiedTableName)>,
{
	parts
		.into_iter()
		.map(|(label, table_name)| format!("{}={}", label.as_str(), table_name.quoted()))
		.collect::<Vec<_>>()
		.join(";")
}

pub(crate) fn schema_instance_key_for_parts<'a, I>(parts: I) -> String
where
	I: IntoIterator<Item = (&'a str, &'a PgQualifiedTableName)>,
{
	parts
		.into_iter()
		.map(|(label, table_name)| format!("{label}={}", table_name.quoted()))
		.collect::<Vec<_>>()
		.join(";")
}

fn schema_ledger_schema_instance_key(table_name: &PgQualifiedTableName) -> String {
	schema_instance_key_for_parts([("table", table_name)])
}

fn schema_ledger_component_schema_version(instance_key: &str) -> ComponentSchemaVersion<'_> {
	ComponentSchemaVersion {
		component: SCHEMA_LEDGER_SCHEMA_COMPONENT,
		instance_key,
		version: SCHEMA_LEDGER_SCHEMA_VERSION,
		fingerprint: SCHEMA_LEDGER_SCHEMA_FINGERPRINT,
	}
}

async fn execute_schema_ledger_migration_in_current_transaction(
	tx: &mut Tx<'_>,
	table_name: &PgQualifiedTableName,
) -> Result<(), SchemaError> {
	let statement = build_create_schema_ledger_table_statement(table_name);
	execute_create_schema_ledger_table_statement_in_current_transaction(tx, &statement).await?;
	validate_schema_ledger_with_transaction_table(tx, table_name).await
}

async fn execute_create_schema_ledger_table_statement_in_current_transaction(
	tx: &mut Tx<'_>,
	statement: &str,
) -> Result<(), SchemaError> {
	SchemaLedgerCreateAttempt::begin(tx)
		.await?
		.create(statement)
		.await
}

fn is_concurrent_schema_ledger_create_race(error: &sqlx::Error) -> bool {
	match error
		.as_database_error()
		.and_then(|database_error| database_error.code())
		.map(PgSqlState::from_code)
	{
		Some(PgSqlState::UniqueViolation) => {
			error
				.as_database_error()
				.and_then(|database_error| database_error.constraint())
				== Some("pg_type_typname_nsp_index")
		}
		Some(PgSqlState::Other(code)) => code == "42P07" || code == "42710",
		_ => false,
	}
}

async fn validate_schema_ledger_with_transaction_table(
	tx: &mut Tx<'_>,
	table_name: &PgQualifiedTableName,
) -> Result<(), SchemaError> {
	let catalog = SchemaLedgerCatalog::new(table_name);
	validate_schema_ledger_required_columns(tx, &catalog).await?;
	validate_schema_ledger_primary_key(tx, &catalog).await?;
	validate_schema_ledger_required_check_constraints(tx, &catalog).await
}

async fn validate_schema_ledger_required_columns(
	tx: &mut Tx<'_>,
	catalog: &SchemaLedgerCatalog<'_>,
) -> Result<(), SchemaError> {
	let quoted_table_name = catalog.quoted_table_name();
	let statement = PG_COLUMN_METADATA_SQL;
	tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		SCHEMA_LEDGER_VALIDATE_COLUMNS_LABEL,
		Some(statement),
	);
	let actual_columns =
		owned_portable_query_as::<(String, String, bool, Option<String>, bool)>(statement)
			.bind_validated(quoted_table_name.as_str())
			.fetch_all(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(SCHEMA_LEDGER_VALIDATE_COLUMNS_LABEL))?;

	for column in SCHEMA_LEDGER_REQUIRED_COLUMNS {
		let name = column.name();
		let expected_type = column.validation_type();
		let expected_not_null = true;
		let expected_collation = column.required_collation();
		let Some((_, actual_type, actual_not_null, actual_collation, actual_writable)) =
			actual_columns
				.iter()
				.find(|(column_name, ..)| column_name == name)
		else {
			return Err(SchemaError::mismatch(format!(
				"required column {:?} not found on schema ledger table {}",
				name, quoted_table_name
			)));
		};

		if actual_type != expected_type {
			return Err(SchemaError::mismatch(format!(
				"schema ledger table {} column {:?} has type {:?}, expected {:?}",
				quoted_table_name, name, actual_type, expected_type
			)));
		}
		if !*actual_writable {
			return Err(SchemaError::mismatch(format!(
				"schema ledger table {} column {:?} must accept explicit writes",
				quoted_table_name, name
			)));
		}
		if *actual_not_null != expected_not_null {
			return Err(SchemaError::mismatch(format!(
				"schema ledger table {} column {:?} has not-null {}, expected {}",
				quoted_table_name, name, actual_not_null, expected_not_null
			)));
		}
		if actual_collation.as_deref() != expected_collation {
			return Err(SchemaError::mismatch(format!(
				"schema ledger table {} column {:?} has collation {:?}, expected {:?}",
				quoted_table_name,
				name,
				actual_collation.as_deref(),
				expected_collation
			)));
		}
	}

	Ok(())
}

async fn validate_schema_ledger_primary_key(
	tx: &mut Tx<'_>,
	catalog: &SchemaLedgerCatalog<'_>,
) -> Result<(), SchemaError> {
	let quoted_table_name = catalog.quoted_table_name();
	let statement = concat!(
		r#"
        SELECT EXISTS (
            SELECT 1
            FROM pg_catalog.pg_index idx
            JOIN pg_catalog.pg_class index_class ON index_class.oid OPERATOR(pg_catalog.=) idx.indexrelid
            JOIN pg_catalog.pg_attribute component_attr
              ON component_attr.attrelid OPERATOR(pg_catalog.=) idx.indrelid
             AND component_attr.attname OPERATOR(pg_catalog.=) $2
             AND NOT component_attr.attisdropped
            JOIN pg_catalog.pg_attribute instance_attr
              ON instance_attr.attrelid OPERATOR(pg_catalog.=) idx.indrelid
             AND instance_attr.attname OPERATOR(pg_catalog.=) $3
             AND NOT instance_attr.attisdropped
            JOIN pg_catalog.pg_opclass component_opclass
              ON component_opclass.oid OPERATOR(pg_catalog.=) idx.indclass[0]
             AND component_opclass.opcmethod OPERATOR(pg_catalog.=) index_class.relam
             AND component_opclass.opcnamespace OPERATOR(pg_catalog.=) 'pg_catalog'::pg_catalog.regnamespace
            JOIN pg_catalog.pg_opclass instance_opclass
              ON instance_opclass.oid OPERATOR(pg_catalog.=) idx.indclass[1]
             AND instance_opclass.opcmethod OPERATOR(pg_catalog.=) index_class.relam
             AND instance_opclass.opcnamespace OPERATOR(pg_catalog.=) 'pg_catalog'::pg_catalog.regnamespace
            WHERE idx.indrelid OPERATOR(pg_catalog.=) pg_catalog.to_regclass($1)
              AND idx.indisprimary
              AND idx.indimmediate
              AND idx.indnkeyatts OPERATOR(pg_catalog.=) 2
              AND idx.indpred IS NULL
              AND idx.indexprs IS NULL
              AND idx.indkey[0] OPERATOR(pg_catalog.=) component_attr.attnum
              AND idx.indkey[1] OPERATOR(pg_catalog.=) instance_attr.attnum
              AND component_opclass.opcname OPERATOR(pg_catalog.=) ANY (ARRAY['text_ops', 'text_pattern_ops']::pg_catalog.text[])
              AND instance_opclass.opcname OPERATOR(pg_catalog.=) ANY (ARRAY['text_ops', 'text_pattern_ops']::pg_catalog.text[])
              AND idx.indoption[0] OPERATOR(pg_catalog.=) 0
              AND idx.indoption[1] OPERATOR(pg_catalog.=) 0
              AND idx.indcollation[0] OPERATOR(pg_catalog.=) component_attr.attcollation
              AND idx.indcollation[1] OPERATOR(pg_catalog.=) instance_attr.attcollation"#,
		crate::db::builtin_btree_index_conditions!(),
		r#"
        )
        "#
	);
	tx.record_database_operation(
		DatabaseOperationKind::FetchOne,
		SCHEMA_LEDGER_VALIDATE_PRIMARY_KEY_LABEL,
		Some(statement),
	);
	let has_primary_key = owned_portable_query_scalar::<bool>(statement)
		.bind_validated(quoted_table_name.as_str())
		.bind_validated(SchemaLedgerColumn::Component.name())
		.bind_validated(SchemaLedgerColumn::InstanceKey.name())
		.fetch_one(tx.inner.as_mut())
		.await
		.map_err(|error| error.with_operation_label(SCHEMA_LEDGER_VALIDATE_PRIMARY_KEY_LABEL))?;

	if !has_primary_key {
		let primary_key_columns = schema_ledger_column_list(&SCHEMA_LEDGER_PRIMARY_KEY_COLUMNS);
		return Err(SchemaError::mismatch(format!(
			"schema ledger table {} must have primary key ({})",
			quoted_table_name, primary_key_columns
		)));
	}

	Ok(())
}

async fn validate_schema_ledger_required_check_constraints(
	tx: &mut Tx<'_>,
	catalog: &SchemaLedgerCatalog<'_>,
) -> Result<(), SchemaError> {
	let quoted_table_name = catalog.quoted_table_name();
	let statement = crate::db::catalog::BUILTIN_CHECK_EXPRESSIONS_SQL;
	tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		SCHEMA_LEDGER_VALIDATE_CHECK_CONSTRAINTS_LABEL,
		Some(statement),
	);
	let normalized_check_expressions = owned_portable_query_scalar::<String>(statement)
		.bind_validated(quoted_table_name.as_str())
		.fetch_all(tx.inner.as_mut())
		.await
		.map_err(|error| {
			error.with_operation_label(SCHEMA_LEDGER_VALIDATE_CHECK_CONSTRAINTS_LABEL)
		})?
		.into_iter()
		.map(|expression| crate::db::catalog::normalize_builtin_check_expression(&expression))
		.collect::<Vec<String>>();

	for column in SCHEMA_LEDGER_CHECKED_COLUMNS {
		let required_expression = column
			.normalized_check_constraint()
			.expect("checked schema ledger column must define a constraint");
		if !normalized_check_expressions
			.iter()
			.any(|expression| expression == &required_expression)
		{
			return Err(SchemaError::mismatch(format!(
				"schema ledger table {} must enforce CHECK ({})",
				quoted_table_name, required_expression
			)));
		}
	}

	Ok(())
}

#[doc(hidden)]
pub fn build_create_schema_ledger_table_statement(table_name: &PgQualifiedTableName) -> String {
	SchemaLedgerCatalog::new(table_name).create_table_statement()
}

pub(crate) fn validate_component_schema_version(
	component_schema_version: ComponentSchemaVersion<'_>,
) -> Result<(), SchemaError> {
	validate_bounded_nonempty_schema_ledger_text(
		SchemaLedgerColumn::Component.name(),
		component_schema_version.component,
		MAX_SCHEMA_LEDGER_COMPONENT_LEN,
	)?;
	validate_bounded_nonempty_schema_ledger_text(
		SchemaLedgerColumn::InstanceKey.name(),
		component_schema_version.instance_key,
		MAX_SCHEMA_LEDGER_INSTANCE_KEY_LEN,
	)?;
	validate_bounded_nonempty_schema_ledger_text(
		SchemaLedgerColumn::SchemaFingerprint.name(),
		component_schema_version.fingerprint,
		MAX_SCHEMA_LEDGER_FINGERPRINT_LEN,
	)?;
	if component_schema_version.version <= 0 {
		return Err(SchemaError::mismatch(format!(
			"schema ledger version must be positive, got {}",
			component_schema_version.version
		)));
	}
	Ok(())
}

fn validate_bounded_nonempty_schema_ledger_text(
	label: &str,
	value: &str,
	max_len: usize,
) -> Result<(), SchemaError> {
	if value.is_empty() {
		return Err(SchemaError::mismatch(format!(
			"schema ledger {label} must not be empty"
		)));
	}
	if value.len() > max_len {
		return Err(SchemaError::mismatch(format!(
			"schema ledger {label} is {} bytes, maximum is {}",
			value.len(),
			max_len
		)));
	}
	if value.as_bytes().contains(&0) {
		return Err(SchemaError::mismatch(format!(
			"schema ledger {label} must not contain null bytes"
		)));
	}
	Ok(())
}

pub(crate) async fn read_component_schema_for_migration(
	tx: &mut Tx<'_>,
	table_name: &PgQualifiedTableName,
	component_schema_version: ComponentSchemaVersion<'_>,
) -> Result<Option<RecordedComponentSchemaVersion>, SchemaError> {
	validate_component_schema_version(component_schema_version)?;
	execute_schema_ledger_migration_in_current_transaction(tx, table_name).await?;
	claim_or_lock_component_schema_version_row_in_current_transaction(
		tx,
		table_name,
		component_schema_version,
	)
	.await
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::db::PgSchemaName;
	use sqlx::Execute;
	use std::borrow::Cow;

	fn table() -> PgQualifiedTableName {
		PgQualifiedTableName::new(
			PgSchemaName::from_identifier_text("__paranoid").expect("table"),
			PgIdentifier::new("schema_ledger").expect("table"),
		)
	}

	fn version() -> ComponentSchemaVersion<'static> {
		ComponentSchemaVersion {
			component: "cook.pages",
			instance_key: "table=\"cook\".\"pages\"",
			version: 3,
			fingerprint: "cook.pages.v3",
		}
	}

	/// The statement with its `$n` placeholders replaced by the literals of `values`.
	fn rendered(template: String, values: &[&str]) -> String {
		let mut query = owned_portable_query(sqlx::AssertSqlSafe(template));
		for value in values {
			query = query.bind(*value);
		}
		let raw = query.into_raw_sql().expect("raw sql");
		Execute::<'_, sqlx::Postgres>::sql(raw).as_str().to_owned()
	}

	fn compact(sql: &str) -> String {
		sql.split_whitespace().collect::<Vec<_>>().join(" ")
	}

	#[test]
	fn the_ledger_is_created_bounded_and_c_collated_at_the_caller_qualified_relation() {
		let table = table();
		let sql = build_create_schema_ledger_table_statement(&table);
		assert!(sql.contains("CREATE TABLE IF NOT EXISTS \"__paranoid\".\"schema_ledger\""));
		for column in ["component", "instance_key", "schema_fingerprint"] {
			assert!(
				sql.contains(&format!(
					"{column} pg_catalog.text COLLATE pg_catalog.\"C\" NOT NULL CHECK"
				)),
				"{column}"
			);
		}
		for bound in [
			"pg_catalog.octet_length(component) OPERATOR(pg_catalog.<=) 128",
			"pg_catalog.octet_length(instance_key) OPERATOR(pg_catalog.<=) 1024",
			"pg_catalog.octet_length(schema_fingerprint) OPERATOR(pg_catalog.<=) 256",
			"PRIMARY KEY (component, instance_key)",
		] {
			assert!(sql.contains(bound), "{bound}");
		}
		assert!(sql.contains("schema_version INTEGER NOT NULL CHECK"));
		for forbidden in ["advisory", "listen", "notify"] {
			assert!(!sql.to_lowercase().contains(forbidden), "{forbidden}");
		}
	}

	#[test]
	fn claiming_a_row_never_overwrites_an_existing_one() {
		let table = table();
		let sql = compact(&rendered(
			SchemaLedgerCatalog::new(&table).claim_component_version_statement(),
			&["cook.pages", "table", "3", "cook.pages.v3"],
		));
		assert!(sql.starts_with("INSERT INTO \"__paranoid\".\"schema_ledger\" ("));
		assert!(sql.contains("ON CONFLICT (component, instance_key) DO NOTHING"));
		assert!(!sql.contains("DO UPDATE"));
	}

	#[test]
	fn locking_and_nonlocking_reads_differ_only_in_the_row_lock() {
		let table = table();
		let catalog = SchemaLedgerCatalog::new(&table);
		let values = ["cook.pages", "table=\"cook\".\"pages\""];
		let locked = compact(&rendered(
			catalog.lock_component_version_statement(),
			&values,
		));
		let fetched = compact(&rendered(
			catalog.fetch_component_version_statement(),
			&values,
		));
		assert_eq!(
			fetched,
			"SELECT schema_version, schema_fingerprint FROM \"__paranoid\".\"schema_ledger\" WHERE component OPERATOR(pg_catalog.=) (E'cook.pages') AND instance_key OPERATOR(pg_catalog.=) (E'table=\"cook\".\"pages\"')"
		);
		assert_eq!(locked, format!("{fetched} FOR UPDATE"));
	}

	#[test]
	fn completion_and_upgrade_compare_the_recorded_version_and_fingerprint() {
		let table = table();
		let catalog = SchemaLedgerCatalog::new(&table);
		let fresh = compact(&rendered(
			catalog.complete_fresh_component_version_statement(),
			&["cook.pages", "table=x", "3", "cook.pages.v3"],
		));
		assert!(fresh.starts_with("UPDATE \"__paranoid\".\"schema_ledger\" SET applied_at = pg_catalog.statement_timestamp() WHERE component OPERATOR(pg_catalog.=) (E'cook.pages')"));
		assert!(fresh.contains("schema_version OPERATOR(pg_catalog.=)"));
		assert!(fresh.contains("schema_fingerprint OPERATOR(pg_catalog.=)"));

		let update = compact(&catalog.update_component_version_statement());
		assert!(update.contains(
			"SET schema_version = $5, schema_fingerprint = $6, applied_at = pg_catalog.statement_timestamp()"
		));
		assert!(update.contains(
			"AND schema_version OPERATOR(pg_catalog.=) $3 AND schema_fingerprint OPERATOR(pg_catalog.=) $4"
		));
	}

	#[test]
	fn the_ledger_identity_is_bounded_in_bytes_and_the_version_is_positive() {
		assert!(validate_component_schema_version(version()).is_ok());
		let long_component = "c".repeat(MAX_SCHEMA_LEDGER_COMPONENT_LEN + 1);
		let long_instance = "i".repeat(MAX_SCHEMA_LEDGER_INSTANCE_KEY_LEN + 1);
		let long_fingerprint = "f".repeat(MAX_SCHEMA_LEDGER_FINGERPRINT_LEN + 1);
		let accepted_edge = ComponentSchemaVersion {
			component: &"c".repeat(MAX_SCHEMA_LEDGER_COMPONENT_LEN),
			instance_key: &"i".repeat(MAX_SCHEMA_LEDGER_INSTANCE_KEY_LEN),
			version: i32::MAX,
			fingerprint: &"f".repeat(MAX_SCHEMA_LEDGER_FINGERPRINT_LEN),
		};
		assert!(validate_component_schema_version(accepted_edge).is_ok());
		for rejected in [
			ComponentSchemaVersion {
				component: "",
				..version()
			},
			ComponentSchemaVersion {
				instance_key: "",
				..version()
			},
			ComponentSchemaVersion {
				fingerprint: "",
				..version()
			},
			ComponentSchemaVersion {
				version: 0,
				..version()
			},
			ComponentSchemaVersion {
				version: -1,
				..version()
			},
			ComponentSchemaVersion {
				component: &long_component,
				..version()
			},
			ComponentSchemaVersion {
				instance_key: &long_instance,
				..version()
			},
			ComponentSchemaVersion {
				fingerprint: &long_fingerprint,
				..version()
			},
			ComponentSchemaVersion {
				component: "a\0b",
				..version()
			},
		] {
			assert!(
				matches!(
					validate_component_schema_version(rejected),
					Err(SchemaError::SchemaMismatch { .. })
				),
				"{rejected:?}"
			);
		}
		// A multi-byte character counts by its UTF-8 bytes.
		let two_bytes_each = "é".repeat(MAX_SCHEMA_LEDGER_COMPONENT_LEN / 2 + 1);
		assert!(
			validate_component_schema_version(ComponentSchemaVersion {
				component: &two_bytes_each,
				..version()
			})
			.is_err()
		);
	}

	#[test]
	fn the_ledger_registers_itself_under_an_instance_key_derived_from_its_table() {
		assert_eq!(
			schema_ledger_schema_instance_key(&table()),
			"table=\"__paranoid\".\"schema_ledger\""
		);
		let other = PgQualifiedTableName::new(
			PgSchemaName::from_identifier_text("tenant").expect("table"),
			PgIdentifier::new("schema_ledger").expect("table"),
		);
		assert_ne!(
			schema_ledger_schema_instance_key(&table()),
			schema_ledger_schema_instance_key(&other)
		);
		let key = schema_ledger_schema_instance_key(&table());
		let version = schema_ledger_component_schema_version(&key);
		assert_eq!(version.component, "schema_ledger");
		assert_eq!(version.version, 1);
		assert_eq!(version.fingerprint, "paranoid.schema_ledger.v1");
		assert!(validate_component_schema_version(version).is_ok());
	}

	#[derive(Debug)]
	struct ReportedFailureError {
		code: &'static str,
		constraint: Option<&'static str>,
	}

	impl std::fmt::Display for ReportedFailureError {
		fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
			write!(formatter, "reported failure {}", self.code)
		}
	}

	impl std::error::Error for ReportedFailureError {}

	impl sqlx::error::DatabaseError for ReportedFailureError {
		fn message(&self) -> &str {
			"reported failure"
		}

		fn code(&self) -> Option<Cow<'_, str>> {
			Some(Cow::Borrowed(self.code))
		}

		fn constraint(&self) -> Option<&str> {
			self.constraint
		}

		fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
			self
		}

		fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
			self
		}

		fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
			self
		}

		fn kind(&self) -> sqlx::error::ErrorKind {
			sqlx::error::ErrorKind::Other
		}
	}

	fn reported(code: &'static str, constraint: Option<&'static str>) -> sqlx::Error {
		sqlx::Error::Database(Box::new(ReportedFailureError { code, constraint }))
	}

	#[test]
	fn only_the_known_concurrent_catalog_races_are_recovered() {
		for race in [
			reported("42P07", None),
			reported("42710", None),
			reported("23505", Some("pg_type_typname_nsp_index")),
		] {
			assert!(is_concurrent_schema_ledger_create_race(&race), "{race:?}");
		}
		for other in [
			reported("42501", None),
			reported("23505", Some("some_other_constraint")),
			reported("23505", None),
			sqlx::Error::PoolTimedOut,
		] {
			assert!(
				!is_concurrent_schema_ledger_create_race(&other),
				"{other:?}"
			);
		}
	}
}
