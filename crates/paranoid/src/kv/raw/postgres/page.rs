//! Fixed ordinary raw page observations. Admission and logical ownership belong to callers.

mod headers;

use crate::db::AuditedSqlBuilder;
use crate::kv::Key;
use crate::kv::raw::model::page::RecordKeySelection;
use crate::kv::raw::model::page::{RawValuePageEntry, RecordPageRequest, RecordPageVisibility};
use crate::records::{
	RecordIdentity, RecordKey, RecordNamespace, StateTimestampMicros, StoredRecord,
};

use super::v10::relocating_point::PgRelocatingPointStorage;
use super::v10::source_point::{
	PgSourcePointStorage, SourcePhysicalError, SourcePhysicalRecord,
	append_source_snapshot_projection, decode_source_record,
};
use super::{
	AuditedSql, Bytea, DatabaseError, DatabaseOperationKind, PgPointStorage, PgQualifiedTableName,
	StateStorageError, Tx, decode_physical_record, owned_portable_query_as,
};

const READ_RAW_VALUE_PAGE_LABEL: &str = "db.state_storage.read_raw_value_page";

type PageRow = (
	Option<Bytea>,
	Option<Bytea>,
	Option<Bytea>,
	Option<i64>,
	Option<i64>,
	Option<String>,
	Option<bool>,
	Option<Bytea>,
	Option<i64>,
	Option<i64>,
	Option<bool>,
	Option<bool>,
	i64,
);

pub(crate) struct RawPhysicalPage {
	pub(crate) entries: Vec<RawValuePageEntry>,
	pub(crate) observed_at: StateTimestampMicros,
}

pub(crate) struct RawPhysicalPageRequest<'a> {
	pub(crate) namespace: &'a RecordNamespace,
	pub(crate) page: &'a RecordPageRequest,
}
impl RawPhysicalPageRequest<'_> {
	fn physical_selection(&self) -> &RecordKeySelection {
		self.page.selection()
	}
}

