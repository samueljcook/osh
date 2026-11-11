//! Selected values and exact new-reservation cleanup are prepared before the value write.

use std::collections::BTreeSet;

use crate::db::{AuditedSql, AuditedSqlBuilder, DatabaseOperationKind};
use crate::records::{EncodedRecord, RecordIdentity, RecordKey, RecordNamespace};

use super::{RecordSlotError, SlotOwner};
use crate::db::bytea::Bytea;
use crate::db::owned_portable_query_as;
use crate::records::postgres::reservation_cleanup::{
	NativeReservationCleanupKeysParam, NativeReservationCleanupSqlFields,
	append_native_reservation_cleanup, write_selected_slot_record,
};

const CLEANUP_LABEL: &str = "db.state_storage.record_slot.cleanup";
type CleanupRow = (i64, Bytea, Bytea);

struct PreparedCleanup {
	sql: Option<AuditedSql>,
	keys: Vec<Vec<u8>>,
	expected: BTreeSet<RecordKey>,
}
/// Removes the reservation rows that this acquisition inserted and that no write fills: the
/// unselected inserted reservations, and `selected_reservation` when a kept selection inserted it.
fn prepare_cleanup(
	owner: &SlotOwner<'_, '_>,
	selected_reservation: Option<&RecordKey>,
) -> PreparedCleanup {
	let expected = owner
		.inserted
		.iter()
		.chain(selected_reservation)
		.cloned()
		.collect::<BTreeSet<_>>();
	let keys = expected.iter().map(|key| key.as_bytes().to_vec()).collect();
	let sql = if expected.is_empty() {
		None
	} else {
		let mut sql = AuditedSqlBuilder::new();
		append_native_reservation_cleanup(
			&mut sql,
			NativeReservationCleanupSqlFields {
				table: owner.storage.table(),
				keys_param: NativeReservationCleanupKeysParam::Second,
			},
		);
		Some(sql.finish())
	};
	PreparedCleanup {
		sql,
		keys,
		expected,
	}
}
pub(super) async fn write(
	owner: &mut SlotOwner<'_, '_>,
	identity: &RecordIdentity,
	encoded: EncodedRecord,
) -> Result<(), RecordSlotError> {
	let cleanup = prepare_cleanup(owner, None);
	write_selected_slot_record(owner.storage, owner.tx, identity, &encoded)
		.await
		.map_err(|error| owner.identify(error.into(), identity.clone()))?;
	cleanup_prepared(owner, cleanup).await
}
pub(super) async fn keep(
	owner: &mut SlotOwner<'_, '_>,
	selected_reservation: Option<&RecordKey>,
) -> Result<(), RecordSlotError> {
	let cleanup = prepare_cleanup(owner, selected_reservation);
	cleanup_prepared(owner, cleanup).await
}
async fn cleanup_prepared(
	owner: &mut SlotOwner<'_, '_>,
	prepared: PreparedCleanup,
) -> Result<(), RecordSlotError> {
	cleanup(owner, prepared)
		.await
		.map_err(|error| RecordSlotError::Cleanup {
			source: Box::new(error),
		})
}
async fn cleanup(
	owner: &mut SlotOwner<'_, '_>,
	prepared: PreparedCleanup,
) -> Result<(), RecordSlotError> {
	let PreparedCleanup {
		sql,
		keys,
		expected,
	} = prepared;
	let Some(sql) = sql else {
		return Ok(());
	};
	owner.tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		CLEANUP_LABEL,
		Some(sql.as_str()),
	);
	let rows = owned_portable_query_as::<CleanupRow>(sql)
		.bind_validated(owner.namespace.as_bytes())
		.bind_validated(keys)
		.fetch_all(owner.tx.inner.as_mut())
		.await
		.map_err(|error| error.with_operation_label(CLEANUP_LABEL))?;
	let mut seen = BTreeSet::new();
	for (tag, namespace, key) in rows {
		let namespace = RecordNamespace::new(namespace.0).map_err(|_| RecordSlotError::Shape)?;
		let key = RecordKey::new(key.0);
		let identity = RecordIdentity::new(namespace, key.clone());
		if tag != 2
			|| identity.namespace() != owner.namespace
			|| !expected.contains(&key)
			|| !seen.insert(key)
		{
			return Err(owner.identify(RecordSlotError::Coverage, identity));
		}
	}
	if let Some(key) = expected.difference(&seen).next() {
		return Err(owner.identify_error_by_key(RecordSlotError::Coverage, key));
	}
	Ok(())
}
