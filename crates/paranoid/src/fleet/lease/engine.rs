//! Admitted native lease point operations with owned private-authority commit delivery.
//!
//! Each handle maps an exact parsed lease key into its declared bytewise namespace.
//! Acquisitions, renewals, and releases use owned control-pool transactions. An uncertain or
//! undelivered grant remains private and may occupy the lease until its stored expiry.
//! Managed heartbeat, acknowledged undelivered cleanup, and scoped guard work are separate.

use std::{num::NonZeroU32, sync::Arc};
use tokio::runtime::Handle;

use super::cleanup_target::LeaseCleanupTarget;
use super::driver_timing::LeaseControlTiming;
use super::state::{
	LeaseAcquisition, LeaseBindingError, LeaseComponent, LeaseOperationError,
	LeaseRepresentationError, TentativeLeaseAuthority,
};
use super::v10::source_bridge::{LeaseSourceBindingError, LeaseSourceBridge, LeaseSourceImport};
use super::v10::source_execution::LeaseSourceExecutionError;
use super::v10::source_store::{LeaseSourceOrigin, LeaseSourceStorageError, PgLeaseSourceStorage};
use super::{Claim, ClaimDuration, HolderPrefix, HolderSnapshot, LeaseKey, RandomnessError, Token};
use crate::db::{
	Database, DatabaseError, TransactionFailure, WritePool, WriteTx, run_transaction_read,
	run_transaction_write,
};
use crate::installation::ProtocolError;
use crate::records::postgres::{PgPointStorage, StateStorageError};
use crate::records::{PointRecordRead, RecordIdentity, RecordKey, RecordNamespace};
use crate::runtime::admission::bindings::InstallationStorageRoute;
use crate::runtime::admission::{
	NativeAdmissionError, NativeAdmittedContracts, NativeOperationError, PgNativeAdmission,
};

/// Shared actual lease code, exact physical namespace, and runtime admission/control capacity.
pub(crate) struct NativeLeaseBindings {
	pub(crate) originating_runtime: Handle,
	pub(crate) timing: LeaseControlTiming,
	pub(crate) component: LeaseComponent,
	pub(crate) storage: PgPointStorage,
	pub(crate) source: Option<NativeLeaseSourceBindings>,
	pub(crate) namespace: RecordNamespace,
	pub(crate) admission: Arc<PgNativeAdmission>,
	pub(crate) conflict_attempts: NonZeroU32,
	pub(crate) control_pool: WritePool,
}

/// One validated aggregate mapping and the actual typed importer used by registration.
/// Its fields grant no source-writer retirement or usable private Claim authority.
pub(crate) struct NativeLeaseSourceBindings {
	pub(crate) storage: PgLeaseSourceStorage,
	pub(crate) bridge: Arc<LeaseSourceBridge>,
}

/// One logical lease, retaining admission bindings without owning runtime lifecycle.
#[derive(Clone)]
pub(crate) struct PgNativeLease {
	pub(super) bindings: Arc<NativeLeaseBindings>,
	key: LeaseKey,
	identity: RecordIdentity,
	sharing: LeaseV10Sharing,
}

/// Whether v10 deployments can take a lease.
#[derive(Clone, Copy)]
enum LeaseV10Sharing {
	/// v10 deployments take leases of the same key, so while v10 writers remain the lease's
	/// record is in v10's tables.
	SharedWithV10,
	/// No v10 deployment takes the lease, so its record is in Paranoid's storage on every
	/// route.
	NativeOnly,
}

enum LeaseReleaseAuthorization<'a> {
	Claim(&'a Claim),
	Cleanup(&'a LeaseCleanupTarget),
}

impl LeaseReleaseAuthorization<'_> {
	fn key(&self) -> &LeaseKey {
		match self {
			Self::Claim(claim) => claim.key(),
			Self::Cleanup(target) => target.key(),
		}
	}
}

