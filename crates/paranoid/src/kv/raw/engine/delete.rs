//! Current admitted Writer access owns one complete bounded raw deletion interval.

mod cleanup;
mod namespace;
use super::error::{NativeRawError, RawDeletionFailure, RawOperationFailure};
use super::store::NativeRawKvBindings;
use super::store::RawSourceRouteError;
use crate::db::{DatabaseError, WriteDatabase, run_transaction_write};
use crate::kv::raw::model::aggregate::{RawAggregateInputError, require_source_selection};
use crate::kv::raw::model::delete::{RecordDeleteBatchRequest, RecordDeleteBatchResult};
use crate::kv::raw::postgres::delete::{
	PgRawDeleteBatch, RawDeleteAcquisitionFields, RawDeleteLayout, RawDeleteStorageError,
};
use crate::kv::raw::postgres::slot::RawRejectionSettlement;
use crate::records::RecordIdentity;
use crate::records::postgres::PgPointStorage;
use crate::runtime::admission::bindings::InstallationStorageRoute;
use crate::runtime::admission::{NativeAdmittedContracts, NativeOperationError, PgNativeAdmission};
use crate::scope::{RawRole, RawScopeDeclaration};
pub(super) use cleanup::{cleanup_core, cleanup_scope};
pub(in crate::kv) use namespace::{NativeRawSelectionDeleteError, RawSelectionDeleteFailure};
pub(super) use namespace::{delete_selection_core, delete_selection_scope};
use std::sync::Arc;

pub(in crate::kv) type NativeRawDeleteError = NativeRawError<RawDeleteFailure>;

#[derive(Debug, thiserror::Error)]
pub(in crate::kv) enum RawDeleteFailure {
	#[error("raw deletion source selection is invalid")]
	Input(#[from] RawAggregateInputError),
	#[error("raw deletion v10 route failed")]
	Route(#[from] RawSourceRouteError),
	#[error("raw deletion physical operation failed")]
	Storage(#[from] RawDeleteStorageError),
}
impl RawOperationFailure for RawDeleteFailure {
	const OPERATION: &'static str = "raw deletion";
	fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Route(e) => e.database_error(),
			Self::Storage(e) => e.database_error(),
			Self::Input(_) => None,
		}
	}
	fn involves_database_failure(&self) -> bool {
		match self {
			Self::Route(e) => e.involves_database_failure(),
			Self::Storage(_) | Self::Input(_) => self.database_error().is_some(),
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Route(e) => e.commit_outcome_is_unknown(),
			Self::Storage(_) | Self::Input(_) => self
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
		}
	}
	fn record_identity(&self) -> Option<&RecordIdentity> {
		match self {
			Self::Storage(e) => e.identity(),
			Self::Input(_) | Self::Route(_) => None,
		}
	}
	fn input_index(&self) -> Option<usize> {
		None
	}
}
impl RawDeletionFailure for RawDeleteFailure {
	/// A failure before the batch guard leaves no tracking unfinished. The batch guard finishes
	/// its tracking only after it deleted its records, so each of its failures requires a
	/// rollback.
	fn settlement(&self) -> RawRejectionSettlement {
		match self {
			Self::Input(_) | Self::Route(_) => RawRejectionSettlement::KnownRestored,
			Self::Storage(_) => RawRejectionSettlement::WholeRollbackRequired,
		}
	}
}
/// The location of the core namespace's records on the admitted route.
pub(in crate::kv) fn core_placement<'a>(
	bindings: &'a NativeRawKvBindings,
	contracts: &NativeAdmittedContracts,
) -> Result<RawDeleteLayout<'a>, RawSourceRouteError> {
	Ok(match contracts.storage_route() {
		InstallationStorageRoute::NativeFresh | InstallationStorageRoute::NativeImported => {
			RawDeleteLayout::Native(&bindings.storage)
		}
		InstallationStorageRoute::SourceCoexistence => {
			RawDeleteLayout::Source(bindings.source_storage()?.source())
		}
		InstallationStorageRoute::SourceRelocation => {
			RawDeleteLayout::Relocating(bindings.source_storage()?.relocation())
		}
	})
}
/// The location of a selection's records on the admitted route. A route that still uses v10
/// storage requires a selection prefix that is a valid v10 key prefix.
fn core_layout<'a, E>(
	bindings: &'a NativeRawKvBindings,
	contracts: &NativeAdmittedContracts,
	selection: &crate::kv::raw::model::page::RecordKeySelection,
) -> Result<RawDeleteLayout<'a>, E>
where
	E: From<RawSourceRouteError> + From<RawAggregateInputError>,
{
	let layout = core_placement(bindings, contracts)?;
	if matches!(
		layout,
		RawDeleteLayout::Source(_) | RawDeleteLayout::Relocating(_)
	) {
		require_source_selection(selection)?;
	}
	Ok(layout)
}
pub(super) async fn delete_core(
	bindings: &Arc<NativeRawKvBindings>,
	database: impl WriteDatabase,
	request: &RecordDeleteBatchRequest,
) -> Result<RecordDeleteBatchResult, NativeRawDeleteError> {
	run_transaction_write(database, "kv.native_store.delete_batch", async |tx| {
		bindings
			.admission
			.execute_write_with_contracts(tx, async |contracts, tx| {
				let declaration = bindings.component.declaration();
				let _bound = contracts
					.bind(&declaration)
					.map_err(NativeOperationError::Admission)?;
				let result = async {
					let layout =
						core_layout::<RawDeleteFailure>(bindings, &contracts, request.selection())?;
					let guard = PgRawDeleteBatch::acquire(RawDeleteAcquisitionFields {
						tx,
						layout,
						namespace: &bindings.namespace,
						request,
					})
					.await?;
					guard.delete().await.map_err(RawDeleteFailure::from)
				}
				.await;
				Ok(result)
			})
			.await
			.map_err(NativeRawDeleteError::from_operation)?
			.map_err(NativeRawError::Operation)
	})
	.await
}
pub(super) struct NativeRawScopeDeleteFields<'a> {
	pub(super) declaration: &'a RawScopeDeclaration,
	pub(super) storage: &'a PgPointStorage,
	pub(super) admission: &'a PgNativeAdmission,
}
pub(super) async fn delete_scope(
	fields: NativeRawScopeDeleteFields<'_>,
	database: impl WriteDatabase,
	request: &RecordDeleteBatchRequest,
) -> Result<RecordDeleteBatchResult, NativeRawDeleteError> {
	run_transaction_write(database, "kv.native_scope.delete_batch", async |tx| {
		fields
			.admission
			.execute_write_with_contracts(tx, async |contracts, tx| {
				let declaration = fields.declaration.declaration();
				let _bound = contracts
					.bind_raw_scope(fields.declaration, &declaration, RawRole::Writer)
					.map_err(NativeOperationError::Admission)?;
				let result = async {
					let guard = PgRawDeleteBatch::acquire(RawDeleteAcquisitionFields {
						tx,
						layout: RawDeleteLayout::Native(fields.storage),
						namespace: fields.declaration.identity().namespace(),
						request,
					})
					.await?;
					guard.delete().await.map_err(RawDeleteFailure::from)
				}
				.await;
				Ok(result)
			})
			.await
			.map_err(NativeRawDeleteError::from_operation)?
			.map_err(NativeRawError::Operation)
	})
	.await
}
