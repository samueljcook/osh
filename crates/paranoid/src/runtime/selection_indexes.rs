//! Transactional maintenance of additive record-selection indexes.

use super::PgRuntime;
use crate::db::{DatabaseOperationKind, owned_portable_query, run_transaction_write};
use crate::installation::component_ledger::NativeComponentLedgerError;
use crate::installation::{
	ComponentSchema, ComponentSchemaStatement, ComponentSchemaValidationCheck,
	ComponentSchemaVersion,
};
use crate::queue::postgres::due::{
	task_due_index_name, task_due_index_statement, task_due_index_validation_check,
};

const INDEX_MIGRATION_SETTINGS_SQL: &str = "SET LOCAL search_path TO pg_catalog; \
	SELECT pg_catalog.set_config('lock_timeout', $1::pg_catalog.text, true)";

impl PgRuntime {
	pub(super) async fn install_queue_list_index(&self) -> Result<(), NativeComponentLedgerError> {
		use crate::queue::postgres::list_index;
		let bindings = self.administration.native_bindings();
		let table = bindings.queue().storage().table();
		let statements =
			[
				ComponentSchemaStatement::from_audited_sql(list_index::create_index_statement(
					table, true,
				))
				.expect("the Queue listing index is one fixed DDL statement"),
			];
		let checks = [
			ComponentSchemaValidationCheck::from_audited_boolean_expression(
				list_index::index_validation_check(table),
			)
			.expect("the Queue listing index has a fixed catalog check"),
		];
		let instance_key = table.quoted().to_string();
		let schema = ComponentSchema::new(
			ComponentSchemaVersion {
				component: "paranoid.queue-task-list-index",
				instance_key: &instance_key,
				version: 1,
				fingerprint: "valued.namespace-task-c-key.btree.v1",
			},
			&statements,
			&[],
			&checks,
		)
		.expect("the Queue listing index schema is complete");
		self.install_selection_index_schema(&schema).await
	}

	/// This method uses the shared schema ledger so that concurrent requests serialize and
	/// failed DDL rolls back with its ledger entry. Another call can resolve uncertain completion.
	pub(super) async fn install_queue_task_index(&self) -> Result<(), NativeComponentLedgerError> {
		let bindings = self.administration.native_bindings();
		let queue = bindings.queue();
		let table = queue.storage().table();
		let index = task_due_index_name();
		let statements = [
			ComponentSchemaStatement::from_audited_sql(task_due_index_statement(
				table, &index, true,
			))
			.expect("the Queue index is one fixed DDL statement"),
		];
		let checks = [
			ComponentSchemaValidationCheck::from_audited_boolean_expression(
				task_due_index_validation_check(table, &index),
			)
			.expect("the Queue index has a fixed catalog check"),
		];
		let instance_key = table.quoted().to_string();
		let schema = ComponentSchema::new(
			ComponentSchemaVersion {
				component: "paranoid.queue-task-selection-index",
				instance_key: &instance_key,
				version: 1,
				fingerprint: "pending.namespace-task-due-key.btree.v1",
			},
			&statements,
			&[],
			&checks,
		)
		.expect("the Queue index schema is complete");
		self.install_selection_index_schema(&schema).await
	}

	pub(super) async fn install_record_expiry_indexes(
		&self,
	) -> Result<(), NativeComponentLedgerError> {
		use crate::records::postgres::record_expiry::{
			record_expiry_index_check, record_expiry_index_name, record_expiry_index_statement,
		};
		let bindings = self.administration.native_bindings();
		for storage in bindings.raw_record_containers() {
			let table = storage.table();
			let index = record_expiry_index_name(table);
			let statements =
				[
					ComponentSchemaStatement::from_audited_sql(record_expiry_index_statement(
						table, &index, true,
					))
					.expect("the expiry index is one fixed DDL statement"),
				];
			let checks = [
				ComponentSchemaValidationCheck::from_audited_boolean_expression(
					record_expiry_index_check(table, &index),
				)
				.expect("the expiry index has a fixed catalog check"),
			];
			let instance_key = table.quoted().to_string();
			let schema = ComponentSchema::new(
				ComponentSchemaVersion {
					component: "paranoid.record-expiry-index",
					instance_key: &instance_key,
					version: 1,
					fingerprint: "valued.namespace-expiry-key.btree.v1",
				},
				&statements,
				&[],
				&checks,
			)
			.expect("the expiry index schema is complete");
			self.install_selection_index_schema(&schema).await?;
		}
		Ok(())
	}

	async fn install_selection_index_schema(
		&self,
		schema: &ComponentSchema<'_>,
	) -> Result<(), NativeComponentLedgerError> {
		run_transaction_write(
			&self.leases.control_pool,
			"db.install_selection_index",
			async |tx| {
				// This owned transaction contains no application callback. A fixed path makes
				// PostgreSQL's catalog expression output independent of the role's search path.
				tx.record_database_operation(
					DatabaseOperationKind::Execute,
					"db.configure_index_migration",
					Some(INDEX_MIGRATION_SETTINGS_SQL),
				);
				owned_portable_query(INDEX_MIGRATION_SETTINGS_SQL)
					.bind_validated(format!(
						"{}ms",
						super::parameters::UPGRADE_CONTROL_PIN_WAIT.as_millis()
					))
					.execute(tx.inner.as_mut())
					.await
					.map_err(NativeComponentLedgerError::Database)?;
				self.component_ledger.migrate(&mut *tx, schema).await?;
				Ok(())
			},
		)
		.await
	}
}