/// Default formatting exposes no key or opaque body; the known identity has a typed accessor.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RawPageStorageError {
	#[error("raw value page database work failed")]
	Database(#[from] DatabaseError),
	#[error("raw value page native record is invalid")]
	Native(#[from] StateStorageError),
	#[error("raw value page source record is invalid")]
	Source(#[from] SourcePhysicalError),
	#[error("raw value page source-domain key is not UTF-8")]
	SourceUtf8(#[from] std::str::Utf8Error),
	#[error("raw value page source-domain key is not canonical")]
	SourceKey(#[from] crate::kv::KeyError),
	#[error("raw value page has an invalid observation shape")]
	InvalidObservation,
	#[error("raw value page violates its selection, ordering, visibility, or row limit")]
	Bounds,
	#[error("raw value page does not share one authoritative clock")]
	InconsistentClock,
	#[error("raw value page found source and native values at the same identity")]
	DuplicatePhysicalValues,
	#[error("raw value page record could not be observed")]
	Record {
		identity: RecordIdentity,
		#[source]
		source: Box<Self>,
	},
}
impl RawPageStorageError {
	pub(crate) fn identity(&self) -> Option<&RecordIdentity> {
		match self {
			Self::Record { identity, .. } => Some(identity),
			_ => None,
		}
	}
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Database(error) => Some(error),
			Self::Native(error) => error.database_error(),
			Self::Record { source, .. } => source.database_error(),
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

pub(super) enum RawReadLayout<'a> {
	Native(&'a PgPointStorage),
	Source(&'a PgSourcePointStorage),
	Relocating(&'a PgRelocatingPointStorage),
}
impl RawReadLayout<'_> {
	pub(super) fn is_source_domain(&self) -> bool {
		!matches!(self, Self::Native(_))
	}
}

impl PgPointStorage {
	pub(crate) async fn read_raw_page(
		&self,
		tx: &mut Tx<'_>,
		request: RawPhysicalPageRequest<'_>,
	) -> Result<RawPhysicalPage, RawPageStorageError> {
		read_page(tx, request, RawReadLayout::Native(self)).await
	}
}
impl PgSourcePointStorage {
	pub(crate) async fn read_raw_page(
		&self,
		tx: &mut Tx<'_>,
		request: RawPhysicalPageRequest<'_>,
	) -> Result<RawPhysicalPage, RawPageStorageError> {
		read_page(tx, request, RawReadLayout::Source(self)).await
	}
}
impl PgRelocatingPointStorage {
	pub(crate) async fn read_raw_page(
		&self,
		tx: &mut Tx<'_>,
		request: RawPhysicalPageRequest<'_>,
	) -> Result<RawPhysicalPage, RawPageStorageError> {
		read_page(tx, request, RawReadLayout::Relocating(self)).await
	}
}

async fn read_page(
	tx: &mut Tx<'_>,
	request: RawPhysicalPageRequest<'_>,
	layout: RawReadLayout<'_>,
) -> Result<RawPhysicalPage, RawPageStorageError> {
	let mut completion = tx.track_state_mutation();
	let query = build_query(&layout);
	let prefix = request.page.selection().prefix();
	let upper = prefix.and_then(|prefix| prefix.exclusive_upper_bound());
	tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		READ_RAW_VALUE_PAGE_LABEL,
		Some(query.as_str()),
	);
	let query = owned_portable_query_as::<PageRow>(query)
		.bind_validated(request.namespace.as_bytes())
		.bind_validated(request.page.after_key().map(RecordKey::as_bytes))
		.bind_validated(i64::from(request.page.limit().get()))
		.bind_validated(prefix.map(|prefix| prefix.as_bytes()))
		.bind_validated(upper.as_deref())
		.bind_validated(request.page.visibility() == RecordPageVisibility::AllValued);
	let query = if layout.is_source_domain() {
		query
			.bind_validated(text_bound(
				request.page.after_key().map(RecordKey::as_bytes),
			))
			.bind_validated(text_bound(prefix.map(|prefix| prefix.as_bytes())))
			.bind_validated(text_bound(upper.as_deref()))
	} else {
		query
	};
	let rows = query
		.fetch_all(tx.inner.as_mut())
		.await
		.map_err(|error| error.with_operation_label(READ_RAW_VALUE_PAGE_LABEL))?;
	let result = parse_rows(request, rows, layout.is_source_domain())?;
	completion.complete();
	Ok(result)
}

// Byte bounds remain authoritative. Valid text bounds also permit an index seek; partial
// UTF-8 prefixes and other byte bounds retain their original byte comparison.
pub(super) fn text_bound(bytes: Option<&[u8]>) -> Option<&str> {
	let text = std::str::from_utf8(bytes?).ok()?;
	(!text.contains('\0')).then_some(text)
}

const KEY_BOUNDS_SQL: &str = r#"
WHERE ($2::pg_catalog.bytea IS NULL OR key_bytes OPERATOR(pg_catalog.>) $2::pg_catalog.bytea)
  AND ($4::pg_catalog.bytea IS NULL OR key_bytes OPERATOR(pg_catalog.>=) $4::pg_catalog.bytea)
  AND ($5::pg_catalog.bytea IS NULL OR key_bytes OPERATOR(pg_catalog.<) $5::pg_catalog.bytea)
"#;

#[derive(Clone, Copy)]
enum PageProjection {
	Body,
	Header,
}
impl PageProjection {
	fn native_input(self) -> &'static str {
		match self {
			Self::Body => "body",
			Self::Header => "body IS NOT NULL AS body",
		}
	}
	fn source_input(self) -> &'static str {
		match self {
			Self::Body => "value",
			Self::Header => "value IS NOT NULL AS value",
		}
	}
	fn null_body(self) -> &'static str {
		match self {
			Self::Body => "NULL::pg_catalog.bytea",
			Self::Header => "NULL::pg_catalog.bool",
		}
	}
}

fn append_native(
	sql: &mut AuditedSqlBuilder,
	table: &PgQualifiedTableName,
	projection: PageProjection,
) {
	sql.push_text("native_rows AS (SELECT namespace, key AS key_bytes, ");
	sql.push_text(projection.native_input());
	sql.push_text(", expires_at, updated_at, index_fields FROM ONLY ");
	sql.push_argument(table);
	sql.push_text(" WHERE namespace OPERATOR(pg_catalog.=) $1::pg_catalog.bytea AND body IS NOT NULL), native_selected_rows AS (SELECT * FROM native_rows ");
	sql.push_text(KEY_BOUNDS_SQL);
	sql.push_text(")");
}

fn append_source(
	sql: &mut AuditedSqlBuilder,
	table: &PgQualifiedTableName,
	projection: PageProjection,
) {
	sql.push_text("source_raw AS (SELECT key, value, expires_at, updated_at, FALSE AS inserted_reservation FROM (SELECT key, ");
	sql.push_text(projection.source_input());
	sql.push_text(
		", expires_at, updated_at, pg_catalog.convert_to(key, 'UTF8') AS key_bytes FROM ONLY ",
	);
	sql.push_argument(table);
	sql.push_text(") source_rows ");
	sql.push_text(KEY_BOUNDS_SQL);
	sql.push_text(" AND ($7::pg_catalog.text IS NULL OR key OPERATOR(pg_catalog.~>~) $7::pg_catalog.text) AND ($8::pg_catalog.text IS NULL OR key OPERATOR(pg_catalog.~>=~) $8::pg_catalog.text) AND ($9::pg_catalog.text IS NULL OR key OPERATOR(pg_catalog.~<~) $9::pg_catalog.text)");
	append_source_snapshot_projection(sql);
}

fn build_query(layout: &RawReadLayout<'_>) -> AuditedSql {
	build_query_with_projection(layout, PageProjection::Body)
}

fn build_query_with_projection(
	layout: &RawReadLayout<'_>,
	projection: PageProjection,
) -> AuditedSql {
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text("WITH ");
	match layout {
		RawReadLayout::Native(storage) => append_native(&mut sql, storage.table(), projection),
		RawReadLayout::Source(storage) => append_source(&mut sql, storage.table(), projection),
		RawReadLayout::Relocating(storage) => {
			append_source(&mut sql, storage.source_table(), projection);
			sql.push_text(", ");
			append_native(&mut sql, storage.native_storage().table(), projection);
		}
	}
	sql.push_text(", logical_records AS (");
	append_logical_projection_with_projection(&mut sql, layout, projection);
	sql.push_text(
		r#"
), observation AS MATERIALIZED (
    SELECT pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.clock_timestamp())
        OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8 AS observed_at,
        $1::pg_catalog.bytea AS requested_namespace
)
SELECT record.key_bytes, record.native_namespace, record.native_body, record.native_expiry,
    record.native_update, record.native_indexes, record.source_present, record.source_body,
    record.source_expiry, record.source_update, record.expiry_valid, record.update_valid,
    observation.observed_at
FROM observation LEFT JOIN LATERAL (
    SELECT * FROM logical_records
    WHERE $6::pg_catalog.bool
       OR (native_namespace IS NOT NULL AND source_present)
       OR (source_present AND NOT expiry_valid)
       OR (CASE WHEN native_namespace IS NOT NULL THEN native_expiry ELSE source_expiry END IS NULL)
       OR (CASE WHEN native_namespace IS NOT NULL THEN native_expiry ELSE source_expiry END
           OPERATOR(pg_catalog.>) observation.observed_at)
"#,
	);
	sql.push_text(if matches!(layout, RawReadLayout::Source(_)) {
		" ORDER BY source_key USING OPERATOR(pg_catalog.~<~) "
	} else {
		" ORDER BY key_bytes USING OPERATOR(pg_catalog.<) "
	});
	sql.push_text(
		r#"
    LIMIT $3
) record ON TRUE
ORDER BY record.key_bytes USING OPERATOR(pg_catalog.<)
"#,
	);
	sql.finish()
}