impl PgNativeLease {
	/// A lease that v10 deployments can also take.
	pub(crate) fn new(bindings: Arc<NativeLeaseBindings>, key: LeaseKey) -> Self {
		Self::with_sharing(bindings, key, LeaseV10Sharing::SharedWithV10)
	}

	/// A lease that no v10 deployment takes, such as the computation lease of a cache scope
	/// that starts with no stored values in v11.
	pub(crate) fn new_native_only(bindings: Arc<NativeLeaseBindings>, key: LeaseKey) -> Self {
		Self::with_sharing(bindings, key, LeaseV10Sharing::NativeOnly)
	}

	fn with_sharing(
		bindings: Arc<NativeLeaseBindings>,
		key: LeaseKey,
		sharing: LeaseV10Sharing,
	) -> Self {
		let identity = RecordIdentity::new(
			bindings.namespace.clone(),
			RecordKey::new(key.as_str().as_bytes().to_vec()),
		);
		Self {
			bindings,
			key,
			identity,
			sharing,
		}
	}

	/// The route of this lease's record under `contracts`.
	fn storage_route(&self, contracts: &NativeAdmittedContracts) -> InstallationStorageRoute {
		match self.sharing {
			LeaseV10Sharing::SharedWithV10 => contracts.storage_route(),
			LeaseV10Sharing::NativeOnly => InstallationStorageRoute::NativeImported,
		}
	}

	/// The wait after owned work for one complete release attempt of this lease.
	pub(crate) fn close_wait(&self) -> super::owned_driver::close::NativeLeaseCloseWait {
		super::owned_driver::close::NativeLeaseCloseWait::one_release_attempt(&self.bindings.timing)
	}

	fn source(&self) -> Result<&NativeLeaseSourceBindings, NativeLeaseError> {
		NativeLeaseReadBindings {
			component: &self.bindings.component,
			storage: &self.bindings.storage,
			source: self.bindings.source.as_ref(),
			namespace: &self.bindings.namespace,
			installation: self.bindings.admission.installation_binding(),
		}
		.source()
	}

	pub(crate) fn key(&self) -> &LeaseKey {
		&self.key
	}

	pub(crate) fn identity(&self) -> &RecordIdentity {
		&self.identity
	}

	pub(crate) fn shares_admission(&self, admission: &PgNativeAdmission) -> bool {
		std::ptr::eq(self.bindings.admission.as_ref(), admission)
	}

	/// A holder is diagnostic material at the admitted read's authoritative observation.
	pub(crate) async fn fetch_holder(
		&self,
		database: impl Database,
	) -> Result<Option<HolderSnapshot>, NativeLeaseError> {
		run_transaction_read(database, "fleet.native_lease.fetch_holder", async |tx| {
			let bindings = Arc::clone(&self.bindings);
			let key = self.key.clone();
			self.bindings
				.admission
				.execute_read_with_contracts(tx, async move |contracts, tx| {
					NativeLeaseReadBindings {
						component: &bindings.component,
						storage: &bindings.storage,
						source: bindings.source.as_ref(),
						namespace: &bindings.namespace,
						installation: bindings.admission.installation_binding(),
					}
					.fetch_holder(tx, &contracts, &key, self.storage_route(&contracts))
					.await
				})
				.await
				.map_err(NativeLeaseError::from_operation)
		})
		.await
	}

	/// Generates private acquisition material and delivers it only after known owned COMMIT.
	pub(crate) async fn try_acquire(
		&self,
		holder_prefix: &HolderPrefix,
		duration: ClaimDuration,
	) -> Result<Option<Claim>, NativeLeaseError> {
		let acquisition = LeaseAcquisition {
			holder_id: holder_prefix.generate_holder_id()?,
			lease_token: Token::generate()?,
			duration,
		};
		let tentative = run_transaction_write(
			&self.bindings.control_pool,
			"fleet.native_lease.try_acquire",
			async |tx| {
				self.bindings
					.admission
					.execute_write_with_contracts(tx, async move |contracts, tx| {
						Ok(self.acquire_admitted(contracts, tx, acquisition).await)
					})
					.await
					.map_err(NativeLeaseError::from_operation)?
			},
		)
		.await?;
		Ok(tentative.map(TentativeLeaseAuthority::into_claim_after_known_commit))
	}

