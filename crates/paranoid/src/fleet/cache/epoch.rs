//! Source-aware cache invalidation metadata under actual runtime admission.
//!
//! This internal point operation grants no cache-computation or lease authority.

use std::{num::NonZeroU32, sync::Arc};

use crate::db::{DatabaseError, TransactionFailure, Tx, WriteDatabase, run_transaction_write};
use crate::installation::ProtocolError;
use crate::installation::contract::{BoundComponentContract, ComponentDeclaration};
use crate::records::postgres::v10::relocating_point::{
	PgRelocatingPointStorage, RelocatingPointBinding, RelocatingPointError,
};
use crate::records::postgres::v10::source_point::{PgSourcePointStorage, SourcePointError};
use crate::records::postgres::{PgPointStorage, StateStorageError};
use crate::records::{
	EncodedRecord, PointRecordLock, RecordIdentity, RecordMetadata, RelocatingPointRecordLock,
	RelocationRecord, StoredRecord,
};
use crate::runtime::admission::bindings::{
	InstallationStorageRoute, PgInstallationAdmissionBinding,
};
use crate::runtime::admission::{
	NativeAdmissionError, NativeAdmittedContracts, NativeOperationError, PgNativeAdmission,
};

use super::epoch_state::CacheEpochComponent;
use crate::fleet::CoalescingCacheKey;
use crate::fleet::scalar::{FleetScalarCode, NativeFleetScalarResource};
use crate::fleet::scalar_codec::{ScalarSchemaBinding, ScalarStateError};
use crate::fleet::v10::scalar_source_bridge::{
	BoundScalarFleetSourceBridge, ScalarFleetSourceBody, ScalarFleetSourceImport,
	ScalarFleetSourceImportError,
};

pub(crate) struct NativeCacheEpochFields {
	pub(crate) key: CoalescingCacheKey,
	pub(crate) code: Arc<FleetScalarCode>,
	pub(crate) source: Option<PgRelocatingPointStorage>,
	pub(crate) admission: Arc<PgNativeAdmission>,
	pub(crate) conflict_attempts: NonZeroU32,
}