/// Shared exact valued-source/native projection before each operation selects rows.
pub(super) fn append_logical_projection(sql: &mut AuditedSqlBuilder, layout: &RawReadLayout<'_>) {
	append_logical_projection_with_projection(sql, layout, PageProjection::Body);
}

fn append_logical_projection_with_projection(
	sql: &mut AuditedSqlBuilder,
	layout: &RawReadLayout<'_>,
	projection: PageProjection,
) {
	match layout {
		RawReadLayout::Native(_) => {
			sql.push_text(
				r#"
SELECT key_bytes, namespace AS native_namespace, body AS native_body,
    expires_at AS native_expiry, updated_at AS native_update,
    index_fields::pg_catalog.text AS native_indexes, FALSE AS source_present,
    "#,
			);
			sql.push_text(projection.null_body());
			sql.push_text(
				r#" AS source_body, NULL::pg_catalog.int8 AS source_expiry,
    NULL::pg_catalog.int8 AS source_update, NULL::pg_catalog.bool AS expiry_valid,
    NULL::pg_catalog.bool AS update_valid FROM native_selected_rows
"#,
			);
		}
		RawReadLayout::Source(_) => {
			sql.push_text(
				r#"
SELECT key AS source_key, pg_catalog.convert_to(key, 'UTF8') AS key_bytes,
    NULL::pg_catalog.bytea AS native_namespace, "#,
			);
			sql.push_text(projection.null_body());
			sql.push_text(
				r#" AS native_body,
    NULL::pg_catalog.int8 AS native_expiry, NULL::pg_catalog.int8 AS native_update,
    NULL::pg_catalog.text AS native_indexes, TRUE AS source_present,
    value AS source_body, expiry_micros AS source_expiry, update_micros AS source_update,
    expiry_valid, update_valid FROM source_snapshot
"#,
			);
		}
		RawReadLayout::Relocating(_) => sql.push_text(
			r#"
SELECT COALESCE(native.key_bytes, pg_catalog.convert_to(source.key, 'UTF8')) AS key_bytes,
    native.namespace AS native_namespace, native.body AS native_body,
    native.expires_at AS native_expiry, native.updated_at AS native_update,
    native.index_fields::pg_catalog.text AS native_indexes,
    source.key IS NOT NULL AS source_present, source.value AS source_body,
    source.expiry_micros AS source_expiry, source.update_micros AS source_update,
    source.expiry_valid, source.update_valid
FROM native_selected_rows native FULL OUTER JOIN source_snapshot source
  ON native.key_bytes OPERATOR(pg_catalog.=) pg_catalog.convert_to(source.key, 'UTF8')
"#,
		),
	}
}

