//! Bounded native valued headers share one statement clock without transferring bodies.

use std::fmt;

use crate::kv::raw::model::page::{RecordKeySelection, RecordPageRequest, RecordPageVisibility};
use crate::records::{
	RecordIdentity, RecordKey, RecordMetadata, RecordNamespace, StateTimestampMicros,
};

use super::{
	AuditedSql, Bytea, DatabaseError, DatabaseOperationKind, NativePhysicalHeaderFields,
	PgPointStorage, PgQualifiedTableName, StateStorageError, Tx, decode_physical_header,
	owned_portable_query_as,
};

const READ_KEY_HEADERS_LABEL: &str = "db.state_storage.read_key_headers";
type HeaderRow = (
	Option<Bytea>,
	Option<Bytea>,
	Option<bool>,
	Option<i64>,
	Option<i64>,
	Option<String>,
	i64,
);

pub(crate) struct KeyHeaderPageFields<'a> {
	pub(crate) namespace: &'a RecordNamespace,
	pub(crate) page: &'a RecordPageRequest,
	pub(crate) selection: &'a RecordKeySelection,
}
pub(crate) struct KeyHeaderEntry {
	pub(crate) key: RecordKey,
	pub(crate) metadata: RecordMetadata,
}
pub(crate) struct KeyHeaderPage {
	pub(crate) entries: Vec<KeyHeaderEntry>,
	pub(crate) observed_at: StateTimestampMicros,
}

#[derive(thiserror::Error)]
pub(crate) enum KeyHeaderPageError {
	#[error("key header page database work failed")]
	Database(#[from] DatabaseError),
	#[error("key header page physical header is invalid")]
	Physical(#[from] StateStorageError),
	#[error("key header page has an invalid observation shape")]
	InvalidObservation,
	#[error("key header page violates namespace, selection, ordering, visibility or row limit")]
	Bounds,
	#[error("key header page does not share one authoritative clock")]
	InconsistentClock,
	#[error("key header page record could not be observed")]
	Record {
		identity: RecordIdentity,
		#[source]
		source: Box<Self>,
	},
}
impl KeyHeaderPageError {
	pub(crate) fn identity(&self) -> Option<&RecordIdentity> {
		match self {
			Self::Record { identity, .. } => Some(identity),
			_ => None,
		}
	}
	pub(crate) fn primary(&self) -> &Self {
		match self {
			Self::Record { source, .. } => source.primary(),
			_ => self,
		}
	}
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self.primary() {
			Self::Database(source) => Some(source),
			Self::Physical(source) => source.database_error(),
			_ => None,
		}
	}
	fn with_identity(self, identity: RecordIdentity) -> Self {
		Self::Record {
			identity,
			source: Box::new(self),
		}
	}
}
impl fmt::Debug for KeyHeaderPageError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("KeyHeaderPageError { actual_cause_retained: true }")
	}
}

fn query(table: &PgQualifiedTableName) -> AuditedSql {
	crate::audited_sql!(
		r#"
WITH observation AS MATERIALIZED (
    SELECT pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.clock_timestamp())
        OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8 AS observed_at
)
SELECT record.namespace, record.key, record.body_present, record.expires_at,
    record.updated_at, record.index_fields, observation.observed_at
FROM observation LEFT JOIN LATERAL (
    SELECT namespace, key, body IS NOT NULL AS body_present, expires_at, updated_at,
        index_fields::pg_catalog.text AS index_fields
    FROM ONLY {table}
    WHERE namespace OPERATOR(pg_catalog.=) $1::pg_catalog.bytea
      AND body IS NOT NULL
      AND ($2::pg_catalog.bytea IS NULL OR key OPERATOR(pg_catalog.>) $2::pg_catalog.bytea)
      AND ($4::pg_catalog.bytea IS NULL OR key OPERATOR(pg_catalog.>=) $4::pg_catalog.bytea)
      AND ($5::pg_catalog.bytea IS NULL OR key OPERATOR(pg_catalog.<) $5::pg_catalog.bytea)
      AND ($6::pg_catalog.bool OR expires_at IS NULL
           OR expires_at OPERATOR(pg_catalog.>) observation.observed_at)
    ORDER BY key USING OPERATOR(pg_catalog.<)
    LIMIT $3::pg_catalog.int8
) record ON TRUE
ORDER BY record.key USING OPERATOR(pg_catalog.<)
"#,
		table = table
	)
}

