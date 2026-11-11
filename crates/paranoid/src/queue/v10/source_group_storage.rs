//! Descriptor-bound source observations under one admitted canonical Queue attempt.
//!
//! The consuming attempt retains native/map authority through exact atomic source application.
//! Observations supply no retirement, lock, promotion, or worker grant proof and must not escape
//! as authority for a different transaction. Unrequested records and probes remain unknown.

use sqlx::postgres::PgRow;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::source_auxiliary_bridge::{
	QueueAuxiliarySourceBridge, QueueAuxiliarySourceImportError, QueueSourceDeadLetter,
	QueueSourceDeadLetterRequest, QueueSourcePause, QueueSourcePauseRequest,
};
use super::source_bridge::{
	PreparedQueueSourceJob, QueueSourceBridge, QueueSourceImportError, QueueSourceJob,
	QueueSourceTimestamp,
};
use super::source_imports::{QueueSourceImportBindingError, QueueSourceImportDefs};
use crate::db::{
	DatabaseError, DatabaseOperationKind, SchemaError, TransactionFailure, WriteTx,
	owned_portable_query, owned_portable_query_as,
};
use crate::installation::contract::BoundComponentContract;
use crate::installation::creation::PgNativeQueueStorage;
use crate::installation::v10::{PgV10SourceDescriptor, PgV10SourceRole};
use crate::queue::auxiliary_state::{AuxiliaryRepresentationError, QueuePauseScope};
use crate::queue::group_state::{
	QueueAuxiliaryComponent, QueueGroupError, QueueSourceFacts, SourceProbe, SourceRecord,
};
use crate::queue::state::{QueueJobBindingError, QueueJobComponent};
use crate::queue::{DedupeKey, JobId, PublishedV10QueueLayout, TaskName, paused_task_key};
use crate::records::postgres::HeldNativeRow;
use crate::records::{
	RecordGroupRead, RecordIdentity, RecordKey, RecordNamespace, StateTimestampMicros, StoredRecord,
};
use crate::runtime::admission::queue::NativeQueueSourceBindingError;

pub(crate) mod apply;
pub(in crate::queue) mod attempt;
pub(crate) mod context;
mod queries;
mod request;
mod rows;
pub(crate) use request::QueueSourceGroupRequest;
use request::RequestedSources;

pub(crate) struct QueueSourceGroupReaders<'a, 'p, 'd> {
	pub(in crate::queue) jobs: &'a QueueJobComponent,
	pub(in crate::queue) job_contract: &'a BoundComponentContract<'p, 'd, QueueJobComponent>,
	pub(in crate::queue) imports: &'a QueueSourceImportDefs,
	pub(in crate::queue) source_job_contract: &'a BoundComponentContract<'p, 'd, QueueSourceBridge>,
	pub(in crate::queue) auxiliary: &'a QueueAuxiliaryComponent,
	pub(in crate::queue) auxiliary_contract:
		&'a BoundComponentContract<'p, 'd, QueueAuxiliaryComponent>,
	pub(in crate::queue) source_auxiliary_contract:
		&'a BoundComponentContract<'p, 'd, QueueAuxiliarySourceBridge>,
}
impl QueueSourceGroupReaders<'_, '_, '_> {
	pub(in crate::queue) fn require_layout(
		&self,
		layout: PublishedV10QueueLayout,
	) -> Result<(), QueueSourceGroupStorageError> {
		self.jobs.require_bound_policy(self.job_contract)?;
		self.auxiliary
			.require_bound_policy(self.auxiliary_contract)?;
		self.imports.require_bound_pair(
			self.source_job_contract,
			self.source_auxiliary_contract,
			layout,
		)?;
		Ok(())
	}
}

pub(crate) struct PgQueueSourceGroupStorage {
	descriptor: Arc<PgV10SourceDescriptor>,
	native: PgNativeQueueStorage,
	queries: queries::SourceQueries,
}

