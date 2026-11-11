//! A page of classified FleetState candidates and its cursor share an exact owned transaction.

use std::{fmt, num::NonZeroU32};

use crate::fleet::counter::state::CounterRepresentationError;
use crate::fleet::scalar::FleetScalarCode;
use crate::fleet::scalar_codec::ScalarStateError;
use crate::fleet::scalar_codec::ValueError;
use crate::fleet::v10::scalar_source_bridge::{
	ScalarFleetSourceImport, ScalarFleetSourceImportError, ScalarFleetSourceRecord,
};
use crate::fleet::v10::source_state_keys::{FleetSourceFamily, FleetSourceKey, FleetSourceKeyKind};
use crate::installation::contract::ContractStateReadError;
use crate::installation::v10::candidate_page::{
	PgV10SourceCandidateStorage, SourceCandidateKey, SourceCandidatePageError,
	SourceCandidatePageRequest, SourceCandidateSelection,
};
use crate::kv::Key;
use crate::records::PointRecordLock;
use crate::records::postgres::v10::relocating_point::{
	RelocatingPointBinding, RelocatingPointError,
};
use crate::records::postgres::v10::source_point::SourcePointError;
use crate::records::{
	EncodedRecord, RecordIdentity, RecordKey, RelocatingPointRecordLock, RelocationRecord,
	SourcePointRecord, StoredRecord,
};
use crate::runtime::admission::bindings::{
	InstallationAdmissionBindingError, PgInstallationAdmissionBinding,
};
use crate::runtime::admission::upgrade::contracts::NativeUpgradeContracts;
use crate::runtime::admission::{
	NativeAdmissionError,
	upgrade::{NativeUpgradeAdmissionError, NativeUpgradeInvocation},
};
use crate::runtime::targets::ResolvedNativeRuntimeTarget;
use crate::upgrade::work::v10::source_checkpoint::{
	PgV10ConversionProcedure, SourceCheckpointContext, SourceCheckpointError,
	SourceConversionCursor, SourceScanProgress, SourceScanUpdate,
};
use crate::upgrade::work::{UpgradeWork, UpgradeWorkError};

use super::super::{ControlProgressError, PgControlProgressLock, SourceConversionBatch};

pub(in crate::upgrade) mod selection;
use selection::SourceFleetDeclarations;