fn parse_rows(
	request: RawPhysicalPageRequest<'_>,
	rows: Vec<PageRow>,
	is_source_domain: bool,
) -> Result<RawPhysicalPage, RawPageStorageError> {
	let observed_at = StateTimestampMicros::new(
		rows.first()
			.ok_or(RawPageStorageError::InvalidObservation)?
			.12,
	);
	let row_count = rows.len();
	if row_count > request.page.limit().get() as usize {
		return Err(RawPageStorageError::Bounds);
	}
	let mut entries: Vec<RawValuePageEntry> = Vec::with_capacity(row_count);
	for row in rows {
		let clock = row.12;
		let (
			key,
			native_namespace,
			native_body,
			native_expires_at,
			native_updated_at,
			native_indexes,
			source_present,
			source_body,
			source_expires_at,
			source_updated_at,
			expires_at_valid,
			updated_at_valid,
			_,
		) = row;
		let Some(key) = key else {
			if row_count != 1
				|| native_namespace.is_some()
				|| native_body.is_some()
				|| native_expires_at.is_some()
				|| native_updated_at.is_some()
				|| native_indexes.is_some()
				|| source_present.is_some()
				|| source_body.is_some()
				|| source_expires_at.is_some()
				|| source_updated_at.is_some()
				|| expires_at_valid.is_some()
				|| updated_at_valid.is_some()
			{
				return Err(RawPageStorageError::InvalidObservation);
			}
			continue;
		};
		let key = RecordKey::new(key.0);
		let identity = RecordIdentity::new(request.namespace.clone(), key.clone());
		let entry = (|| {
			if clock != observed_at.as_i64() {
				return Err(RawPageStorageError::InconsistentClock);
			}
			if !request.physical_selection().contains(&key)
				|| request.page.after_key().is_some_and(|after| &key <= after)
				|| entries.last().is_some_and(|entry| entry.key() >= &key)
			{
				return Err(RawPageStorageError::Bounds);
			}
			if is_source_domain {
				Key::from_persisted_text(std::str::from_utf8(key.as_bytes())?.to_owned())?;
			}
			let source_present = source_present.ok_or(RawPageStorageError::InvalidObservation)?;
			let record = decode_raw_physical_record(
				&identity,
				RawPhysicalRecordFields {
					native_namespace,
					native_body,
					native_expires_at,
					native_updated_at,
					native_indexes,
					source_present,
					source_body,
					source_expires_at,
					source_updated_at,
					expires_at_valid,
					updated_at_valid,
				},
			)?
			.ok_or(RawPageStorageError::InvalidObservation)?;
			let (bytes, metadata, _) = record.into_parts();
			if request.page.visibility() == RecordPageVisibility::Live
				&& !metadata.is_live_at(observed_at)
			{
				return Err(RawPageStorageError::Bounds);
			}
			Ok(RawValuePageEntry::new(key, bytes, metadata))
		})()
		.map_err(|error: RawPageStorageError| error.with_identity(identity))?;
		entries.push(entry);
	}
	Ok(RawPhysicalPage {
		entries,
		observed_at,
	})
}

