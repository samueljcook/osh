//! A page of opaque v10 Raw keys and its cursor share one owned transaction.

use std::{fmt, num::NonZeroU32};

use crate::installation::v10::candidate_page::{
	PgV10SourceCandidateStorage, SourceCandidateKey, SourceCandidatePageError,
	SourceCandidatePageRequest, SourceCandidateSelection,
};
use crate::kv::Key;
use crate::records::postgres::v10::relocating_point::{
	RelocatingPointBinding, RelocatingPointError,
};
use crate::records::{RecordIdentity, RecordKey, RelocatingPointRecordLock, RelocationRecord};
use crate::runtime::admission::NativeAdmissionError;
use crate::runtime::admission::bindings::PgInstallationAdmissionBinding;
use crate::runtime::admission::upgrade::{NativeUpgradeAdmissionError, NativeUpgradeInvocation};
use crate::runtime::targets::ResolvedNativeRuntimeTarget;
use crate::upgrade::work::v10::source_checkpoint::{
	PgV10ConversionProcedure, SourceCheckpointContext, SourceCheckpointError,
	SourceConversionCursor, SourceScanProgress, SourceScanUpdate,
};
use crate::upgrade::work::{UpgradeWork, UpgradeWorkError};

use super::super::{ControlProgressError, PgControlProgressLock, SourceConversionBatch};

pub(super) mod selection;

use selection::{BoundSourceRawSelection, SourceRawSelectionFields, bind_source_raw_selection};

pub(in crate::upgrade) struct SourceRawConversionFields<'a> {
	pub(crate) invocation: &'a NativeUpgradeInvocation<'a>,
	pub(crate) resolved: &'a ResolvedNativeRuntimeTarget,
	pub(crate) conflict_attempts: NonZeroU32,
	pub(crate) batch: SourceConversionBatch,
}

/// The outer owned transaction must commit before the result is published.
pub(in crate::upgrade) struct SourceRawConversionResult {
	pub(crate) observed: crate::upgrade::observation::NativeUpgradeObservedControl,
	pub(crate) work: UpgradeWork,
}

