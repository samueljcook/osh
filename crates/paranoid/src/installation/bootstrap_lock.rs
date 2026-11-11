use crate::db::{DatabaseOperationKind, WriteTx, owned_portable_query};
use crate::installation::BootstrapError;

const ADVISORY_LOCK_CLASS_ID: i32 = i32::from_be_bytes(*b"para");
const ADVISORY_LOCK_OBJECT_ID: i32 = i32::from_be_bytes(*b"boot");
const ACQUIRE_TRANSACTION_LOCK_LABEL: &str = "db.installation.acquire_transaction_lock";

pub(crate) async fn acquire_transaction_lock(tx: &mut WriteTx<'_>) -> Result<(), BootstrapError> {
	let statement = "SELECT pg_catalog.pg_advisory_xact_lock($1, $2)";
	tx.record_database_operation(
		DatabaseOperationKind::Execute,
		ACQUIRE_TRANSACTION_LOCK_LABEL,
		Some(statement),
	);
	owned_portable_query(statement)
		.bind_validated(ADVISORY_LOCK_CLASS_ID)
		.bind_validated(ADVISORY_LOCK_OBJECT_ID)
		.execute(tx.inner.as_mut())
		.await
		.map_err(|error| error.with_operation_label(ACQUIRE_TRANSACTION_LOCK_LABEL))?;
	Ok(())
}