	/// Renews matching authority with generated replacement material, delivered after known COMMIT.
	pub(crate) async fn try_renew(
		&self,
		authority: &Claim,
		duration: ClaimDuration,
	) -> Result<Option<Claim>, NativeLeaseError> {
		let next_token = Token::generate()?;
		let tentative = run_transaction_write(
			&self.bindings.control_pool,
			"fleet.native_lease.try_renew",
			async |tx| {
				self.bindings
					.admission
					.execute_write_with_contracts(tx, async move |contracts, tx| {
						Ok(self
							.renew_admitted(contracts, tx, authority, next_token, duration)
							.await)
					})
					.await
					.map_err(NativeLeaseError::from_operation)?
			},
		)
		.await?;
		Ok(tentative.map(TentativeLeaseAuthority::into_claim_after_known_commit))
	}

	/// Acknowledges an owned release only after known COMMIT, retaining durable fencing history.
	pub(crate) async fn release(&self, authority: &Claim) -> Result<bool, NativeLeaseError> {
		run_transaction_write(
			&self.bindings.control_pool,
			"fleet.native_lease.release",
			async |tx| {
				self.bindings
					.admission
					.execute_write_with_contracts(tx, async move |contracts, tx| {
						Ok(self.release_admitted(contracts, tx, authority).await)
					})
					.await
					.map_err(NativeLeaseError::from_operation)?
			},
		)
		.await
	}
	/// Runs the actual selected lease declarations inside already authenticated admission.
	pub(crate) async fn acquire_admitted(
		&self,
		contracts: NativeAdmittedContracts,
		tx: &mut WriteTx<'_>,
		acquisition: LeaseAcquisition,
	) -> Result<Option<TentativeLeaseAuthority>, NativeLeaseError> {
		let bindings = Arc::clone(&self.bindings);
		let identity = self.identity.clone();
		let key = self.key.clone();
		let declaration = bindings.component.declaration();
		let contract = contracts
			.bind(&declaration)
			.map_err(NativeLeaseError::Admission)?;
		async {
			let schema = contract.transitions().bind_schema(&contract, key.clone())?;
			match self.storage_route(&contracts) {
				InstallationStorageRoute::NativeFresh
				| InstallationStorageRoute::NativeImported => {
					let lock = bindings
						.storage
						.lock(tx, &identity, bindings.conflict_attempts)
						.await?;
					schema
						.acquire(lock, acquisition)
						.await
						.map_err(mutation_error)
				}
				InstallationStorageRoute::SourceCoexistence => {
					let source = self.source()?;
					let declaration = source.bridge.declaration();
					let bridge = contracts
						.bind(&declaration)
						.map_err(NativeLeaseError::Admission)?;
					let sql = bridge
						.transitions()
						.require_sql_execution(&bridge)
						.map_err(NativeLeaseError::SourceBinding)?;
					sql.acquire(&source.storage, tx, &key, acquisition)
						.await
						.map_err(NativeLeaseError::SourceExecution)
				}
				InstallationStorageRoute::SourceRelocation => {
					let source = self.source()?;
					let declaration = source.bridge.declaration();
					let bridge = contracts
						.bind(&declaration)
						.map_err(NativeLeaseError::Admission)?;
					let lock = source
						.storage
						.lock(tx, key, bindings.conflict_attempts)
						.await?;
					let point = lock.into_point_lock(&bridge, &schema).await?;
					schema
						.acquire(point, acquisition)
						.await
						.map_err(mutation_error)
				}
			}
		}
		.await
	}

