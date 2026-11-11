//! Selection prepares every physical command before its first write.
use super::{PgRawDeleteBatch, RawDeleteStorageError, RecordDeletionStep, cmds};
use crate::kv::raw::model::delete::{
	RecordDeleteBatchOutcomeFields, RecordDeleteBatchResult, RecordDeleteKind,
	RecordDeleteRetained, RecordDeleteRetainedReason,
};
use crate::records::{RecordKey, StateTimestampMicros};
use std::collections::BTreeSet;
pub(super) struct Selection {
	pub(super) deleted: Vec<RecordKey>,
	pub(super) examined: usize,
	pub(super) retained: Vec<RecordDeleteRetained>,
	pub(super) more_candidates: bool,
}
impl Selection {
	pub(super) fn into_outcome(self, observed_at: StateTimestampMicros) -> RecordDeleteBatchResult {
		RecordDeleteBatchResult::new(RecordDeleteBatchOutcomeFields {
			observed_at,
			deleted: self.deleted,
			examined: self.examined,
			retained: self.retained,
			more_candidates: self.more_candidates,
		})
	}
}
pub(super) struct Prepared {
	cmds: cmds::PreparedCmds,
	pub(super) selection: Selection,
}
pub(super) fn prepare<Clock>(
	batch: &PgRawDeleteBatch<'_, '_, Clock>,
	observed_at: Option<StateTimestampMicros>,
) -> Result<Prepared, RawDeleteStorageError> {
	let observed_at = match batch.request.kind() {
		RecordDeleteKind::Expired => Some(observed_at.ok_or(RawDeleteStorageError::Shape)?),
		RecordDeleteKind::AllValued { .. } => None,
	};
	let mut retained = Vec::new();
	let mut eligible = Vec::new();
	for (index, candidate) in batch.records.candidates.iter().enumerate() {
		let native = &candidate.native;
		let source = &candidate.source;
		let needs_native = batch.layout.native_table().is_some();
		let needs_source = batch.layout.source_table().is_some();
		let refusal = if needs_native && !native.is_held() {
			Some(if native.is_present() {
				RecordDeleteRetainedReason::NativeUnheld
			} else {
				RecordDeleteRetainedReason::ChangedOrGone
			})
		} else if needs_source && source.is_present() && !source.is_held() {
			Some(RecordDeleteRetainedReason::SourceUnheld)
		} else {
			None
		};
		if let Some(reason) = refusal {
			retained.push(RecordDeleteRetained::new(candidate.key.clone(), reason));
			continue;
		}
		let value = match (native.value(), source.value()) {
			(Some(_), Some(_)) => {
				return Err(batch.records.identify(
					RawDeleteStorageError::DuplicatePhysicalValues,
					index,
					RecordDeletionStep::Source,
				));
			}
			(Some(metadata), None) => Some((metadata, false)),
			(None, Some(metadata)) => Some((metadata, true)),
			(None, None) => None,
		};
		let Some((metadata, source_role)) = value else {
			retained.push(RecordDeleteRetained::new(
				candidate.key.clone(),
				RecordDeleteRetainedReason::ChangedOrGone,
			));
			continue;
		};
		if batch.request.kind() == RecordDeleteKind::Expired
			&& observed_at.is_some_and(|clock| metadata.is_live_at(clock))
		{
			retained.push(RecordDeleteRetained::new(
				candidate.key.clone(),
				RecordDeleteRetainedReason::BecameLive,
			));
			continue;
		}
		eligible.push((
			metadata.expires_at,
			candidate.key.clone(),
			index,
			source_role,
		));
	}
	eligible.sort_by(|a, b| {
		if batch.request.kind() == RecordDeleteKind::Expired {
			a.0.cmp(&b.0).then(a.1.cmp(&b.1))
		} else {
			a.1.cmp(&b.1)
		}
	});
	let mut native = BTreeSet::new();
	let mut source = BTreeSet::new();
	let mut deleted = Vec::new();
	for (_, key, index, source_role) in eligible {
		if deleted.len() == batch.request.limits().max_deletions().get() as usize {
			retained.push(RecordDeleteRetained::new(
				key,
				RecordDeleteRetainedReason::MaxDeletionsReached,
			));
		} else {
			deleted.push(key);
			if source_role {
				source.insert(index);
			} else {
				native.insert(index);
			}
		}
	}
	let prepared = cmds::prepare(cmds::CmdFields {
		records: &batch.records,
		layout: batch.layout,
		native,
		source,
		cleanup: batch.records.inserted_reservations(),
	})?;
	Ok(Prepared {
		cmds: prepared,
		selection: Selection {
			deleted,
			examined: batch.records.candidates.len(),
			retained,
			more_candidates: batch.more,
		},
	})
}
pub(super) async fn apply<Clock>(
	batch: &mut PgRawDeleteBatch<'_, '_, Clock>,
	prepared: Prepared,
) -> Result<Selection, RawDeleteStorageError> {
	cmds::apply(batch.tx, &batch.records, prepared.cmds).await?;
	Ok(prepared.selection)
}
