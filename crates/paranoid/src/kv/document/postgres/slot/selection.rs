//! Ordered NULL reservation and opportunistic Header locking precede the actual final clock.

use std::collections::BTreeSet;

use crate::db::{AuditedSql, AuditedSqlBuilder, DatabaseOperationKind, PgQualifiedTableName};
use crate::records::{
	RecordIdentity, RecordKey, RecordMetadata, RecordNamespace, StateTimestampMicros,
};

use super::{RecordSlotError, SlotOwner};
use crate::db::owned_portable_query_as;
use crate::kv::raw::postgres::delete::{acquire, records::HeaderRow};
use crate::records::postgres::{NativePhysicalHeaderFields, decode_physical_header};

const ACQUIRE_LABEL: &str = "db.state_storage.record_slot.acquire";
const SELECT_LABEL: &str = "db.state_storage.record_slot.select";

#[derive(Clone, Copy)]
enum SelectionQuery {
	ReserveAndSelect,
	Select,
}

pub(super) async fn acquire(
	owner: &mut SlotOwner<'_, '_>,
	remaining: &[RecordKey],
) -> Result<
	(
		Option<(RecordKey, Option<RecordMetadata>)>,
		StateTimestampMicros,
	),
	RecordSlotError,
> {
	execute(owner, remaining, SelectionQuery::ReserveAndSelect).await
}
pub(super) async fn select(
	owner: &mut SlotOwner<'_, '_>,
	remaining: &[RecordKey],
) -> Result<
	(
		Option<(RecordKey, Option<RecordMetadata>)>,
		StateTimestampMicros,
	),
	RecordSlotError,
> {
	execute(owner, remaining, SelectionQuery::Select).await
}
async fn execute(
	owner: &mut SlotOwner<'_, '_>,
	remaining: &[RecordKey],
	selection: SelectionQuery,
) -> Result<
	(
		Option<(RecordKey, Option<RecordMetadata>)>,
		StateTimestampMicros,
	),
	RecordSlotError,