pub(in crate::upgrade) struct SourceFleetConversionFields<'a> {
	pub(crate) invocation: &'a NativeUpgradeInvocation<'a>,
	pub(crate) resolved: &'a ResolvedNativeRuntimeTarget,
	pub(crate) conflict_attempts: NonZeroU32,
	pub(crate) batch: SourceConversionBatch,
}
/// This is tentative progress, without whole-Fleet coverage or source-retirement authority.
pub(in crate::upgrade) struct SourceFleetConversionResult {
	pub(crate) observed: crate::upgrade::observation::NativeUpgradeObservedControl,
	pub(crate) work: UpgradeWork,
}
pub(crate) struct SourceFleetFailureKey(Key);
impl fmt::Debug for SourceFleetFailureKey {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("SourceFleetFailureKey")
			.field("key_byte_len", &self.0.as_str().len())
			.finish_non_exhaustive()
	}
}
#[derive(Debug, thiserror::Error)]
pub(crate) enum SourceFleetConversionError {
	#[error("source Fleet progress guard failed")]
	Progress(#[from] ControlProgressError),
	#[error("source Fleet private maintenance authentication failed")]
	Enrollment(#[from] NativeUpgradeAdmissionError),
	#[error("source Fleet component support failed")]
	Admission(#[from] NativeAdmissionError),
	#[error("source Fleet installation association failed")]
	Binding(#[from] InstallationAdmissionBindingError),
	#[error("source Fleet complete work context failed")]
	Work(#[from] UpgradeWorkError),
	#[error("source Fleet checkpoint failed")]
	Checkpoint(#[from] SourceCheckpointError),
	#[error("source Fleet candidate page failed")]
	Page(#[from] SourceCandidatePageError),
	#[error("source Fleet Counter conversion failed")]
	Counter(#[from] CounterRepresentationError),
	#[error("source Fleet native Counter body is not readable under the policy")]
	CounterRead(#[from] ContractStateReadError<ValueError>),
	#[error("source Fleet native scalar conversion failed")]
	Scalar(#[from] ScalarStateError),
	#[error("source Fleet scalar reader failed")]
	ScalarImport(#[from] ScalarFleetSourceImportError),
	#[error("source Fleet source adapter is absent")]
	SourceStorageAbsent,
	#[error("source Fleet conversion requires the imported installation")]
	ImportedRequired,
	#[error("source Fleet code or target differs from its invocation")]
	TargetMismatch,
	#[error("source Fleet family {family:?} is not implemented by this fixed batch")]
	UnsupportedFamily {
		family: FleetSourceFamily,
		key: SourceFleetFailureKey,
	},
	#[error("source Fleet canonical point failed")]
	Point {
		key: SourceFleetFailureKey,
		#[source]
		source: Box<RelocatingPointError>,
	},
	#[error("source Fleet row could not be deleted")]
	Discard {
		key: SourceFleetFailureKey,
		#[source]
		source: Box<SourcePointError>,
	},
	#[error("source Fleet identified record preparation failed")]
	Record {
		key: SourceFleetFailureKey,
		#[source]
		source: Box<Self>,
	},
}
impl SourceFleetConversionError {
	/// Returns an actual retained driver cause without replacing the primary typed failure.
	pub(crate) fn database_error(&self) -> Option<&crate::db::DatabaseError> {
		match self {
			Self::Record { source, .. } => source.database_error(),
			Self::Progress(error) => error.database_error(),
			Self::Enrollment(error) => error.database_error(),
			Self::Admission(error) => error.database_error(),
			Self::Binding(error) => error.database_error(),
			Self::Page(error) => error.database_error(),
			Self::Point { source, .. } => source.database_error(),
			Self::Discard { source, .. } => source.database_error(),
			_ => None,
		}
	}

	fn discard(key: &Key, source: SourcePointError) -> Self {
		Self::Discard {
			key: SourceFleetFailureKey(key.clone()),
			source: Box::new(source),
		}
	}
	fn point(key: &Key, source: RelocatingPointError) -> Self {
		Self::Point {
			key: SourceFleetFailureKey(key.clone()),
			source: Box::new(source),
		}
	}
	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::Progress(error) => error.involves_database_failure(),
			Self::Enrollment(error) => error.involves_database_failure(),
			Self::Admission(error) => error.involves_database_failure(),
			Self::Binding(error) => error.involves_database_failure(),
			Self::Page(error) => error.database_error().is_some(),
			Self::Point { source, .. } => source.involves_database_failure(),
			Self::Discard { source, .. } => source.involves_database_failure(),
			Self::Record { source, .. } => source.involves_database_failure(),
			_ => false,
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Progress(error) => error.commit_outcome_is_unknown(),
			Self::Enrollment(error) => error.commit_outcome_is_unknown(),
			Self::Admission(error) => error.commit_outcome_is_unknown(),
			Self::Binding(error) => error.commit_outcome_is_unknown(),
			Self::Page(error) => error.commit_outcome_is_unknown(),
			Self::Point { source, .. } => source.commit_outcome_is_unknown(),
			Self::Discard { source, .. } => source.commit_outcome_is_unknown(),
			Self::Record { source, .. } => source.commit_outcome_is_unknown(),
			_ => false,
		}
	}
}

fn prepare_scalar_source(
	contracts: &NativeUpgradeContracts<'_>,
	code: &FleetScalarCode,
	key: &FleetSourceKey,
	record: &SourcePointRecord,
) -> Result<EncodedRecord, SourceFleetConversionError> {
	let declaration = code.source.declaration();
	let bound = contracts.bind(&declaration)?;
	let reader = code.source.bind(&bound)?;
	match reader.import(ScalarFleetSourceRecord { key, record })? {
		ScalarFleetSourceImport::OnceCompletion { value, metadata } => {
			let declaration = code.once.declaration();
			let bound = contracts.bind(&declaration)?;
			Ok(code
				.once
				.bind_schema(&bound)?
				.encode_record(&value, metadata)?)
		}
		ScalarFleetSourceImport::CacheEpoch { value, metadata } => {
			let declaration = code.cache_epoch.declaration();
			let bound = contracts.bind(&declaration)?;
			Ok(code
				.cache_epoch
				.bind_schema(&bound)?
				.encode_record(&value, metadata)?)
		}
		ScalarFleetSourceImport::TopicSequence { value, metadata } => {
			let declaration = code.topic.declaration();
			let bound = contracts.bind(&declaration)?;
			Ok(code
				.topic
				.bind_sequence(&bound)?
				.encode_record(&value, metadata)?)
		}
		ScalarFleetSourceImport::SubscriptionCursor { value, metadata } => {
			let declaration = code.topic.declaration();
			let bound = contracts.bind(&declaration)?;
			Ok(code
				.topic
				.bind_cursor(&bound)?
				.encode_record(&value, metadata)?)
		}
	}
}
fn require_scalar_native(
	contracts: &NativeUpgradeContracts<'_>,
	code: &FleetScalarCode,
	key: &FleetSourceKey,
	record: &StoredRecord,
) -> Result<(), SourceFleetConversionError> {
	match key.kind() {
		FleetSourceKeyKind::OnceCompletion(_) => {
			let declaration = code.once.declaration();
			let bound = contracts.bind(&declaration)?;
			code.once.bind_schema(&bound)?.decode_record(record)?;
		}
		FleetSourceKeyKind::CacheEpoch(_) => {
			let declaration = code.cache_epoch.declaration();
			let bound = contracts.bind(&declaration)?;
			code.cache_epoch
				.bind_schema(&bound)?
				.decode_record(record)?;
		}
		FleetSourceKeyKind::TopicSequence(_) => {
			let declaration = code.topic.declaration();
			let bound = contracts.bind(&declaration)?;
			code.topic.bind_sequence(&bound)?.decode_record(record)?;
		}
		FleetSourceKeyKind::SubscriptionCursor { .. } => {
			let declaration = code.topic.declaration();
			let bound = contracts.bind(&declaration)?;
			code.topic.bind_cursor(&bound)?.decode_record(record)?;
		}
		_ => {
			return Err(SourceFleetConversionError::UnsupportedFamily {
				family: key.kind().family(),
				key: SourceFleetFailureKey(key.key().clone()),
			});
		}
	}
	Ok(())
}

fn prepare_fleet_record(
	record: &RelocationRecord,
	parsed: &FleetSourceKey,
	contracts: &NativeUpgradeContracts<'_>,
	code: &crate::runtime::targets::NativeRuntimeTargetCode,
	counter_schema: &crate::fleet::counter::state::CounterSchemaBinding<'_>,
) -> Result<Option<EncodedRecord>, SourceFleetConversionError> {
	let counters = code.counters();
	let is_counter = matches!(parsed.kind(), FleetSourceKeyKind::Counter(_));
	Ok(match record {
		RelocationRecord::Source(record) => {
			let record = if is_counter {
				counters
					.component
					.convert_source_record(record, counter_schema)?
			} else {
				prepare_scalar_source(contracts, &code.fleet_scalar().code, parsed, record)?
			};
			Some(record)
		}
		RelocationRecord::Native(record) => {
			if is_counter {
				counter_schema.require_native_record(record)?;
			} else {
				require_scalar_native(contracts, &code.fleet_scalar().code, parsed, record)?;
			}
			None
		}
		RelocationRecord::Absent => None,
	})
}

impl PgControlProgressLock<'_, '_> {
	/// Unsupported families block before cursor advancement; source rows are never filtered by
	/// expiry.
	pub(in crate::upgrade) async fn convert_source_fleet_page(
		mut self,
		fields: SourceFleetConversionFields<'_>,
	) -> Result<SourceFleetConversionResult, SourceFleetConversionError> {
		if !fields
			.resolved
			.target()
			.matches_target(fields.invocation.target())
		{
			return Err(SourceFleetConversionError::TargetMismatch);
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
		let counters = code.counters();
		let declarations = SourceFleetDeclarations::new(code);
		let selected = declarations.bind(&contracts)?;
		let counter_schema = selected.counter_schema()?;
		let invariant = selected.invariant();
		work.require_source_checkpoint_invariants(
			self.control.component_policies(),
			self.control.scope_policies(),
		)?;
		let procedure = PgV10ConversionProcedure::FleetStateRelocationV1;
		let after = match work.source_checkpoint(procedure) {
			None => None,
			Some(checkpoint) => {
				if !checkpoint.invariant().matches_invariant(invariant) {
					return Err(SourceCheckpointError::StoredInvariantMismatch.into());
				}
				match checkpoint.progress() {
					SourceScanProgress::Scanned { .. } => {
						drop(contracts);
						let observed = self.keep_observed().await?;
						return Ok(SourceFleetConversionResult { observed, work });
					}
					SourceScanProgress::Scanning { after_key: None } => None,
					SourceScanProgress::Scanning {
						after_key: Some(SourceConversionCursor::FleetState(key)),
					} => Some(key.clone()),
					_ => return Err(SourceCheckpointError::CursorProcedureMismatch.into()),
				}
			}
		};
		let PgInstallationAdmissionBinding::Imported {
			source: descriptor, ..
		} = self.progress_storage.installation
		else {
			return Err(SourceFleetConversionError::ImportedRequired);
		};
		let discovery = PgV10SourceCandidateStorage::new(descriptor);
		let request = SourceCandidatePageRequest::new(
			SourceCandidateSelection::FleetState {
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
			let SourceCandidateKey::FleetState(key) = candidate_key else {
				return Err(SourceCheckpointError::CursorProcedureMismatch.into());
			};
			let parsed = self
				.progress_storage
				.installation
				.classify_fleet_source_key(key.clone())?;
			if parsed.kind().is_carried_into_v11() {
				let (storage, binding) =
					if let FleetSourceKeyKind::Counter(resource) = parsed.kind() {
						let source = counters
							.source
							.as_ref()
							.ok_or(SourceFleetConversionError::SourceStorageAbsent)?;
						let binding = RelocatingPointBinding::new(
							key.clone(),
							RecordIdentity::new(
								counters.namespace.clone(),
								RecordKey::new(resource.as_str().as_bytes().to_vec()),
							),
						);
						contracts.require_counter_source_storage(source.relocation(), &binding)?;
						(source.relocation(), binding)
					} else if matches!(
						parsed.kind(),
						FleetSourceKeyKind::OnceCompletion(_) | FleetSourceKeyKind::CacheEpoch(_)
					) {
						let scalar = code.fleet_scalar();
						let storage = scalar
							.source
							.as_ref()
							.ok_or(SourceFleetConversionError::SourceStorageAbsent)?;
						let (_, binding) = contracts.scalar_source_binding(key)?;
						contracts.require_scalar_source_storage(storage, &binding)?;
						(storage, binding)
					} else {
						return Err(SourceFleetConversionError::UnsupportedFamily {
							family: parsed.kind().family(),
							key: SourceFleetFailureKey(key.clone()),
						});
					};
				let lock = storage
					.lock(
						self.physical.companion_tx(),
						&binding,
						fields.conflict_attempts,
					)
					.await
					.map_err(|error| SourceFleetConversionError::point(key, error))?;
				let prepared = prepare_fleet_record(
					lock.snapshot().record(),
					&parsed,
					&contracts,
					code,
					&counter_schema,
				)
				.map_err(|error| SourceFleetConversionError::Record {
					key: SourceFleetFailureKey(key.clone()),
					source: Box::new(error),
				})?;
				match prepared {
					Some(record) => lock.write(record).await,
					None => lock.keep().await,
				}
				.map_err(|error| SourceFleetConversionError::point(key, error))?;
			} else {
				// v11 starts every Semaphore, Throttler, cache, and Topic empty, so data
				// retirement deletes their v10 rows instead of moving them.
				let source = counters
					.source
					.as_ref()
					.ok_or(SourceFleetConversionError::SourceStorageAbsent)?
					.relocation()
					.source_point_storage();
				let lock = source
					.lock(self.physical.companion_tx(), key, fields.conflict_attempts)
					.await
					.map_err(|error| SourceFleetConversionError::discard(key, error))?;
				lock.delete()
					.await
					.map_err(|error| SourceFleetConversionError::discard(key, error))?;
			}
			last_converted = Some(key);
		}
		let update = match last_converted {
			Some(key) => SourceScanUpdate::Advance {
				after_key: SourceConversionCursor::FleetState(key.clone()),
			},
			None => SourceScanUpdate::Scanned {
				last_key: after.map(SourceConversionCursor::FleetState),
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
			invariant,
			update,
		)?;
		drop(contracts);
		let observed = self.persist_work(&work).await?;
		Ok(SourceFleetConversionResult { observed, work })
	}
}
