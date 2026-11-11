//! Raw pages and live counts.
use super::{PgRaw, PgRawError, RawHandleOwner, error::RawOperationCause};
use crate::db::Database;
use crate::kv::{
	raw::model::aggregate::RecordLiveCount,
	raw::model::page::{RawValuePage, RecordKeyPage, RecordKeySelection, RecordPageRequest},
};
impl PgRaw<'_> {
	/// Reads one page of records with their values, in key order, in one statement. A cursor
	/// from a previous page continues after that page's last key. A cursor is valid only with a
	/// handle of the runtime that read its page, and only for the same namespace, selection, and
	/// visibility; another runtime rejects it, also when it opened the same installation.
	/// Records can change between pages.
	pub async fn read_page(
		&self,
		database: impl Database,
		request: &RecordPageRequest,
	) -> Result<RawValuePage, PgRawError> {
		let result = match &self.owner {
			RawHandleOwner::Core(store) => store.read_page(database, request).await,
			RawHandleOwner::Scope(store) => store.read_page(database, request).await,
		};
		result.map_err(|error| self.selection_error(RawOperationCause::Page(error)))
	}
	/// Reads one page of keys and their metadata, without values, in key order, in one
	/// statement. A page of the core namespace leaves out the keys that document scopes own.
	/// While the namespace is still in v10 storage, the prefix and the cursor key must be
	/// valid v10 keys. A cursor from a key page or a value page continues either kind of page.
	/// Records can change between pages.
	pub async fn read_keys(
		&self,
		database: impl Database,
		request: &RecordPageRequest,
	) -> Result<RecordKeyPage, PgRawError> {
		let result = match &self.owner {
			RawHandleOwner::Core(store) => store.read_keys(database, request).await,
			RawHandleOwner::Scope(store) => store.read_keys(database, request).await,
		};
		result.map_err(|error| self.selection_error(RawOperationCause::Page(error)))
	}
	/// Counts the live records of a selection in one statement.
	pub async fn count_live(
		&self,
		database: impl Database,
		selection: &RecordKeySelection,
	) -> Result<RecordLiveCount, PgRawError> {
		let result = match &self.owner {
			RawHandleOwner::Core(store) => store.count_live(database, selection).await,
			RawHandleOwner::Scope(store) => store.count_live(database, selection).await,
		};
		result.map_err(|error| self.selection_error(RawOperationCause::Count(error)))
	}
}