pub(crate) struct SourceRawFailureKey(Key);
impl fmt::Debug for SourceRawFailureKey {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("SourceRawFailureKey")
			.field("key_byte_len", &self.0.as_str().len())
			.finish_non_exhaustive()
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SourceRawConversionError {
	#[error("source raw conversion progress guard failed")]
	Progress(#[from] ControlProgressError),
	#[error("source raw conversion private participant admission failed")]
	Enrollment(#[from] NativeUpgradeAdmissionError),
	#[error("source raw conversion component support failed")]
	Admission(#[from] NativeAdmissionError),
	#[error("source raw conversion full work context failed")]
	Work(#[from] UpgradeWorkError),
	#[error("source raw conversion checkpoint failed")]
	Checkpoint(#[from] SourceCheckpointError),
	#[error("source raw conversion physical candidate page failed")]
	Page(#[from] SourceCandidatePageError),
	#[error("source raw conversion has no verified source storage")]
	SourceStorageAbsent,
	#[error("source raw conversion requires the imported association")]
	ImportedRequired,
	#[error("source raw conversion code or target differs from its invocation")]
	TargetMismatch,
	#[error("source raw conversion fixed point failed")]
	Point {
		key: SourceRawFailureKey,
		#[source]
		source: Box<RelocatingPointError>,
	},
}
impl SourceRawConversionError {
	/// Returns an actual retained driver cause without replacing the primary typed failure.
	pub(crate) fn database_error(&self) -> Option<&crate::db::DatabaseError> {
		match self {
			Self::Progress(error) => error.database_error(),
			Self::Enrollment(error) => error.database_error(),
			Self::Admission(error) => error.database_error(),
			Self::Page(error) => error.database_error(),
			Self::Point { source, .. } => source.database_error(),
			_ => None,
		}
	}

	fn point(key: &Key, source: RelocatingPointError) -> Self {
		Self::Point {
			key: SourceRawFailureKey(key.clone()),
			source: Box::new(source),
		}
	}
	pub(crate) fn source_key(&self) -> Option<&Key> {
		match self {
			Self::Point { key, .. } => Some(&key.0),
			_ => None,
		}
	}
	/// The stored bytes of the key of the item that could not move: the key of a failed
	/// conversion, or a stored key that the page read could not accept.
	pub(in crate::upgrade) fn stored_source_key(&self) -> Option<&[u8]> {
		match self {
			Self::Point { key, .. } => Some(key.0.as_str().as_bytes()),
			Self::Page(error) => error.stored_key(),
			_ => None,
		}
	}
	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::Progress(error) => error.involves_database_failure(),
			Self::Enrollment(error) => error.involves_database_failure(),
			Self::Admission(error) => error.involves_database_failure(),
			Self::Page(error) => error.database_error().is_some(),
			Self::Point { source, .. } => source.involves_database_failure(),
			_ => false,
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Progress(error) => error.commit_outcome_is_unknown(),
			Self::Enrollment(error) => error.commit_outcome_is_unknown(),
			Self::Admission(error) => error.commit_outcome_is_unknown(),
			Self::Page(error) => error.commit_outcome_is_unknown(),
			Self::Point { source, .. } => source.commit_outcome_is_unknown(),
			_ => false,
		}
	}
}

impl PgControlProgressLock<'_, '_> {
	/// Moves a page of v10 Raw keys, byte for byte, into the core Raw namespace.
	pub(in crate::upgrade) async fn convert_source_raw_page(
		mut self,
		fields: SourceRawConversionFields<'_>,
	) -> Result<SourceRawConversionResult, SourceRawConversionError> {
		if !fields
			.resolved
			.target()
			.matches_target(fields.invocation.target())
		{
			return Err(SourceRawConversionError::TargetMismatch);
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
		let raw = code.raw_kv();
		let declaration = raw.component.declaration();
		let BoundSourceRawSelection {
			storage,
			bound,
			invariant,
		} = bind_source_raw_selection(SourceRawSelectionFields {
			resolved: fields.resolved,
			contracts: &contracts,
			declaration: &declaration,
		})?;
		work.require_source_checkpoint_invariants(
			self.control.component_policies(),
			self.control.scope_policies(),
		)?;
		let procedure = PgV10ConversionProcedure::RawBytesRelocationV1;
		let after = match work.source_checkpoint(procedure) {
			None => None,
			Some(checkpoint) => {
				if !checkpoint.invariant().matches_invariant(&invariant) {
					return Err(SourceCheckpointError::StoredInvariantMismatch.into());
				}
				match checkpoint.progress() {
					SourceScanProgress::Scanned { .. } => {
						drop(contracts);
						let observed = self.keep_observed().await?;
						return Ok(SourceRawConversionResult { observed, work });
					}
					SourceScanProgress::Scanning { after_key: None } => None,
					SourceScanProgress::Scanning {
						after_key: Some(SourceConversionCursor::RawBytes(key)),
					} => Some(key.clone()),
					_ => return Err(SourceCheckpointError::CursorProcedureMismatch.into()),
				}
			}
		};
		let PgInstallationAdmissionBinding::Imported {
			source: descriptor, ..
		} = self.progress_storage.installation
		else {
			return Err(SourceRawConversionError::ImportedRequired);
		};
		let discovery = PgV10SourceCandidateStorage::new(descriptor);
		let request = SourceCandidatePageRequest::new(
			SourceCandidateSelection::RawKv {
				after: after.clone(),
			},
			fields.batch.limit,
		);
		let page = discovery
			.read_page(self.physical.companion_tx(), &request)
			.await?;
		let mut last_converted = None;
		for (converted, candidate_key) in page.keys().iter().enumerate() {
			if !fields.batch.takes_another(converted) {
				break;
			}
			let SourceCandidateKey::RawKv(key) = candidate_key else {
				return Err(SourceCheckpointError::CursorProcedureMismatch.into());
			};
			let binding = RelocatingPointBinding::new(
				key.clone(),
				RecordIdentity::new(
					raw.namespace.clone(),
					RecordKey::new(key.as_str().as_bytes().to_vec()),
				),
			);
			let point_error = |error| SourceRawConversionError::point(key, error);
			let lock = storage
				.lock(
					self.physical.companion_tx(),
					&binding,
					fields.conflict_attempts,
				)
				.await
				.map_err(point_error)?;
			let moved = match lock.snapshot().record() {
				RelocationRecord::Source(record) => {
					Some(bound.transitions().convert_source_record(record))
				}
				RelocationRecord::Native(_) | RelocationRecord::Absent => None,
			};
			match moved {
				Some(record) => lock.write(record).await,
				None => lock.keep().await,
			}
			.map_err(point_error)?;
			last_converted = Some(key);
		}
		let update = match last_converted {
			Some(key) => SourceScanUpdate::Advance {
				after_key: SourceConversionCursor::RawBytes(key.clone()),
			},
			None => SourceScanUpdate::Scanned {
				last_key: after.map(SourceConversionCursor::RawBytes),
			},
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
		drop(contracts);
		let observed = self.persist_work(&work).await?;
		Ok(SourceRawConversionResult { observed, work })
	}
}
