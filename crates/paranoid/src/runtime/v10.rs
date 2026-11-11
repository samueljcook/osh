//! Phase-aware startup preparation from actual verified installation associations.
//!
//! The source gate and control remain pinned while source adapters are validated. The
//! resulting mappings contain no cached admission decision; each operation observes phase.

mod queue;
use queue::NativeQueueSourcePreparation;
pub(crate) use queue::prepare_recorded_queue_source;

use crate::fleet::counter::engine::NativeCounterSourceStorage;
use crate::fleet::lease::v10::source_store::{LeaseSourceStorageError, PgLeaseSourceStorage};
use crate::fleet::lease::{LeaseSourceBridge, NativeLeaseSourceBindings};
use crate::installation::contract::StateContractError;
use crate::installation::control_state::InstallationControlError;
use crate::kv::raw::engine::store::{NativeRawKvSourceStorage, NativeRawKvSourceStorageFields};
use crate::queue::engine::NativeQueueSourceBindings;
use std::sync::Arc;

use super::admission::bindings::{
	InstallationAdmissionBindingError, InstallationStorageRoute, PgInstallationAdmissionBinding,
};
use crate::db::{DatabaseError, Pool, SchemaError, TransactionFailure, run_transaction_read};
use crate::installation::ProtocolError;
use crate::installation::administration::{NativeAdministrationError, PgNativeAdministration};
use crate::installation::control_storage::ControlPinError;
use crate::installation::v10::PgV10SourceRole;
use crate::records::postgres::v10::relocating_point::{
	PgRelocatingPointStorage, RelocatingPointError,
};

/// The v10 adapters of a runtime's handles. Startup prepares them under the storage route
/// that it observes.
pub(crate) enum RuntimeSourceAdapters {
	/// The installation keeps all its data in Paranoid's own tables, and no handle reads v10
	/// data.
	Native,
	/// The runtime still reads or moves v10 data, and every family has its v10 adapter.
	Source(Box<SourceAdapters>),
}

/// The v10 adapter of each family that reads or moves v10 data.
pub(crate) struct SourceAdapters {
	pub(crate) counter: NativeCounterSourceStorage,
	pub(super) raw_kv: NativeRawKvSourceStorage,
	pub(crate) lease: NativeLeaseSourceBindings,
	pub(crate) queue: Arc<NativeQueueSourceBindings>,
	pub(super) fleet_scalar: PgRelocatingPointStorage,
}

pub(crate) struct NativeSourcePreparationFields<'a> {
	pub(crate) pool: &'a Pool,
	pub(crate) installation: &'a PgInstallationAdmissionBinding,
	pub(crate) administration: &'a PgNativeAdministration,
	pub(crate) lease_bridge: &'a Arc<LeaseSourceBridge>,
	pub(crate) queue: Option<NativeQueueSourcePreparation>,
}

