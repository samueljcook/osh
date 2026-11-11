//! One consuming native typed-slot boundary retains exact reservations and actual clock.

mod apply;
mod error;
mod selection;

pub(crate) use error::RecordSlotError;

use std::collections::BTreeSet;

use crate::kv::raw::model::slot::RawSlotCandidates;
use crate::records::{
	EncodedRecord, RecordIdentity, RecordKey, RecordMetadata, RecordNamespace, StateTimestampMicros,
};

use crate::db::{StateMutationCompletion, WriteTx};
use crate::records::postgres::{PgPointStorage, ReservationOwnership};

pub(crate) struct RecordSlotAcquisitionFields<'a, 'tx> {
	pub(crate) storage: &'a PgPointStorage,
	pub(crate) tx: &'a mut WriteTx<'tx>,
	pub(crate) namespace: &'a RecordNamespace,
	pub(crate) candidates: &'a RawSlotCandidates,
}
struct SlotOwner<'a, 'tx> {
	storage: &'a PgPointStorage,
	tx: &'a mut WriteTx<'tx>,
	namespace: &'a RecordNamespace,
	candidates: &'a RawSlotCandidates,
	/// The reservation rows that this acquisition inserted, except a selected row.
	inserted: BTreeSet<RecordKey>,
	completion: StateMutationCompletion,
}
/// The row that a slot selection locked.
#[derive(Clone, Copy)]
enum SelectedSlotRow {
	/// A reservation row, which this acquisition found or inserted.
	Reservation(ReservationOwnership),
	/// An expired value.
	Expired,
}
pub(crate) enum PgRecordSlotSelection<'a, 'tx> {
	Unavailable { observed_at: StateTimestampMicros },
	Selected(Box<PgRecordSlotSelected<'a, 'tx>>),
}
pub(crate) struct PgRecordSlotSelected<'a, 'tx> {
	owner: SlotOwner<'a, 'tx>,
	identity: RecordIdentity,
	row: SelectedSlotRow,
	input_index: usize,
	observed_at: StateTimestampMicros,
}
impl<'a, 'tx> PgRecordSlotSelection<'a, 'tx> {
	pub(crate) async fn acquire(
		fields: RecordSlotAcquisitionFields<'a, 'tx>,
	) -> Result<Self, RecordSlotError> {
		if fields.candidates.keys().is_empty() {
			return Err(RecordSlotError::Shape);
		}
		let completion = fields.tx.track_state_mutation();
		let mut owner = SlotOwner {
			storage: fields.storage,
			tx: fields.tx,
			namespace: fields.namespace,
			candidates: fields.candidates,
			inserted: BTreeSet::new(),
			completion,
		};
		let mut remaining = owner.candidates.keys().to_vec();
		let mut observation = selection::acquire(&mut owner, &remaining).await?;
		loop {
			let (selected, observed_at) = observation;
			if let Some((key, metadata)) = selected {
				if metadata.is_none_or(|metadata| !metadata.is_live_at(observed_at)) {
					let input_index = owner
						.candidates
						.input_index(&key)
						.ok_or(RecordSlotError::Coverage)?;
					let row = selected_row(&mut owner, &key, metadata)?;
					return Ok(Self::Selected(Box::new(PgRecordSlotSelected {
						identity: RecordIdentity::new(owner.namespace.clone(), key),
						owner,
						row,
						input_index,
						observed_at,
					})));
				}
				remaining.retain(|candidate| candidate != &key);
				if !remaining.is_empty() {
					observation = selection::select(&mut owner, &remaining).await?;
					continue;
				}
			} else if !owner.inserted.is_empty() {
				return Err(RecordSlotError::Coverage);
			}
			apply::keep(&mut owner, None).await?;
			owner.completion.complete();
			return Ok(Self::Unavailable { observed_at });
		}
	}
}
/// Moves a selected key out of the inserted reservations into the selected row's state. A
/// reservation row that this acquisition inserted has no value.
fn selected_row(
	owner: &mut SlotOwner<'_, '_>,
	key: &RecordKey,
	metadata: Option<RecordMetadata>,
) -> Result<SelectedSlotRow, RecordSlotError> {
	let inserted = owner.inserted.remove(key);
	match (metadata, inserted) {
		(None, true) => Ok(SelectedSlotRow::Reservation(ReservationOwnership::Inserted)),
		(None, false) => Ok(SelectedSlotRow::Reservation(
			ReservationOwnership::Preexisting,
		)),
		(Some(_), false) => Ok(SelectedSlotRow::Expired),
		(Some(_), true) => Err(owner.identify_error_by_key(RecordSlotError::Coverage, key)),
	}
}
impl PgRecordSlotSelected<'_, '_> {
	pub(crate) fn identity(&self) -> &RecordIdentity {
		&self.identity
	}
	pub(crate) fn input_index(&self) -> usize {
		self.input_index
	}
	pub(crate) fn observed_at(&self) -> StateTimestampMicros {
		self.observed_at
	}
	pub(crate) async fn apply(
		self: Box<Self>,
		encoded: EncodedRecord,
	) -> Result<(), RecordSlotError> {
		let Self {
			mut owner,
			identity,
			..
		} = *self;
		apply::write(&mut owner, &identity, encoded).await?;
		owner.completion.complete();
		Ok(())
	}
	pub(crate) async fn keep(self: Box<Self>) -> Result<(), RecordSlotError> {
		let Self {
			mut owner,
			identity,
			row,
			..
		} = *self;
		let selected_reservation = matches!(
			row,
			SelectedSlotRow::Reservation(ReservationOwnership::Inserted)
		)
		.then_some(identity.key());
		apply::keep(&mut owner, selected_reservation).await?;
		owner.completion.complete();
		Ok(())
	}
}
impl SlotOwner<'_, '_> {
	fn identify(&self, source: RecordSlotError, identity: RecordIdentity) -> RecordSlotError {
		let input_index = (identity.namespace() == self.namespace)
			.then(|| self.candidates.input_index(identity.key()))
			.flatten();
		source.with_identity(identity, input_index)
	}
	fn identify_error_by_key(&self, source: RecordSlotError, key: &RecordKey) -> RecordSlotError {
		self.identify(
			source,
			RecordIdentity::new(self.namespace.clone(), key.clone()),
		)
	}
}
