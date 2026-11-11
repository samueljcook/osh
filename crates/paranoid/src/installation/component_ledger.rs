//! The native ledger of application component schemas.
//!
//! Each entry records one application component instance's schema version and fingerprint.
//! A migration locks the entry, writes the new version, and runs the application's statements
//! and validation checks in the same transaction, so a failed statement or check rolls the
//! entry back with the statements, and the entry's row lock serializes concurrent instances.
//!
//! On an imported installation, while a published v10 release still writes, the operations
//! use the v10 ledger row that the published release uses. After v10 writers retire, a v10
//! release can no longer change that row, so a component without a native entry reads its
//! frozen v10 row until a native migration writes the native entry. Removing the v10 ledger
//! table would therefore first require copying those rows into native entries.

use std::num::NonZeroU32;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::binding_fingerprint::append_framed;
use super::component_schema::{
	ComponentSchema, ComponentSchemaMigrationResult, execute_component_schema_checks,
	execute_component_schema_plan,
};
use super::schema_ledger::{
	ComponentSchemaRegistration, fetch_component_registration_in_current_transaction,
	fetch_component_schema_version_row_in_current_transaction, read_component_schema_for_migration,
	record_component_schema_migration_completion_in_current_transaction,
};
use super::{
	ComponentSchemaMigrationPlan, ComponentSchemaVersion, ProtocolError,
	RecordedComponentSchemaVersion,
};
use crate::db::{
	Database, DatabaseError, PgQualifiedTableName, SchemaError, TransactionFailure, WriteDatabase,
	run_transaction_read, run_transaction_write,
};
use crate::installation::contract::{
	BoundComponentContract, BoundStateSchema, ComponentDeclaration, ComponentDeclarationFields,
	ComponentPolicy, ComponentPolicyFields, ComponentSettings, ComponentSettingsDeclaration,
	ComponentSettingsRequirement, ContractStateReadError, RepresentationPolicy, SchemaPolicy,
	SemanticContractDef, StateContractError, StateStorageEnvelope,
};
use crate::records::postgres::{PgPointStorage, StateStorageError};
use crate::records::state_schema::{
	StateRepresentationFormat, StateSchema, StateSchemaError, StateVersion,
};
use crate::records::{
	EncodedRecord, IndexFields, PointRecordLock, RecordIdentity, RecordKey, RecordMetadata,
	RecordNamespace, StoredRecord,
};
use crate::runtime::admission::bindings::{
	InstallationStorageRoute, PgInstallationAdmissionBinding,
};
use crate::runtime::admission::{NativeAdmissionError, NativeOperationError, PgNativeAdmission};

const LEDGER_SCHEMA: &str = "paranoid.component-schema-ledger";
const LEDGER_COMPONENT: &str = "paranoid.component-schema-ledger";
const LEDGER_SEMANTICS: &str = "paranoid.component-schema-ledger.v1";
const JSON_LEDGER_REPRESENTATION_VERSION: u32 = 1;
const LEDGER_KEY_DOMAIN: &[u8] = b"paranoid.component-schema-ledger.v1";

/// The stored form of one ledger entry.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LedgerEntryWire {
	version: i32,
	fingerprint: String,
}

/// A ledger entry's JSON is invalid.
#[derive(Debug, thiserror::Error)]
#[error("component schema ledger entry JSON is invalid: {0}")]
pub(crate) struct LedgerCodecError(primitives_rs::serialization::json::JsonDecodeFailureSummary);

fn decode_entry(bytes: &[u8]) -> Result<RecordedComponentSchemaVersion, LedgerCodecError> {
	let wire = serde_json::from_slice::<LedgerEntryWire>(bytes).map_err(|error| {
		LedgerCodecError(
			primitives_rs::serialization::json::JsonDecodeFailureSummary::from_serde_error(&error),
		)
	})?;
	Ok(RecordedComponentSchemaVersion {
		version: wire.version,
		fingerprint: wire.fingerprint,
	})
}

fn encode_entry(entry: &RecordedComponentSchemaVersion) -> Result<Vec<u8>, LedgerCodecError> {
	serde_json::to_vec(&LedgerEntryWire {
		version: entry.version,
		fingerprint: entry.fingerprint.clone(),
	})
	.map_err(|error| {
		LedgerCodecError(
			primitives_rs::serialization::json::JsonDecodeFailureSummary::from_serde_error(&error),
		)
	})
}

