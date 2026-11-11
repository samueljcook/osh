//! One physical header page retains original metadata without transferring record bodies.

use super::{
	PageProjection, RawPageStorageError, RawPhysicalPageRequest, RawReadLayout,
	build_query_with_projection, text_bound,
};
use crate::db::bytea::Bytea;
use crate::db::{DatabaseOperationKind, Tx, owned_portable_query_as};
use crate::kv::Key;
use crate::kv::raw::model::page::RecordKeySelection;
use crate::kv::raw::model::page::{RecordPageRequest, RecordPageVisibility};
use crate::kv::raw::postgres::key_headers::{KeyHeaderEntry, KeyHeaderPage};
use crate::records::postgres::v10::relocating_point::PgRelocatingPointStorage;
use crate::records::postgres::v10::source_point::{
	PgSourcePointStorage, SourcePhysicalHeaderFields, decode_source_header,
};
use crate::records::postgres::{
	NativePhysicalHeaderFields, PgPointStorage, decode_physical_header,
};
use crate::records::{RecordIdentity, RecordKey, RecordNamespace, StateTimestampMicros};

const READ_RAW_KEY_PAGE_LABEL: &str = "db.state_storage.raw_page.read_raw_key_page";
type HeaderRow = (
	Option<Bytea>,
	Option<Bytea>,
	Option<bool>,
	Option<i64>,
	Option<i64>,
	Option<String>,
	Option<bool>,
	Option<bool>,
	Option<i64>,
	Option<i64>,
	Option<bool>,
	Option<bool>,
	i64,
);

struct HeaderPageFields<'a> {
	namespace: &'a RecordNamespace,
	page: &'a RecordPageRequest,
	selection: &'a RecordKeySelection,
}

impl PgPointStorage {
	pub(crate) async fn read_raw_key_page(
		&self,
		tx: &mut Tx<'_>,
		request: RawPhysicalPageRequest<'_>,
	) -> Result<KeyHeaderPage, RawPageStorageError> {
		read_raw_headers(tx, request, RawReadLayout::Native(self)).await
	}
}
impl PgSourcePointStorage {
	pub(crate) async fn read_raw_key_page(
		&self,
		tx: &mut Tx<'_>,
		request: RawPhysicalPageRequest<'_>,
	) -> Result<KeyHeaderPage, RawPageStorageError> {
		read_raw_headers(tx, request, RawReadLayout::Source(self)).await
	}
}
impl PgRelocatingPointStorage {
	pub(crate) async fn read_raw_key_page(
		&self,
		tx: &mut Tx<'_>,
		request: RawPhysicalPageRequest<'_>,
	) -> Result<KeyHeaderPage, RawPageStorageError> {
		read_raw_headers(tx, request, RawReadLayout::Relocating(self)).await
	}
}
async fn read_raw_headers(
	tx: &mut Tx<'_>,
	request: RawPhysicalPageRequest<'_>,
	layout: RawReadLayout<'_>,
) -> Result<KeyHeaderPage, RawPageStorageError> {
	let fields = HeaderPageFields {
		namespace: request.namespace,
		page: request.page,
		selection: request.physical_selection(),
	};
	read_headers(tx, fields, layout).await
}

async fn read_headers(
	tx: &mut Tx<'_>,
	fields: HeaderPageFields<'_>,
	layout: RawReadLayout<'_>,
) -> Result<KeyHeaderPage, RawPageStorageError> {
	if layout.is_source_domain()
		&& let Some(after) = fields.page.after_key()
	{
		Key::from_persisted_text(std::str::from_utf8(after.as_bytes())?.to_owned())?;
	}
	let mut completion = tx.track_state_mutation();
	let query = build_query_with_projection(&layout, PageProjection::Header);
	let prefix = fields.selection.prefix();
	let upper = prefix.and_then(|prefix| prefix.exclusive_upper_bound());
	tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		READ_RAW_KEY_PAGE_LABEL,
		Some(query.as_str()),
	);
	let query = owned_portable_query_as::<HeaderRow>(query)
		.bind_validated(fields.namespace.as_bytes())
		.bind_validated(fields.page.after_key().map(RecordKey::as_bytes))
		.bind_validated(i64::from(fields.page.limit().get()))
		.bind_validated(prefix.map(|prefix| prefix.as_bytes()))
		.bind_validated(upper.as_deref())
		.bind_validated(fields.page.visibility() == RecordPageVisibility::AllValued);
	let query = if layout.is_source_domain() {
		query
			.bind_validated(text_bound(fields.page.after_key().map(RecordKey::as_bytes)))
			.bind_validated(text_bound(prefix.map(|prefix| prefix.as_bytes())))
			.bind_validated(text_bound(upper.as_deref()))
	} else {
		query
	};
	let rows = query
		.fetch_all(tx.inner.as_mut())
		.await
		.map_err(|error| error.with_operation_label(READ_RAW_KEY_PAGE_LABEL))?;
	let page = parse_rows(fields, rows, layout.is_source_domain())?;
	completion.complete();
	Ok(page)
}

