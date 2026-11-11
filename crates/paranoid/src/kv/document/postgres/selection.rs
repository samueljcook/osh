//! One-statement opaque document selection from an actual configured compound expression plan.
//!
//! This kernel proves neither participant query admission nor physical index/coverage readiness.
//! Its caller authenticates current query use before protected IO. Row limits bound output count,
//! and the explicit wait bounds the dispatched fetch; neither bounds bytes, planning or parsing.

use std::{cmp::Ordering, ops::Bound, time::Duration};

use primitives_rs::time::Deadline;

use crate::kv::document::query::{
	DocumentQueryCursor, DocumentQueryDirection, DocumentQueryInnerError, DocumentQueryPosition,
	DocumentQueryPositionComparison, ScopedDocumentIndexQuery,
};
use crate::records::{IndexScalar, RecordIdentity, RecordKey, StateTimestampMicros, StoredRecord};

use super::index::{PgDocumentIndexError, PgDocumentIndexProjectionError, PgDocumentQueryBinding};
use crate::db::bytea::Bytea;
use crate::db::{
	AuditedSql, AuditedSqlBuilder, DatabaseError, DatabaseOperationKind, Tx,
	owned_portable_query_as,
};
use crate::records::postgres::{StateStorageError, decode_physical_record};

const SELECT_LABEL: &str = "db.state_storage.document_selection.select";

type SelectionRow = (
	Option<Bytea>,
	Option<Bytea>,
	Option<Bytea>,
	Option<i64>,
	Option<i64>,
	Option<String>,
	i64,
);

pub(in crate::kv) struct PgDocumentSelectionFields<'r, 'a, 's, 'd, T, C, P> {
	pub(in crate::kv) binding: &'r PgDocumentQueryBinding<'a, 's, 'd, T, C, P>,
	pub(in crate::kv) query: &'r ScopedDocumentIndexQuery<'a, 's, 'd, T, C, P>,
	pub(in crate::kv) statement_wait_timeout: Duration,
}

/// Parsed actual code/configuration and positive fetch wait; no admission or physical proof.
pub(in crate::kv) struct PgDocumentSelectionRequest<'r, 'a, 's, 'd, T, C, P> {
	binding: &'r PgDocumentQueryBinding<'a, 's, 'd, T, C, P>,
	query: &'r ScopedDocumentIndexQuery<'a, 's, 'd, T, C, P>,
	statement_wait_timeout: Duration,
}

impl<'r, 'a, 's, 'd, T, C, P> PgDocumentSelectionRequest<'r, 'a, 's, 'd, T, C, P> {
	pub(crate) fn new(
		fields: PgDocumentSelectionFields<'r, 'a, 's, 'd, T, C, P>,
	) -> Result<Self, PgPhysicalDocumentSelectionError> {
		fields.binding.require_query(fields.query)?;
		if fields.statement_wait_timeout.is_zero()
			|| Deadline::new_from_base(
				tokio::time::Instant::now().into_std(),
				fields.statement_wait_timeout,
			)
			.is_err()
		{
			return Err(PgPhysicalDocumentSelectionError::InvalidStatementWaitTimeout);
		}
		Ok(Self {
			binding: fields.binding,
			query: fields.query,
			statement_wait_timeout: fields.statement_wait_timeout,
		})
	}
	pub(crate) fn query(&self) -> &ScopedDocumentIndexQuery<'a, 's, 'd, T, C, P> {
		self.query
	}
}

pub(in crate::kv) struct PgPhysicalDocumentSelectionEntry {
	pub(in crate::kv) identity: RecordIdentity,
	pub(in crate::kv) record: StoredRecord,
	pub(in crate::kv) values: Vec<IndexScalar>,
}

/// One current statement observation. A last-position cursor is not a has-more proof.
pub(in crate::kv) struct PgPhysicalDocumentSelectionPage {
	pub(in crate::kv) entries: Vec<PgPhysicalDocumentSelectionEntry>,
	pub(in crate::kv) observed_at: StateTimestampMicros,
	pub(in crate::kv) continuation: Option<DocumentQueryCursor>,
}