/// The code-owned ledger entry schema.
pub(crate) struct ComponentLedgerComponent {
	schema: StateSchema<RecordedComponentSchemaVersion, LedgerCodecError>,
}

impl ComponentLedgerComponent {
	pub(crate) fn new() -> Self {
		Self {
			schema: StateSchema::new(crate::records::state_schema::StateSchemaFields {
				identifier: LEDGER_SCHEMA,
				versions: vec![StateVersion {
					version: JSON_LEDGER_REPRESENTATION_VERSION,
					format: StateRepresentationFormat::Json,
					decoder: decode_entry,
					encoder: Some(encode_entry),
				}],
			})
			.expect("the ledger declares one positive representation version"),
		}
	}

	pub(crate) fn declaration(&self) -> ComponentDeclaration<'_, Self> {
		ComponentDeclaration::new(ComponentDeclarationFields {
			identifier: LEDGER_COMPONENT,
			fixed_schemas: vec![self.schema.capabilities()],
			settings: ComponentSettingsDeclaration::Parameterless,
			semantics: vec![SemanticContractDef {
				identifier: LEDGER_SEMANTICS,
				transitions: self,
				settings: ComponentSettingsRequirement::Parameterless,
			}],
		})
		.expect("the ledger declares one schema and one semantic implementation")
	}

	/// The only policy: the native JSON writer and its reader.
	pub(crate) fn initial_policy(&self) -> Result<ComponentPolicy, StateContractError> {
		let native = RepresentationPolicy {
			version: JSON_LEDGER_REPRESENTATION_VERSION,
			envelope: StateStorageEnvelope::NativeJson,
		};
		ComponentPolicy::new(ComponentPolicyFields {
			component_identifier: LEDGER_COMPONENT.to_owned(),
			semantic_contract: LEDGER_SEMANTICS.to_owned(),
			schemas: vec![SchemaPolicy::new(
				self.schema.identifier().to_owned(),
				native,
				vec![native],
			)?],
			settings: ComponentSettings::Parameterless,
		})
	}

	/// Checks that a bound policy selects the native writer of this code.
	pub(crate) fn require_bound_policy(
		&self,
		component: &BoundComponentContract<'_, '_, Self>,
	) -> Result<(), StateContractError> {
		self.bind_schema(component).map(|_| ())
	}

	fn bind_schema<'a>(
		&'a self,
		component: &'a BoundComponentContract<'_, '_, Self>,
	) -> Result<
		BoundStateSchema<'a, RecordedComponentSchemaVersion, LedgerCodecError>,
		StateContractError,
	> {
		component.bind_schema(&self.schema)
	}
}

/// A migration's plan, and the v10 ledger when the v10 row is the entry.
enum PreparedMigration<'s> {
	V10 {
		ledger: PgQualifiedTableName,
		plan: ComponentSchemaMigrationPlan<'s>,
	},
	Native {
		plan: ComponentSchemaMigrationPlan<'s>,
	},
}

impl<'s> PreparedMigration<'s> {
	fn plan(&self) -> &ComponentSchemaMigrationPlan<'s> {
		match self {
			Self::V10 { plan, .. } | Self::Native { plan } => plan,
		}
	}
}

/// The ledger's placement and the runtime's admission.
pub(crate) struct NativeComponentLedgerBindings {
	pub(crate) component: ComponentLedgerComponent,
	pub(crate) storage: PgPointStorage,
	pub(crate) namespace: RecordNamespace,
	pub(crate) admission: Arc<PgNativeAdmission>,
	pub(crate) conflict_attempts: NonZeroU32,
}

impl NativeComponentLedgerBindings {
	fn identity(&self, version: ComponentSchemaVersion<'_>) -> RecordIdentity {
		let mut key = Vec::new();
		for part in [
			LEDGER_KEY_DOMAIN,
			version.component.as_bytes(),
			version.instance_key.as_bytes(),
		] {
			append_framed(&mut key, part);
		}
		RecordIdentity::new(self.namespace.clone(), RecordKey::new(key))
	}

