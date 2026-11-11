//! Current actual Writer admission owns one complete atomic namespace transaction.
use super::{NativeRawScopeDeleteFields, core_layout};
use crate::db::{DatabaseError, WriteDatabase, run_transaction_write};
use crate::kv::raw::engine::error::{NativeRawError, RawDeletionFailure, RawOperationFailure};
use crate::kv::raw::engine::store::{NativeRawKvBindings, RawSourceRouteError};
use crate::kv::raw::model::aggregate::RawAggregateInputError;
use crate::kv::raw::model::namespace::{RecordSelectionDeleteRequest, RecordSelectionDeleteResult};
use crate::kv::raw::postgres::delete::{
	RawDeleteLayout,
	namespace::{PgRawNamespaceDeletion, RawNamespaceDeletionFields, RawNamespaceStorageError},
};
use crate::kv::raw::postgres::slot::RawRejectionSettlement;
use crate::records::RecordIdentity;
use crate::runtime::admission::NativeOperationError;
use crate::scope::RawRole;
use std::sync::Arc;

pub(in crate::kv) type NativeRawSelectionDeleteError = NativeRawError<RawSelectionDeleteFailure>;

#[derive(Debug, thiserror::Error)]
pub(in crate::kv) enum RawSelectionDeleteFailure {
	#[error("raw namespace source selection is invalid")]
	Input(#[from] RawAggregateInputError),
	#[error("raw namespace v10 route failed")]
	Route(#[from] RawSourceRouteError),
	#[error("raw namespace physical operation failed")]
	Storage(#[from] RawNamespaceStorageError),
}
impl RawOperationFailure for RawSelectionDeleteFailure {
	const OPERATION: &'static str = "raw namespace deletion";
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
impl RawDeletionFailure for RawSelectionDeleteFailure {
	/// A failure before the deletion guard leaves no tracking unfinished. The deletion guard
	/// reports whether it finished its tracking after its own failure.
	fn settlement(&self) -> RawRejectionSettlement {
		match self {
			Self::Input(_) | Self::Route(_) => RawRejectionSettlement::KnownRestored,
			Self::Storage(error) => error.settlement(),
		}
	}
}
pub(in crate::kv) async fn delete_selection_core(
	bindings: &Arc<NativeRawKvBindings>,
	database: impl WriteDatabase,
	request: &RecordSelectionDeleteRequest,
) -> Result<RecordSelectionDeleteResult, NativeRawSelectionDeleteError> {
	run_transaction_write(
		database,
		"kv.native_store.delete_selection_atomically",
		async |tx| {
			bindings
				.admission
				.execute_write_with_contracts(tx, async |contracts, tx| {
					let declaration = bindings.component.declaration();
					let _bound = contracts
						.bind(&declaration)
						.map_err(NativeOperationError::Admission)?;
					let result = async {
						let layout = core_layout::<RawSelectionDeleteFailure>(
							bindings,
							&contracts,
							request.selection(),
						)?;
						PgRawNamespaceDeletion::new(RawNamespaceDeletionFields {
							tx,
							layout,
							namespace: &bindings.namespace,
							request,
						})
						.delete()
						.await
						.map_err(RawSelectionDeleteFailure::from)
					}
					.await;
					Ok(result)
				})
				.await
				.map_err(NativeRawSelectionDeleteError::from_operation)?
				.map_err(NativeRawError::Operation)
		},
	)
	.await
}
pub(in crate::kv) async fn delete_selection_scope(
	fields: NativeRawScopeDeleteFields<'_>,
	database: impl WriteDatabase,
	request: &RecordSelectionDeleteRequest,
) -> Result<RecordSelectionDeleteResult, NativeRawSelectionDeleteError> {
	run_transaction_write(
		database,
		"kv.native_scope.delete_selection_atomically",
		async |tx| {
			fields
				.admission
				.execute_write_with_contracts(tx, async |contracts, tx| {
					let declaration = fields.declaration.declaration();
					let _bound = contracts
						.bind_raw_scope(fields.declaration, &declaration, RawRole::Writer)
						.map_err(NativeOperationError::Admission)?;
					Ok(PgRawNamespaceDeletion::new(RawNamespaceDeletionFields {
						tx,
						layout: RawDeleteLayout::Native(fields.storage),
						namespace: fields.declaration.identity().namespace(),
						request,
					})
					.delete()
					.await
					.map_err(RawSelectionDeleteFailure::from))
				})
				.await
				.map_err(NativeRawSelectionDeleteError::from_operation)?
				.map_err(NativeRawError::Operation)
		},
	)
	.await
}
