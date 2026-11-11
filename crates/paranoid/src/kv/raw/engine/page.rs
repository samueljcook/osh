//! One admitted read interval for core and explicitly scoped raw value and key pages.

use std::future::Future;
use std::sync::Arc;

use crate::db::{Database, DatabaseError, Tx, run_transaction_read};
use crate::kv::raw::postgres::delete::RawDeleteLayout;
use crate::kv::raw::postgres::key_headers::KeyHeaderPage;
use crate::kv::raw::postgres::page::{
	RawPageStorageError, RawPhysicalPage, RawPhysicalPageRequest,
};
use crate::records::RecordIdentity;
use crate::records::postgres::PgPointStorage;
use crate::runtime::admission::{NativeOperationError, PgNativeAdmission};
use crate::scope::{RawRole, RawScopeDeclaration};

use super::error::{NativeRawError, RawOperationFailure};
use super::store::{NativeRawKvBindings, RawSourceRouteError};
use crate::kv::raw::model::page::{
	RawPageContext, RawPageOwner, RawValuePage, RecordKeyPage, RecordKeyPageFields,
	RecordPageRequest, RecordPageRequestError,
};

pub(in crate::kv) type NativeRawPageError = NativeRawError<RawPageFailure>;

#[derive(Debug, thiserror::Error)]
pub(in crate::kv) enum RawPageFailure {
	#[error("raw page request does not match the admitted read")]
	Request(#[from] RecordPageRequestError),
	#[error("raw page v10 route failed")]
	Route(#[from] RawSourceRouteError),
	#[error("raw page physical read failed")]
	Storage(#[from] RawPageStorageError),
}
impl RawOperationFailure for RawPageFailure {
	const OPERATION: &'static str = "raw page";
	fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Route(error) => error.database_error(),
			Self::Storage(error) => error.database_error(),
			Self::Request(_) => None,
		}
	}
	fn involves_database_failure(&self) -> bool {
		match self {
			Self::Route(error) => error.involves_database_failure(),
			Self::Storage(error) => error.database_error().is_some(),
			Self::Request(_) => false,
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Route(error) => error.commit_outcome_is_unknown(),
			Self::Storage(error) => error
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
			Self::Request(_) => false,
		}
	}
	fn record_identity(&self) -> Option<&RecordIdentity> {
		match self {
			Self::Storage(error) => error.identity(),
			Self::Request(_) | Self::Route(_) => None,
		}
	}
	fn input_index(&self) -> Option<usize> {
		None
	}
}

/// What a page holds: the records' values, or only their keys and metadata.
pub(super) trait RawPageProjection {
	type Physical;
	type Page;
	const CORE_OPERATION: &'static str;
	const SCOPE_OPERATION: &'static str;
	fn read<'a>(
		location: RawDeleteLayout<'a>,
		tx: &'a mut Tx<'_>,
		physical: RawPhysicalPageRequest<'a>,
	) -> impl Future<Output = Result<Self::Physical, RawPageStorageError>> + Send;
	fn finish(
		request: &RecordPageRequest,
		context: RawPageContext,
		physical: Self::Physical,
	) -> Self::Page;
}

/// A page of records with their values.
pub(super) struct Values;
impl RawPageProjection for Values {
	type Physical = RawPhysicalPage;
	type Page = RawValuePage;
	const CORE_OPERATION: &'static str = "kv.native_store.read_page";
	const SCOPE_OPERATION: &'static str = "kv.native_scope.read_page";
	async fn read<'a>(
		location: RawDeleteLayout<'a>,
		tx: &'a mut Tx<'_>,
		physical: RawPhysicalPageRequest<'a>,
	) -> Result<RawPhysicalPage, RawPageStorageError> {
		match location {
			RawDeleteLayout::Native(storage) => storage.read_raw_page(tx, physical).await,
			RawDeleteLayout::Source(storage) => storage.read_raw_page(tx, physical).await,
			RawDeleteLayout::Relocating(storage) => storage.read_raw_page(tx, physical).await,
		}
	}
	fn finish(
		request: &RecordPageRequest,
		context: RawPageContext,
		physical: RawPhysicalPage,
	) -> RawValuePage {
		request.finish(context, physical.entries, physical.observed_at)
	}
}

