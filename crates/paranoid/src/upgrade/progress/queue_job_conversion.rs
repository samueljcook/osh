//! Queue payload conversion writes both records and its checkpoint in one admitted transaction.

use super::{ControlProgressError, PgControlProgressLock};
use crate::installation::contract::StateContractError;
use crate::queue::upgrade::{
	QueueJobConversionRecords, QueueJobUpgradeError, record_job_id, record_keys,
};
use crate::records::postgres::scan::RecordPageError;
use crate::records::postgres::{StateStorageError, encode_index_fields};
use crate::records::{NamespacePageRequest, RecordIdentity};
use crate::runtime::admission::bindings::InstallationStorageRoute;
use crate::runtime::admission::upgrade::{NativeUpgradeAdmissionError, NativeUpgradeInvocation};
use crate::runtime::targets::ResolvedNativeRuntimeTarget;
use crate::upgrade::policy::UpgradeCompatibilityError;
use crate::upgrade::work::queue_checkpoint::{
	QueueJobCheckpointContext, QueueJobCheckpointError, QueueJobScanProgress,
};
use crate::upgrade::work::{UpgradeWork, UpgradeWorkError};
use std::{collections::BTreeSet, num::NonZeroU32};

pub(in crate::upgrade) struct QueueJobConversionPageFields<'a> {
	pub(crate) invocation: &'a NativeUpgradeInvocation<'a>,
	pub(crate) resolved: &'a ResolvedNativeRuntimeTarget,
	pub(crate) limit: NonZeroU32,
	pub(crate) conflict_attempts: NonZeroU32,
}
pub(in crate::upgrade) struct QueueJobConversionPageResult {
	pub(crate) observed: crate::upgrade::observation::NativeUpgradeObservedControl,
	pub(crate) work: UpgradeWork,
}
#[derive(Debug, thiserror::Error)]
pub(crate) enum QueueJobConversionPageError {
	#[error("Queue job conversion private admission failed")]
	Admission(#[from] NativeUpgradeAdmissionError),
	#[error("Queue job conversion progress guard failed")]
	Progress(#[from] ControlProgressError),
	#[error("Queue job conversion work context failed")]
	Work(#[from] UpgradeWorkError),
	#[error("Queue job conversion target failed")]
	Candidate(#[from] UpgradeCompatibilityError),
	#[error("Queue job conversion requires retired source data")]
	NativeRouteRequired,
	#[error("Queue job conversion code binding failed")]
	Contract(#[from] StateContractError),
	#[error("Queue job conversion record preparation failed")]
	Preparation(#[from] QueueJobUpgradeError),
	#[error("Queue job conversion key discovery failed")]
	Page(#[from] RecordPageError),
	#[error("Queue job conversion canonical acquisition or write failed")]
	Storage(#[from] StateStorageError),
}
impl QueueJobConversionPageError {
	/// Returns an actual retained driver cause without replacing the primary typed failure.
	pub(crate) fn database_error(&self) -> Option<&crate::db::DatabaseError> {
		match self {
			Self::Admission(error) => error.database_error(),
			Self::Progress(error) => error.database_error(),
			Self::Page(crate::records::postgres::scan::RecordPageError::Database(error)) => {
				Some(error)
			}
			Self::Page(crate::records::postgres::scan::RecordPageError::Record(error)) => {
				error.database_error()
			}
			Self::Storage(error) => error.database_error(),
			_ => None,
		}
	}

	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::Admission(error) => error.involves_database_failure(),
			Self::Progress(error) => error.involves_database_failure(),
			Self::Storage(error) | Self::Page(RecordPageError::Record(error)) => {
				error.database_error().is_some()
			}
			Self::Page(RecordPageError::Database(_)) => true,
			_ => false,
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Admission(error) => error.commit_outcome_is_unknown(),
			Self::Progress(error) => error.commit_outcome_is_unknown(),
			Self::Storage(error) | Self::Page(RecordPageError::Record(error)) => error
				.database_error()
				.is_some_and(crate::db::DatabaseError::commit_outcome_is_unknown),
			Self::Page(RecordPageError::Database(error)) => error.commit_outcome_is_unknown(),
			_ => false,
		}
	}
}

impl PgControlProgressLock<'_, '_> {
	pub(in crate::upgrade) async fn convert_queue_job_page(
		mut self,
		fields: QueueJobConversionPageFields<'_>,
	) -> Result<QueueJobConversionPageResult, QueueJobConversionPageError> {
		let candidate = fields.resolved.candidate();
		let mut work = UpgradeWork::decode_json(
			self.control
				.pending_upgrade()
				.ok_or(ControlProgressError::WorkAbsent)?,
		)?;
		if !work.target().matches_target(fields.invocation.target()) {
			return Err(UpgradeWorkError::WorkNotCurrent.into());
		}
		let expected = work.checkpoint_context();
		work.require_checkpoint_context(&self.control, candidate, self.origin, &expected)?;
		work.require_source_checkpoint_invariants(
			self.control.component_policies(),
			self.control.scope_policies(),
		)?;
		work.require_queue_job_checkpoint_invariant(self.control.component_policies())?;
		if !work.is_unfinished() {
			return Err(UpgradeWorkError::TerminalWork.into());
		}
		fields
			.invocation
			.authenticate_current(self.physical.companion_tx(), &self.control)
			.await?;
		candidate.require_control_target(
			&self.control,
			&self.progress_storage.administration.declaration(),
			self.progress_storage.administration.participant_schema(),
			self.progress_storage.administration.control_component(),
		)?;
		self.require_conversion_writer()?;
		if !matches!(
			self.route,
			InstallationStorageRoute::NativeFresh | InstallationStorageRoute::NativeImported
		) {
			return Err(QueueJobConversionPageError::NativeRouteRequired);
		}
		let checkpoint = work.queue_job_checkpoint().ok_or(UpgradeWorkError::from(
			QueueJobCheckpointError::CheckpointAbsent,
		))?;
		let after_key = match checkpoint.progress() {
			QueueJobScanProgress::Scanning { after_key } => after_key.clone(),
			QueueJobScanProgress::Scanned { .. } => {
				let observed = self.keep_observed().await?;
				return Ok(QueueJobConversionPageResult { observed, work });
			}
		};
		let queue = fields.resolved.code().queue();
		let declaration = queue.jobs.declaration();
		let contract = declaration.bind(checkpoint.invariant())?;
		let placement = self.progress_storage.installation.queue_storage();
		let storage = placement.storage();
		let namespace = placement.jobs();
		let request = NamespacePageRequest::new(namespace.clone(), after_key, fields.limit);
		let keys = storage
			.read_conversion_keys(self.physical.companion_tx(), &request)
			.await?;
		let scanned = keys.len() < fields.limit.get() as usize;
		let last_key = keys
			.last()
			.cloned()
			.or_else(|| request.after_key().cloned());
		let jobs = keys
			.iter()
			.map(record_job_id)
			.collect::<Result<BTreeSet<_>, _>>()?;
		let identities = jobs
			.iter()
			.flat_map(|id| record_keys(*id))
			.map(|key| RecordIdentity::new(namespace.clone(), key))
			.collect();
		let (records, write_guard) = storage
			.lock_conversion_batch(
				self.physical.companion_tx(),
				identities,
				fields.conflict_attempts,
			)
			.await?
			.into_records_and_guard();
		let mut prepared = Vec::new();
		for job_id in jobs {
			let [job_key, payload_key] = record_keys(job_id);
			let job = records
				.get(&RecordIdentity::new(namespace.clone(), job_key))
				.expect("the canonical group contains both Queue keys")
				.value();
			let payload = records
				.get(&RecordIdentity::new(namespace.clone(), payload_key))
				.expect("the canonical group contains both Queue keys")
				.value();
			for (key, encoded) in queue.jobs.prepare_payload_separation(
				&contract,
				QueueJobConversionRecords {
					job_id,
					job,
					payload,
				},
			)? {
				let projections = encode_index_fields(encoded.record().index_fields());
				prepared.push((
					RecordIdentity::new(namespace.clone(), key),
					encoded,
					projections,
				));
			}
		}
		let progress = if scanned {
			QueueJobScanProgress::Scanned { last_key }
		} else {
			QueueJobScanProgress::Scanning {
				after_key: last_key,
			}
		};
		work.record_queue_job_scan(
			QueueJobCheckpointContext {
				control: &self.control,
				candidate,
				origin: self.origin,
				expected: &expected,
			},
			progress,
		)?;
		write_guard.write_each_and_cleanup(prepared).await?;
		let observed = self.persist_work(&work).await?;
		Ok(QueueJobConversionPageResult { observed, work })
	}
}
