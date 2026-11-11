//! Bounded descriptor-bound physical source-key discovery without record authority.
//!
//! The owning fixed aggregate must establish actual source/control pinning, current phase,
//! descriptor association, and transaction ordering. This adapter grants none of those facts.
//! Discovery returns no body, clock, mutation permission, coverage, or retirement proof.

use crate::db::bytea::Bytea;
use crate::db::{DatabaseOperationKind, Tx, owned_portable_query_as};
use crate::fleet::lease;
use crate::kv;
use crate::queue::{JobId, TaskName, parse_source_pause_scope};

use super::{PgV10SourceDescriptor, PgV10SourceRole};

mod queries;
mod types;
use queries::SourceQueries;
pub(crate) use types::{
	QueueSourcePauseKey, SourceCandidateBoundsError, SourceCandidateFailureContext,
	SourceCandidateKey, SourceCandidateKeyError, SourceCandidatePage, SourceCandidatePageError,
	SourceCandidatePageRequest, SourceCandidateRole, SourceCandidateSelection,
};

const READ_CANDIDATE_PAGE_LABEL: &str = "db.v10_source_binding.read_candidate_page";

/// Immutable descriptor and fixed role queries. Construction performs no source IO or admission.
pub(crate) struct PgV10SourceCandidateStorage {
	queries: SourceQueries,
}
impl PgV10SourceCandidateStorage {
	pub(crate) fn new(descriptor: &PgV10SourceDescriptor) -> Self {
		Self {
			queries: SourceQueries::new(descriptor),
		}
	}

	/// Reads all returned keys in one statement. None starts before every physical key. Whole
	/// RawKv/FleetState tables and both lease tables are accounted without prefix filters. Expiry,
	/// body, task payload, claim state, and authority do not affect discovery. The row budget
	/// limits output. A RawKv, FleetState, Queue job, Queue dead-letter, or Queue pause page
	/// orders by an indexed key, and a lease page takes at most the row budget from each lease
	/// table in the order of its primary key. Failure or
	/// cancellation poisons the exact caller transaction through its unfinished IO token.
	pub(crate) async fn read_page(
		&self,
		tx: &mut Tx<'_>,
		request: &SourceCandidatePageRequest,
	) -> Result<SourceCandidatePage, SourceCandidatePageError> {
		let mut completion = tx.track_state_mutation();
		let query = self.queries.for_role(request.selection().role());
		tx.record_database_operation(
			DatabaseOperationKind::FetchAll,
			READ_CANDIDATE_PAGE_LABEL,
			Some(query.as_str()),
		);
		let rows = owned_portable_query_as::<(Bytea, Option<Bytea>)>(query.clone())
			.bind_validated(request.selection().after_bytes())
			.bind_validated(i64::from(request.limit().get()))
			.fetch_all(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label(READ_CANDIDATE_PAGE_LABEL))?;
		let page = parse_page(request, rows)?;
		completion.complete();
		Ok(page)
	}
}

fn parse_page(
	request: &SourceCandidatePageRequest,
	rows: Vec<(Bytea, Option<Bytea>)>,
) -> Result<SourceCandidatePage, SourceCandidatePageError> {
	let role = request.selection().role();
	let mut keys: Vec<SourceCandidateKey> = Vec::with_capacity(rows.len());
	for (row_index, (raw_key, task)) in rows.into_iter().enumerate() {
		let parsed = parse_key(
			role,
			&raw_key.0,
			task.as_ref().map(|value| value.0.as_slice()),
		);
		let key = match parsed {
			Ok(key) => key,
			Err(source) => {
				return Err(SourceCandidatePageError::Key {
					context: Box::new(SourceCandidateFailureContext { raw_key: raw_key.0 }),
					source: Box::new(source),
				});
			}
		};
		let source = if row_index >= request.limit().get() as usize {
			Some(SourceCandidateBoundsError::LimitExceeded)
		} else if key.role() != role {
			Some(SourceCandidateBoundsError::RoleMismatch)
		} else if request
			.selection()
			.after_bytes()
			.is_some_and(|bound| key.as_bytes() <= bound)
		{
			Some(SourceCandidateBoundsError::LowerBoundViolation)
		} else if keys
			.last()
			.is_some_and(|previous| key.as_bytes() <= previous.as_bytes())
		{
			Some(SourceCandidateBoundsError::KeyOrderViolation)
		} else {
			None
		};
		if let Some(source) = source {
			return Err(SourceCandidatePageError::Bounds {
				context: Box::new(SourceCandidateFailureContext { raw_key: raw_key.0 }),
				source,
			});
		}
		keys.push(key);
	}
	Ok(SourceCandidatePage { role, keys })
}

fn parse_key(
	role: SourceCandidateRole,
	bytes: &[u8],
	task: Option<&[u8]>,
) -> Result<SourceCandidateKey, SourceCandidateKeyError> {
	if role != SourceCandidateRole::QueuePauses && task.is_some() {
		return Err(SourceCandidateKeyError::UnexpectedTask);
	}
	match role {
		SourceCandidateRole::RawKv | SourceCandidateRole::FleetState => {
			let text = std::str::from_utf8(bytes).map_err(SourceCandidateKeyError::KeyUtf8)?;
			let key = kv::Key::from_persisted_text(text.to_owned())
				.map_err(SourceCandidateKeyError::Kv)?;
			Ok(match role {
				SourceCandidateRole::RawKv => SourceCandidateKey::RawKv(key),
				_ => SourceCandidateKey::FleetState(key),
			})
		}
		SourceCandidateRole::LeaseAggregate => {
			let text = std::str::from_utf8(bytes).map_err(SourceCandidateKeyError::KeyUtf8)?;
			let parts = text
				.strip_suffix(lease::LEASE_KEY_SEPARATOR)
				.ok_or(SourceCandidateKeyError::LeaseFinalSeparator)?;
			let key = lease::LeaseKey::from_parts(parts.split(lease::LEASE_KEY_SEPARATOR))
				.map_err(SourceCandidateKeyError::Lease)?;
			Ok(SourceCandidateKey::LeaseAggregate(key))
		}
		SourceCandidateRole::QueueJobs | SourceCandidateRole::QueueDeadLetters => {
			let id = JobId::from_bytes(bytes).map_err(SourceCandidateKeyError::Id)?;
			Ok(match role {
				SourceCandidateRole::QueueJobs => SourceCandidateKey::QueueJobs(id),
				_ => SourceCandidateKey::QueueDeadLetters(id),
			})
		}
		SourceCandidateRole::QueuePauses => {
			let key = std::str::from_utf8(bytes).map_err(SourceCandidateKeyError::KeyUtf8)?;
			let task = task
				.map(|bytes| {
					let text =
						std::str::from_utf8(bytes).map_err(SourceCandidateKeyError::TaskUtf8)?;
					TaskName::new(text).map_err(SourceCandidateKeyError::Task)
				})
				.transpose()?;
			let scope = parse_source_pause_scope(key, task.as_ref())
				.map_err(SourceCandidateKeyError::Pause)?;
			Ok(SourceCandidateKey::QueuePauses(
				QueueSourcePauseKey::from_scope(scope),
			))
		}
	}
}