impl RuntimeSourceAdapters {
	pub(crate) async fn prepare(
		fields: NativeSourcePreparationFields<'_>,
	) -> Result<Self, NativeSourcePreparationError> {
		let NativeSourcePreparationFields {
			pool,
			installation,
			administration,
			lease_bridge,
			queue,
		} = fields;
		let native = administration.native_bindings();
		let control_component = administration.control_component();
		run_transaction_read(pool, "db.native_runtime.prepare_storage", async |tx| {
			let source_phase = installation.pin_source(tx).await?;
			native.control().validate_existing(tx).await?;
			let snapshot = native.control().read_pinned_shared(tx).await?;
			let decoded = control_component
				.decode_record(native.control().identity(), &snapshot)?
				.ok_or(NativeSourcePreparationError::ControlAbsent)?;
			let declaration = administration.declaration();
			let policy = decoded
				.state()
				.component(declaration.identifier())
				.ok_or(NativeSourcePreparationError::AdministrativePolicyAbsent)?;
			let bound = declaration.bind(policy)?;
			let bound_control = bound.bind_schema(control_component.schema())?;
			control_component.require_bound_reader(&decoded, &bound_control)?;
			administration.require_bound_policy(&bound)?;
			let bound_backend = bound.bind_schema(administration.backend_schema().schema())?;
			let route =
				installation.verify_control(decoded.state(), source_phase, &bound_backend)?;
			native
				.participants()
				.storage()
				.validate_existing(tx)
				.await?;
			if matches!(
				route,
				InstallationStorageRoute::NativeFresh | InstallationStorageRoute::NativeImported
			) {
				return Ok(Self::Native);
			}
			let PgInstallationAdmissionBinding::Imported { source, .. } = installation else {
				return Err(NativeSourcePreparationError::SourceMappingAbsent);
			};
			let queue = queue
				.map(NativeQueueSourcePreparation::bind)
				.ok_or(NativeSourcePreparationError::QueueSourceMappingAbsent)?;
			installation.verify_queue_source_storage(
				queue.storage.descriptor(),
				queue.storage.native_storage(),
			)?;
			let fleet_scalar_placement = native.fleet_scalar();
			let counter = PgRelocatingPointStorage::validate_existing(
				native.counters().storage().clone(),
				source.table(PgV10SourceRole::FleetState).clone(),
				tx,
			)
			.await?;
			let fleet_scalar = counter
				.rebind_existing_native(fleet_scalar_placement.storage().clone(), tx)
				.await?;
			installation.verify_fleet_scalar_source_pair(&fleet_scalar)?;
			let raw_kv = PgRelocatingPointStorage::validate_existing(
				native.raw_kv().storage().clone(),
				source.table(PgV10SourceRole::RawKv).clone(),
				tx,
			)
			.await?;
			let lease_source = crate::fleet::lease::StoreConfig::new_with_fencing_counter_table(
				source.table(PgV10SourceRole::FleetClaims).clone(),
				source.table(PgV10SourceRole::FleetFencing).clone(),
				source.protocol().clone(),
			);
			let lease = PgLeaseSourceStorage::validate_existing(
				&lease_source,
				native.leases().storage().clone(),
				native.leases().namespace().clone(),
				tx,
			)
			.await?;
			installation.verify_raw_source_storage(&raw_kv, native.raw_kv().namespace())?;
			installation.verify_lease_source_storage(&lease, native.leases().namespace())?;
			Ok(Self::Source(Box::new(SourceAdapters {
				counter: NativeCounterSourceStorage::new(counter),
				raw_kv: NativeRawKvSourceStorage::new(NativeRawKvSourceStorageFields {
					relocation: raw_kv,
				}),
				lease: NativeLeaseSourceBindings {
					storage: lease,
					bridge: Arc::clone(lease_bridge),
				},
				queue,
				fleet_scalar,
			})))
		})
		.await
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeSourcePreparationError {
	#[error("native installation control record is absent")]
	ControlAbsent,
	#[error("native installation mandatory administrative policy is absent")]
	AdministrativePolicyAbsent,
	#[error("native installation source route has no imported association")]
	SourceMappingAbsent,
	#[error("native installation source Queue route has no verified source mapping")]
	QueueSourceMappingAbsent,
	#[error("native installation source Queue mapping differs from its installation")]
	QueuePlacement(#[from] super::admission::queue::NativeQueuePlacementError),
	#[error("native installation source preparation rejected installation mapping or phase")]
	Binding(#[from] InstallationAdmissionBindingError),
	#[error("native installation source preparation could not pin control")]
	ControlPin(#[from] ControlPinError),
	#[error("native installation source preparation rejected control representation")]
	Control(#[from] InstallationControlError),
	#[error("native installation source preparation rejected administrative contract")]
	Contract(#[from] StateContractError),
	#[error("native installation source preparation rejected administration writers")]
	Administration(#[from] NativeAdministrationError),
	#[error("native installation source preparation rejected physical schema")]
	Schema(#[from] SchemaError),
	#[error("native installation source point preparation failed")]
	Point(#[from] RelocatingPointError),
	#[error("native installation source lease preparation failed")]
	Lease(#[from] LeaseSourceStorageError),
	#[error("native installation source preparation database work failed")]
	Database(#[source] DatabaseError),
	#[error("native installation source preparation transaction protocol failed")]
	Protocol(#[source] ProtocolError),
	#[error("native installation source preparation and rollback failed")]
	RollbackFailed {
		#[source]
		operation_error: Box<Self>,
		rollback_error: DatabaseError,
	},
}

impl NativeSourcePreparationError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Binding(error) => error.database_error(),
			Self::ControlPin(error) => error.database_error(),
			Self::Schema(error) => error.database_error(),
			Self::Point(error) => error.database_error(),
			Self::Lease(error) => error.database_error(),
			Self::Database(error) => Some(error),
			Self::RollbackFailed {
				operation_error,
				rollback_error,
			} => operation_error.database_error().or(Some(rollback_error)),
			_ => None,
		}
	}
}

impl TransactionFailure for NativeSourcePreparationError {
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
			Self::Binding(error) => error.involves_database_failure(),
			Self::ControlPin(error) => error.database_error().is_some(),
			Self::Schema(error) => error.involves_database_failure(),
			Self::Point(error) => error.involves_database_failure(),
			Self::Lease(error) => error.involves_database_failure(),
			Self::Database(_) | Self::RollbackFailed { .. } => true,
			_ => false,
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Binding(error) => error.commit_outcome_is_unknown(),
			Self::ControlPin(error) => error
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
			Self::Schema(error) => error.commit_outcome_is_unknown(),
			Self::Point(error) => error.commit_outcome_is_unknown(),
			Self::Lease(error) => error.commit_outcome_is_unknown(),
			Self::Database(error) => error.commit_outcome_is_unknown(),
			Self::RollbackFailed {
				operation_error,
				rollback_error,
			} => {
				operation_error.commit_outcome_is_unknown()
					|| rollback_error.commit_outcome_is_unknown()
			}
			_ => false,
		}
	}
}