pub(crate) enum QueueSourceGroupObservation {
	RequiresReacquisition(BTreeSet<RecordIdentity>),
	Observed(QueueSourceGroupRead),
}
/// Actual physical presence from either existing group snapshots or the fixed pause acquisition.
/// No provisional timestamp is needed before the source observer's final authoritative clock.
#[derive(Clone, Copy)]
pub(crate) enum QueueSourceNativeRecords<'a> {
	Group(&'a RecordGroupRead),
	Acquired(&'a BTreeMap<RecordIdentity, HeldNativeRow<StoredRecord>>),
}
impl QueueSourceNativeRecords<'_> {
	fn contains_key(self, identity: &RecordIdentity) -> bool {
		match self {
			Self::Group(group) => group.snapshots().contains_key(identity),
			Self::Acquired(records) => records.contains_key(identity),
		}
	}
	fn contains_value(self, identity: &RecordIdentity) -> bool {
		match self {
			Self::Group(group) => group
				.snapshots()
				.get(identity)
				.is_some_and(|snapshot| snapshot.record().is_some()),
			Self::Acquired(records) => records
				.get(identity)
				.is_some_and(|row| row.value().is_some()),
		}
	}
}

pub(crate) struct QueueSourceGroupRead {
	facts: QueueSourceFacts,
	observed_at: StateTimestampMicros,
}
impl QueueSourceGroupRead {
	pub(in crate::queue) fn facts(&self) -> &QueueSourceFacts {
		&self.facts
	}
	pub(crate) fn observed_at(&self) -> StateTimestampMicros {
		self.observed_at
	}
}
impl std::fmt::Debug for QueueSourceGroupObservation {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::RequiresReacquisition(identities) => f
				.debug_struct("RequiresReacquisition")
				.field("identity_count", &identities.len())
				.finish(),

