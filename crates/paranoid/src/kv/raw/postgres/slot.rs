//! Fixed opportunistic slots retain neutral history and one consuming selected-key boundary.
mod apply;
mod error;
mod observation;
mod relocation;
mod seed;
mod selection;
mod transaction;
use super::WriteTx;
use super::delete::{RawDeleteLayout, records::RawDeleteRecords};
use crate::kv::Ttl;
use crate::kv::raw::model::slot::{RawSlotCandidates, RawSlotSourceKeys};
use crate::kv::raw::model::slot::{
	RawSlotSeedIntent, RawWritePreparationFields, RawWriteResult, prepare_raw_write,
};
use crate::records::{
	EncodedRecord, RecordIdentity, RecordKey, RecordNamespace, StateTimestampMicros,
};
pub(crate) use error::{RawSlotRetryReasonError, RawSlotStorageError};
use std::collections::BTreeSet;
use std::num::NonZeroU32;
use transaction::SlotMutation;
pub(crate) struct RawSlotAcquisitionFields<'a, 'tx> {
	pub(crate) tx: &'a mut WriteTx<'tx>,
	pub(crate) layout: RawDeleteLayout<'a>,
	pub(crate) namespace: &'a RecordNamespace,
	pub(crate) candidates: &'a RawSlotCandidates,
	pub(crate) source_keys: Option<&'a RawSlotSourceKeys<'a>>,
	pub(crate) ttl: Ttl,
	pub(crate) conflict_attempts: NonZeroU32,
}
struct SlotOwner<'a, 'tx> {
	tx: &'a mut WriteTx<'tx>,
	layout: RawDeleteLayout<'a>,
	namespace: &'a RecordNamespace,
	candidates: &'a RawSlotCandidates,
	source_keys: Option<&'a RawSlotSourceKeys<'a>>,
	ttl: Ttl,
	conflict_attempts: NonZeroU32,
	records: RawDeleteRecords,
	seed: Option<RawSlotSeedIntent>,
	seeded: BTreeSet<RecordKey>,
	mutation: SlotMutation,
}
pub(crate) enum PgRawSlotSelection<'a, 'tx> {
	Unavailable { observed_at: StateTimestampMicros },
	Selected(Box<PgRawSlotSelected<'a, 'tx>>),
}
pub(crate) struct PgRawSlotSelected<'a, 'tx> {
	owner: SlotOwner<'a, 'tx>,
	index: usize,
	observed_at: StateTimestampMicros,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// After a failed slot preparation or a failed Raw deletion, whether the caller's transaction
/// can still commit.
pub enum RawRejectionSettlement {
	/// Paranoid finished tracking the operation and undid every change that the operation made,
	/// so the transaction can continue and commit.
	KnownRestored,
	/// Paranoid did not finish tracking the operation or could not undo its changes, so
	/// Paranoid rejects committing the transaction, and the transaction must roll back.
	WholeRollbackRequired,
}
impl<'a, 'tx> PgRawSlotSelection<'a, 'tx> {
	pub(crate) async fn acquire(
		fields: RawSlotAcquisitionFields<'a, 'tx>,
	) -> Result<Self, RawSlotStorageError> {
		let completion = fields.tx.track_state_mutation();
		let mut owner = SlotOwner {
			tx: fields.tx,
			layout: fields.layout,
			namespace: fields.namespace,
			candidates: fields.candidates,
			source_keys: fields.source_keys,
			ttl: fields.ttl,
			conflict_attempts: fields.conflict_attempts,
			records: RawDeleteRecords::new(fields.namespace.clone()),
			seed: None,
			seeded: BTreeSet::new(),
			mutation: SlotMutation::Direct(completion),
		};
		if owner.candidates.keys().is_empty() || owner.ttl.positive_micros().is_none() {
			return Err(RawSlotStorageError::Shape);
		}
		if let Some(source) = owner.source_keys
			&& (!std::ptr::eq(source.candidates(), owner.candidates)
				|| source.texts().len() != owner.candidates.keys().len())
		{
			return Err(RawSlotStorageError::Shape);
		}
		let opportunity = match owner.layout {
			RawDeleteLayout::Native(_) => {
				async {
					seed::native(&mut owner).await?;
					selection::select(&mut owner).await
				}
				.await
			}
			RawDeleteLayout::Source(_) => {
				async {
					seed::source(&mut owner).await?;
					selection::select(&mut owner).await
				}
				.await
			}
			RawDeleteLayout::Relocating(_) => relocation::select(&mut owner).await,
		};
		match opportunity {
			Ok(selection::Opportunity::Unavailable(observed_at)) => {
				owner.complete_mutation();
				Ok(Self::Unavailable { observed_at })
			}
			Ok(selection::Opportunity::Selected(selected)) => {
				Ok(Self::Selected(Box::new(PgRawSlotSelected {
					owner,
					index: selected.index,
					observed_at: selected.observed_at,
				})))
			}
			Err(error) => {
				let error = owner.identify_error(error);
				if error.known_restored() {
					owner.complete_mutation();
				}
				Err(error)
			}
		}
	}
}
impl PgRawSlotSelected<'_, '_> {
	pub(crate) fn identity(&self) -> RecordIdentity {
		self.owner
			.identity(&self.owner.records.candidates[self.index].key)
	}
	pub(crate) fn observed_at(&self) -> StateTimestampMicros {
		self.observed_at
	}
	/// Consumes an exact opaque body after selection; no application computation runs here.
	pub(crate) async fn write_bytes(
		self,
		body: Vec<u8>,
	) -> Result<RawWriteResult, RawSlotStorageError> {
		let outcome = match prepare_raw_write(RawWritePreparationFields {
			observed_at: self.observed_at,
			ttl: self.owner.ttl,
		}) {
			Ok(outcome) => outcome,
			Err(source) => {
				let key = self.owner.records.candidates[self.index].key.clone();
				let error = self.owner.identify_error_by_key(source.into(), &key);
				return Err(self.owner.reject_error(error).await);
			}
		};
		self.apply(outcome.encode(body)).await?;
		Ok(outcome)
	}
	/// The fixed typed/encrypted owner can prepare actual selected-key bytes before consuming
	/// apply.
	pub(crate) async fn apply(mut self, record: EncodedRecord) -> Result<(), RawSlotStorageError> {
		let key = self.owner.records.candidates[self.index].key.clone();
		let prepared = (|| {
			let expected = prepare_raw_write(RawWritePreparationFields {
				observed_at: self.observed_at,
				ttl: self.owner.ttl,
			})?;
			if record.record().metadata() != expected.metadata() {
				return Err(RawSlotStorageError::Shape);
			}
			apply::prepare_selected(&self.owner, self.index, record)
		})();
		let prepared = match prepared {
			Ok(prepared) => prepared,
			Err(error) => {
				let error = self.owner.identify_error_by_key(error, &key);
				return Err(self.owner.reject_error(error).await);
			}
		};
		if let Err(error) = apply::execute(&mut self.owner, prepared).await {
			return Err(self.owner.identify_error(error));
		}
		self.owner.release().await?;
		self.owner.complete_mutation();
		Ok(())
	}
	pub(crate) async fn reject(mut self) -> Result<RawRejectionSettlement, RawSlotStorageError> {
		match &self.owner.mutation {
			SlotMutation::Direct(_) => {
				return Ok(RawRejectionSettlement::WholeRollbackRequired);
			}
			SlotMutation::Attempt(_) => {}
			SlotMutation::Unresolved | SlotMutation::Finished => {
				unreachable!("a selected slot retains its mutation owner")
			}
		}
		self.owner.restore_attempt().await?;
		self.owner.complete_mutation();
		Ok(RawRejectionSettlement::KnownRestored)
	}
}
impl SlotOwner<'_, '_> {
	fn native_table(&self) -> Result<&crate::db::PgQualifiedTableName, RawSlotStorageError> {
		self.layout.native_table().ok_or(RawSlotStorageError::Shape)
	}
	fn source_table(&self) -> Result<&crate::db::PgQualifiedTableName, RawSlotStorageError> {
		self.layout.source_table().ok_or(RawSlotStorageError::Shape)
	}
	fn source_texts(&self) -> Result<&[String], RawSlotStorageError> {
		self.source_keys
			.map(RawSlotSourceKeys::texts)
			.ok_or(RawSlotStorageError::Shape)
	}
	fn source_text(&self, key: &RecordKey) -> Result<&str, RawSlotStorageError> {
		let index = self
			.candidates
			.keys()
			.binary_search(key)
			.map_err(|_| self.identify_error_by_key(RawSlotStorageError::Coverage, key))?;
		self.source_texts()?
			.get(index)
			.map(String::as_str)
			.ok_or_else(|| self.identify_error_by_key(RawSlotStorageError::Shape, key))
	}
	fn identity(&self, key: &RecordKey) -> RecordIdentity {
		RecordIdentity::new(self.namespace.clone(), key.clone())
	}
	fn identify_error_by_key(
		&self,
		error: RawSlotStorageError,
		key: &RecordKey,
	) -> RawSlotStorageError {
		error.with_identity(self.identity(key), self.candidates.input_index(key))
	}
	fn identify_error(&self, error: RawSlotStorageError) -> RawSlotStorageError {
		if error.input_index().is_some() {
			return error;
		}
		let key = error.identity().map(|identity| identity.key().clone());
		match key {
			Some(key) => self.identify_error_by_key(error, &key),
			None => error,
		}
	}
	async fn reject_error(mut self, primary: RawSlotStorageError) -> RawSlotStorageError {
		match &self.mutation {
			SlotMutation::Direct(_) => return primary,
			SlotMutation::Attempt(_) => {}
			SlotMutation::Unresolved | SlotMutation::Finished => {
				unreachable!("preparation retains its mutation owner")
			}
		}
		if let Err(cleanup) = self.restore_attempt().await {
			return RawSlotStorageError::RejectionCleanup {
				primary_error: Box::new(primary),
				cleanup_error: Box::new(cleanup),
			};
		}
		self.complete_mutation();
		RawSlotStorageError::Restored {
			primary: Box::new(primary),
		}
	}
}