	fn v10_ledger(&self) -> Result<PgQualifiedTableName, NativeComponentLedgerError> {
		match self.admission.installation_binding().as_ref() {
			PgInstallationAdmissionBinding::Imported { source, .. } => {
				Ok(source.protocol_ledger().clone())
			}
			PgInstallationAdmissionBinding::Fresh { .. } => {
				Err(NativeComponentLedgerError::V10LedgerAbsent)
			}
		}
	}

	/// Migrates one application component schema in a write transaction.
	///
	/// The admitted part locks and plans the ledger entry. The application's statements and
	/// checks run after it in the same transaction because `Tx` rejects
	/// application SQL while a native operation is unfinished.
	pub(crate) async fn migrate(
		&self,
		database: impl WriteDatabase,
		schema: &ComponentSchema<'_>,
	) -> Result<ComponentSchemaMigrationResult, NativeComponentLedgerError> {
		let steps = schema.migration_steps();
		run_transaction_write(database, "db.component_ledger.migrate", async |tx| {
			let prepared = self
				.admission
				.execute_write_with_contracts(tx, async |contracts, tx| {
					self.prepare_migration(&contracts, tx, schema, &steps).await
				})
				.await
				.map_err(NativeComponentLedgerError::from_operation)?;
			let outcome = execute_component_schema_plan(tx, schema, prepared.plan())
				.await
				.map_err(NativeComponentLedgerError::Schema)?;
			if let PreparedMigration::V10 { ledger, plan } = &prepared {
				let prior = match plan {
					ComponentSchemaMigrationPlan::FreshInstall => Some(None),
					ComponentSchemaMigrationPlan::Upgrade { from, .. } => Some(Some(from)),
					ComponentSchemaMigrationPlan::AlreadyCurrent
					| ComponentSchemaMigrationPlan::AlreadyCompatible { .. } => None,
				};
				if let Some(prior) = prior {
					record_component_schema_migration_completion_in_current_transaction(
						tx,
						ledger,
						schema.current_version(),
						prior,
					)
					.await
					.map_err(NativeComponentLedgerError::Schema)?;
				}
			}
			Ok(outcome)
		})
		.await
	}