			Self::Observed(read) => f
				.debug_struct("Observed")
				.field("record_count", &read.facts.records.len())
				.field("active_probe_count", &read.facts.active_dedupe.len())
				.field("original_probe_count", &read.facts.originals.len())
				.finish_non_exhaustive(),
		}
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum QueueSourceGroupStorageError {
	#[error("source Queue route or association failed")]
	Binding(#[from] NativeQueueSourceBindingError),
	#[error("source Queue recorded physical schema failed")]
	Schema(#[from] SchemaError),
	#[error("source Queue database observation failed")]
	Database(#[from] DatabaseError),
	#[error("source Queue native job binding failed")]
	JobBinding(#[from] QueueJobBindingError),
	#[error("source Queue group binding failed")]
	GroupBinding(#[from] QueueGroupError),
	#[error("source Queue auxiliary binding failed")]
	AuxiliaryBinding(#[from] AuxiliaryRepresentationError),
	#[error("source Queue job importer failed")]
	JobImport(#[from] QueueSourceImportError),
	#[error("source Queue auxiliary importer failed")]
	AuxiliaryImport(#[from] QueueAuxiliarySourceImportError),
	#[error("source Queue physical field is invalid: {field}")]
	InvalidPhysical { field: &'static str },
	#[error("source Queue observation requires READ COMMITTED")]
	Isolation,
	#[error("source Queue source definitions cannot bind the installed pair")]
	Defs(#[from] QueueSourceImportBindingError),
	#[error("source Queue requested identity is outside its Queue namespaces: {identity:?}")]
	ForeignIdentity { identity: RecordIdentity },
	#[error("source Queue requested key is invalid: {identity:?}")]
	InvalidIdentity { identity: RecordIdentity },
	#[error("source Queue source observation returned multiple rows for one identity")]
	MultipleRows,
	#[error("source Queue result does not cover its requested markers and keys")]
	RequestCoverage,
	#[error("source Queue and native both contain a physical value at {identity:?}")]
	DuplicatePhysical { identity: RecordIdentity },
	#[error("source Queue probe and protected source body disagree")]
	ProbeBodyMismatch,
	#[error("source Queue attempt belongs to another native container")]
	AttemptContainerMismatch,
}

impl QueueSourceGroupStorageError {
	/// Returns an actual retained driver cause without replacing the primary typed failure.
	pub(crate) fn database_error(&self) -> Option<&crate::db::DatabaseError> {
		match self {
			Self::Binding(NativeQueueSourceBindingError::Admission(error)) => {
				error.database_error()
			}
			Self::Schema(error) => error.database_error(),
			Self::Database(error) => Some(error),
			_ => None,
		}
	}

	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::Binding(NativeQueueSourceBindingError::Admission(error)) => {
				error.involves_database_failure()
			}
			Self::Schema(error) => error.involves_database_failure(),
			Self::Database(_) => true,
			_ => false,
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Binding(NativeQueueSourceBindingError::Admission(error)) => {
				error.commit_outcome_is_unknown()
			}
			Self::Schema(error) => error.commit_outcome_is_unknown(),
			Self::Database(error) => error.commit_outcome_is_unknown(),
			_ => false,
		}
	}
}

impl PgQueueSourceGroupStorage {
	/// Reuses source physical validation completed by this actual verified installation wrapper.
	/// Each operation still checks its current route and exact immutable source/native mapping.
	pub(crate) fn from_verified_recorded(
		installation: &crate::installation::creation::v10::recorded_source::PgRecordedImportedInstallation,
	) -> Option<Self> {
		if installation.observed_phase()
			== crate::installation::creation::ImportedPhase::SourceDataRetired
		{
			return None;
		}
		let native = installation.bindings().queue().clone();
		let descriptor = Arc::new(installation.source_descriptor().clone());
		let queries = queries::SourceQueries::new(&descriptor);
		Some(Self {
			descriptor,
			native,
			queries,
		})
	}

	pub(crate) fn descriptor(&self) -> &PgV10SourceDescriptor {
		&self.descriptor
	}
	pub(crate) fn native_storage(&self) -> &PgNativeQueueStorage {
		&self.native
	}

	/// Only the owning canonical attempt calls this fixed-domain IO path on its exact Tx.
	/// A detached snapshot or escaped contracts do not authorize a different transaction.
	pub(crate) async fn observe_in_attempt(
		&self,
		tx: &mut WriteTx<'_>,
		native: QueueSourceNativeRecords<'_>,
		contracts: context::QueueSourceContext<'_, '_>,
		request: QueueSourceGroupRequest,
		readers: &QueueSourceGroupReaders<'_, '_, '_>,
	) -> Result<QueueSourceGroupObservation, QueueSourceGroupStorageError> {
		contracts.require_queue_source_storage(&self.descriptor, &self.native)?;
		let layout = self.descriptor.layout().queue_layout();
		readers.require_layout(layout)?;
		let mut requested = RequestedSources::parse(request, &self.native, readers)?;
		let missing = requested.missing(native);
		if !missing.is_empty() {
			return Ok(QueueSourceGroupObservation::RequiresReacquisition(missing));
		}
		let mut facts = QueueSourceFacts {
			records: BTreeMap::new(),
			active_dedupe: BTreeMap::new(),
			originals: BTreeMap::new(),
		};
		let active_keys: Vec<_> = requested
			.active_dedupe
			.values()
			.map(|(task, _)| task.as_str().as_bytes().to_vec())
			.collect();
		let active_dedupe_keys: Vec<_> = requested
			.active_dedupe
			.values()
			.map(|(_, dedupe)| dedupe.as_str().as_bytes().to_vec())
			.collect();
		let active_owners = self
			.probe_set(
				tx,
				&self.queries.active_dedupe,
				&active_keys,
				Some(&active_dedupe_keys),
			)
			.await?;
		for (identity, owner) in requested.active_dedupe.keys().zip(active_owners) {
			facts.active_dedupe.insert(
				identity.clone(),
				owner.map_or(SourceProbe::Absent, SourceProbe::Present),
			);
		}
		let original_keys: Vec<_> = requested
			.originals
			.values()
			.map(|id| id.as_bytes().to_vec())
			.collect();
		let original_owners = self
			.probe_set(tx, &self.queries.original, &original_keys, None)
			.await?;
		for (identity, owner) in requested.originals.keys().zip(original_owners) {
			facts.originals.insert(
				identity.clone(),
				owner.map_or(SourceProbe::Absent, SourceProbe::Present),
			);
		}
		let active: Vec<_> = requested
			.active_dedupe
			.keys()
			.filter_map(|identity| match facts.active_dedupe.get(identity) {
				Some(SourceProbe::Present(id)) => Some(*id),
				_ => None,
			})
			.collect();
		for id in active {
			requested.include_job(&self.native, id);
		}
		let originals: Vec<_> = requested
			.originals
			.keys()
			.filter_map(|identity| match facts.originals.get(identity) {
				Some(SourceProbe::Present(id)) => Some(*id),
				_ => None,
			})
			.collect();
		for id in originals {
			requested.include_dead_letter(&self.native, id);
		}

		let missing = requested.missing(native);
		if !missing.is_empty() {
			return Ok(QueueSourceGroupObservation::RequiresReacquisition(missing));
		}

		let auxiliary = readers
			.source_auxiliary_contract
			.transitions()
			.bind(readers.source_auxiliary_contract)?;
		// Native whole-set authority precedes pause, job, then dead letter source row locks.
		let pause_keys: Vec<_> = requested
			.pauses
			.values()
			.map(|scope| match scope {
				QueuePauseScope::Global => crate::queue::GLOBAL_PAUSE_KEY.as_bytes().to_vec(),
				QueuePauseScope::Task(task) => paused_task_key(task).into_bytes(),
			})
			.collect();
		let pause_rows = self
			.row_set(tx, &self.queries.pauses, &pause_keys, "key")
			.await?;
		for ((identity, scope), row) in requested.pauses.iter().zip(pause_rows) {
			let source = if let Some(row) = row {
				check_duplicate(native, identity)?;
				let source = rows::pause(&row, row_ctx(&self.queries.pauses))?;
				let imported = auxiliary.import_pause(QueueSourcePauseRequest {
					scope: scope.clone(),
					source,
				})?;
				SourceRecord::Pause {
					state: imported.state,
					metadata: imported.metadata,
				}
			} else {
				SourceRecord::Absent
			};
			facts.records.insert(identity.clone(), source);
		}
		let job_keys: Vec<_> = requested
			.jobs
			.values()
			.map(|id| id.as_bytes().to_vec())
			.collect();
		let job_rows = self
			.row_set(tx, &self.queries.jobs, &job_keys, "id")
			.await?;
		for ((identity, selection), row) in requested.jobs.iter().zip(job_rows) {
			let binding = readers.jobs.bind_schema(readers.job_contract, *selection)?;
			let source = if let Some(row) = row {
				check_duplicate(native, identity)?;
				let physical = rows::job(&row, row_ctx(&self.queries.jobs), layout)?;
				let imported = match physical {
					rows::PhysicalJob::Published(source) => {
						binding.import_published_source(readers.source_job_contract, source)
					}
					rows::PhysicalJob::Prepared(source) => {
						binding.import_prepared_source(readers.source_job_contract, source)
					}
				};
				SourceRecord::Job(imported?.state)
			} else {
				SourceRecord::Absent
			};
			facts.records.insert(identity.clone(), source);
		}
		let dead_letter_keys: Vec<_> = requested
			.dead_letters
			.values()
			.map(|id| id.as_bytes().to_vec())
			.collect();
		let dead_letter_rows = self
			.row_set(tx, &self.queries.dead_letters, &dead_letter_keys, "id")
			.await?;
		for ((identity, selection), row) in requested.dead_letters.iter().zip(dead_letter_rows) {
			let source = if let Some(row) = row {
				check_duplicate(native, identity)?;
				let source = rows::dead_letter(&row, row_ctx(&self.queries.dead_letters))?;
				let imported = auxiliary.import_dead_letter(QueueSourceDeadLetterRequest {
					dead_letter_id: *selection,
					source,
				});
				SourceRecord::DeadLetter(imported?.state)
			} else {
				SourceRecord::Absent
			};
			facts.records.insert(identity.clone(), source);
		}
		check_probe_bodies(&requested, &facts, &self.native)?;
		tx.record_database_operation(
			DatabaseOperationKind::FetchOne,
			"queue.source_group.observe_clock",
			Some(queries::CLOCK_AND_ISOLATION_SQL),
		);
		let clock = owned_portable_query_as::<(i64, String)>(queries::CLOCK_AND_ISOLATION_SQL)
			.fetch_one(tx.inner.as_mut())
			.await
			.map_err(|error| error.with_operation_label("queue.source_group.observe_clock"))?;
		if clock.1 != "read committed" {
			return Err(QueueSourceGroupStorageError::Isolation);
		}
		Ok(QueueSourceGroupObservation::Observed(
			QueueSourceGroupRead {
				facts,
				observed_at: StateTimestampMicros::new(clock.0),
			},
		))
	}

	async fn probe_set(
		&self,
		tx: &mut WriteTx<'_>,
		query: &crate::db::AuditedSql,
		keys: &[Vec<u8>],
		dedupe: Option<&[Vec<u8>]>,
	) -> Result<Vec<Option<JobId>>, QueueSourceGroupStorageError> {
		if keys.is_empty() {
			return Ok(Vec::new());
		}
		tx.record_database_operation(
			DatabaseOperationKind::FetchAll,
			"queue.source_group.probe_set",
			Some(query.as_str()),
		);
		let builder = owned_portable_query(query.clone()).bind_validated(keys);
		let builder = if let Some(dedupe) = dedupe {
			builder.bind_validated(dedupe)
		} else {
			builder
		};
		let returned = builder
			.fetch_all(tx.inner.as_mut())
			.await
			.map_err(|e| e.with_operation_label("queue.source_group.probe_set"))?;
		let checked = checked_request_rows(returned, keys, dedupe, row_ctx(query))?;
		checked
			.into_iter()
			.map(|row| optional_id(&row, row_ctx(query)))
			.collect()
	}
	async fn row_set(
		&self,
		tx: &mut WriteTx<'_>,
		query: &crate::db::AuditedSql,
		keys: &[Vec<u8>],
		physical_key: &'static str,
	) -> Result<Vec<Option<PgRow>>, QueueSourceGroupStorageError> {
		if keys.is_empty() {
			return Ok(Vec::new());
		}
		tx.record_database_operation(
			DatabaseOperationKind::FetchAll,
			"queue.source_group.lock_role",
			Some(query.as_str()),
		);
		let returned = owned_portable_query(query.clone())
			.bind_validated(keys)
			.fetch_all(tx.inner.as_mut())
			.await
			.map_err(|e| e.with_operation_label("queue.source_group.lock_role"))?;
		let ctx = row_ctx(query);
		let checked = checked_request_rows(returned, keys, None, ctx)?;
		checked
			.into_iter()
			.zip(keys)
			.map(|(row, key)| {
				let actual: Option<crate::db::bytea::Bytea> = ctx.column(&row, physical_key)?;
				match actual {
					None => Ok(None),
					Some(actual) if actual.0 == *key => Ok(Some(row)),
					Some(_) => Err(QueueSourceGroupStorageError::RequestCoverage),
				}
			})
			.collect()
	}
}

fn row_ctx(query: &crate::db::AuditedSql) -> super::rows::RowCtx<'_> {
	super::rows::RowCtx {
		operation: "queue.source_group.decode",
		statement: query.as_str(),
	}
}
/// Every requested marker has exactly one result, including an explicit null owner/body.
/// A second owner for a protected association is an integrity error, not a choice of owner.
fn checked_request_rows(
	returned: Vec<PgRow>,
	keys: &[Vec<u8>],
	dedupe: Option<&[Vec<u8>]>,
	ctx: super::rows::RowCtx<'_>,
) -> Result<Vec<PgRow>, QueueSourceGroupStorageError> {
	if returned.len() > keys.len() {
		return Err(QueueSourceGroupStorageError::MultipleRows);
	}
	if returned.len() != keys.len() {
		return Err(QueueSourceGroupStorageError::RequestCoverage);
	}
	for (index, (row, key)) in returned.iter().zip(keys).enumerate() {
		let marker: i64 = ctx.column(row, "marker")?;
		let expected = i64::try_from(index).ok().and_then(|v| v.checked_add(1));
		let actual: crate::db::bytea::Bytea = ctx.column(row, "request_key")?;
		if Some(marker) != expected || actual.0 != *key {
			return Err(QueueSourceGroupStorageError::RequestCoverage);
		}
		if let Some(dedupe) = dedupe {
			let actual: crate::db::bytea::Bytea = ctx.column(row, "request_dedupe")?;
			if dedupe.get(index) != Some(&actual.0) {
				return Err(QueueSourceGroupStorageError::RequestCoverage);
			}
		}
	}
	Ok(returned)
}
fn optional_id(
	row: &PgRow,
	ctx: super::rows::RowCtx<'_>,
) -> Result<Option<JobId>, QueueSourceGroupStorageError> {
	let bytes: Option<crate::db::bytea::Bytea> = ctx.column(row, "id")?;
	bytes
		.map(|v| {
			JobId::from_bytes(&v.0)
				.map_err(|_| QueueSourceGroupStorageError::InvalidPhysical { field: "id" })
		})
		.transpose()
}

fn check_duplicate(
	native: QueueSourceNativeRecords<'_>,
	identity: &RecordIdentity,
) -> Result<(), QueueSourceGroupStorageError> {
	if native.contains_value(identity) {
		return Err(QueueSourceGroupStorageError::DuplicatePhysical {
			identity: identity.clone(),
		});
	}
	Ok(())
}
fn check_probe_bodies(
	requested: &RequestedSources,
	facts: &QueueSourceFacts,
	storage: &PgNativeQueueStorage,
) -> Result<(), QueueSourceGroupStorageError> {
	for (identity, selection) in &requested.active_dedupe {
		if let Some(SourceProbe::Present(id)) = facts.active_dedupe.get(identity) {
			let job = request::id_identity(storage.jobs(), *id);
			let Some(SourceRecord::Job(state)) = facts.records.get(&job) else {
				return Err(QueueSourceGroupStorageError::ProbeBodyMismatch);
			};
			if state.job().task_name != selection.0
				|| state.job().dedupe_key.as_ref() != Some(&selection.1)
				|| !matches!(
					state.job().status(),
					crate::queue::JobStatus::Pending | crate::queue::JobStatus::Running
				) {
				return Err(QueueSourceGroupStorageError::ProbeBodyMismatch);
			}
		}
	}
	for (identity, selection) in &requested.originals {
		if let Some(SourceProbe::Present(id)) = facts.originals.get(identity) {
			let dead_letter = request::id_identity(storage.dead_letters(), *id);
			let Some(SourceRecord::DeadLetter(state)) = facts.records.get(&dead_letter) else {
				return Err(QueueSourceGroupStorageError::ProbeBodyMismatch);
			};
			if state.dead_letter().original_job_id != *selection {
				return Err(QueueSourceGroupStorageError::ProbeBodyMismatch);
			}
		}
	}
	Ok(())
}
