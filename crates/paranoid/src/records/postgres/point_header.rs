//! Native point headers share the actual physical parser without transferring body bytes.

use crate::records::{RecordHeaderObservation, RecordIdentity, StateTimestampMicros};

use super::{
	AuditedSql, DatabaseOperationKind, NativePhysicalHeaderFields, PgPointStorage,
	PgQualifiedTableName, StateStorageError, Tx, decode_physical_header, invalid_record,
	owned_portable_query_as,
};

const READ_POINT_HEADER_LABEL: &str = "db.state_storage.read_point_header";

pub(super) fn read_query(table: &PgQualifiedTableName) -> AuditedSql {
	crate::audited_sql!(
		r#"
WITH observation AS MATERIALIZED (
    SELECT pg_catalog.floor(EXTRACT(EPOCH FROM pg_catalog.clock_timestamp())
        OPERATOR(pg_catalog.*) 1000000)::pg_catalog.int8 AS observed_at
)
SELECT record.namespace IS NOT NULL, record.body IS NOT NULL,
    record.expires_at, record.updated_at, record.index_fields::pg_catalog.text,
    observation.observed_at
FROM observation
LEFT JOIN ONLY {table} record
    ON record.namespace OPERATOR(pg_catalog.=) $1
   AND record.key OPERATOR(pg_catalog.=) $2
"#,
		table = table
	)
}

impl PgPointStorage {
	/// Observes actual metadata and physical framing only; no body validity is established.
	pub(crate) async fn read_header(
		&self,
		tx: &mut Tx<'_>,
		identity: &RecordIdentity,
	) -> Result<RecordHeaderObservation, StateStorageError> {
		let mut completion = tx.track_state_mutation();
		tx.record_database_operation(
			DatabaseOperationKind::FetchOne,
			READ_POINT_HEADER_LABEL,
			Some(self.queries.read_header.as_str()),
		);
		let (present, body_present, expires_at, updated_at, index_fields, observed_at) =
			owned_portable_query_as::<(bool, bool, Option<i64>, Option<i64>, Option<String>, i64)>(
				self.queries.read_header.clone(),
			)
			.bind_validated(identity.namespace().as_bytes())
			.bind_validated(identity.key().as_bytes())
			.fetch_one(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(READ_POINT_HEADER_LABEL))?;
		let metadata = if present {
			let index_fields =
				index_fields.ok_or_else(|| invalid_record(identity, "index map is null"))?;
			decode_physical_header(NativePhysicalHeaderFields {
				identity,
				body_present,
				expires_at,
				updated_at,
				index_fields: &index_fields,
			})?
			.map(|header| header.metadata)
		} else {
			if body_present
				|| expires_at.is_some()
				|| updated_at.is_some()
				|| index_fields.is_some()
			{
				return Err(invalid_record(
					identity,
					"missing point header contains record fields",
				));
			}
			None
		};
		let observation =
			RecordHeaderObservation::new(metadata, StateTimestampMicros::new(observed_at));
		completion.complete();
		Ok(observation)
	}
}

use super::v10::relocating_point::{
	PgRelocatingPointStorage, RelocatingPointBinding, RelocatingPointError,
};
use super::v10::source_point::{
	PgSourcePointStorage, SourcePhysicalHeaderFields, SourcePointError, SourceSnapshotQueryKind,
	decode_source_header, source_snapshot_query,
};

impl PgSourcePointStorage {
	pub(crate) async fn read_header(
		&self,
		tx: &mut Tx<'_>,
		key: &crate::records::v10_key::Key,
	) -> Result<RecordHeaderObservation, SourcePointError> {
		let mut completion = tx.track_state_mutation();
		let query = source_snapshot_query(self.table(), SourceSnapshotQueryKind::ReadHeader);
		const LABEL: &str = "db.state_storage.source_point.read_header";
		tx.record_database_operation(DatabaseOperationKind::FetchOne, LABEL, Some(query.as_str()));
		let (
			present,
			body_present,
			expires_at,
			updated_at,
			expires_at_valid,
			updated_at_valid,
			clock,
		) = owned_portable_query_as::<(bool, bool, Option<i64>, Option<i64>, bool, bool, i64)>(query)
			.bind_validated(key.as_str())
			.fetch_one(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(LABEL))?;
		let metadata = present
			.then(|| {
				decode_source_header(SourcePhysicalHeaderFields {
					body_present,
					expires_at,
					updated_at,
					expires_at_valid,
					updated_at_valid,
				})
			})
			.transpose()?;
		completion.complete();
		Ok(RecordHeaderObservation::new(
			metadata,
			StateTimestampMicros::new(clock),
		))
	}
}

impl PgRelocatingPointStorage {
	pub(crate) async fn read_header(
		&self,
		tx: &mut Tx<'_>,
		binding: &RelocatingPointBinding,
	) -> Result<RecordHeaderObservation, RelocatingPointError> {
		let mut completion = tx.track_state_mutation();
		let query = source_snapshot_query(
			self.source_table(),
			SourceSnapshotQueryKind::RelocationHeaderRead {
				native: self.native_storage().table(),
			},
		);
		const LABEL: &str = "db.state_storage.relocating_point.read_header";
		tx.record_database_operation(DatabaseOperationKind::FetchOne, LABEL, Some(query.as_str()));
		let (
			native_present,
			body_present,
			expires_at,
			updated_at,
			indexes,
			source_present,
			source_body,
			source_expires,
			source_updated,
			expires_at_valid,
			updated_at_valid,
			clock,
		) = owned_portable_query_as::<(
			bool,
			bool,
			Option<i64>,
			Option<i64>,
			Option<String>,
			bool,
			bool,
			Option<i64>,
			Option<i64>,
			bool,
			bool,
			i64,
		)>(query)
		.bind_validated(binding.identity().namespace().as_bytes())
		.bind_validated(binding.identity().key().as_bytes())
		.bind_validated(binding.source_key().as_str())
		.fetch_one(tx.inner.as_mut())
		.await
		.map_err(|error| error.with_operation_label(LABEL))?;
		let native = if native_present {
			decode_physical_header(NativePhysicalHeaderFields {
				identity: binding.identity(),
				body_present,
				expires_at,
				updated_at,
				index_fields: indexes
					.as_deref()
					.ok_or_else(|| invalid_record(binding.identity(), "index map is null"))?,
			})?
			.map(|header| header.metadata)
		} else {
			None
		};
		let source = if source_present {
			Some(
				decode_source_header(SourcePhysicalHeaderFields {
					body_present: source_body,
					expires_at: source_expires,
					updated_at: source_updated,
					expires_at_valid,
					updated_at_valid,
				})
				.map_err(|error| super::v10::relocating_point::source_error(binding, error))?,
			)
		} else {
			None
		};
		if native.is_some() && source.is_some() {
			return Err(RelocatingPointError::DuplicatePhysicalRecords {
				identity: binding.identity().clone(),
			});
		}
		completion.complete();
		Ok(RecordHeaderObservation::new(
			native.or(source),
			StateTimestampMicros::new(clock),
		))
	}
}