	/// Runs the actual selected lease declarations inside already authenticated admission.
	pub(crate) async fn renew_admitted(
		&self,
		contracts: NativeAdmittedContracts,
		tx: &mut WriteTx<'_>,
		authority: &Claim,
		next_token: Token,
		duration: ClaimDuration,
	) -> Result<Option<TentativeLeaseAuthority>, NativeLeaseError> {
		let bindings = Arc::clone(&self.bindings);
		let identity = self.identity.clone();
		let key = self.key.clone();
		let declaration = bindings.component.declaration();
		let contract = contracts
			.bind(&declaration)
			.map_err(NativeLeaseError::Admission)?;
		async {
			let schema = contract.transitions().bind_schema(&contract, key.clone())?;
			match self.storage_route(&contracts) {
				InstallationStorageRoute::NativeFresh
				| InstallationStorageRoute::NativeImported => {
					if authority.key() != &key {
						return Ok(None);
					}
					let lock = bindings
						.storage
						.lock(tx, &identity, bindings.conflict_attempts)
						.await?;
					schema
						.renew(lock, authority, next_token, duration)
						.await
						.map_err(mutation_error)
				}
				InstallationStorageRoute::SourceCoexistence => {
					let source = self.source()?;
					let declaration = source.bridge.declaration();
					let bridge = contracts
						.bind(&declaration)
						.map_err(NativeLeaseError::Admission)?;
					let sql = bridge
						.transitions()
						.require_sql_execution(&bridge)
						.map_err(NativeLeaseError::SourceBinding)?;
					if authority.key() != &key {
						return Ok(None);
					}
					sql.renew(&source.storage, tx, &key, authority, next_token, duration)
						.await
						.map_err(NativeLeaseError::SourceExecution)
				}
				InstallationStorageRoute::SourceRelocation => {
					let source = self.source()?;
					let declaration = source.bridge.declaration();
					let bridge = contracts
						.bind(&declaration)
						.map_err(NativeLeaseError::Admission)?;
					if authority.key() != &key {
						return Ok(None);
					}
					let lock = source
						.storage
						.lock(tx, key, bindings.conflict_attempts)
						.await?;
					let point = lock.into_point_lock(&bridge, &schema).await?;
					schema
						.renew(point, authority, next_token, duration)
						.await
						.map_err(mutation_error)
				}
			}
		}
		.await
	}

	/// Runs the actual selected lease declarations inside already authenticated admission.
	pub(crate) async fn release_admitted(
		&self,
		contracts: NativeAdmittedContracts,
		tx: &mut WriteTx<'_>,
		authority: &Claim,
	) -> Result<bool, NativeLeaseError> {
		self.release_authorized_admitted(contracts, tx, LeaseReleaseAuthorization::Claim(authority))
			.await
	}

	pub(crate) async fn release_cleanup_admitted(
		&self,
		contracts: NativeAdmittedContracts,
		tx: &mut WriteTx<'_>,
		target: &LeaseCleanupTarget,
	) -> Result<bool, NativeLeaseError> {
		self.release_authorized_admitted(contracts, tx, LeaseReleaseAuthorization::Cleanup(target))
			.await
	}