impl PgPointStorage {
	pub(crate) async fn read_key_headers(
		&self,
		tx: &mut Tx<'_>,
		fields: KeyHeaderPageFields<'_>,
	) -> Result<KeyHeaderPage, KeyHeaderPageError> {
		let mut completion = tx.track_state_mutation();
		let query = query(self.table());
		let prefix = fields.selection.prefix();
		let upper = prefix.and_then(|prefix| prefix.exclusive_upper_bound());
		tx.record_database_operation(
			DatabaseOperationKind::FetchAll,
			READ_KEY_HEADERS_LABEL,
			Some(query.as_str()),
		);
		let rows = owned_portable_query_as::<HeaderRow>(query)
			.bind_validated(fields.namespace.as_bytes())
			.bind_validated(fields.page.after_key().map(RecordKey::as_bytes))
			.bind_validated(i64::from(fields.page.limit().get()))
			.bind_validated(prefix.map(|prefix| prefix.as_bytes()))
			.bind_validated(upper.as_deref())
			.bind_validated(fields.page.visibility() == RecordPageVisibility::AllValued)
			.fetch_all(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(READ_KEY_HEADERS_LABEL))?;
		let result = parse_rows(fields, rows)?;
		completion.complete();
		Ok(result)
	}
}

fn parse_rows(
	fields: KeyHeaderPageFields<'_>,
	rows: Vec<HeaderRow>,
) -> Result<KeyHeaderPage, KeyHeaderPageError> {
	let observed_at = StateTimestampMicros::new(
		rows.first()
			.ok_or(KeyHeaderPageError::InvalidObservation)?
			.6,
	);
	let row_count = rows.len();
	if row_count > fields.page.limit().get() as usize {
		return Err(KeyHeaderPageError::Bounds);
	}
	let mut entries: Vec<KeyHeaderEntry> = Vec::with_capacity(row_count);
	for (namespace, key, body_present, expires_at, updated_at, index_fields, clock) in rows {
		let Some(key) = key else {
			if row_count != 1
				|| namespace.is_some()
				|| body_present.is_some()
				|| expires_at.is_some()
				|| updated_at.is_some()
				|| index_fields.is_some()
			{
				return Err(KeyHeaderPageError::InvalidObservation);
			}
			continue;
		};
		let key = RecordKey::new(key.0);
		let identity = RecordIdentity::new(fields.namespace.clone(), key.clone());
		let entry = (|| {
			if clock != observed_at.as_i64() {
				return Err(KeyHeaderPageError::InconsistentClock);
			}
			if namespace.as_ref().map(|namespace| namespace.0.as_slice())
				!= Some(fields.namespace.as_bytes())
				|| !fields.selection.contains(&key)
				|| fields.page.after_key().is_some_and(|after| &key <= after)
				|| entries.last().is_some_and(|entry| entry.key >= key)
			{
				return Err(KeyHeaderPageError::Bounds);
			}
			if body_present != Some(true) {
				return Err(KeyHeaderPageError::InvalidObservation);
			}
			let header = decode_physical_header(NativePhysicalHeaderFields {
				identity: &identity,
				body_present: true,
				expires_at,
				updated_at,
				index_fields: index_fields
					.as_deref()
					.ok_or(KeyHeaderPageError::InvalidObservation)?,
			})?
			.ok_or(KeyHeaderPageError::InvalidObservation)?;
			if fields.page.visibility() == RecordPageVisibility::Live
				&& !header.metadata.is_live_at(observed_at)
			{
				return Err(KeyHeaderPageError::Bounds);
			}
			Ok(KeyHeaderEntry {
				key,
				metadata: header.metadata,
			})
		})()
		.map_err(|error: KeyHeaderPageError| error.with_identity(identity))?;
		entries.push(entry);
	}
	Ok(KeyHeaderPage {
		entries,
		observed_at,
	})
}
