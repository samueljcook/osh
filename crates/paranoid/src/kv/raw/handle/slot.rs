//! Raw slots: a write under one free key of several candidates.
use super::{PgRaw, PgRawError, RawHandleOwner, error::RawOperationCause};
use crate::db::WriteDatabase;
use crate::kv::{
	raw::model::slot::{RawSlotRequest, RawSlotResult},
	raw::model::slot_preparation::EncryptedRawSlotRequest,
};
impl PgRaw<'_> {
	/// Writes the value under one candidate key that holds no live record and that no other
	/// transaction has locked. When every candidate is live or locked, the result is
	/// `Unavailable` and Paranoid writes nothing. To lock an absent candidate, Paranoid creates
	/// an expired placeholder record for it, and it keeps the placeholders that it does not
	/// select. An empty request returns `Empty` without database work.
	///
	/// A slot is a write and nothing more: holding a slot does not exclude other processes from
	/// any work.
	pub async fn acquire_slot(
		&self,
		database: impl WriteDatabase,
		request: RawSlotRequest,
	) -> Result<RawSlotResult, PgRawError> {
		let result = match &self.owner {
			RawHandleOwner::Core(store) => store.acquire_slot(database, request).await,
			RawHandleOwner::Scope(store) => store.acquire_slot(database, request).await,
		};
		result.map_err(|error| self.selection_error(RawOperationCause::Slot(error)))
	}
	/// Selects a slot like `acquire_slot` and then encrypts the plaintext once, for the
	/// selected key, with the request's associated data. When the preparation fails, the error
	/// has the kind `ExpirationOverflow` if the TTL ends later than the latest representable
	/// time, and the kind `Preparation` otherwise. In both cases,
	/// `PgRawError::rejection_settlement` reports whether the caller's transaction can still
	/// commit. An empty request returns `Empty` without database work or encryption.
	pub async fn acquire_encrypted_slot(
		&self,
		database: impl WriteDatabase,
		request: EncryptedRawSlotRequest<'_>,
	) -> Result<RawSlotResult, PgRawError> {
		let result = match &self.owner {
			RawHandleOwner::Core(store) => store.acquire_encrypted_slot(database, request).await,
			RawHandleOwner::Scope(store) => store.acquire_encrypted_slot(database, request).await,
		};
		result.map_err(|error| self.selection_error(RawOperationCause::Slot(error)))
	}
}