	async fn release_authorized_admitted(
		&self,
		contracts: NativeAdmittedContracts,
		tx: &mut WriteTx<'_>,
		authority: LeaseReleaseAuthorization<'_>,
	) -> Result<bool, NativeLeaseError> {
		let bindings = Arc::clone(&self.bindings);
		let identity = self.identity.clone();
		let key = self.key.clone();
		let declaration = bindings.component.declaration();
		let contract = contracts
			.bind(&declaration)
			.map_err(NativeLeaseError::Admission)?;
		async {
			let schema = contract.transitions().bind_schema(&contract, key.clone())?;
			match self.storage_route(&contracts) {
				InstallationStorageRoute::NativeFresh
				| InstallationStorageRoute::NativeImported => {
					if authority.key() != &key {
						return Ok(false);
					}
					let lock = bindings
						.storage
						.lock(tx, &identity, bindings.conflict_attempts)
						.await?;
					match authority {
						LeaseReleaseAuthorization::Claim(claim) => {
							schema.release(lock, claim).await
						}
						LeaseReleaseAuthorization::Cleanup(target) => {
							schema.release_cleanup(lock, target).await
						}
					}
					.map_err(mutation_error)
				}
				InstallationStorageRoute::SourceCoexistence => {
					let source = self.source()?;
					let declaration = source.bridge.declaration();
					let bridge = contracts
						.bind(&declaration)
						.map_err(NativeLeaseError::Admission)?;
					let sql = bridge
						.transitions()
						.require_sql_execution(&bridge)
						.map_err(NativeLeaseError::SourceBinding)?;
					if authority.key() != &key {
						return Ok(false);
					}
					match authority {
						LeaseReleaseAuthorization::Claim(claim) => {
							sql.release(&source.storage, tx, &key, claim).await
						}
						LeaseReleaseAuthorization::Cleanup(target) => {
							sql.release_cleanup(&source.storage, tx, &key, target).await
						}
					}
					.map_err(NativeLeaseError::SourceExecution)
				}
				InstallationStorageRoute::SourceRelocation => {
					let source = self.source()?;
					let declaration = source.bridge.declaration();
					let bridge = contracts
						.bind(&declaration)
						.map_err(NativeLeaseError::Admission)?;
					if authority.key() != &key {
						return Ok(false);
					}
					let lock = source
						.storage
						.lock(tx, key, bindings.conflict_attempts)
						.await?;
					let point = lock.into_point_lock(&bridge, &schema).await?;
					match authority {
						LeaseReleaseAuthorization::Claim(claim) => {
							schema.release(point, claim).await
						}
						LeaseReleaseAuthorization::Cleanup(target) => {
							schema.release_cleanup(point, target).await
						}
					}
					.map_err(mutation_error)
				}
			}
		}
		.await
	}
}