	async fn prepare_migration<'s>(
		&self,
		contracts: &crate::runtime::admission::NativeAdmittedContracts,
		tx: &mut crate::db::WriteTx<'_>,
		schema: &ComponentSchema<'_>,
		steps: &'s [super::ComponentSchemaMigrationStep<'s>],
	) -> Result<PreparedMigration<'s>, NativeOperationError<NativeComponentLedgerError>> {
		let fail = NativeOperationError::Operation;
		let route = contracts.storage_route();
		let version = schema.current_version();
		schema
			.plan(None, steps)
			.map_err(|error| fail(NativeComponentLedgerError::Schema(error)))?;
		if route == InstallationStorageRoute::SourceCoexistence {
			let ledger = self.v10_ledger().map_err(fail)?;
			let recorded = read_component_schema_for_migration(tx, &ledger, version)
				.await
				.map_err(|error| fail(NativeComponentLedgerError::Schema(error)))?;
			let plan = schema
				.plan(recorded, steps)
				.map_err(|error| fail(NativeComponentLedgerError::Schema(error)))?;
			return Ok(PreparedMigration::V10 { ledger, plan });
		}
		let declaration = self.component.declaration();
		let contract = contracts
			.bind(&declaration)
			.map_err(NativeOperationError::Admission)?;
		let bound = self
			.component
			.bind_schema(&contract)
			.map_err(|error| fail(NativeComponentLedgerError::Contract(error)))?;
		// After v10 writers retire, a component that no native migration has touched yet keeps
		// its version in the v10 ledger. A v10 release can no longer change that row, so
		// reading it before the native lock is exact.
		let v10_recorded = if reads_frozen_v10_ledger(route) {
			let ledger = self.v10_ledger().map_err(fail)?;
			fetch_component_schema_version_row_in_current_transaction(tx, &ledger, version)
				.await
				.map_err(|error| fail(NativeComponentLedgerError::Schema(error)))?
		} else {
			None
		};
		let lock = self
			.storage
			.lock(tx, &self.identity(version), self.conflict_attempts)
			.await
			.map_err(|error| fail(NativeComponentLedgerError::Storage(error)))?;
		let native = match lock
			.snapshot()
			.record()
			.map(|record| decode_record(&bound, record))
			.transpose()
		{
			Ok(native) => native,
			Err(error) => return Err(fail(keep_after(lock, error).await)),
		};
		let target = RecordedComponentSchemaVersion {
			version: version.version,
			fingerprint: version.fingerprint.to_owned(),
		};
		let native_is_current = native.as_ref() == Some(&target);
		let recorded = native.or(v10_recorded);
		// The entry is written before the statements run. The transaction keeps the entry's
		// row lock until it ends, so a concurrent migration waits, and a failed statement or
		// check rolls the entry back with the statements.
		if let Some(recorded) = &recorded
			&& (native_is_current
				|| (recorded.version > target.version
					&& schema
						.checks_for(recorded.version, &recorded.fingerprint)
						.is_some()))
		{
			lock.keep()
				.await
				.map_err(|error| fail(NativeComponentLedgerError::Storage(error)))?;
			return Ok(PreparedMigration::Native {
				plan: if recorded == &target {
					ComponentSchemaMigrationPlan::AlreadyCurrent
				} else {
					ComponentSchemaMigrationPlan::AlreadyCompatible {
						recorded: recorded.clone(),
					}
				},
			});
		}
		let observed_at = lock.snapshot().observed_at();
		let prepared = schema
			.plan(recorded, steps)
			.map_err(NativeComponentLedgerError::Schema)
			.and_then(|plan| {
				encode_record(
					&bound,
					&target,
					RecordMetadata {
						expires_at: None,
						updated_at: observed_at,
					},
				)
				.map(|record| (plan, record))
			});
		match prepared {
			Ok((plan, record)) => {
				lock.write(record)
					.await
					.map_err(|error| fail(NativeComponentLedgerError::Storage(error)))?;
				Ok(PreparedMigration::Native { plan })
			}
			Err(error) => Err(fail(keep_after(lock, error).await)),
		}
	}

	/// Checks the recorded version and fingerprint and runs the validation checks in one read
	/// transaction, without changing anything.
	pub(crate) async fn validate(
		&self,
		database: impl Database,
		schema: &ComponentSchema<'_>,
	) -> Result<(), NativeComponentLedgerError> {
		run_transaction_read(database, "db.component_ledger.validate", async |tx| {
			let (contracts, mut registration) = self
				.admission
				.execute_read_with_contracts(tx, async |contracts, tx| {
					let registration = self.read_registration(tx, &contracts, schema).await?;
					Ok((contracts, registration))
				})
				.await
				.map_err(NativeComponentLedgerError::from_operation)?;
			for _ in 0..self.conflict_attempts.get() {
				let checks = require_supported(registration.clone(), schema)?;
				let checked = execute_component_schema_checks(tx, checks).await;
				if checked
					.as_ref()
					.is_err_and(TransactionFailure::involves_database_failure)
				{
					return checked.map_err(NativeComponentLedgerError::Schema);
				}
				// Forward-only migrations commit DDL and ledger together. Matching
				// observations prove that no migration committed across these checks.
				let after = self
					.read_registration(tx, &contracts, schema)
					.await
					.map_err(NativeComponentLedgerError::from_operation)?;
				if after == registration {
					return checked.map_err(NativeComponentLedgerError::Schema);
				}
				registration = after;
			}
			Err(NativeComponentLedgerError::Schema(SchemaError::mismatch(
				"component schema changed repeatedly during validation",
			)))
		})
		.await
	}

	/// Reports the recorded version and fingerprint of `schema`'s component instance.
	pub(crate) async fn fetch(
		&self,
		database: impl Database,
		schema: &ComponentSchema<'_>,
	) -> Result<ComponentSchemaRegistration, NativeComponentLedgerError> {
		run_transaction_read(database, "db.component_ledger.fetch", async |tx| {
			self.admission
				.execute_read_with_contracts(tx, async |contracts, tx| {
					self.read_registration(tx, &contracts, schema).await
				})
				.await
				.map_err(NativeComponentLedgerError::from_operation)
		})
		.await
	}

	async fn read_registration(
		&self,
		tx: &mut crate::db::Tx<'_>,
		contracts: &crate::runtime::admission::NativeAdmittedContracts,
		schema: &ComponentSchema<'_>,
	) -> Result<ComponentSchemaRegistration, NativeOperationError<NativeComponentLedgerError>> {
		let fail = NativeOperationError::Operation;
		if contracts.storage_route() == InstallationStorageRoute::SourceCoexistence {
			let ledger = self.v10_ledger().map_err(fail)?;
			return fetch_component_registration_in_current_transaction(
				tx,
				&ledger,
				schema.current_version(),
			)
			.await
			.map_err(|error| fail(NativeComponentLedgerError::Schema(error)));
		}
		let declaration = self.component.declaration();
		let contract = contracts
			.bind(&declaration)
			.map_err(NativeOperationError::Admission)?;
		let bound = self
			.component
			.bind_schema(&contract)
			.map_err(|error| fail(NativeComponentLedgerError::Contract(error)))?;
		let version = schema.current_version();
		let snapshot = self
			.storage
			.read(tx, &self.identity(version))
			.await
			.map_err(|error| fail(NativeComponentLedgerError::Storage(error)))?;
		let mut recorded = snapshot
			.record()
			.map(|record| decode_record(&bound, record))
			.transpose()
			.map_err(fail)?;
		if recorded.is_none() && reads_frozen_v10_ledger(contracts.storage_route()) {
			let ledger = self.v10_ledger().map_err(fail)?;
			recorded =
				fetch_component_schema_version_row_in_current_transaction(tx, &ledger, version)
					.await
					.map_err(|error| fail(NativeComponentLedgerError::Schema(error)))?;
		}
		Ok(match recorded {
			Some(recorded) => ComponentSchemaRegistration::Recorded {
				version: recorded.version,
				fingerprint: recorded.fingerprint,
			},
			None => ComponentSchemaRegistration::Absent,
		})
	}
}

