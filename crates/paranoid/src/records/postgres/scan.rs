//! Bounded physical record observations for library-owned controller discovery.
//!
//! Callers supply a validated container and establish controller prerequisites separately.
//! Each page or maximum is one statement observation, without durable completion or admission.

use crate::records::{
	InvalidRecordPageError, NamespacePageRequest, RecordIdentity, RecordKey, RecordNamespace,
	RecordPageEntry, RecordPageRead, StateTimestampMicros,
};

use super::{
	Bytea, DatabaseError, DatabaseOperationKind, PgPointStorage, StateStorageError, Tx,
	decode_physical_record, owned_portable_query_as,
};

const READ_PAGE_LABEL: &str = "db.state_storage.read_page";
const READ_VERIFICATION_PAGE_LABEL: &str = "db.state_storage.read_verification_page";
const READ_MAXIMUM_VALUED_KEY_LABEL: &str = "db.state_storage.read_maximum_valued_key";

type PageRow = (
	Option<Bytea>,
	Option<Bytea>,
	Option<Bytea>,
	Option<i64>,
	Option<i64>,
	Option<String>,
	i64,
);
type MaximumRow = (Option<Bytea>, Option<Bytea>, i64);

/// An inclusive upper bound with the existing exclusive lower cursor and row limit.
/// Parsing excludes an exhausted or reversed range; this request grants no verification authority.
pub(crate) struct RecordVerificationPageRequest {
	page: NamespacePageRequest,
	upper_key: RecordKey,
}
impl RecordVerificationPageRequest {
	pub(crate) fn new(
		page: NamespacePageRequest,
		upper_key: RecordKey,
	) -> Result<Self, RecordPageError> {
		if page.after_key().is_some_and(|after| after >= &upper_key) {
			return Err(RecordPageError::InvalidVerificationRange);
		}
		Ok(Self { page, upper_key })
	}
	pub(crate) fn page(&self) -> &NamespacePageRequest {
		&self.page
	}
	pub(crate) fn upper_key(&self) -> &RecordKey {
		&self.upper_key
	}
}

/// Empty differs from an actual empty byte key. Neither observation supplies a writer barrier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RecordMaximumValuedKey {
	Empty,
	Key(RecordKey),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecordMaximumRead {
	maximum: RecordMaximumValuedKey,
}
impl RecordMaximumRead {
	pub(crate) fn maximum(&self) -> &RecordMaximumValuedKey {
		&self.maximum
	}
}

/// Page failures expose no raw identity, body, projection values, or driver parameters.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RecordPageError {
	#[error("native record page database work failed")]
	Database(#[from] DatabaseError),
	#[error("native record page physical parsing failed")]
	Record(#[from] StateStorageError),
	#[error("native record page violates its requested bounds")]
	Bounds(#[from] InvalidRecordPageError),
	#[error("native record page has an invalid physical observation shape")]
	InvalidObservationShape,
	#[error("native record page does not share one authoritative clock")]
	InconsistentClock,
	#[error(
		"native record verification page requires a lower cursor below its inclusive upper key"
	)]
	InvalidVerificationRange,
	#[error("native record verification page contains a key above its inclusive upper key")]
	UpperBoundViolation,
}

