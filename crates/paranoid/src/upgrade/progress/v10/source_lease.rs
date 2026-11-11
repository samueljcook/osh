//! A page of logical lease aggregates and its source cursor share one owned transaction.

use std::{fmt, num::NonZeroU32};

use crate::fleet::lease::v10::source_store::{LeaseSourceOrigin, LeaseSourceStorageError};
use crate::fleet::lease::{LeaseBindingError, LeaseKey};
use crate::installation::v10::candidate_page::{
	PgV10SourceCandidateStorage, SourceCandidateKey, SourceCandidatePageError,
	SourceCandidatePageRequest, SourceCandidateSelection,
};
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

use selection::{
	BoundSourceLeaseSelection, SourceLeaseSelectionFields, bind_source_lease_selection,
};

pub(in crate::upgrade) struct SourceLeaseConversionFields<'a> {
	pub(crate) invocation: &'a NativeUpgradeInvocation<'a>,
	pub(crate) resolved: &'a ResolvedNativeRuntimeTarget,
	pub(crate) conflict_attempts: NonZeroU32,
	pub(crate) batch: SourceConversionBatch,
}

/// Counts and cursors remain tentative until the outer owned transaction commits.
pub(in crate::upgrade) struct SourceLeaseConversionResult {
	pub(crate) observed: crate::upgrade::observation::NativeUpgradeObservedControl,
	pub(crate) work: UpgradeWork,
}

pub(crate) struct SourceLeaseFailureKey(LeaseKey);
impl fmt::Debug for SourceLeaseFailureKey {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("SourceLeaseFailureKey")
			.field("key_byte_len", &self.0.as_str().len())
			.finish_non_exhaustive()
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SourceLeaseConversionError {
	#[error("source lease conversion progress guard failed")]
	Progress(#[from] ControlProgressError),
	#[error("source lease conversion private participant admission failed")]
	Enrollment(#[from] NativeUpgradeAdmissionError),
	#[error("source lease conversion component support failed")]
	Admission(#[from] NativeAdmissionError),
	#[error("source lease conversion full work context failed")]
	Work(#[from] UpgradeWorkError),
	#[error("source lease conversion checkpoint failed")]
	Checkpoint(#[from] SourceCheckpointError),
	#[error("source lease conversion native policy failed")]
	Policy(#[from] LeaseBindingError),
	#[error("source lease conversion physical candidate page failed")]
	Page(#[from] SourceCandidatePageError),
	#[error("source lease conversion has no verified source storage")]
	SourceStorageAbsent,
	#[error("source lease conversion requires the imported association")]
	ImportedRequired,
	#[error("source lease conversion code or target differs from its invocation")]
	TargetMismatch,
	#[error("source lease conversion source origin changed during canonical conversion")]
	OriginChanged,
	#[error("source lease conversion fixed aggregate failed")]
	Lease {
		key: SourceLeaseFailureKey,
		#[source]
		source: Box<LeaseSourceStorageError>,
	},
}
impl SourceLeaseConversionError {
	/// Returns an actual retained driver cause without replacing the primary typed failure.
	pub(crate) fn database_error(&self) -> Option<&crate::db::DatabaseError> {
		match self {
			Self::Progress(error) => error.database_error(),
			Self::Enrollment(error) => error.database_error(),
			Self::Admission(error) => error.database_error(),
			Self::Page(error) => error.database_error(),
			Self::Lease { source, .. } => source.database_error(),
			_ => None,
		}
	}

	fn lease(key: &LeaseKey, source: LeaseSourceStorageError) -> Self {
		Self::Lease {
			key: SourceLeaseFailureKey(key.clone()),
			source: Box::new(source),
		}
	}
	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::Progress(error) => error.involves_database_failure(),
			Self::Enrollment(error) => error.involves_database_failure(),
			Self::Admission(error) => error.involves_database_failure(),
			Self::Page(error) => error.database_error().is_some(),
			Self::Lease { source, .. } => source.involves_database_failure(),
			_ => false,
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Progress(error) => error.commit_outcome_is_unknown(),
			Self::Enrollment(error) => error.commit_outcome_is_unknown(),
			Self::Admission(error) => error.commit_outcome_is_unknown(),
			Self::Page(error) => error.commit_outcome_is_unknown(),
			Self::Lease { source, .. } => source.commit_outcome_is_unknown(),
			_ => false,
		}
	}
}

impl PgControlProgressLock<'_, '_> {
	/// Accounts a page of discovered aggregates or a terminal empty page. No Claim is returned.
	pub(in crate::upgrade) async fn convert_source_lease_page(
		mut self,
		fields: SourceLeaseConversionFields<'_>,
	) -> Result<SourceLeaseConversionResult, SourceLeaseConversionError> {
		if !fields
			.resolved
			.target()
			.matches_target(fields.invocation.target())
		{
			return Err(SourceLeaseConversionError::TargetMismatch);
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
		let leases = code.leases();
		let source = leases
			.source
			.as_ref()
			.ok_or(SourceLeaseConversionError::SourceStorageAbsent)?;
		let native_declaration = leases.component.declaration();
		let bridge_declaration = source.bridge.declaration();
		let BoundSourceLeaseSelection {
			native,
			bridge,
			invariant,
		} = bind_source_lease_selection(SourceLeaseSelectionFields {
			code: leases,
			contracts: &contracts,
			native_declaration: &native_declaration,
			bridge_declaration: &bridge_declaration,
		})?;
		work.require_source_checkpoint_invariants(
			self.control.component_policies(),
			self.control.scope_policies(),
		)?;
		let procedure = PgV10ConversionProcedure::LeaseAggregateRelocationV1;
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
						return Ok(SourceLeaseConversionResult { observed, work });
					}
					SourceScanProgress::Scanning { after_key: None } => None,
					SourceScanProgress::Scanning {
						after_key: Some(SourceConversionCursor::LeaseAggregate(key)),
					} => Some(key.clone()),
					_ => return Err(SourceCheckpointError::CursorProcedureMismatch.into()),
				}
			}
		};
		let PgInstallationAdmissionBinding::Imported {
			source: descriptor, ..
		} = self.progress_storage.installation
		else {
			return Err(SourceLeaseConversionError::ImportedRequired);
		};
		let discovery = PgV10SourceCandidateStorage::new(descriptor);
		let request = SourceCandidatePageRequest::new(
			SourceCandidateSelection::LeaseAggregate {
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
			let SourceCandidateKey::LeaseAggregate(key) = candidate_key else {
				return Err(SourceCheckpointError::CursorProcedureMismatch.into());
			};
			let schema = native.transitions().bind_schema(&native, key.clone())?;
			let aggregate = source
				.storage
				.lock(
					self.physical.companion_tx(),
					key.clone(),
					fields.conflict_attempts,
				)
				.await
				.map_err(|error| SourceLeaseConversionError::lease(key, error))?;
			let from_source = matches!(aggregate.snapshot().origin(), LeaseSourceOrigin::Source(_));
			let moved = aggregate
				.convert(&bridge, &schema)
				.await
				.map_err(|error| SourceLeaseConversionError::lease(key, error))?;
			if moved != from_source {
				return Err(SourceLeaseConversionError::OriginChanged);
			}
			last_converted = Some(key);
		}
		let update = match last_converted {
			Some(key) => SourceScanUpdate::Advance {
				after_key: SourceConversionCursor::LeaseAggregate(key.clone()),
			},
			None => SourceScanUpdate::Scanned {
				last_key: after.map(SourceConversionCursor::LeaseAggregate),
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
		Ok(SourceLeaseConversionResult { observed, work })
	}
}
