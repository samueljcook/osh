//! Closed raw deletion authority over one immutable bounded candidate window.
pub(in crate::kv) mod acquire;
mod apply;
pub(super) mod cmds;
mod discovery;
mod error;
pub(crate) mod namespace;
pub(in crate::kv) mod records;
use super::v10::{relocating_point::PgRelocatingPointStorage, source_point::PgSourcePointStorage};
use super::{Bytea, PgPointStorage, StateMutationCompletion, WriteTx};
use crate::kv::raw::model::delete::{RecordDeleteBatchRequest, RecordDeleteBatchResult};
use crate::kv::raw::model::page::RecordKeySelection;
use crate::records::{RecordNamespace, StateTimestampMicros};
pub(crate) use error::RawDeleteStorageError;
pub(crate) use error::RecordDeletionStep;
use records::RawDeleteRecords;

#[derive(Clone, Copy)]
pub(crate) enum RawDeleteLayout<'a> {
	Native(&'a PgPointStorage),
	Source(&'a PgSourcePointStorage),
	Relocating(&'a PgRelocatingPointStorage),
}
impl RawDeleteLayout<'_> {
	pub(in crate::kv) fn native_table(&self) -> Option<&crate::db::PgQualifiedTableName> {
		match self {
			Self::Native(s) => Some(s.table()),
			Self::Relocating(s) => Some(s.native_storage().table()),
			Self::Source(_) => None,
		}
	}
	pub(in crate::kv) fn source_table(&self) -> Option<&crate::db::PgQualifiedTableName> {
		match self {
			Self::Source(s) => Some(s.table()),
			Self::Relocating(s) => Some(s.source_table()),
			Self::Native(_) => None,
		}
	}
}
pub(crate) struct RawDeleteAcquisitionFields<'a, 'tx> {
	pub(crate) tx: &'a mut WriteTx<'tx>,
	pub(crate) layout: RawDeleteLayout<'a>,
	pub(crate) namespace: &'a RecordNamespace,
	pub(crate) request: &'a RecordDeleteBatchRequest,
}
pub(crate) struct PgRawDeleteBatch<'a, 'tx, Clock = StateTimestampMicros> {
	tx: &'a mut WriteTx<'tx>,
	layout: RawDeleteLayout<'a>,
	records: RawDeleteRecords,
	request: &'a RecordDeleteBatchRequest,
	more: bool,
	completion: StateMutationCompletion,
	observed_at: Clock,
}
impl<'a, 'tx> PgRawDeleteBatch<'a, 'tx> {
	pub(crate) async fn acquire(
		fields: RawDeleteAcquisitionFields<'a, 'tx>,
	) -> Result<Self, RawDeleteStorageError> {
		let completion = fields.tx.track_state_mutation();
		let mut batch = PgRawDeleteBatch {
			tx: fields.tx,
			layout: fields.layout,
			records: RawDeleteRecords::new(fields.namespace.clone()),
			request: fields.request,
			more: false,
			completion,
			observed_at: (),
		};
		let mut observed_at = discovery::discover_timed(&mut batch).await?;
		if matches!(batch.layout, RawDeleteLayout::Relocating(_))
			&& !batch.records.candidates.is_empty()
		{
			observed_at = acquire::relocation(&mut batch).await?;
		}
		Ok(batch.with_observation(observed_at))
	}
	pub(crate) async fn delete(mut self) -> Result<RecordDeleteBatchResult, RawDeleteStorageError> {
		let observed_at = self.observed_at;
		let prepared = apply::prepare(&self, Some(observed_at))?;
		let selection = apply::apply(&mut self, prepared).await?;
		self.completion.complete();
		Ok(selection.into_outcome(observed_at))
	}
}
impl<'a, 'tx, Clock> PgRawDeleteBatch<'a, 'tx, Clock> {
	fn physical_selection(&self) -> &'a RecordKeySelection {
		self.request.selection()
	}
	fn with_observation<Observed>(
		self,
		observed_at: Observed,
	) -> PgRawDeleteBatch<'a, 'tx, Observed> {
		PgRawDeleteBatch {
			tx: self.tx,
			layout: self.layout,
			records: self.records,
			request: self.request,
			more: self.more,
			completion: self.completion,
			observed_at,
		}
	}
}
#[derive(Clone, Copy)]
enum Observation {
	Timed,
	UntimedAllValued,
}
impl Observation {
	fn is_timed(self) -> bool {
		matches!(self, Self::Timed)
	}
}