/// Whether a component without a native entry reads its frozen v10 ledger row: on an imported
/// installation after v10 writers retire.
fn reads_frozen_v10_ledger(route: InstallationStorageRoute) -> bool {
	matches!(
		route,
		InstallationStorageRoute::SourceRelocation | InstallationStorageRoute::NativeImported
	)
}

fn require_supported<'a>(
	registration: ComponentSchemaRegistration,
	schema: &ComponentSchema<'a>,
) -> Result<&'a [super::ComponentSchemaValidationCheck<'a>], NativeComponentLedgerError> {
	let version = schema.current_version();
	match registration {
		ComponentSchemaRegistration::Recorded {
			version: recorded,
			fingerprint,
		} if schema.checks_for(recorded, &fingerprint).is_some() => Ok(schema
			.checks_for(recorded, &fingerprint)
			.expect("the matching layout has checks")),
		ComponentSchemaRegistration::Recorded {
			version: recorded,
			fingerprint,
		} => Err(NativeComponentLedgerError::Schema(SchemaError::mismatch(
			format!(
				"component schema ledger for component {:?} instance {:?} records version {recorded} fingerprint {fingerprint:?}, expected version {} fingerprint {:?}",
				version.component, version.instance_key, version.version, version.fingerprint
			),
		))),
		ComponentSchemaRegistration::Absent => Err(NativeComponentLedgerError::Schema(
			SchemaError::mismatch(format!(
				"component schema ledger has no entry for component {:?} instance {:?}",
				version.component, version.instance_key
			)),
		)),
	}
}

async fn keep_after<L: PointRecordLock<Error = StateStorageError>>(
	lock: L,
	operation: NativeComponentLedgerError,
) -> NativeComponentLedgerError {
	match lock.keep().await {
		Ok(()) => operation,
		Err(cleanup) => NativeComponentLedgerError::CleanupFailed {
			operation_error: Box::new(operation),
			cleanup_error: cleanup,
		},
	}
}

fn decode_record(
	bound: &BoundStateSchema<'_, RecordedComponentSchemaVersion, LedgerCodecError>,
	record: &StoredRecord,
) -> Result<RecordedComponentSchemaVersion, NativeComponentLedgerError> {
	bound
		.decode_document(record.body())
		.map(|(_, entry)| entry)
		.map_err(NativeComponentLedgerError::Read)
}