> {
	let sql = query(owner.storage.table(), selection);
	let operation = match selection {
		SelectionQuery::ReserveAndSelect => ACQUIRE_LABEL,
		SelectionQuery::Select => SELECT_LABEL,
	};
	owner.tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		operation,
		Some(sql.as_str()),
	);
	let keys = remaining
		.iter()
		.map(|key| key.as_bytes().to_vec())
		.collect::<Vec<_>>();
	let rows = owned_portable_query_as::<HeaderRow>(sql)
		.bind_validated(owner.namespace.as_bytes())
		.bind_validated(keys)
		.fetch_all(owner.tx.inner.as_mut())
		.await
		.map_err(|error| error.with_operation_label(operation))?;
	let expected = remaining.iter().collect::<BTreeSet<_>>();
	let mut current = None;
	let mut clock = None;
	for row in rows {
		if clock.is_some() {
			return Err(RecordSlotError::Coverage);
		}
		match row.0 {
			1 if matches!(selection, SelectionQuery::ReserveAndSelect) && current.is_none() => {
				let (identity, metadata) = parse_header(owner, row)?;
				if metadata.is_some() || !owner.inserted.insert(identity.key().clone()) {
					return Err(owner.identify(RecordSlotError::Coverage, identity));
				}
			}
			2 => {
				let (identity, metadata) = parse_header(owner, row)?;
				if current.is_some() || !expected.contains(identity.key()) {
					return Err(owner.identify(RecordSlotError::Coverage, identity));
				}
				current = Some((identity.key().clone(), metadata));
			}
			4 => clock = Some(acquire::parse_clock(row).map_err(RecordSlotError::Clock)?),
			_ => return Err(RecordSlotError::Coverage),
		}
	}
	Ok((current, clock.ok_or(RecordSlotError::Coverage)?))
}
fn parse_header(
	owner: &SlotOwner<'_, '_>,
	row: HeaderRow,
) -> Result<(RecordIdentity, Option<RecordMetadata>), RecordSlotError> {
	let (
		_,
		key,
		rank,
		present,
		held,
		namespace,
		body,
		expiry,
		update,
		indexes,
		expiry_valid,
		update_valid,
		clock,
	) = row;
	let key = RecordKey::new(key.ok_or(RecordSlotError::Shape)?.0);
	let namespace = RecordNamespace::new(namespace.ok_or(RecordSlotError::Shape)?.0)
		.map_err(|_| RecordSlotError::Shape)?;
	let identity = RecordIdentity::new(namespace, key);
	let parsed = (|| {
		if identity.namespace() != owner.namespace
			|| owner.candidates.input_index(identity.key()).is_none()
			|| rank.is_some()
			|| present != Some(true)
			|| held != Some(true)
			|| expiry_valid.is_some()
			|| update_valid.is_some()
			|| clock.is_some()
		{
			return Err(RecordSlotError::Coverage);
		}
		let header = decode_physical_header(NativePhysicalHeaderFields {
			identity: &identity,
			body_present: body.ok_or(RecordSlotError::Shape)?,
			expires_at: expiry,
			updated_at: update,
			index_fields: &indexes.ok_or(RecordSlotError::Shape)?,
		})?;
		Ok(header.map(|header| header.metadata))
	})();
	let metadata = parsed.map_err(|error| owner.identify(error, identity.clone()))?;
	Ok((identity, metadata))
}
fn query(table: &PgQualifiedTableName, selection: SelectionQuery) -> AuditedSql {
	let mut sql = AuditedSqlBuilder::new();
	if matches!(selection, SelectionQuery::ReserveAndSelect) {
		sql.push_text("WITH ordered_keys AS MATERIALIZED (SELECT key FROM pg_catalog.unnest($2::pg_catalog.bytea[]) input(key) ORDER BY key USING OPERATOR(pg_catalog.<)) INSERT INTO ");
		sql.push_argument(table);
		sql.push_text(" (namespace,key,body,expires_at,updated_at,index_fields) SELECT $1::pg_catalog.bytea,key,NULL,NULL,NULL,'{}'::pg_catalog.jsonb FROM ordered_keys ORDER BY key USING OPERATOR(pg_catalog.<) ON CONFLICT (namespace,key) DO NOTHING RETURNING 1::pg_catalog.int8,key,NULL::pg_catalog.int8,TRUE,TRUE,namespace,body IS NOT NULL,expires_at,updated_at,index_fields::pg_catalog.text,NULL::pg_catalog.bool,NULL::pg_catalog.bool,NULL::pg_catalog.int8; ");
	}
	sql.push_text("WITH locked_native AS MATERIALIZED (SELECT namespace,key,body IS NOT NULL AS body_present,expires_at,updated_at,index_fields FROM ONLY ");
	sql.push_argument(table);
	sql.push_text(" WHERE namespace OPERATOR(pg_catalog.=) $1::pg_catalog.bytea AND key OPERATOR(pg_catalog.=) ANY($2::pg_catalog.bytea[]) AND (body IS NULL OR (body IS NOT NULL AND expires_at OPERATOR(pg_catalog.<=) (EXTRACT(EPOCH FROM pg_catalog.statement_timestamp()) OPERATOR(pg_catalog.*) 1000000))) ORDER BY key USING OPERATOR(pg_catalog.<) LIMIT 1 FOR UPDATE SKIP LOCKED) SELECT 2::pg_catalog.int8,key,NULL::pg_catalog.int8,TRUE,TRUE,namespace,body_present,expires_at,updated_at,index_fields::pg_catalog.text,NULL::pg_catalog.bool,NULL::pg_catalog.bool,NULL::pg_catalog.int8 FROM locked_native");
	acquire::append_clock(&mut sql);
	sql.finish()
}