#[derive(Debug, thiserror::Error)]
pub(in crate::kv) enum PgPhysicalDocumentSelectionError {
	#[error("document selection statement wait must be positive and fit the monotonic clock")]
	InvalidStatementWaitTimeout,
	#[error("document selection fetch wait elapsed with statement completion unconfirmed")]
	StatementWaitElapsed,
	#[error("document selection record preparation failed")]
	SelectedRecord {
		identity: RecordIdentity,
		#[source]
		source: Box<Self>,
	},
	#[error("document selection index binding failed")]
	Binding(#[from] PgDocumentIndexError),
	#[error("document selection projection integrity failed")]
	Projection(#[from] PgDocumentIndexProjectionError),
	#[error("document selection database work failed")]
	Database(#[from] DatabaseError),
	#[error("document selection physical record failed")]
	Record(#[from] StateStorageError),
	#[error("document selection neutral query position failed")]
	Query(#[from] DocumentQueryInnerError),
	#[error("document selection has an invalid empty or valued observation shape")]
	InvalidObservationShape,
	#[error("document selection does not share one statement clock")]
	InconsistentClock,
	#[error("document selection exceeds its row limit")]
	RowLimitExceeded,
	#[error("document selection contains another namespace")]
	NamespaceMismatch,
	#[error("document selection contains a value expired at its statement clock")]
	ExpiredRecord,
	#[error("document selection contains a position outside its predicate or continuation")]
	PositionOutsideSelection,
	#[error("document selection violates strict complete tuple and key order")]
	PositionOrderViolation,
}

impl PgPhysicalDocumentSelectionError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::SelectedRecord { source, .. } => source.database_error(),
			Self::Database(error) => Some(error),
			Self::Record(error) => error.database_error(),

			_ => None,
		}
	}
	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::SelectedRecord { source, .. } => source.involves_database_failure(),

			_ => self.database_error().is_some(),
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::SelectedRecord { source, .. } => source.commit_outcome_is_unknown(),

			_ => self
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
		}
	}
}

/// Reads protected rows only after its caller authenticates query use and logical readiness.
/// Successful completion includes all sentinel, physical and neutral-position checks.
pub(in crate::kv) async fn select_opaque<T, C, P>(
	tx: &mut Tx<'_>,
	request: &PgDocumentSelectionRequest<'_, '_, '_, '_, T, C, P>,
) -> Result<PgPhysicalDocumentSelectionPage, PgPhysicalDocumentSelectionError> {
	let query = build_selection_query(request)?;
	let deadline = Deadline::new_from_base(
		tokio::time::Instant::now().into_std(),
		request.statement_wait_timeout,
	)
	.map_err(|_| PgPhysicalDocumentSelectionError::InvalidStatementWaitTimeout)?;
	let interruption = tx.statement_interruption_marker();
	let mut completion = tx.track_state_mutation();
	tx.record_database_operation(
		DatabaseOperationKind::FetchAll,
		SELECT_LABEL,
		Some(query.as_str()),
	);
	let rows = tokio::time::timeout_at(
		tokio::time::Instant::from_std(deadline.instant()),
		owned_portable_query_as::<SelectionRow>(query).fetch_all(tx.inner.as_mut()),
	)
	.await
	.map_err(|_| {
		interruption.mark_interrupted();
		PgPhysicalDocumentSelectionError::StatementWaitElapsed
	})?
	.map_err(|error| error.with_operation_label(SELECT_LABEL))?;
	let page = parse_selection(request, rows)?;
	completion.complete();
	Ok(page)
}