/// A page of keys and metadata without values.
pub(super) struct Keys;
impl RawPageProjection for Keys {
	type Physical = KeyHeaderPage;
	type Page = RecordKeyPage;
	const CORE_OPERATION: &'static str = "kv.native_store.read_keys";
	const SCOPE_OPERATION: &'static str = "kv.native_scope.read_keys";
	async fn read<'a>(
		location: RawDeleteLayout<'a>,
		tx: &'a mut Tx<'_>,
		physical: RawPhysicalPageRequest<'a>,
	) -> Result<KeyHeaderPage, RawPageStorageError> {
		match location {
			RawDeleteLayout::Native(storage) => storage.read_raw_key_page(tx, physical).await,
			RawDeleteLayout::Source(storage) => storage.read_raw_key_page(tx, physical).await,
			RawDeleteLayout::Relocating(storage) => storage.read_raw_key_page(tx, physical).await,
		}
	}
	fn finish(
		request: &RecordPageRequest,
		context: RawPageContext,
		physical: KeyHeaderPage,
	) -> RecordKeyPage {
		RecordKeyPage::from_headers(RecordKeyPageFields {
			page: physical,
			request,
			context,
		})
	}
}

/// Checks the cursor's context before the transaction. While the namespace is still in v10
/// storage, the request must also be a valid v10 key range.
pub(super) async fn read_core<P: RawPageProjection>(
	bindings: &Arc<NativeRawKvBindings>,
	database: impl Database,
	request: &RecordPageRequest,
) -> Result<P::Page, NativeRawPageError> {
	let context = RawPageContext {
		installation: Arc::clone(bindings.admission.installation_binding()),
		owner: RawPageOwner::Core,
		table: bindings.storage.table().clone(),
		namespace: bindings.namespace.clone(),
	};
	request
		.require_context(&context)
		.map_err(|error| NativeRawError::Operation(error.into()))?;
	run_transaction_read(database, P::CORE_OPERATION, async |tx| {
		bindings
			.admission
			.execute_read_with_contracts(tx, async |contracts, tx| {
				let declaration = bindings.component.declaration();
				let _bound = contracts
					.bind(&declaration)
					.map_err(NativeOperationError::Admission)?;
				let result: Result<_, RawPageFailure> = async {
					let location = super::delete::core_placement(bindings, &contracts)?;
					if !matches!(location, RawDeleteLayout::Native(_)) {
						request.require_source_domain()?;
					}
					let physical = RawPhysicalPageRequest {
						namespace: &bindings.namespace,
						page: request,
					};
					let physical = P::read(location, tx, physical).await?;
					Ok(P::finish(request, context, physical))
				}
				.await;
				result.map_err(NativeOperationError::Operation)
			})
			.await
			.map_err(NativeRawPageError::from_operation)
	})
	.await
}

pub(super) struct NativeRawScopePageFields<'a> {
	pub(super) declaration: &'a RawScopeDeclaration,
	pub(super) storage: &'a PgPointStorage,
	pub(super) admission: &'a PgNativeAdmission,
}
pub(super) async fn read_scope<P: RawPageProjection>(
	fields: NativeRawScopePageFields<'_>,
	database: impl Database,
	request: &RecordPageRequest,
) -> Result<P::Page, NativeRawPageError> {
	let context = RawPageContext {
		installation: Arc::clone(fields.admission.installation_binding()),
		owner: RawPageOwner::Scope(fields.declaration.identity().clone()),
		table: fields.storage.table().clone(),
		namespace: fields.declaration.identity().namespace().clone(),
	};
	request
		.require_context(&context)
		.map_err(|error| NativeRawError::Operation(error.into()))?;
	run_transaction_read(database, P::SCOPE_OPERATION, async |tx| {
		fields
			.admission
			.execute_read_with_contracts(tx, async |contracts, tx| {
				let declaration = fields.declaration.declaration();
				let _bound = contracts
					.bind_raw_scope(fields.declaration, &declaration, RawRole::Reader)
					.map_err(NativeOperationError::Admission)?;
				let physical = RawPhysicalPageRequest {
					namespace: fields.declaration.identity().namespace(),
					page: request,
				};
				let physical = P::read(RawDeleteLayout::Native(fields.storage), tx, physical)
					.await
					.map_err(|error| NativeOperationError::Operation(error.into()))?;
				Ok(P::finish(request, context, physical))
			})
			.await
			.map_err(NativeRawPageError::from_operation)
	})
	.await
}