/// Physical fields shared by raw pages and complete requested-key observations.
pub(super) struct RawPhysicalRecordFields {
	pub(super) native_namespace: Option<Bytea>,
	pub(super) native_body: Option<Bytea>,
	pub(super) native_expires_at: Option<i64>,
	pub(super) native_updated_at: Option<i64>,
	pub(super) native_indexes: Option<String>,
	pub(super) source_present: bool,
	pub(super) source_body: Option<Bytea>,
	pub(super) source_expires_at: Option<i64>,
	pub(super) source_updated_at: Option<i64>,
	pub(super) expires_at_valid: Option<bool>,
	pub(super) updated_at_valid: Option<bool>,
}

/// One parsed physical origin before a consumer chooses its representation.
pub(super) enum PhysicalRecordObservation {
	Native(StoredRecord),
	Source(SourcePhysicalRecord),
	Absent,
}
impl PhysicalRecordObservation {
	pub(super) fn into_raw(self) -> Option<StoredRecord> {
		match self {
			Self::Native(record) => Some(record),
			Self::Source(record) => Some(record.into_point()),
			Self::Absent => None,
		}
	}
}

/// Projects one actual parsed physical origin to the existing opaque Raw representation.
pub(super) fn decode_raw_physical_record(
	identity: &RecordIdentity,
	fields: RawPhysicalRecordFields,
) -> Result<Option<StoredRecord>, RawPageStorageError> {
	decode_physical_observation(identity, fields).map(PhysicalRecordObservation::into_raw)
}

/// Decodes one actual physical origin or statement-local absence; it grants no lock.
pub(super) fn decode_physical_observation(
	identity: &RecordIdentity,
	fields: RawPhysicalRecordFields,
) -> Result<PhysicalRecordObservation, RawPageStorageError> {
	let RawPhysicalRecordFields {
		native_namespace,
		native_body,
		native_expires_at,
		native_updated_at,
		native_indexes,
		source_present,
		source_body,
		source_expires_at,
		source_updated_at,
		expires_at_valid,
		updated_at_valid,
	} = fields;
	if native_namespace.is_some() && source_present {
		return Err(RawPageStorageError::DuplicatePhysicalValues);
	}
	match native_namespace {
		Some(namespace) => {
			if namespace.0 != identity.namespace().as_bytes()
				|| source_body.is_some()
				|| source_expires_at.is_some()
				|| source_updated_at.is_some()
				|| expires_at_valid.is_some()
				|| updated_at_valid.is_some()
			{
				return Err(RawPageStorageError::InvalidObservation);
			}
			decode_physical_record(
				identity,
				(
					native_body,
					native_expires_at,
					native_updated_at,
					native_indexes.ok_or(RawPageStorageError::InvalidObservation)?,
				),
			)
			.map(|record| match record {
				Some(record) => PhysicalRecordObservation::Native(record),
				None => PhysicalRecordObservation::Absent,
			})
			.map_err(RawPageStorageError::from)
		}
		None => {
			if native_body.is_some()
				|| native_expires_at.is_some()
				|| native_updated_at.is_some()
				|| native_indexes.is_some()
			{
				return Err(RawPageStorageError::InvalidObservation);
			}
			if !source_present {
				if source_body.is_some()
					|| source_expires_at.is_some()
					|| source_updated_at.is_some()
					|| expires_at_valid.is_some()
					|| updated_at_valid.is_some()
				{
					return Err(RawPageStorageError::InvalidObservation);
				}
				return Ok(PhysicalRecordObservation::Absent);
			}
			Ok(PhysicalRecordObservation::Source(decode_source_record((
				source_body,
				source_expires_at,
				source_updated_at,
				expires_at_valid.ok_or(RawPageStorageError::InvalidObservation)?,
				updated_at_valid.ok_or(RawPageStorageError::InvalidObservation)?,
			))?))
		}
	}
}
