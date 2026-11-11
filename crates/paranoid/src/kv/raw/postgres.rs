//! PostgreSQL adapters for Raw KV and shared namespace header operations.

pub(crate) mod aggregate;
pub(crate) mod delete;
pub(crate) mod key_headers;
pub(crate) mod many;
pub(crate) mod page;
pub(crate) mod slot;

use crate::db::bytea::Bytea;
use crate::db::{
	AuditedSql, DatabaseError, DatabaseOperationKind, PgQualifiedTableName, Tx, WriteTx,
	owned_portable_query_as,
};
use crate::records::postgres::{
	NativePhysicalHeaderFields, PgPointStorage, ReservationOwnership, StateMutationCompletion,
	StateStorageError, attempt_boundary, decode_physical_header, decode_physical_record,
	encode_index_fields, record_many, reservation_cleanup, v10,
};
