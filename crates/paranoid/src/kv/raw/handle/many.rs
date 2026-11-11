//! Raw operations on several records.
use super::{PgRaw, PgRawError, RawHandleOwner, error::RawOperationCause};
use crate::db::{Database, WriteDatabase};
use crate::kv::raw::model::many::{
	RawManyReadRequest, RawManyReadResult, RawManySetRequest, RawManyWriteResult,
};
impl PgRaw<'_> {
	/// Reads every requested record in one statement and returns one entry per key, in request
	/// order. An empty request returns `Empty` without database work.
	pub async fn get_many(
		&self,
		database: impl Database,
		request: &RawManyReadRequest,
	) -> Result<RawManyReadResult, PgRawError> {
		let result = match &self.owner {
			RawHandleOwner::Core(store) => store.get_many(database, request).await,
			RawHandleOwner::Scope(store) => store.get_many(database, request).await,
		};
		result.map_err(|error| self.selection_error(RawOperationCause::Many(error)))
	}
	/// Replaces the values of every requested record in one operation, with one database time
	/// and one TTL for all of them. Either every record is written or none is. An empty request
	/// returns `Empty` without database work. A write inside the caller's transaction becomes
	/// durable only when the caller commits.
	pub async fn set_many(
		&self,
		database: impl WriteDatabase,
		request: RawManySetRequest,
	) -> Result<RawManyWriteResult, PgRawError> {
		let result = match &self.owner {
			RawHandleOwner::Core(store) => store.set_many(database, request).await,
			RawHandleOwner::Scope(store) => store.set_many(database, request).await,
		};
		result.map_err(|error| self.selection_error(RawOperationCause::Many(error)))
	}
}