/// Exact admission, primitive, point cleanup, and owned transaction ending failures.
#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeLeaseError {
	#[error("native lease registered transaction failed")]
	Registered(#[source] crate::runtime::admission::lease::LeaseRegistrationError),
	#[error("lease has no source mapping for the admitted source route")]
	SourceMappingAbsent,
	#[error("lease source storage does not match the runtime container and namespace")]
	StorageMappingMismatch,
	#[error("native-only lease placement is invalid during source-writer coexistence")]
	NativePlacementDuringCoexistence,
	#[error("source lease SQL semantic binding failed")]
	SourceBinding(#[source] LeaseSourceBindingError),
	#[error("source lease coexistence execution failed")]
	SourceExecution(#[source] LeaseSourceExecutionError),
	#[error("native lease randomness failed")]
	Randomness(#[from] RandomnessError),
	#[error("native lease admission failed")]
	Admission(#[source] NativeAdmissionError),
	#[error("native lease schema binding failed")]
	Binding(#[from] LeaseBindingError),
	#[error("native lease state read failed")]
	Read(#[source] LeaseRepresentationError),
	#[error("native lease point mutation failed")]
	Mutation(#[source] LeaseOperationError<LeaseStorageError>),
	#[error("native lease storage work failed")]
	Storage(#[from] LeaseStorageError),
	#[error("native lease database work failed")]
	Database(#[source] DatabaseError),
	#[error("native lease transaction protocol failed")]
	Protocol(#[source] ProtocolError),
	#[error("native lease work failed and rollback also failed")]
	RollbackFailed {
		#[source]
		operation_error: Box<Self>,
		rollback_error: DatabaseError,
	},
}

impl NativeLeaseError {
	pub(crate) fn from_operation(error: NativeOperationError<Self>) -> Self {
		match error {
			NativeOperationError::Admission(error) => Self::Admission(error),
			NativeOperationError::Operation(error) => error,
		}
	}
}

impl TransactionFailure for NativeLeaseError {
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
			Self::Registered(error) => error.involves_database_failure(),
			Self::Storage(error) => error.involves_database_failure(),
			Self::SourceExecution(error) => error.involves_database_failure(),
			Self::Mutation(error) => mutation_involves_database_failure(error),
			Self::Randomness(_)
			| Self::Binding(_)
			| Self::Read(_)
			| Self::SourceBinding(_)
			| Self::Protocol(_)
			| Self::SourceMappingAbsent
			| Self::StorageMappingMismatch
			| Self::NativePlacementDuringCoexistence => false,
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Database(error) => error.commit_outcome_is_unknown(),
			Self::Admission(error) => error.commit_outcome_is_unknown(),
			Self::Registered(error) => error.commit_outcome_is_unknown(),
			Self::Storage(error) => error.commit_outcome_is_unknown(),
			Self::SourceExecution(error) => error.commit_outcome_is_unknown(),
			Self::Mutation(error) => mutation_commit_outcome_is_unknown(error),
			Self::RollbackFailed {
				operation_error,
				rollback_error,
			} => {
				operation_error.commit_outcome_is_unknown()
					|| rollback_error.commit_outcome_is_unknown()
			}
			Self::Randomness(_)
			| Self::Binding(_)
			| Self::Read(_)
			| Self::SourceBinding(_)
			| Self::Protocol(_)
			| Self::SourceMappingAbsent
			| Self::StorageMappingMismatch
			| Self::NativePlacementDuringCoexistence => false,
		}
	}
}

fn mutation_involves_database_failure(error: &LeaseOperationError<LeaseStorageError>) -> bool {
	match error {
		LeaseOperationError::Storage(error) | LeaseOperationError::KeepCleanupFailed(error) => {
			error.involves_database_failure()
		}
		LeaseOperationError::CleanupFailed {
			operation_error,
			cleanup_error,
		} => {
			mutation_involves_database_failure(operation_error)
				|| cleanup_error.involves_database_failure()
		}
		LeaseOperationError::Representation(_) | LeaseOperationError::Transition(_) => false,
	}
}

fn mutation_commit_outcome_is_unknown(error: &LeaseOperationError<LeaseStorageError>) -> bool {
	match error {
		LeaseOperationError::Storage(error) | LeaseOperationError::KeepCleanupFailed(error) => {
			error.commit_outcome_is_unknown()
		}
		LeaseOperationError::CleanupFailed {
			operation_error,
			cleanup_error,
		} => {
			mutation_commit_outcome_is_unknown(operation_error)
				|| cleanup_error.commit_outcome_is_unknown()
		}
		LeaseOperationError::Representation(_) | LeaseOperationError::Transition(_) => false,
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum LeaseStorageError {
	#[error("native lease record storage failed")]
	Native(#[from] StateStorageError),
	#[error("source lease aggregate storage or import failed")]
	Source(#[from] LeaseSourceStorageError),
}
impl LeaseStorageError {
	fn involves_database_failure(&self) -> bool {
		match self {
			Self::Native(error) => error.database_error().is_some(),
			Self::Source(error) => error.involves_database_failure(),
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Native(error) => error
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
			Self::Source(error) => error.commit_outcome_is_unknown(),
		}
	}
}
impl From<StateStorageError> for NativeLeaseError {
	fn from(error: StateStorageError) -> Self {
		Self::Storage(error.into())
	}
}
impl From<LeaseSourceStorageError> for NativeLeaseError {
	fn from(error: LeaseSourceStorageError) -> Self {
		Self::Storage(error.into())
	}
}
fn mutation_error<E: Into<LeaseStorageError>>(error: LeaseOperationError<E>) -> NativeLeaseError {
	NativeLeaseError::Mutation(map_mutation_error(error))
}
fn map_mutation_error<E: Into<LeaseStorageError>>(
	error: LeaseOperationError<E>,
) -> LeaseOperationError<LeaseStorageError> {
	match error {
		LeaseOperationError::Representation(error) => LeaseOperationError::Representation(error),
		LeaseOperationError::Transition(error) => LeaseOperationError::Transition(error),
		LeaseOperationError::Storage(error) => LeaseOperationError::Storage(error.into()),
		LeaseOperationError::KeepCleanupFailed(error) => {
			LeaseOperationError::KeepCleanupFailed(error.into())
		}
		LeaseOperationError::CleanupFailed {
			operation_error,
			cleanup_error,
		} => LeaseOperationError::CleanupFailed {
			operation_error: Box::new(map_mutation_error(*operation_error)),
			cleanup_error: cleanup_error.into(),
		},
	}
}

/// Borrowed read bindings contain no lease acquisition or participant authority.
pub(crate) struct NativeLeaseReadBindings<'a> {
	pub(crate) component: &'a LeaseComponent,
	pub(crate) storage: &'a PgPointStorage,
	pub(crate) source: Option<&'a NativeLeaseSourceBindings>,
	pub(crate) namespace: &'a RecordNamespace,
	pub(crate) installation:
		&'a crate::runtime::admission::bindings::PgInstallationAdmissionBinding,
}
impl<'a> NativeLeaseReadBindings<'a> {
	fn source(&self) -> Result<&'a NativeLeaseSourceBindings, NativeLeaseError> {
		let source = self.source.ok_or(NativeLeaseError::SourceMappingAbsent)?;
		self.installation
			.verify_lease_source_storage(&source.storage, self.namespace)
			.map_err(NativeAdmissionError::from)
			.map_err(NativeLeaseError::Admission)?;
		if !source
			.storage
			.native_storage()
			.shares_container(self.storage)
			|| source.storage.namespace() != self.namespace
		{
			return Err(NativeLeaseError::StorageMappingMismatch);
		}
		Ok(source)
	}
	pub(crate) async fn fetch_holder(
		&self,
		tx: &mut crate::db::Tx<'_>,
		contracts: &NativeAdmittedContracts,
		key: &LeaseKey,
		route: InstallationStorageRoute,
	) -> Result<Option<HolderSnapshot>, NativeOperationError<NativeLeaseError>> {
		let declaration = self.component.declaration();
		let contract = contracts
			.bind(&declaration)
			.map_err(NativeOperationError::Admission)?;
		let result = async {
			let schema = contract.transitions().bind_schema(&contract, key.clone())?;
			match route {
				InstallationStorageRoute::NativeFresh
				| InstallationStorageRoute::NativeImported => {
					let snapshot = self
						.storage
						.read(
							tx,
							&RecordIdentity::new(
								self.namespace.clone(),
								RecordKey::new(key.as_str().as_bytes().to_vec()),
							),
						)
						.await?;
					schema
						.decode_live_holder(&snapshot)
						.map_err(NativeLeaseError::Read)
				}
				route @ (InstallationStorageRoute::SourceCoexistence
				| InstallationStorageRoute::SourceRelocation) => {
					let source = self.source()?;
					let declaration = source.bridge.declaration();
					let bridge = contracts
						.bind(&declaration)
						.map_err(NativeLeaseError::Admission)?;
					let snapshot = source.storage.read(tx, key).await?;
					let observed_at = snapshot.observed_at();
					match snapshot.into_origin() {
						LeaseSourceOrigin::Absent => Ok(None),
						LeaseSourceOrigin::Source(rows) => {
							let imported = bridge
								.transitions()
								.import(key, rows)
								.map_err(LeaseSourceStorageError::Import)?;
							Ok(match imported {
								LeaseSourceImport::Absent => None,
								LeaseSourceImport::Present { state, .. } => {
									contract.transitions().live_holder(key, &state, observed_at)
								}
							})
						}
						LeaseSourceOrigin::Native(record) => {
							if route == InstallationStorageRoute::SourceCoexistence {
								return Err(NativeLeaseError::NativePlacementDuringCoexistence);
							}
							schema
								.decode_live_holder(&PointRecordRead::new(
									Some(record),
									observed_at,
								))
								.map_err(NativeLeaseError::Read)
						}
					}
				}
			}
		}
		.await;
		result.map_err(NativeOperationError::Operation)
	}
}