impl PgPointStorage {
	/// Discovers valued keys for conversion. Callers must acquire and decode every selected
	/// record before changing it; this observation supplies no metadata or mutation authority.
	pub(crate) async fn read_conversion_keys(
		&self,
		tx: &mut Tx<'_>,
		request: &NamespacePageRequest,
	) -> Result<Vec<RecordKey>, RecordPageError> {
		let mut completion = tx.track_state_mutation();
		let query = crate::audited_sql!(
			"SELECT key FROM ONLY {table} WHERE namespace OPERATOR(pg_catalog.=) $1 \
			 AND body IS NOT NULL \
			 AND ($2::pg_catalog.bytea IS NULL OR key OPERATOR(pg_catalog.>) $2::pg_catalog.bytea) \
			 ORDER BY key USING OPERATOR(pg_catalog.<) LIMIT $3",
			table = &self.table,
		);
		const LABEL: &str = "db.state_storage.read_conversion_keys";
		tx.record_database_operation(DatabaseOperationKind::FetchAll, LABEL, Some(query.as_str()));
		let rows = owned_portable_query_as::<(Bytea,)>(query)
			.bind_validated(request.namespace().as_bytes())
			.bind_validated(request.after_key().map(RecordKey::as_bytes))
			.bind_validated(i64::from(request.limit().get()))
			.fetch_all(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(LABEL))?;
		if rows.len() > request.limit().get() as usize {
			return Err(InvalidRecordPageError::LimitExceeded.into());
		}
		let mut keys = Vec::with_capacity(rows.len());
		for (bytes,) in rows {
			let key = RecordKey::new(bytes.0);
			if request.after_key().is_some_and(|after| &key <= after) {
				return Err(InvalidRecordPageError::LowerBoundViolation.into());
			}
			if keys.last().is_some_and(|previous| previous >= &key) {
				return Err(InvalidRecordPageError::KeyOrderViolation.into());
			}
			keys.push(key);
		}
		completion.complete();
		Ok(keys)
	}

	/// Reads one bounded namespace page, including expired values and excluding reservations.
	pub(crate) async fn read_page(
		&self,
		tx: &mut Tx<'_>,
		request: &NamespacePageRequest,
	) -> Result<RecordPageRead, RecordPageError> {
		self.read_page_bounded(tx, request, None, READ_PAGE_LABEL)
			.await
	}

	/// Reads a closed verification range through one full-row statement and its materialized clock.
	/// No body refetch, projector, encoder, writer exclusion or durable checkpoint is supplied
	/// here.
	pub(crate) async fn read_verification_page(
		&self,
		tx: &mut Tx<'_>,
		request: &RecordVerificationPageRequest,
	) -> Result<RecordPageRead, RecordPageError> {
		self.read_page_bounded(
			tx,
			request.page(),
			Some(request.upper_key()),
			READ_VERIFICATION_PAGE_LABEL,
		)
		.await
	}

