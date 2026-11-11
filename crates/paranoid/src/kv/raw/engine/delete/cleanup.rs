//! Each paced iteration ends its own actual pool transaction before any delay.
use super::super::scope::PgNativeRawScope;
use super::super::store::PgNativeRawKvStore;
use crate::db::WritePool;
use crate::kv::raw::model::cleanup::{
	RawExpiredCleanupRunError, RawExpiredCleanupRunRequest, RawExpiredCleanupRunResult,
	run::{ExpiredCleanupRunFields, run_expired_cleanup},
};

enum CleanupHandle<'a, 'scope> {
	Core(&'a PgNativeRawKvStore),
	Scope(&'a PgNativeRawScope<'scope>),
}
pub(in crate::kv) async fn cleanup_core(
	store: &PgNativeRawKvStore,
	pool: &WritePool,
	request: &RawExpiredCleanupRunRequest,
) -> Result<RawExpiredCleanupRunResult, RawExpiredCleanupRunError> {
	run(CleanupHandle::Core(store), pool, request).await
}
pub(in crate::kv) async fn cleanup_scope(
	scope: &PgNativeRawScope<'_>,
	pool: &WritePool,
	request: &RawExpiredCleanupRunRequest,
) -> Result<RawExpiredCleanupRunResult, RawExpiredCleanupRunError> {
	run(CleanupHandle::Scope(scope), pool, request).await
}
async fn run(
	handle: CleanupHandle<'_, '_>,
	pool: &WritePool,
	request: &RawExpiredCleanupRunRequest,
) -> Result<RawExpiredCleanupRunResult, RawExpiredCleanupRunError> {
	let handle = &handle;
	run_expired_cleanup(ExpiredCleanupRunFields {
		request,
		delete_committed_batch_fn: move |batch| async move {
			match handle {
				CleanupHandle::Core(store) => store.delete_batch(pool, batch).await,
				CleanupHandle::Scope(scope) => scope.delete_batch(pool, batch).await,
			}
		},
	})
	.await
}
