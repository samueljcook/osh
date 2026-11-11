//! One current admitted snapshot supplies each exact raw live count.

use super::error::{NativeRawError, RawOperationFailure};
use super::store::NativeRawKvBindings;
use super::store::RawSourceRouteError;
use crate::db::{Database, DatabaseError, run_transaction_read};
use crate::kv::raw::model::aggregate::{
	RawAggregateInputError, RecordLiveCount, require_source_selection,
};
use crate::kv::raw::model::page::RecordKeySelection;
use crate::kv::raw::postgres::aggregate::{RawAggregateStorageError, RawCountSelection};
use crate::records::RecordIdentity;
use crate::records::postgres::PgPointStorage;
use crate::runtime::admission::bindings::InstallationStorageRoute;
use crate::runtime::admission::{NativeOperationError, PgNativeAdmission};
use crate::scope::{RawRole, RawScopeDeclaration};
use std::sync::Arc;

pub(in crate::kv) type NativeRawAggregateError = NativeRawError<RawAggregateFailure>;

#[derive(Debug, thiserror::Error)]
pub(in crate::kv) enum RawAggregateFailure {
	#[error("raw aggregate source input is invalid")]
	Input(#[from] RawAggregateInputError),
	#[error("raw aggregate v10 route failed")]
	Route(#[from] RawSourceRouteError),
	#[error("raw aggregate physical observation failed")]
	Storage(#[from] RawAggregateStorageError),
}
impl RawOperationFailure for RawAggregateFailure {
	const OPERATION: &'static str = "raw aggregate";
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
pub(super) async fn count_core(
	bindings: &Arc<NativeRawKvBindings>,
	database: impl Database,
	selection: &RecordKeySelection,
) -> Result<RecordLiveCount, NativeRawAggregateError> {
	run_transaction_read(database, "kv.native_store.count_live", async |tx| {
		bindings
			.admission
			.execute_read_with_contracts(tx, async |contracts, tx| {
				let declaration = bindings.component.declaration();
				let _bound = contracts
					.bind(&declaration)
					.map_err(NativeOperationError::Admission)?;
				let result: Result<_, RawAggregateFailure> = async {
					let fields = RawCountSelection {
						namespace: &bindings.namespace,
						selection,
					};
					let result = match contracts.storage_route() {
						InstallationStorageRoute::NativeFresh
						| InstallationStorageRoute::NativeImported => {
							bindings.storage.count_raw_live(tx, fields).await?
						}
						route @ (InstallationStorageRoute::SourceCoexistence
						| InstallationStorageRoute::SourceRelocation) => {
							let source = bindings.source_storage()?;
							require_source_selection(selection)?;
							if route == InstallationStorageRoute::SourceCoexistence {
								source.source().count_raw_live(tx, fields).await?
							} else {
								source.relocation().count_raw_live(tx, fields).await?
							}
						}
					};
					Ok(result)
				}
				.await;
				result.map_err(NativeOperationError::Operation)
			})
			.await
			.map_err(NativeRawAggregateError::from_operation)
	})
	.await
}
pub(super) struct NativeRawScopeAggregateFields<'a> {
	pub(super) declaration: &'a RawScopeDeclaration,
	pub(super) storage: &'a PgPointStorage,
	pub(super) admission: &'a PgNativeAdmission,
}
pub(super) async fn count_scope(
	fields: NativeRawScopeAggregateFields<'_>,
	database: impl Database,
	selection: &RecordKeySelection,
) -> Result<RecordLiveCount, NativeRawAggregateError> {
	run_transaction_read(database, "kv.native_scope.count_live", async |tx| {
		fields
			.admission
			.execute_read_with_contracts(tx, async |contracts, tx| {
				let declaration = fields.declaration.declaration();
				let _bound = contracts
					.bind_raw_scope(fields.declaration, &declaration, RawRole::Reader)
					.map_err(NativeOperationError::Admission)?;
				fields
					.storage
					.count_raw_live(
						tx,
						RawCountSelection {
							namespace: fields.declaration.identity().namespace(),
							selection,
						},
					)
					.await
					.map_err(RawAggregateFailure::Storage)
					.map_err(NativeOperationError::Operation)
			})
			.await
			.map_err(NativeRawAggregateError::from_operation)
	})
	.await
}