	async fn read_page_bounded(
		&self,
		tx: &mut Tx<'_>,
		request: &NamespacePageRequest,
		upper_key: Option<&RecordKey>,
		operation: &'static str,
	) -> Result<RecordPageRead, RecordPageError> {
		let mut completion = tx.track_state_mutation();
		let query = crate::audited_sql!(
			r#"
WITH observation AS MATERIALIZED (
    SELECT pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.clock_timestamp())
        OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8 AS observed_at
)
SELECT record.namespace, record.key, record.body, record.expires_at,
    record.updated_at, record.index_fields::pg_catalog.text, observation.observed_at
FROM observation
LEFT JOIN LATERAL (
    SELECT namespace, key, body, expires_at, updated_at, index_fields
    FROM ONLY {table}
    WHERE namespace OPERATOR(pg_catalog.=) $1
      AND ($2::pg_catalog.bytea IS NULL OR key OPERATOR(pg_catalog.>) $2::pg_catalog.bytea)
      AND ($4::pg_catalog.bytea IS NULL OR key OPERATOR(pg_catalog.<=) $4::pg_catalog.bytea)
      AND body IS NOT NULL
    ORDER BY key USING OPERATOR(pg_catalog.<)
    LIMIT $3
) record ON true
ORDER BY record.key USING OPERATOR(pg_catalog.<)
"#,
			table = &self.table
		);
		tx.record_database_operation(
			DatabaseOperationKind::FetchAll,
			operation,
			Some(query.as_str()),
		);
		let rows = owned_portable_query_as::<PageRow>(query)
			.bind_validated(request.namespace().as_bytes())
			.bind_validated(request.after_key().map(RecordKey::as_bytes))
			.bind_validated(i64::from(request.limit().get()))
			.bind_validated(upper_key.map(RecordKey::as_bytes))
			.fetch_all(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(operation))?;
		let page = parse_page_rows(request, rows)?;
		if upper_key.is_some_and(|upper| {
			page.entries()
				.iter()
				.any(|entry| entry.identity().key() > upper)
		}) {
			return Err(RecordPageError::UpperBoundViolation);
		}
		completion.complete();
		Ok(page)
	}

	/// Observes at most one maximum valued key and one clock in the exact namespace.
	/// Expired values are included. Empty observations exclude only rows visible to this statement.
	pub(crate) async fn read_maximum_valued_key(
		&self,
		tx: &mut Tx<'_>,
		namespace: &RecordNamespace,
	) -> Result<RecordMaximumRead, RecordPageError> {
		let mut completion = tx.track_state_mutation();
		let query = crate::audited_sql!(
			r#"
WITH observation AS MATERIALIZED (
    SELECT pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.clock_timestamp())
        OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8 AS observed_at
)
SELECT record.namespace, record.key, observation.observed_at
FROM observation
LEFT JOIN LATERAL (
    SELECT namespace, key
    FROM ONLY {table}
    WHERE namespace OPERATOR(pg_catalog.=) $1 AND body IS NOT NULL
    ORDER BY key USING OPERATOR(pg_catalog.>)
    LIMIT 1
) record ON true
"#,
			table = &self.table
		);
		tx.record_database_operation(
			DatabaseOperationKind::FetchAll,
			READ_MAXIMUM_VALUED_KEY_LABEL,
			Some(query.as_str()),
		);
		let mut rows = owned_portable_query_as::<MaximumRow>(query)
			.bind_validated(namespace.as_bytes())
			.fetch_all(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(READ_MAXIMUM_VALUED_KEY_LABEL))?;
		if rows.len() != 1 {
			return Err(RecordPageError::InvalidObservationShape);
		}
		let (record_namespace, key, _observed_at) =
			rows.pop().ok_or(RecordPageError::InvalidObservationShape)?;
		let maximum = match (record_namespace, key) {
			(None, None) => RecordMaximumValuedKey::Empty,
			(Some(record_namespace), Some(key)) => {
				if record_namespace.0 != namespace.as_bytes() {
					return Err(InvalidRecordPageError::NamespaceMismatch.into());
				}
				RecordMaximumValuedKey::Key(RecordKey::new(key.0))
			}
			_ => return Err(RecordPageError::InvalidObservationShape),
		};
		completion.complete();
		Ok(RecordMaximumRead { maximum })
	}
}

/// Both ordinary and ranged pages use this same strict full-row decoder.
fn parse_page_rows(
	request: &NamespacePageRequest,
	rows: Vec<PageRow>,
) -> Result<RecordPageRead, RecordPageError> {
	let observed_at = rows
		.first()
		.ok_or(RecordPageError::InvalidObservationShape)?
		.6;
	let row_count = rows.len();
	let mut entries = Vec::with_capacity(row_count);
	for (namespace, key, body, expires_at, updated_at, index_fields, clock) in rows {
		if clock != observed_at {
			return Err(RecordPageError::InconsistentClock);
		}
		let Some(namespace) = namespace else {
			if row_count != 1
				|| key.is_some()
				|| body.is_some()
				|| expires_at.is_some()
				|| updated_at.is_some()
				|| index_fields.is_some()
			{
				return Err(RecordPageError::InvalidObservationShape);
			}
			continue;
		};
		let namespace = RecordNamespace::new(namespace.0)
			.map_err(|_| RecordPageError::InvalidObservationShape)?;
		let key = key.ok_or(RecordPageError::InvalidObservationShape)?;
		let body = body.ok_or(RecordPageError::InvalidObservationShape)?;
		let index_fields = index_fields.ok_or(RecordPageError::InvalidObservationShape)?;
		let identity = RecordIdentity::new(namespace, RecordKey::new(key.0));
		let record = decode_physical_record(
			&identity,
			(Some(body), expires_at, updated_at, index_fields),
		)?
		.ok_or(RecordPageError::InvalidObservationShape)?;
		entries.push(RecordPageEntry::new(identity, record));
	}
	Ok(RecordPageRead::parse(
		request,
		entries,
		StateTimestampMicros::new(observed_at),
	)?)
}