/// A retained handle keeps actual code and admission, independently from lifecycle ownership.
#[derive(Clone)]
pub(crate) struct PgNativeCacheEpoch {
	code: Arc<FleetScalarCode>,
	storage: PgPointStorage,
	source: Option<CacheEpochSourceStorage>,
	admission: Arc<PgNativeAdmission>,
	key: CoalescingCacheKey,
	identity: RecordIdentity,
	conflict_attempts: NonZeroU32,
}
/// Both operation builders retain the same exact validated source relation.
#[derive(Clone)]
struct CacheEpochSourceStorage {
	source: PgSourcePointStorage,
	relocation: PgRelocatingPointStorage,
}
impl PgNativeCacheEpoch {
	pub(crate) fn new(fields: NativeCacheEpochFields) -> Result<Self, NativeCacheEpochError> {
		let installation = fields.admission.installation_binding();
		let native = match installation.as_ref() {
			PgInstallationAdmissionBinding::Fresh { native }
			| PgInstallationAdmissionBinding::Imported { native, .. } => native,
		};
		let placement = native.fleet_scalar();
		let resource = NativeFleetScalarResource::CacheEpoch(&fields.key);
		let identity = RecordIdentity::new(placement.cache_epoch().clone(), resource.record_key());
		if let Some(adapter) = &fields.source {
			installation
				.verify_fleet_scalar_source_pair(adapter)
				.map_err(NativeAdmissionError::from)?;
		}
		Ok(Self {
			storage: placement.storage().clone(),
			code: fields.code,
			source: fields.source.map(|relocation| CacheEpochSourceStorage {
				source: relocation.source_point_storage(),
				relocation,
			}),
			identity,
			key: fields.key,
			admission: fields.admission,
			conflict_attempts: fields.conflict_attempts,
		})
	}
	fn source(
		&self,
	) -> Result<
		(
			&CacheEpochSourceStorage,
			RelocatingPointBinding,
			crate::fleet::v10::source_state_keys::FleetSourceKey,
		),
		NativeCacheEpochError,
	> {
		let source = self
			.source
			.as_ref()
			.ok_or(NativeCacheEpochError::SourceMappingAbsent)?;
		let installation = self.admission.installation_binding();
		let PgInstallationAdmissionBinding::Imported {
			source: registered_source,
			..
		} = installation.as_ref()
		else {
			return Err(NativeCacheEpochError::SourceMappingAbsent);
		};
		let key = NativeFleetScalarResource::CacheEpoch(&self.key)
			.source_key(registered_source.fleet_root())
			.map_err(NativeCacheEpochError::SourceKey)?;
		let (_, binding) = installation
			.scalar_source_binding(&key)
			.map_err(NativeAdmissionError::from)?;
		if binding.identity() != &self.identity {
			return Err(NativeCacheEpochError::SourceMappingAbsent);
		}
		let parsed = installation
			.verify_fleet_scalar_source_storage(&source.relocation, &binding)
			.map_err(NativeAdmissionError::from)?;
		Ok((source, binding, parsed))
	}
	pub(super) fn shares_admission(&self, admission: &PgNativeAdmission) -> bool {
		std::ptr::eq(self.admission.as_ref(), admission)
	}
	pub(in crate::fleet) fn key(&self) -> &CoalescingCacheKey {
		&self.key
	}
	pub(in crate::fleet) fn declaration(&self) -> ComponentDeclaration<'_, CacheEpochComponent> {
		self.code.cache_epoch.declaration()
	}
	pub(in crate::fleet) fn bind_read<'a>(
		&'a self,
		fields: CacheEpochReadBindingFields<'a, '_, '_>,
	) -> Result<BoundCacheEpochRead<'a>, NativeCacheEpochError> {
		Ok(BoundCacheEpochRead {
			epoch: self,
			contracts: fields.contracts,
			schema: self.code.cache_epoch.bind_schema(fields.component)?,
		})
	}

	/// Increments the exact signed epoch without deleting payloads or acquiring computation
	/// authority.
	pub(crate) async fn invalidate(
		&self,
		database: impl WriteDatabase,
	) -> Result<i64, NativeCacheEpochError> {
		run_transaction_write(
			database,
			"fleet.native_cache_epoch.invalidate",
			async |tx| {
				self.admission
					.execute_write_with_contracts(tx, async |contracts, tx| {
						let declaration = self.code.cache_epoch.declaration();
						let bound = contracts
							.bind(&declaration)
							.map_err(NativeOperationError::Admission)?;
						let result = async {
							let schema = self.code.cache_epoch.bind_schema(&bound)?;
							match contracts.storage_route() {
								InstallationStorageRoute::NativeFresh
								| InstallationStorageRoute::NativeImported => {
									let lock = self
										.storage
										.lock(tx, &self.identity, self.conflict_attempts)
										.await?;
									let prepared = prepare_increment(
										&self.code.cache_epoch,
										read_native(&schema, lock.snapshot().record()),
										CacheEpochWriter::Native(&schema),
										lock.snapshot().observed_at(),
									);
									finish_point(lock, prepared).await
								}
								InstallationStorageRoute::SourceCoexistence => {
									let (source, binding, parsed) = self.source()?;
									let bridge_declaration = self.code.source.declaration();
									let bridge_contract = contracts.bind(&bridge_declaration)?;
									let bridge = self.code.source.bind(&bridge_contract)?;
									let lock = source
										.source
										.lock(tx, binding.source_key(), self.conflict_attempts)
										.await?;
									let prepared = prepare_increment(
										&self.code.cache_epoch,
										read_source(&bridge, &parsed, lock.snapshot().record()),
										CacheEpochWriter::PublishedSource(&bridge),
										lock.snapshot().observed_at(),
									);
									finish_point(lock, prepared).await
								}
								InstallationStorageRoute::SourceRelocation => {
									let (source, binding, parsed) = self.source()?;
									let bridge_declaration = self.code.source.declaration();
									let bridge_contract = contracts.bind(&bridge_declaration)?;
									let bridge = self.code.source.bind(&bridge_contract)?;
									let lock = source
										.relocation
										.lock(tx, &binding, self.conflict_attempts)
										.await?;
									let prepared = prepare_increment(
										&self.code.cache_epoch,
										read_relocated(
											&schema,
											&bridge,
											&parsed,
											lock.snapshot().record(),
										),
										CacheEpochWriter::Native(&schema),
										lock.snapshot().observed_at(),
									);
									finish_relocation(lock, prepared).await
								}
							}
						}
						.await;
						Ok(result)
					})
					.await
					.map_err(NativeCacheEpochError::from_operation)?
			},
		)
		.await
	}
}