fn build_selection_query<T, C, P>(
	request: &PgDocumentSelectionRequest<'_, '_, '_, '_, T, C, P>,
) -> Result<AuditedSql, PgPhysicalDocumentSelectionError> {
	let binding = request.binding;
	let query = request.query;
	binding.require_query(query)?;
	let mut sql = AuditedSqlBuilder::new();
	sql.push_text(
		"WITH observation AS MATERIALIZED (\
         SELECT pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.clock_timestamp()) \
         OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8 AS observed_at) \
         SELECT selected.namespace, selected.key, selected.body, selected.expires_at, \
         selected.updated_at, selected.index_fields::pg_catalog.text AS projection_wire, \
         observation.observed_at \
         FROM observation LEFT JOIN LATERAL (\
         SELECT namespace, key, body, expires_at, updated_at, index_fields FROM ONLY ",
	);
	sql.push_argument(binding.storage().table());
	sql.push_text(" WHERE ");
	binding.append_valued_scope_predicate(&mut sql);
	sql.push_text(
		" AND (expires_at IS NULL OR expires_at OPERATOR(pg_catalog.>) observation.observed_at)",
	);
	for (field_index, value) in query.equality_prefix().iter().enumerate() {
		sql.push_text(" AND ");
		binding.append_field_expression(field_index, &mut sql)?;
		sql.push_text(" OPERATOR(pg_catalog.=) ");
		binding.append_field_literal(field_index, value, &mut sql)?;
	}
	if let Some(range) = query.range() {
		let field_index = query.equality_prefix().len();
		for (bound, included, excluded) in [
			(
				&range.lower,
				" OPERATOR(pg_catalog.>=) ",
				" OPERATOR(pg_catalog.>) ",
			),
			(
				&range.upper,
				" OPERATOR(pg_catalog.<=) ",
				" OPERATOR(pg_catalog.<) ",
			),
		] {
			let (value, operator) = match bound {
				Bound::Unbounded => continue,
				Bound::Included(value) => (value, included),
				Bound::Excluded(value) => (value, excluded),
			};
			sql.push_text(" AND ");
			binding.append_field_expression(field_index, &mut sql)?;
			sql.push_text(operator);
			binding.append_field_literal(field_index, value, &mut sql)?;
		}
	}
	if let Some(cursor) = query.cursor() {
		sql.push_text(" AND ");
		let compound = !binding.index().fields().is_empty();
		if compound {
			sql.push_text("ROW(");
		}
		for field_index in 0..binding.index().fields().len() {
			binding.append_field_expression(field_index, &mut sql)?;
			sql.push_text(", ");
		}
		binding.append_key_expression(&mut sql);
		if compound {
			sql.push_text(")");
		}
		sql.push_text(match query.direction() {
			DocumentQueryDirection::Ascending => " OPERATOR(pg_catalog.>) ",
			DocumentQueryDirection::Descending => " OPERATOR(pg_catalog.<) ",
		});
		if compound {
			sql.push_text("ROW(");
		}
		for (field_index, value) in cursor.values().iter().enumerate() {
			binding.append_field_literal(field_index, value, &mut sql)?;
			sql.push_text(", ");
		}
		sql.push_argument(cursor.key().as_bytes());
		if compound {
			sql.push_text(")");
		}
	}
	sql.push_text(" ORDER BY ");
	append_order(binding, query.direction(), &mut sql)?;
	sql.push_text(" LIMIT ");
	sql.push_argument(&i64::from(query.limit().get()));
	sql.push_text(") selected ON true ORDER BY ");
	append_order(binding, query.direction(), &mut sql)?;
	Ok(sql.finish())
}

fn append_order<T, C, P>(
	binding: &PgDocumentQueryBinding<'_, '_, '_, T, C, P>,
	direction: DocumentQueryDirection,
	sql: &mut AuditedSqlBuilder,
) -> Result<(), PgDocumentIndexError> {
	let operator = match direction {
		DocumentQueryDirection::Ascending => " USING OPERATOR(pg_catalog.<)",
		DocumentQueryDirection::Descending => " USING OPERATOR(pg_catalog.>)",
	};
	for field_index in 0..binding.index().fields().len() {
		binding.append_field_expression(field_index, sql)?;
		sql.push_text(operator);
		sql.push_text(", ");
	}
	binding.append_key_expression(sql);
	sql.push_text(operator);
	Ok(())
}

