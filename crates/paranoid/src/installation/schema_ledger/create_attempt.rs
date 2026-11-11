//! The ledger creation savepoint owns its exact transaction and rollback obligation.

use crate::db::{DatabaseOperationKind, SchemaError, TransactionPhase, Tx, owned_portable_query};
use crate::records::postgres::StateMutationCompletion;

use super::{
	SCHEMA_LEDGER_CREATE_SAVEPOINT_LABEL, SCHEMA_LEDGER_CREATE_TABLE_LABEL,
	SCHEMA_LEDGER_RELEASE_SAVEPOINT_LABEL, SCHEMA_LEDGER_ROLLBACK_SAVEPOINT_LABEL,
	is_concurrent_schema_ledger_create_race,
};

const CREATE_SAVEPOINT_SQL: &str = "SAVEPOINT __paranoid_schema_ledger_create";
const RESTORE_SAVEPOINT_SQL: &str = "ROLLBACK TO SAVEPOINT __paranoid_schema_ledger_create";
const RELEASE_SAVEPOINT_SQL: &str = "RELEASE SAVEPOINT __paranoid_schema_ledger_create";

pub(super) struct SchemaLedgerCreateAttempt<'a, 'tx> {
	tx: &'a mut Tx<'tx>,
	completion: StateMutationCompletion,
}

impl<'a, 'tx> SchemaLedgerCreateAttempt<'a, 'tx> {
	pub(super) async fn begin(tx: &'a mut Tx<'tx>) -> Result<Self, SchemaError> {
		let completion = tx.track_state_mutation();
		execute_boundary(tx, CreateBoundary::Create).await?;
		Ok(Self { tx, completion })
	}

	pub(super) async fn create(mut self, statement: &str) -> Result<(), SchemaError> {
		self.tx.record_database_operation(
			DatabaseOperationKind::Execute,
			SCHEMA_LEDGER_CREATE_TABLE_LABEL,
			Some(statement),
		);
		let created = owned_portable_query(sqlx::AssertSqlSafe(statement))
			.execute(self.tx.inner.as_mut())
			.await;
		match created {
			Ok(_) => {
				execute_boundary(self.tx, CreateBoundary::Release).await?;
				self.completion.complete();
				Ok(())
			}
			Err(error) if is_concurrent_schema_ledger_create_race(error.driver_error()) => {
				execute_boundary(self.tx, CreateBoundary::Restore).await?;
				execute_boundary(self.tx, CreateBoundary::Release).await?;
				self.completion.complete();
				Ok(())
			}
			Err(error) => {
				let restored = execute_boundary(self.tx, CreateBoundary::Restore).await;
				let released = execute_boundary(self.tx, CreateBoundary::Release).await;
				if restored.is_ok() && released.is_ok() {
					self.completion.complete();
				}
				Err(error
					.with_operation_label(SCHEMA_LEDGER_CREATE_TABLE_LABEL)
					.into())
			}
		}
	}
}

enum CreateBoundary {
	Create,
	Restore,
	Release,
}

impl CreateBoundary {
	fn statement(&self) -> &'static str {
		match self {
			Self::Create => CREATE_SAVEPOINT_SQL,
			Self::Restore => RESTORE_SAVEPOINT_SQL,
			Self::Release => RELEASE_SAVEPOINT_SQL,
		}
	}
	fn label(&self) -> &'static str {
		match self {
			Self::Create => SCHEMA_LEDGER_CREATE_SAVEPOINT_LABEL,
			Self::Restore => SCHEMA_LEDGER_ROLLBACK_SAVEPOINT_LABEL,
			Self::Release => SCHEMA_LEDGER_RELEASE_SAVEPOINT_LABEL,
		}
	}
	fn phase(&self) -> TransactionPhase {
		match self {
			Self::Create => TransactionPhase::Savepoint,
			Self::Restore => TransactionPhase::RollbackToSavepoint,
			Self::Release => TransactionPhase::ReleaseSavepoint,
		}
	}
}

async fn execute_boundary(tx: &mut Tx<'_>, boundary: CreateBoundary) -> Result<(), SchemaError> {
	let label = boundary.label();
	let statement = boundary.statement();
	tx.record_database_operation(DatabaseOperationKind::Execute, label, Some(statement));
	owned_portable_query(statement)
		.execute(tx.inner.as_mut())
		.await
		.map_err(|error| {
			error
				.with_operation_label(label)
				.with_phase(boundary.phase())
		})?;
	Ok(())
}