pub(in crate::fleet) struct CacheEpochReadBindingFields<'a, 'policy, 'code> {
	pub(in crate::fleet) contracts: &'a NativeAdmittedContracts,
	pub(in crate::fleet) component: &'a BoundComponentContract<'policy, 'code, CacheEpochComponent>,
}
/// Borrows the actual admitted interval and its exact component/schema binding.
pub(in crate::fleet) struct BoundCacheEpochRead<'a> {
	epoch: &'a PgNativeCacheEpoch,
	contracts: &'a NativeAdmittedContracts,
	schema: ScalarSchemaBinding<'a, i64>,
}
impl BoundCacheEpochRead<'_> {
	pub(in crate::fleet) async fn read(
		&self,
		tx: &mut Tx<'_>,
	) -> Result<i64, NativeCacheEpochError> {
		let current = match self.contracts.storage_route() {
			InstallationStorageRoute::NativeFresh | InstallationStorageRoute::NativeImported => {
				let snapshot = self.epoch.storage.read(tx, &self.epoch.identity).await?;
				read_native(&self.schema, snapshot.record())?
			}
			InstallationStorageRoute::SourceCoexistence => {
				let (source, binding, parsed) = self.epoch.source()?;
				let declaration = self.epoch.code.source.declaration();
				let component = self.contracts.bind(&declaration)?;
				let bridge = self.epoch.code.source.bind(&component)?;
				let snapshot = source.source.read(tx, binding.source_key()).await?;
				read_source(&bridge, &parsed, snapshot.record())?
			}
			InstallationStorageRoute::SourceRelocation => {
				let (source, binding, parsed) = self.epoch.source()?;
				let declaration = self.epoch.code.source.declaration();
				let component = self.contracts.bind(&declaration)?;
				let bridge = self.epoch.code.source.bind(&component)?;
				let snapshot = source.relocation.read(tx, &binding).await?;
				read_relocated(&self.schema, &bridge, &parsed, snapshot.record())?
			}
		};
		Ok(self.epoch.code.cache_epoch.epoch(current))
	}
}