fn parse_rows(
	fields: HeaderPageFields<'_>,
	rows: Vec<HeaderRow>,
	is_source_domain: bool,
) -> Result<KeyHeaderPage, RawPageStorageError> {
	let observed_at = StateTimestampMicros::new(
		rows.first()
			.ok_or(RawPageStorageError::InvalidObservation)?
			.12,
	);
	let row_count = rows.len();
	if row_count > fields.page.limit().get() as usize {
		return Err(RawPageStorageError::Bounds);
	}
	let mut entries: Vec<KeyHeaderEntry> = Vec::with_capacity(row_count);
	for (
		key,
		native_namespace,
		native_body_present,
		native_expires_at,
		native_updated_at,
		native_indexes,
		source_present,
		source_body_present,
		source_expires_at,
		source_updated_at,
		expires_at_valid,
		updated_at_valid,
		clock,
	) in rows
	{
		let Some(key) = key else {
			if row_count != 1
				|| native_namespace.is_some()
				|| native_body_present.is_some()
				|| native_expires_at.is_some()
				|| native_updated_at.is_some()
				|| native_indexes.is_some()
				|| source_present.is_some()
				|| source_body_present.is_some()
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
		let identity = RecordIdentity::new(fields.namespace.clone(), key.clone());
		let entry = (|| {
			if clock != observed_at.as_i64() {
				return Err(RawPageStorageError::InconsistentClock);
			}
			if !fields.selection.contains(&key)
				|| fields.page.after_key().is_some_and(|after| &key <= after)
				|| entries.last().is_some_and(|entry| entry.key >= key)
			{
				return Err(RawPageStorageError::Bounds);
			}
			if is_source_domain {
				Key::from_persisted_text(std::str::from_utf8(key.as_bytes())?.to_owned())?;
			}
			let source_present = source_present.ok_or(RawPageStorageError::InvalidObservation)?;
			if native_namespace.is_some() && source_present {
				return Err(RawPageStorageError::DuplicatePhysicalValues);
			}
			let metadata = match native_namespace {
				Some(namespace) => {
					if namespace.0 != fields.namespace.as_bytes()
						|| source_body_present.is_some()
						|| source_expires_at.is_some()
						|| source_updated_at.is_some()
						|| expires_at_valid.is_some()
						|| updated_at_valid.is_some()
					{
						return Err(RawPageStorageError::InvalidObservation);
					}
					decode_physical_header(NativePhysicalHeaderFields {
						identity: &identity,
						body_present: native_body_present
							.ok_or(RawPageStorageError::InvalidObservation)?,
						expires_at: native_expires_at,
						updated_at: native_updated_at,
						index_fields: native_indexes
							.as_deref()
							.ok_or(RawPageStorageError::InvalidObservation)?,
					})?
					.ok_or(RawPageStorageError::InvalidObservation)?
					.metadata
				}
				None => {
					if native_body_present.is_some()
						|| native_expires_at.is_some()
						|| native_updated_at.is_some()
						|| native_indexes.is_some()
						|| !source_present
					{
						return Err(RawPageStorageError::InvalidObservation);
					}
					decode_source_header(SourcePhysicalHeaderFields {
						body_present: source_body_present
							.ok_or(RawPageStorageError::InvalidObservation)?,
						expires_at: source_expires_at,
						updated_at: source_updated_at,
						expires_at_valid: expires_at_valid
							.ok_or(RawPageStorageError::InvalidObservation)?,
						updated_at_valid: updated_at_valid
							.ok_or(RawPageStorageError::InvalidObservation)?,
					})?
				}
			};
			if fields.page.visibility() == RecordPageVisibility::Live
				&& !metadata.is_live_at(observed_at)
			{
				return Err(RawPageStorageError::Bounds);
			}
			Ok(KeyHeaderEntry { key, metadata })
		})()
		.map_err(|error: RawPageStorageError| error.with_identity(identity))?;
		entries.push(entry);
	}
	Ok(KeyHeaderPage {
		entries,
		observed_at,
	})
}