fn encode_record(
	bound: &BoundStateSchema<'_, RecordedComponentSchemaVersion, LedgerCodecError>,
	entry: &RecordedComponentSchemaVersion,
	metadata: RecordMetadata,
) -> Result<EncodedRecord, NativeComponentLedgerError> {
	let body = bound
		.encode(entry)
		.map_err(NativeComponentLedgerError::Encode)?;
	EncodedRecord::derive(body, metadata, |_, _| {
		Ok::<_, NativeComponentLedgerError>(IndexFields::new())
	})
}

/// A component schema ledger operation failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeComponentLedgerError {
	#[error("the installation has no published v10 ledger for its source route")]
	V10LedgerAbsent,
	#[error("component schema ledger admission failed")]
	Admission(#[source] NativeAdmissionError),
	#[error("component schema ledger binding failed")]
	Contract(#[source] StateContractError),
	#[error("component schema ledger entry cannot be read")]
	Read(#[source] ContractStateReadError<LedgerCodecError>),
	#[error("component schema ledger entry cannot be encoded")]
	Encode(#[source] StateSchemaError<LedgerCodecError>),
	#[error("component schema ledger storage failed")]
	Storage(#[source] StateStorageError),
	#[error("component schema ledger work failed and its lock cleanup also failed")]
	CleanupFailed {
		#[source]
		operation_error: Box<Self>,
		cleanup_error: StateStorageError,
	},
	#[error("component schema migration or validation failed")]
	Schema(#[source] SchemaError),
	#[error("component schema ledger database work failed")]
	Database(#[source] DatabaseError),
	#[error("component schema ledger transaction protocol failed")]
	Protocol(#[source] ProtocolError),
	#[error("component schema ledger work failed and rollback also failed")]
	RollbackFailed {
		#[source]
		operation_error: Box<Self>,
		rollback_error: DatabaseError,
	},
}

impl NativeComponentLedgerError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Database(error) => Some(error),
			Self::Admission(error) => error.database_error(),
			Self::Storage(error) => error.database_error(),
			Self::Schema(error) => error.database_error(),
			Self::CleanupFailed {
				operation_error,
				cleanup_error,
			} => operation_error
				.database_error()
				.or_else(|| cleanup_error.database_error()),
			Self::RollbackFailed {
				operation_error,
				rollback_error,
			} => operation_error.database_error().or(Some(rollback_error)),
			_ => None,
		}
	}

	fn from_operation(error: NativeOperationError<Self>) -> Self {
		match error {
			NativeOperationError::Admission(error) => Self::Admission(error),
			NativeOperationError::Operation(error) => error,
		}
	}
}

impl TransactionFailure for NativeComponentLedgerError {
	fn from_protocol_error(error: ProtocolError) -> Self {
		Self::Protocol(error)
	}
	fn from_database_error(error: DatabaseError) -> Self {
		Self::Database(error)
	}
	fn from_rollback_failure(operation_error: Self, rollback_error: DatabaseError) -> Self {
		Self::RollbackFailed {
			operation_error: Box::new(operation_error),
			rollback_error,
		}
	}
	fn involves_database_failure(&self) -> bool {
		match self {
			Self::Database(_) | Self::RollbackFailed { .. } => true,
			Self::Schema(error) => error.involves_database_failure(),
			Self::Admission(error) => error.involves_database_failure(),
			Self::Storage(error) => error.database_error().is_some(),
			Self::CleanupFailed { cleanup_error, .. } => cleanup_error.database_error().is_some(),
			Self::V10LedgerAbsent
			| Self::Contract(_)
			| Self::Read(_)
			| Self::Encode(_)
			| Self::Protocol(_) => false,
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Database(error) => error.commit_outcome_is_unknown(),
			Self::RollbackFailed {
				operation_error, ..
			} => operation_error.commit_outcome_is_unknown(),
			Self::Schema(error) => error.commit_outcome_is_unknown(),
			Self::Admission(error) => error.commit_outcome_is_unknown(),
			Self::Storage(error) => error
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
			Self::CleanupFailed {
				operation_error, ..
			} => operation_error.commit_outcome_is_unknown(),
			Self::V10LedgerAbsent
			| Self::Contract(_)
			| Self::Read(_)
			| Self::Encode(_)
			| Self::Protocol(_) => false,
		}
	}
}

/// The class of a failed component schema operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgComponentSchemaErrorKind {
	/// The schema's own migration or validation failed, for example because the ledger records
	/// a newer version or another fingerprint, or a validation check failed. `schema_error`
	/// returns the cause.
	Schema,
	/// A database statement or the transaction's own control failed. The error's source chain
	/// holds the database error.
	Database,
	/// The runtime cannot operate on the ledger under the installation's recorded policy.
	Admission,
	/// The stored ledger entry could not be interpreted or changed.
	State,
}

/// A failed app component schema operation, with its complete cause chain.
pub struct PgComponentSchemaError {
	source: Box<NativeComponentLedgerError>,
}

impl PgComponentSchemaError {
	pub(crate) fn new(source: NativeComponentLedgerError) -> Self {
		Self {
			source: Box::new(source),
		}
	}

	fn involves_database_failure(&self) -> bool {
		self.source.involves_database_failure()
	}

	/// The class of the failure.
	pub fn kind(&self) -> PgComponentSchemaErrorKind {
		fn kind(error: &NativeComponentLedgerError) -> PgComponentSchemaErrorKind {
			match error {
				NativeComponentLedgerError::Schema(_) => PgComponentSchemaErrorKind::Schema,
				NativeComponentLedgerError::Database(_) => PgComponentSchemaErrorKind::Database,
				NativeComponentLedgerError::V10LedgerAbsent
				| NativeComponentLedgerError::Admission(_)
				| NativeComponentLedgerError::Contract(_)
				| NativeComponentLedgerError::Protocol(_) => PgComponentSchemaErrorKind::Admission,
				NativeComponentLedgerError::Read(_)
				| NativeComponentLedgerError::Encode(_)
				| NativeComponentLedgerError::Storage(_) => PgComponentSchemaErrorKind::State,
				NativeComponentLedgerError::CleanupFailed {
					operation_error, ..
				} => kind(operation_error),
				NativeComponentLedgerError::RollbackFailed {
					operation_error, ..
				} => kind(operation_error),
			}
		}
		// A migration statement that failed transiently, such as on a deadlock or a dropped
		// connection, can succeed when repeated; any other schema failure is the schema's own.
		if let NativeComponentLedgerError::Schema(source) = self.source.as_ref() {
			return if crate::db::is_transient_database_failure(source) {
				PgComponentSchemaErrorKind::Database
			} else {
				PgComponentSchemaErrorKind::Schema
			};
		}
		if self.involves_database_failure() {
			return PgComponentSchemaErrorKind::Database;
		}
		kind(&self.source)
	}

	/// The schema's migration or validation failure, when the kind is `Schema`, or when the
	/// kind is `Database` because a migration statement failed transiently.
	pub fn schema_error(&self) -> Option<&SchemaError> {
		fn schema(error: &NativeComponentLedgerError) -> Option<&SchemaError> {
			match error {
				NativeComponentLedgerError::Schema(source) => Some(source),
				NativeComponentLedgerError::CleanupFailed {
					operation_error, ..
				} => schema(operation_error),
				NativeComponentLedgerError::RollbackFailed {
					operation_error, ..
				} => schema(operation_error),
				_ => None,
			}
		}
		schema(&self.source)
	}

	/// Whether the migration's own `COMMIT` failed, which means that the migration may or may
	/// not have committed.
	pub fn commit_outcome_is_unknown(&self) -> bool {
		self.source.commit_outcome_is_unknown()
	}
}

impl std::fmt::Display for PgComponentSchemaError {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str("component schema operation failed")
	}
}

impl std::fmt::Debug for PgComponentSchemaError {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(formatter, "PgComponentSchemaError: {self}")?;
		let mut source = std::error::Error::source(self);
		while let Some(error) = source {
			write!(formatter, ": {error}")?;
			source = error.source();
		}
		Ok(())
	}
}

impl std::error::Error for PgComponentSchemaError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(self.source.as_ref())
	}
}
