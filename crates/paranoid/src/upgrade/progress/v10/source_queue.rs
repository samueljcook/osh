//! A page of v10 Queue records and its typed Work cursor share the actual progress owner.

use crate::installation::v10::candidate_page::{
	PgV10SourceCandidateStorage, QueueSourcePauseKey, SourceCandidateKey, SourceCandidatePageError,
	SourceCandidatePageRequest, SourceCandidateSelection,
};
use crate::queue::engine::QueueRelocationRecord;
use crate::queue::engine::conversion::QueueSourceConversionError;
use crate::runtime::admission::upgrade::{NativeUpgradeAdmissionError, NativeUpgradeInvocation};
use crate::runtime::targets::ResolvedNativeRuntimeTarget;
use crate::upgrade::work::v10::source_checkpoint::{
	PgV10ConversionProcedure, SourceCheckpointContext, SourceCheckpointError,
	SourceConversionCursor, SourceConversionPauseKey, SourceScanProgress, SourceScanUpdate,
};
use crate::upgrade::work::{UpgradeWork, UpgradeWorkError};

use super::super::{ControlProgressError, PgControlProgressLock, SourceConversionBatch};

pub(super) mod selection;
use selection::{BoundSourceQueueSelection, SourceQueueDeclarations};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::upgrade) enum SourceQueueBatchKind {
	Jobs,
	DeadLetters,
	Pauses,
}

impl SourceQueueBatchKind {
	fn procedure(self) -> PgV10ConversionProcedure {
		match self {
			Self::Jobs => PgV10ConversionProcedure::QueueJobRelocationV1,
			Self::DeadLetters => PgV10ConversionProcedure::QueueDeadLetterRelocationV1,
			Self::Pauses => PgV10ConversionProcedure::QueuePauseRelocationV1,
		}
	}
	fn selection(
		self,
		after: Option<&SourceConversionCursor>,
	) -> Result<SourceCandidateSelection, SourceCheckpointError> {
		Ok(match (self, after) {
			(Self::Jobs, None) => SourceCandidateSelection::QueueJobs { after: None },
			(Self::Jobs, Some(SourceConversionCursor::QueueJob(id))) => {
				SourceCandidateSelection::QueueJobs { after: Some(*id) }
			}
			(Self::DeadLetters, None) => SourceCandidateSelection::QueueDeadLetters { after: None },
			(Self::DeadLetters, Some(SourceConversionCursor::QueueDeadLetter(id))) => {
				SourceCandidateSelection::QueueDeadLetters { after: Some(*id) }
			}
			(Self::Pauses, None) => SourceCandidateSelection::QueuePauses { after: None },
			(Self::Pauses, Some(SourceConversionCursor::QueuePause(key))) => {
				SourceCandidateSelection::QueuePauses {
					after: Some(QueueSourcePauseKey::from_scope(key.scope().clone())),
				}
			}
			_ => return Err(SourceCheckpointError::CursorProcedureMismatch),
		})
	}
	fn record(
		self,
		key: &SourceCandidateKey,
	) -> Result<(QueueRelocationRecord, SourceConversionCursor), SourceCheckpointError> {
		Ok(match (self, key) {
			(Self::Jobs, SourceCandidateKey::QueueJobs(id)) => (
				QueueRelocationRecord::Job(*id),
				SourceConversionCursor::QueueJob(*id),
			),
			(Self::DeadLetters, SourceCandidateKey::QueueDeadLetters(id)) => (
				QueueRelocationRecord::DeadLetter(*id),
				SourceConversionCursor::QueueDeadLetter(*id),
			),
			(Self::Pauses, SourceCandidateKey::QueuePauses(key)) => (
				QueueRelocationRecord::Pause(key.scope().clone()),
				SourceConversionCursor::QueuePause(SourceConversionPauseKey::from_scope(
					key.scope().clone(),
				)),
			),
			_ => return Err(SourceCheckpointError::CursorProcedureMismatch),
		})
	}
}

pub(in crate::upgrade) struct SourceQueueConversionFields<'a> {
	pub(crate) invocation: &'a NativeUpgradeInvocation<'a>,
	pub(crate) resolved: &'a ResolvedNativeRuntimeTarget,
	pub(crate) kind: SourceQueueBatchKind,
	pub(crate) batch: SourceConversionBatch,
}