fn parse_selection<T, C, P>(
	request: &PgDocumentSelectionRequest<'_, '_, '_, '_, T, C, P>,
	rows: Vec<SelectionRow>,
) -> Result<PgPhysicalDocumentSelectionPage, PgPhysicalDocumentSelectionError> {
	let query = request.query;
	let binding = request.binding;
	let observed_at = rows
		.first()
		.ok_or(PgPhysicalDocumentSelectionError::InvalidObservationShape)?
		.6;
	if rows.len() > query.limit().get() as usize {
		return Err(PgPhysicalDocumentSelectionError::RowLimitExceeded);
	}
	let row_count = rows.len();
	let observed_at = StateTimestampMicros::new(observed_at);
	let mut entries: Vec<PgPhysicalDocumentSelectionEntry> = Vec::with_capacity(row_count);
	for (namespace, key, body, expires_at, updated_at, projection_wire, clock) in rows {
		if clock != observed_at.as_i64() {
			return Err(PgPhysicalDocumentSelectionError::InconsistentClock);
		}
		let Some(namespace) = namespace else {
			if row_count != 1
				|| key.is_some()
				|| body.is_some()
				|| expires_at.is_some()
				|| updated_at.is_some()
				|| projection_wire.is_some()
			{
				return Err(PgPhysicalDocumentSelectionError::InvalidObservationShape);
			}
			continue;
		};
		if namespace.0 != binding.declaration().identity().namespace().as_bytes() {
			return Err(PgPhysicalDocumentSelectionError::NamespaceMismatch);
		}
		let key = key.ok_or(PgPhysicalDocumentSelectionError::InvalidObservationShape)?;
		let identity = RecordIdentity::new(
			binding.declaration().identity().namespace().clone(),
			RecordKey::new(key.0),
		);
		let (record, values) = (|| {
			let body = body.ok_or(PgPhysicalDocumentSelectionError::InvalidObservationShape)?;
			let projection_wire =
				projection_wire.ok_or(PgPhysicalDocumentSelectionError::InvalidObservationShape)?;
			let record = decode_physical_record(
				&identity,
				(Some(body), expires_at, updated_at, projection_wire),
			)?
			.ok_or(PgPhysicalDocumentSelectionError::InvalidObservationShape)?;
			if !record.metadata().is_live_at(observed_at) {
				return Err(PgPhysicalDocumentSelectionError::ExpiredRecord);
			}
			let values = binding.projected_tuple(record.index_fields())?;
			if !query.matches_position(&values, identity.key())? {
				return Err(PgPhysicalDocumentSelectionError::PositionOutsideSelection);
			}
			if let Some(previous) = entries.last()
				&& query.compare_positions(DocumentQueryPositionComparison {
					left: DocumentQueryPosition {
						values: &previous.values,
						key: previous.identity.key(),
					},
					right: DocumentQueryPosition {
						values: &values,
						key: identity.key(),
					},
				})? != Ordering::Less
			{
				return Err(PgPhysicalDocumentSelectionError::PositionOrderViolation);
			}
			Ok((record, values))
		})()
		.map_err(|error| PgPhysicalDocumentSelectionError::SelectedRecord {
			identity: identity.clone(),
			source: Box::new(error),
		})?;
		entries.push(PgPhysicalDocumentSelectionEntry {
			identity,
			record,
			values,
		});
	}
	let continuation = entries
		.last()
		.map(|entry| {
			query
				.cursor_after(entry.values.clone(), entry.identity.key().clone())
				.map_err(|error| PgPhysicalDocumentSelectionError::SelectedRecord {
					identity: entry.identity.clone(),
					source: Box::new(error.into()),
				})
		})
		.transpose()?;
	Ok(PgPhysicalDocumentSelectionPage {
		entries,
		observed_at,
		continuation,
	})
}
