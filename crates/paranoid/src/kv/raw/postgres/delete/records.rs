//! One requested identity set retains actual physical status and reservation ownership.
use super::super::v10::source_point::{SourcePhysicalHeaderFields, decode_source_header};
use super::super::{NativePhysicalHeaderFields, ReservationOwnership, decode_physical_header};
use super::error::RawDeleteRecordErrorFields;
use super::{RawDeleteLayout, RawDeleteStorageError, RecordDeletionStep};
use crate::records::{RecordIdentity, RecordKey, RecordMetadata, RecordNamespace};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub(in crate::kv) enum PhysicalStatus {
	#[default]
	Unobserved,
	Absent,
	Unheld,
	/// A held reservation row, which this acquisition found or inserted.
	Reservation(ReservationOwnership),
	Value(RecordMetadata),
	Deleted,
}
impl PhysicalStatus {
	pub(in crate::kv) fn is_present(&self) -> bool {
		matches!(self, Self::Unheld | Self::Reservation(_) | Self::Value(_))
	}
	pub(in crate::kv) fn is_held(&self) -> bool {
		matches!(self, Self::Reservation(_) | Self::Value(_) | Self::Deleted)
	}
	pub(in crate::kv) fn is_inserted_reservation(&self) -> bool {
		matches!(self, Self::Reservation(ReservationOwnership::Inserted))
	}
	pub(in crate::kv) fn value(&self) -> Option<RecordMetadata> {
		match self {
			Self::Value(metadata) => Some(*metadata),
			_ => None,
		}
	}
}
pub(in crate::kv) struct Candidate {
	pub(in crate::kv) key: RecordKey,
	pub(in crate::kv) native: PhysicalStatus,
	pub(in crate::kv) source: PhysicalStatus,
}
pub(in crate::kv) struct RawDeleteRecords {
	pub(in crate::kv) namespace: RecordNamespace,
	pub(in crate::kv) candidates: Vec<Candidate>,
	pub(in crate::kv) positions: BTreeMap<RecordKey, usize>,
}
// Body presence distinguishes held values from reservations and unheld rows.
pub(in crate::kv) type HeaderRow = (
	i64,
	Option<super::Bytea>,
	Option<i64>,
	Option<bool>,
	Option<bool>,
	Option<super::Bytea>,
	Option<bool>,
	Option<i64>,
	Option<i64>,
	Option<String>,
	Option<bool>,
	Option<bool>,
	Option<i64>,
);
pub(in crate::kv) fn is_empty_frame(row: &HeaderRow) -> bool {
	row.3.is_none()
		&& row.4.is_none()
		&& row.5.is_none()
		&& row.6.is_none()
		&& row.7.is_none()
		&& row.8.is_none()
		&& row.9.is_none()
		&& row.10.is_none()
		&& row.11.is_none()
		&& row.12.is_none()
}
impl RawDeleteRecords {
	pub(in crate::kv) fn new(namespace: RecordNamespace) -> Self {
		Self {
			namespace,
			candidates: Vec::new(),
			positions: BTreeMap::new(),
		}
	}
	/// The candidates for which this acquisition inserted and still holds a native reservation.
	pub(in crate::kv) fn inserted_reservations(&self) -> BTreeSet<usize> {
		self.candidates
			.iter()
			.enumerate()
			.filter(|(_, candidate)| candidate.native.is_inserted_reservation())
			.map(|(index, _)| index)
			.collect()
	}
	pub(in crate::kv) fn push(&mut self, key: RecordKey) -> Result<(), RawDeleteStorageError> {
		let index = self.candidates.len();
		i64::try_from(index).map_err(|_| {
			self.identify_unexpected(
				RawDeleteStorageError::Shape,
				key.clone(),
				RecordDeletionStep::Discovery,
			)
		})?;
		if self.positions.insert(key.clone(), index).is_some() {
			return Err(self.identify_unexpected(
				RawDeleteStorageError::Coverage,
				key,
				RecordDeletionStep::Discovery,
			));
		}
		self.candidates.push(Candidate {
			key,
			native: PhysicalStatus::default(),
			source: PhysicalStatus::default(),
		});
		Ok(())
	}
	pub(in crate::kv) fn identify_unexpected(
		&self,
		source: RawDeleteStorageError,
		key: RecordKey,
		step: RecordDeletionStep,
	) -> RawDeleteStorageError {
		source.with_identity(RawDeleteRecordErrorFields {
			identity: RecordIdentity::new(self.namespace.clone(), key),
			candidate_rank: None,
			step,
		})
	}
	pub(in crate::kv) fn identify(
		&self,
		source: RawDeleteStorageError,
		index: usize,
		step: RecordDeletionStep,
	) -> RawDeleteStorageError {
		source.with_identity(RawDeleteRecordErrorFields {
			identity: RecordIdentity::new(
				self.namespace.clone(),
				self.candidates[index].key.clone(),
			),
			candidate_rank: Some(index),
			step,
		})
	}
	pub(in crate::kv) fn parse_status(
		&mut self,
		layout: RawDeleteLayout<'_>,
		row: HeaderRow,
	) -> Result<usize, RawDeleteStorageError> {
		let (
			tag,
			key,
			rank,
			present,
			held,
			namespace,
			body_present,
			expiry,
			updated,
			indexes,
			expires_at_valid,
			updated_at_valid,
			clock,
		) = row;
		let key = RecordKey::new(key.ok_or(RawDeleteStorageError::Shape)?.0);
		let step = if tag == 2 {
			RecordDeletionStep::Native
		} else {
			RecordDeletionStep::Source
		};
		let index = *self.positions.get(&key).ok_or_else(|| {
			self.identify_unexpected(RawDeleteStorageError::Coverage, key.clone(), step)
		})?;
		let result = (|| {
			if rank != Some(i64::try_from(index).map_err(|_| RawDeleteStorageError::Shape)?)
				|| clock.is_some()
			{
				return Err(RawDeleteStorageError::Coverage);
			}
			let present = present.ok_or(RawDeleteStorageError::Shape)?;
			let held = held.ok_or(RawDeleteStorageError::Shape)?;
			if held && !present {
				return Err(RawDeleteStorageError::Shape);
			}
			let identity = RecordIdentity::new(self.namespace.clone(), key);
			if !held
				&& (namespace.is_some()
					|| body_present.is_some()
					|| expiry.is_some()
					|| updated.is_some()
					|| indexes.is_some()
					|| expires_at_valid.is_some()
					|| updated_at_valid.is_some())
			{
				return Err(RawDeleteStorageError::Shape);
			}
			let value = match tag {
				2 => {
					if layout.native_table().is_none()
						|| expires_at_valid.is_some()
						|| updated_at_valid.is_some()
					{
						return Err(RawDeleteStorageError::Shape);
					}
					if held {
						if namespace.as_ref().map(|n| n.0.as_slice())
							!= Some(self.namespace.as_bytes())
						{
							return Err(RawDeleteStorageError::Shape);
						}
						decode_physical_header(NativePhysicalHeaderFields {
							identity: &identity,
							body_present: body_present.ok_or(RawDeleteStorageError::Shape)?,
							expires_at: expiry,
							updated_at: updated,
							index_fields: &indexes.ok_or(RawDeleteStorageError::Shape)?,
						})?
						.map(|v| v.metadata)
					} else {
						None
					}
				}
				3 => {
					if layout.source_table().is_none() || namespace.is_some() || indexes.is_some() {
						return Err(RawDeleteStorageError::Shape);
					}
					if matches!(layout, RawDeleteLayout::Relocating(_)) {
						if !self.candidates[index].native.is_held() {
							return Err(RawDeleteStorageError::Shape);
						}
						if present && self.candidates[index].native.value().is_some() {
							return Err(RawDeleteStorageError::DuplicatePhysicalValues);
						}
					}
					if held {
						Some(decode_source_header(SourcePhysicalHeaderFields {
							body_present: body_present.ok_or(RawDeleteStorageError::Shape)?,
							expires_at: expiry,
							updated_at: updated,
							expires_at_valid: expires_at_valid
								.ok_or(RawDeleteStorageError::Shape)?,
							updated_at_valid: updated_at_valid
								.ok_or(RawDeleteStorageError::Shape)?,
						})?)
					} else {
						None
					}
				}
				_ => return Err(RawDeleteStorageError::Coverage),
			};
			Ok(match (present, held, value) {
				(false, false, None) => PhysicalStatus::Absent,
				(true, false, None) => PhysicalStatus::Unheld,
				(true, true, None) => {
					PhysicalStatus::Reservation(ReservationOwnership::Preexisting)
				}
				(true, true, Some(metadata)) => PhysicalStatus::Value(metadata),
				_ => return Err(RawDeleteStorageError::Shape),
			})
		})();
		let status = result.map_err(|e: RawDeleteStorageError| self.identify(e, index, step))?;
		if tag == 2 {
			self.candidates[index].native = status;
		} else {
			self.candidates[index].source = status;
		}
		Ok(index)
	}
}