/// Actual data and cursor remain tentative until the owning wrapper confirms COMMIT.
pub(in crate::upgrade) struct SourceQueueConversionResult {
	pub(crate) observed: crate::upgrade::observation::NativeUpgradeObservedControl,
	pub(crate) work: UpgradeWork,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SourceQueueConversionError {
	#[error("source Queue conversion progress failed")]
	Progress(#[from] ControlProgressError),
	#[error("source Queue conversion Work context failed")]
	Work(#[from] UpgradeWorkError),
	#[error("source Queue conversion checkpoint failed")]
	Checkpoint(#[from] SourceCheckpointError),
	#[error("source Queue conversion private enrollment failed")]
	Enrollment(#[from] NativeUpgradeAdmissionError),
	#[error("source Queue conversion physical key discovery failed")]
	Page(#[from] SourceCandidatePageError),
	#[error("source Queue conversion has no source assignment definitions")]
	SourceDefsAbsent,
	#[error("source Queue conversion requires the imported association")]
	ImportedRequired,
	#[error("source Queue conversion full target differs from its invocation")]
	TargetMismatch,
	#[error("source Queue conversion definition or selected policy failed")]
	Binding(#[source] QueueSourceConversionError),
	#[error("source Queue conversion fixed record operation failed")]
	Record {
		key: SourceCandidateKey,
		#[source]
		source: Box<QueueSourceConversionError>,
	},
}

impl SourceQueueConversionError {
	/// Returns an actual retained driver cause without replacing the primary typed failure.
	pub(crate) fn database_error(&self) -> Option<&crate::db::DatabaseError> {
		match self {
			Self::Progress(error) => error.database_error(),
			Self::Enrollment(error) => error.database_error(),
			Self::Page(error) => error.database_error(),
			Self::Binding(error) => error.database_error(),
			Self::Record { source, .. } => source.database_error(),
			_ => None,
		}
	}

	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::Progress(error) => error.involves_database_failure(),
			Self::Enrollment(error) => error.involves_database_failure(),
			Self::Page(error) => error.database_error().is_some(),
			Self::Binding(error) => error.involves_database_failure(),
			Self::Record { source, .. } => source.involves_database_failure(),
			_ => false,
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Progress(error) => error.commit_outcome_is_unknown(),
			Self::Enrollment(error) => error.commit_outcome_is_unknown(),
			Self::Page(error) => error.commit_outcome_is_unknown(),
			Self::Binding(error) => error.commit_outcome_is_unknown(),
			Self::Record { source, .. } => source.commit_outcome_is_unknown(),
			_ => false,
		}
	}
}

impl PgControlProgressLock<'_, '_> {
	/// A page of Queue records or an empty page is accounted without recovery or authority
	/// delivery.
	pub(in crate::upgrade) async fn convert_source_queue_page(
		mut self,
		fields: SourceQueueConversionFields<'_>,
	) -> Result<SourceQueueConversionResult, SourceQueueConversionError> {
		if !fields
			.resolved
			.target()
			.matches_target(fields.invocation.target())
		{
			return Err(SourceQueueConversionError::TargetMismatch);
		}
		let candidate = fields.resolved.candidate();
		let code = fields.resolved.code();
		let mut work = UpgradeWork::decode_json(
			self.control
				.pending_upgrade()
				.ok_or(ControlProgressError::WorkAbsent)?,
		)?;
		let expected = work.checkpoint_context();
		let (tx, proof) = self.physical.companion_tx_with_proof();
		let contracts = fields
			.invocation
			.source_relocation_contracts(
				tx,
				crate::runtime::admission::upgrade::HeldUpgradeControl::new(
					proof,
					&self.control,
					self.route,
				),
				candidate,
			)
			.await?;
		let declarations = SourceQueueDeclarations::new(code)?;
		let BoundSourceQueueSelection {
			conversion: bound,
			invariant,
		} = declarations.bind(&contracts)?;
		work.require_source_checkpoint_invariants(
			self.control.component_policies(),
			self.control.scope_policies(),
		)?;
		let procedure = fields.kind.procedure();
		let after = match work.source_checkpoint(procedure) {
			None => None,
			Some(checkpoint) => {
				if !checkpoint.invariant().matches_invariant(&invariant) {
					return Err(SourceCheckpointError::StoredInvariantMismatch.into());
				}
				match checkpoint.progress() {
					SourceScanProgress::Scanning { after_key } => after_key.clone(),
					SourceScanProgress::Scanned { .. } => {
						drop(bound);
						drop(contracts);
						let observed = self.keep_observed().await?;
						return Ok(SourceQueueConversionResult { observed, work });
					}
				}
			}
		};
		let crate::runtime::admission::bindings::PgInstallationAdmissionBinding::Imported {
			source: descriptor,
			..
		} = self.progress_storage.installation
		else {
			return Err(SourceQueueConversionError::ImportedRequired);
		};
		let discovery = PgV10SourceCandidateStorage::new(descriptor);
		let page = discovery
			.read_page(
				self.physical.companion_tx(),
				&SourceCandidatePageRequest::new(
					fields.kind.selection(after.as_ref())?,
					fields.batch.limit,
				),
			)
			.await?;
		let mut last_converted = None;
		for (converted, key) in page.keys().iter().enumerate() {
			if !fields.batch.takes_another(converted) {
				break;
			}
			let (record, cursor) = fields.kind.record(key)?;
			bound
				.convert_record(self.physical.companion_tx(), record)
				.await
				.map_err(|error| SourceQueueConversionError::Record {
					key: key.clone(),
					source: Box::new(error),
				})?;
			last_converted = Some(cursor);
		}
		let update = match last_converted {
			Some(cursor) => SourceScanUpdate::Advance { after_key: cursor },
			None => SourceScanUpdate::Scanned { last_key: after },
		};
		work.record_source_scan(
			SourceCheckpointContext {
				control: &self.control,
				candidate,
				origin: self.origin,
				expected: &expected,
			},
			procedure,
			&invariant,
			update,
		)?;
		drop(bound);
		drop(contracts);
		let observed = self.persist_work(&work).await?;
		Ok(SourceQueueConversionResult { observed, work })
	}
}