fn permanent_metadata(observed_at: crate::records::StateTimestampMicros) -> RecordMetadata {
	RecordMetadata {
		expires_at: None,
		updated_at: observed_at,
	}
}
enum CacheEpochWriter<'a, 's> {
	Native(&'a ScalarSchemaBinding<'s, i64>),
	PublishedSource(&'a BoundScalarFleetSourceBridge<'s>),
}
fn prepare_increment(
	component: &CacheEpochComponent,
	current: Result<Option<i64>, NativeCacheEpochError>,
	writer: CacheEpochWriter<'_, '_>,
	observed_at: crate::records::StateTimestampMicros,
) -> Result<(i64, EncodedRecord), NativeCacheEpochError> {
	let next = component.next_epoch(current?)?;
	let metadata = permanent_metadata(observed_at);
	let record = match writer {
		CacheEpochWriter::Native(schema) => schema.encode_record(&next, metadata)?,
		CacheEpochWriter::PublishedSource(bridge) => {
			bridge.encode_published_cache_epoch(next, metadata)?
		}
	};
	Ok((next, record))
}
fn read_native(
	schema: &ScalarSchemaBinding<'_, i64>,
	record: Option<&StoredRecord>,
) -> Result<Option<i64>, NativeCacheEpochError> {
	Ok(record
		.map(|record| schema.decode_record(record))
		.transpose()?)
}
fn read_source(
	bridge: &BoundScalarFleetSourceBridge<'_>,
	key: &crate::fleet::v10::source_state_keys::FleetSourceKey,
	record: Option<&StoredRecord>,
) -> Result<Option<i64>, NativeCacheEpochError> {
	let value = record
		.map(|record| {
			bridge.import_body(ScalarFleetSourceBody {
				key,
				body: record.body(),
				metadata: record.metadata(),
			})
		})
		.transpose()?;
	match value {
		None => Ok(None),
		Some(ScalarFleetSourceImport::CacheEpoch { value, .. }) => Ok(Some(value)),
		_ => Err(NativeCacheEpochError::SourceMappingAbsent),
	}
}
fn read_relocated(
	schema: &ScalarSchemaBinding<'_, i64>,
	bridge: &BoundScalarFleetSourceBridge<'_>,
	key: &crate::fleet::v10::source_state_keys::FleetSourceKey,
	record: &RelocationRecord,
) -> Result<Option<i64>, NativeCacheEpochError> {
	match record {
		RelocationRecord::Native(record) => read_native(schema, Some(record)),
		RelocationRecord::Absent => read_native(schema, None),
		RelocationRecord::Source(record) => match bridge.import_body(ScalarFleetSourceBody {
			key,
			body: record.body(),
			metadata: record.metadata(),
		})? {
			ScalarFleetSourceImport::CacheEpoch { value, .. } => Ok(Some(value)),
			_ => Err(NativeCacheEpochError::SourceMappingAbsent),
		},
	}
}
async fn finish_point<L: PointRecordLock>(
	lock: L,
	prepared: Result<(i64, EncodedRecord), NativeCacheEpochError>,
) -> Result<i64, NativeCacheEpochError>
where
	NativeCacheEpochError: From<L::Error>,
{
	match prepared {
		Ok((next, record)) => {
			lock.write(record).await?;
			Ok(next)
		}
		Err(operation) => Err(cleanup_error(
			operation,
			lock.keep().await.map_err(Into::into),
		)),
	}
}
async fn finish_relocation<L: RelocatingPointRecordLock>(
	lock: L,
	prepared: Result<(i64, EncodedRecord), NativeCacheEpochError>,
) -> Result<i64, NativeCacheEpochError>
where
	NativeCacheEpochError: From<L::Error>,
{
	match prepared {
		Ok((next, record)) => {
			lock.write(record).await?;
			Ok(next)
		}
		Err(operation) => Err(cleanup_error(
			operation,
			lock.keep().await.map_err(Into::into),
		)),
	}
}
fn cleanup_error(
	operation: NativeCacheEpochError,
	cleanup: Result<(), NativeCacheEpochError>,
) -> NativeCacheEpochError {
	match cleanup {
		Ok(()) => operation,
		Err(cleanup) => NativeCacheEpochError::CleanupFailed {
			operation_error: Box::new(operation),
			cleanup_error: Box::new(cleanup),
		},
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeCacheEpochError {
	#[error("cache epoch has no source mapping for its admitted route")]
	SourceMappingAbsent,
	#[error("cache epoch source key cannot use the recorded Fleet root")]
	SourceKey(#[source] crate::kv::KeyError),
	#[error("cache epoch admission failed")]
	Admission(#[from] NativeAdmissionError),
	#[error("cache epoch state is invalid under the selected policy")]
	State(#[from] ScalarStateError),
	#[error("cache epoch source state is not readable")]
	Source(#[from] ScalarFleetSourceImportError),
	#[error("cache epoch native storage failed")]
	NativeStorage(#[from] StateStorageError),
	#[error("cache epoch source storage failed")]
	SourceStorage(#[from] SourcePointError),
	#[error("cache epoch relocation storage failed")]
	Relocation(#[from] RelocatingPointError),
	#[error("cache epoch rejection and reservation cleanup both failed")]
	CleanupFailed {
		#[source]
		operation_error: Box<Self>,
		cleanup_error: Box<Self>,
	},
	#[error("cache epoch database work failed")]
	Database(#[source] DatabaseError),
	#[error("cache epoch transaction protocol failed")]
	Protocol(#[source] ProtocolError),
	#[error("cache epoch work and rollback both failed")]
	RollbackFailed {
		#[source]
		operation_error: Box<Self>,
		rollback_error: DatabaseError,
	},
}
impl NativeCacheEpochError {
	fn from_operation(error: NativeOperationError<Self>) -> Self {
		match error {
			NativeOperationError::Admission(error) => Self::Admission(error),
			NativeOperationError::Operation(error) => error,
		}
	}
}
impl TransactionFailure for NativeCacheEpochError {
	fn from_protocol_error(error: ProtocolError) -> Self {
		Self::Protocol(error)
	}
	fn from_database_error(error: DatabaseError) -> Self {
		Self::Database(error)
	}
	fn from_rollback_failure(operation_error: Self, rollback_error: DatabaseError) -> Self {
		Self::RollbackFailed {
			operation_error: Box::new(operation_error),
			rollback_error,
		}
	}
	fn involves_database_failure(&self) -> bool {
		match self {
			Self::Database(_) | Self::RollbackFailed { .. } => true,
			Self::Admission(error) => error.involves_database_failure(),
			Self::NativeStorage(error) => error.database_error().is_some(),
			Self::SourceStorage(error) => error.involves_database_failure(),
			Self::Relocation(error) => error.involves_database_failure(),
			Self::CleanupFailed {
				operation_error,
				cleanup_error,
			} => {
				operation_error.involves_database_failure()
					|| cleanup_error.involves_database_failure()
			}
			_ => false,
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Database(error) => error.commit_outcome_is_unknown(),
			Self::RollbackFailed {
				operation_error, ..
			} => operation_error.commit_outcome_is_unknown(),
			Self::Admission(error) => error.commit_outcome_is_unknown(),
			Self::NativeStorage(error) => error
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
			Self::SourceStorage(error) => error.commit_outcome_is_unknown(),
			Self::Relocation(error) => error.commit_outcome_is_unknown(),
			Self::CleanupFailed {
				operation_error,
				cleanup_error,
			} => {
				operation_error.commit_outcome_is_unknown()
					|| cleanup_error.commit_outcome_is_unknown()
			}
			_ => false,
		}
	}
}
