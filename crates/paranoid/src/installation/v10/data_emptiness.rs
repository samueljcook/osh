//! One descriptor-bound physical observation of every source data role.
//!
//! The fixed caller must validate source table shape and retain source/control exclusion.
//! This adapter supplies no phase, data coverage, retirement or mutation authority. The source
//! heap validator rejects both inheritance directions, so its protected ONLY relation domain
//! is the same complete domain used by source candidate pages.

use std::fmt;

use crate::db::{
	AuditedSql, AuditedSqlBuilder, DatabaseError, DatabaseOperationKind, Tx,
	owned_portable_query_as,
};

use super::{PgV10SourceDescriptor, PgV10SourceRole};

const READ_DATA_EMPTINESS_LABEL: &str = "db.v10_source_binding.read_data_emptiness";

// These markers identify this closed operation's actual physical roles, independently of
// result order. Protocol and ledgers are retained metadata and are excluded from this query.
const DATA_ROLES: [(i16, PgV10SourceRole); 7] = [
	(0, PgV10SourceRole::RawKv),
	(1, PgV10SourceRole::FleetState),
	(2, PgV10SourceRole::FleetClaims),
	(3, PgV10SourceRole::FleetFencing),
	(4, PgV10SourceRole::QueueJobs),
	(5, PgV10SourceRole::QueueDeadLetters),
	(6, PgV10SourceRole::QueuePauses),
];

/// Construction composes quoted parsed identities without source IO or admission.
pub(crate) struct PgV10SourceDataEmptinessStorage {
	query: AuditedSql,
}

impl PgV10SourceDataEmptinessStorage {
	pub(crate) fn new(descriptor: &PgV10SourceDescriptor) -> Self {
		let mut query = AuditedSqlBuilder::new();
		for (index, (marker, role)) in DATA_ROLES.iter().enumerate() {
			if index != 0 {
				query.push_text(" UNION ALL ");
			}
			query.push_text("SELECT ");
			query.push_argument(marker);
			query.push_text("::pg_catalog.int2 AS role_marker, EXISTS (SELECT 1 FROM ONLY ");
			query.push_argument(descriptor.table(*role));
			query.push_text(") AS nonempty");
		}
		Self {
			query: query.finish(),
		}
	}

	/// Reads all seven complete tables without key, body, expiry or ownership filters.
	/// Missing tables fail; no table is created, repaired, locked for retirement or inferred empty.
	/// The unfinished IO token poisons the exact transaction on SQL/parse failure or cancellation.
	pub(crate) async fn read(
		&self,
		tx: &mut Tx<'_>,
	) -> Result<SourceDataEmptinessObservation, SourceDataEmptinessError> {
		let mut completion = tx.track_state_mutation();
		tx.record_database_operation(
			DatabaseOperationKind::FetchAll,
			READ_DATA_EMPTINESS_LABEL,
			Some(self.query.as_str()),
		);
		let rows = owned_portable_query_as::<(i16, bool)>(self.query.clone())
			.fetch_all(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(READ_DATA_EMPTINESS_LABEL))?;
		let observation = parse_observation(rows)?;
		completion.complete();
		Ok(observation)
	}
}

/// Complete marker coverage for one statement, without transaction or coverage authority.
pub(crate) struct SourceDataEmptinessObservation {
	nonempty: [bool; DATA_ROLES.len()],
}

impl SourceDataEmptinessObservation {
	pub(crate) fn is_empty(&self) -> bool {
		self.nonempty.iter().all(|nonempty| !nonempty)
	}

	pub(crate) fn nonempty_roles(&self) -> impl Iterator<Item = PgV10SourceRole> + '_ {
		DATA_ROLES
			.iter()
			.zip(self.nonempty)
			.filter_map(|((_, role), nonempty)| nonempty.then_some(*role))
	}
}

impl fmt::Debug for SourceDataEmptinessObservation {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("SourceDataEmptinessObservation")
			.field("nonempty_role_count", &self.nonempty_roles().count())
			.finish_non_exhaustive()
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SourceDataEmptinessError {
	#[error("source data emptiness observation database work failed")]
	Database(#[from] DatabaseError),
	#[error("source data emptiness observation has an unknown role marker {marker}")]
	UnknownMarker { marker: i16 },
	#[error("source data emptiness observation repeats role marker {marker}")]
	DuplicateMarker { marker: i16 },
	#[error("source data emptiness observation omits role marker {marker}")]
	MissingMarker { marker: i16 },
}

impl SourceDataEmptinessError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Database(error) => Some(error),
			Self::UnknownMarker { .. }
			| Self::DuplicateMarker { .. }
			| Self::MissingMarker { .. } => None,
		}
	}
}

fn parse_observation(
	rows: Vec<(i16, bool)>,
) -> Result<SourceDataEmptinessObservation, SourceDataEmptinessError> {
	let mut observed = [None; DATA_ROLES.len()];
	for (marker, nonempty) in rows {
		let index = DATA_ROLES
			.iter()
			.position(|(expected, _)| *expected == marker)
			.ok_or(SourceDataEmptinessError::UnknownMarker { marker })?;
		if observed[index].replace(nonempty).is_some() {
			return Err(SourceDataEmptinessError::DuplicateMarker { marker });
		}
	}
	let mut nonempty = [false; DATA_ROLES.len()];
	for (index, (marker, _)) in DATA_ROLES.iter().enumerate() {
		nonempty[index] =
			observed[index].ok_or(SourceDataEmptinessError::MissingMarker { marker: *marker })?;
	}
	Ok(SourceDataEmptinessObservation { nonempty })
}
