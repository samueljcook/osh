//! Shared exact NULL-reservation deletion text and native slot reservation guard.

use crate::db::{AuditedSqlBuilder, PgQualifiedTableName, WriteTx};
use crate::records::{EncodedRecord, RecordIdentity};

use super::{PgPointStorage, StateStorageError, encode_index_fields};

pub(crate) enum NativeReservationCleanupKeysParam {
	Second,
	Fourth,
}
pub(crate) struct NativeReservationCleanupSqlFields<'a> {
	pub(crate) table: &'a PgQualifiedTableName,
	pub(crate) keys_param: NativeReservationCleanupKeysParam,
}
pub(crate) fn append_native_reservation_cleanup(
	sql: &mut AuditedSqlBuilder,
	fields: NativeReservationCleanupSqlFields<'_>,
) {
	sql.push_text("DELETE FROM ONLY ");
	sql.push_argument(fields.table);
	sql.push_text(" WHERE namespace OPERATOR(pg_catalog.=) $1::pg_catalog.bytea AND key OPERATOR(pg_catalog.=) ANY(");
	sql.push_text(match fields.keys_param {
		NativeReservationCleanupKeysParam::Second => "$2",
		NativeReservationCleanupKeysParam::Fourth => "$4",
	});
	sql.push_text("::pg_catalog.bytea[]) AND body IS NULL AND expires_at IS NULL AND updated_at IS NULL AND index_fields OPERATOR(pg_catalog.=) '{}'::pg_catalog.jsonb RETURNING 2::pg_catalog.int8,namespace,key");
}

/// Writes one selected slot record under an active native slot selection guard.
pub(crate) async fn write_selected_slot_record(
	storage: &PgPointStorage,
	tx: &mut WriteTx<'_>,
	identity: &RecordIdentity,
	encoded: &EncodedRecord,
) -> Result<(), StateStorageError> {
	let indexes = encode_index_fields(encoded.record().index_fields());
	storage.write_record(tx, identity, encoded, &indexes).await
}
