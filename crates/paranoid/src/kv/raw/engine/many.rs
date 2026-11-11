//! One admitted interval observes or replaces a complete raw key set.

mod write;
pub(super) use write::{write_core, write_scope};

use std::sync::Arc;

use crate::db::{Database, DatabaseError, run_transaction_read};
use crate::kv::raw::postgres::many::{
	RawManyStorageError, RawPhysicalManyRequest, SourceRawPhysicalManyRequest,
};
use crate::records::RecordIdentity;
use crate::records::postgres::PgPointStorage;
use crate::runtime::admission::bindings::InstallationStorageRoute;
use crate::runtime::admission::{NativeOperationError, PgNativeAdmission};
use crate::scope::{RawRole, RawScopeDeclaration};

use super::error::{NativeRawError, RawOperationFailure};
use super::store::{NativeRawKvBindings, RawSourceRouteError};
use crate::kv::raw::model::many::{RawManyReadRequest, RawManyReadResult, RawManySourceKeyError};

pub(in crate::kv) type NativeRawManyError = NativeRawError<RawManyFailure>;

#[derive(Debug, thiserror::Error)]
pub(in crate::kv) enum RawManyFailure {
	#[error("raw multi-record physical observation failed")]
	Storage(#[from] RawManyStorageError),
	#[error("raw multi-record physical write failed")]
	Write(#[from] crate::records::postgres::record_many::write::RecordManyWriteError),
	#[error("raw multi-record source key is invalid")]
	SourceKey {
		identity: RecordIdentity,
		#[source]
		source: Box<RawManySourceKeyError>,
	},
	#[error("raw multi-record v10 route failed")]
	Route(#[from] RawSourceRouteError),
}
impl RawOperationFailure for RawManyFailure {
	const OPERATION: &'static str = "raw multi-record";
	fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Storage(error) => error.database_error(),
			Self::Write(error) => error.database_error(),
			Self::Route(error) => error.database_error(),
			Self::SourceKey { .. } => None,
		}
	}
	fn involves_database_failure(&self) -> bool {
		match self {
			Self::Storage(error) => error.database_error().is_some(),
			Self::Write(error) => error.database_error().is_some(),
			Self::Route(error) => error.involves_database_failure(),
			Self::SourceKey { .. } => false,
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Storage(error) => error
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
			Self::Write(error) => error.commit_outcome_is_unknown(),
			Self::Route(error) => error.commit_outcome_is_unknown(),
			Self::SourceKey { .. } => false,
		}
	}
	fn record_identity(&self) -> Option<&RecordIdentity> {
		match self {
			Self::Storage(error) => error.identity(),
			Self::Write(error) => error.identity(),
			Self::SourceKey { identity, .. } => Some(identity),
			Self::Route(_) => None,
		}
	}
	fn input_index(&self) -> Option<usize> {
		match self {
			Self::Storage(error) => error.input_index(),
			Self::Write(error) => error.input_index(),
			Self::SourceKey { source, .. } => Some(source.input_index()),
			Self::Route(_) => None,
		}
	}
}

pub(super) async fn read_core(
	bindings: &Arc<NativeRawKvBindings>,
	database: impl Database,
	request: &RawManyReadRequest,
) -> Result<RawManyReadResult, NativeRawManyError> {
	if request.is_empty() {
		return Ok(RawManyReadResult::Empty);
	}
	run_transaction_read(database, "kv.native_store.get_many", async |tx| {
		bindings
			.admission
			.execute_read_with_contracts(tx, async |contracts, tx| {
				let declaration = bindings.component.declaration();
				let _bound = contracts
					.bind(&declaration)
					.map_err(NativeOperationError::Admission)?;
				let result: Result<_, RawManyFailure> = async {
					let observation = match contracts.storage_route() {
						InstallationStorageRoute::NativeFresh
						| InstallationStorageRoute::NativeImported => {
							bindings
								.storage
								.read_raw_many(
									tx,
									RawPhysicalManyRequest {
										namespace: &bindings.namespace,
										request,
									},
								)
								.await?
						}
						route @ (InstallationStorageRoute::SourceCoexistence
						| InstallationStorageRoute::SourceRelocation) => {
							let source = bindings.source_storage()?;
							let keys = request.source_keys().map_err(|error| {
								RawManyFailure::SourceKey {
									identity: RecordIdentity::new(
										bindings.namespace.clone(),
										error.key().clone(),
									),
									source: Box::new(error),
								}
							})?;
							let fields = SourceRawPhysicalManyRequest {
								namespace: &bindings.namespace,
								keys: &keys,
							};
							if route == InstallationStorageRoute::SourceCoexistence {
								source.source().read_raw_many(tx, fields).await?
							} else {
								source.relocation().read_raw_many(tx, fields).await?
							}
						}
					};
					Ok(observation)
				}
				.await;
				result.map_err(NativeOperationError::Operation)
			})
			.await
			.map_err(NativeRawManyError::from_operation)
	})
	.await
}

pub(super) struct NativeRawScopeManyFields<'a> {
	pub(super) declaration: &'a RawScopeDeclaration,
	pub(super) storage: &'a PgPointStorage,
	pub(super) admission: &'a PgNativeAdmission,
}
pub(super) async fn read_scope(
	fields: NativeRawScopeManyFields<'_>,
	database: impl Database,
	request: &RawManyReadRequest,
) -> Result<RawManyReadResult, NativeRawManyError> {
	if request.is_empty() {
		return Ok(RawManyReadResult::Empty);
	}
	run_transaction_read(database, "kv.native_scope.get_many", async |tx| {
		fields
			.admission
			.execute_read_with_contracts(tx, async |contracts, tx| {
				let declaration = fields.declaration.declaration();
				let _bound = contracts
					.bind_raw_scope(fields.declaration, &declaration, RawRole::Reader)
					.map_err(NativeOperationError::Admission)?;
				let observation = fields
					.storage
					.read_raw_many(
						tx,
						RawPhysicalManyRequest {
							namespace: fields.declaration.identity().namespace(),
							request,
						},
					)
					.await
					.map_err(RawManyFailure::Storage)
					.map_err(NativeOperationError::Operation)?;
				Ok(observation)
			})
			.await
			.map_err(NativeRawManyError::from_operation)
	})
	.await
}
