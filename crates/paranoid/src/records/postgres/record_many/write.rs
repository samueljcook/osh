//! Fixed-key replacement with ordered locks and one completion boundary for the whole set.

use super::super::v10::relocating_point::PgRelocatingPointStorage;
use super::super::v10::source_point::{
	PgSourcePointStorage, SourcePointError, SourceWriteTimestamps,
};
use super::super::{
	DatabaseError, PgPointStorage, ReservationOwnership, StateMutationCompletion,
	StateStorageError, WriteTx,
};
use crate::records::RecordExpirationOverflowError;
use crate::records::{
	EncodedRecord, RecordIdentity, RecordKey, RecordNamespace, StateTimestampMicros,
};
use std::collections::{BTreeMap, BTreeSet};

mod acquire;
pub(crate) mod apply;

pub(crate) enum RecordManyWriteLayout<'a> {
	Native(&'a PgPointStorage),
	Source(&'a PgSourcePointStorage),
	Relocating(&'a PgRelocatingPointStorage),
}
impl RecordManyWriteLayout<'_> {
	fn native_table(&self) -> Option<&crate::db::PgQualifiedTableName> {
		match self {
			Self::Native(s) => Some(s.table()),
			Self::Relocating(s) => Some(s.native_storage().table()),
			Self::Source(_) => None,
		}
	}
	fn source_table(&self) -> Option<&crate::db::PgQualifiedTableName> {
		match self {
			Self::Source(s) => Some(s.table()),
			Self::Relocating(s) => Some(s.source_table()),
			Self::Native(_) => None,
		}
	}
}
pub(crate) struct RecordManyWriteAcquisition<'a, 'tx> {
	pub(crate) tx: &'a mut WriteTx<'tx>,
	pub(crate) layout: RecordManyWriteLayout<'a>,
	pub(crate) namespace: &'a RecordNamespace,
	pub(crate) keys: Vec<RecordKey>,
	pub(crate) source_texts: Option<Vec<String>>,
}
struct RecordManyHeldSet<'a, 'tx> {
	tx: &'a mut WriteTx<'tx>,
	layout: RecordManyWriteLayout<'a>,
	namespace: RecordNamespace,
	keys: Vec<RecordKey>,
	positions: BTreeMap<RecordKey, usize>,
	source_texts: Option<Vec<String>>,
	/// The rows that hold each key, in the order of `keys`. Acquisition fills it.
	rows: Vec<HeldManyRow>,
	completion: StateMutationCompletion,
}
/// The rows that hold one key of the write. The write table is the native table, or the v10
/// table in the v10 layout.
#[derive(Clone, Copy)]
enum HeldManyRow {
	/// A value in the write table.
	Value,
	/// A reservation row in the write table, which this acquisition found or inserted.
	Reservation(ReservationOwnership),
	/// During relocation, a v10 value under a native reservation row. The write moves the value
	/// to the native row and deletes the v10 row.
	SourceValue(ReservationOwnership),
}
pub(crate) struct PgRecordManyWriteLock<'a, 'tx> {
	held: RecordManyHeldSet<'a, 'tx>,
	observed_at: StateTimestampMicros,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RecordManyWriteError {
	#[error("record batch database work failed")]
	Database(#[from] DatabaseError),
	#[error("record batch native record is invalid")]
	Native(#[from] StateStorageError),
	#[error("record batch source record is invalid")]
	Source(#[from] super::super::v10::source_point::SourcePhysicalError),
	#[error("record batch source write metadata is invalid")]
	SourceWrite(#[from] SourcePointError),
	#[error("record batch expiration exceeds the timestamp domain")]
	Expiration(#[from] RecordExpirationOverflowError),
	#[error("record batch returned unknown, repeated, missing, or mismatched identities or tags")]
	Coverage,
	#[error("record batch request, placement, or physical output shape is invalid")]
	Shape,
	#[error("record batch found valued source and native records at the same identity")]
	DuplicatePhysicalValues,
	#[error("record batch record failed")]
	Record {
		identity: RecordIdentity,
		input_index: usize,
		#[source]
		source: Box<Self>,
	},
	#[error("record batch rejection and reservation cleanup both failed")]
	Cleanup {
		#[source]
		primary_error: Box<Self>,
		cleanup_error: Box<Self>,
	},
}
impl RecordManyWriteError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Database(e) => Some(e),
			Self::Native(e) => e.database_error(),
			Self::SourceWrite(e) => e.database_error(),
			Self::Record { source, .. } => source.database_error(),
			Self::Cleanup {
				primary_error,
				cleanup_error,
			} => primary_error
				.database_error()
				.or_else(|| cleanup_error.database_error()),
			_ => None,
		}
	}
	pub(crate) fn identity(&self) -> Option<&RecordIdentity> {
		match self {
			Self::Record { identity, .. } => Some(identity),
			Self::Cleanup {
				primary_error,
				cleanup_error,
			} => primary_error
				.identity()
				.or_else(|| cleanup_error.identity()),
			_ => None,
		}
	}
	pub(crate) fn input_index(&self) -> Option<usize> {
		match self {
			Self::Record { input_index, .. } => Some(*input_index),
			Self::Cleanup {
				primary_error,
				cleanup_error,
			} => primary_error
				.input_index()
				.or_else(|| cleanup_error.input_index()),
			_ => None,
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Cleanup {
				primary_error,
				cleanup_error,
			} => {
				primary_error.commit_outcome_is_unknown()
					|| cleanup_error.commit_outcome_is_unknown()
			}
			Self::Record { source, .. } => source.commit_outcome_is_unknown(),
			Self::SourceWrite(e) => e.commit_outcome_is_unknown(),
			_ => self
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
		}
	}
	fn with_identity(self, identity: RecordIdentity, input_index: usize) -> Self {
		Self::Record {
			identity,
			input_index,
			source: Box::new(self),
		}
	}
}
impl<'a, 'tx> PgRecordManyWriteLock<'a, 'tx> {
	pub(crate) async fn acquire(
		fields: RecordManyWriteAcquisition<'a, 'tx>,
	) -> Result<Self, RecordManyWriteError> {
		let completion = fields.tx.track_state_mutation();
		if fields.keys.is_empty()
			|| fields.layout.source_table().is_some() != fields.source_texts.is_some()
			|| fields.source_texts.as_ref().is_some_and(|texts| {
				texts.len() != fields.keys.len()
					|| texts
						.iter()
						.zip(&fields.keys)
						.any(|(text, key)| text.as_bytes() != key.as_bytes())
			}) {
			return Err(RecordManyWriteError::Shape);
		}
		let positions = fields
			.keys
			.iter()
			.cloned()
			.enumerate()
			.map(|(i, key)| (key, i))
			.collect::<BTreeMap<_, _>>();
		if positions.len() != fields.keys.len() {
			return Err(RecordManyWriteError::Shape);
		}
		let mut held = RecordManyHeldSet {
			tx: fields.tx,
			layout: fields.layout,
			namespace: fields.namespace.clone(),
			keys: fields.keys,
			positions,
			source_texts: fields.source_texts,
			rows: Vec::new(),
			completion,
		};
		let observed_at = acquire::acquire(&mut held).await?;
		Ok(Self { held, observed_at })
	}
	pub(crate) fn observed_at(&self) -> StateTimestampMicros {
		self.observed_at
	}
	pub(crate) async fn replace(
		mut self,
		records: Vec<EncodedRecord>,
	) -> Result<(), RecordManyWriteError> {
		let timestamps = if matches!(self.held.layout, RecordManyWriteLayout::Source(_)) {
			let metadata = match records.first() {
				Some(record) => record.record().metadata(),
				None => return Err(self.reject(RecordManyWriteError::Shape).await),
			};
			match SourceWriteTimestamps::parse(metadata) {
				Ok(timestamps) => Some(timestamps),
				Err(error) => return Err(self.reject(RecordManyWriteError::from(error)).await),
			}
		} else {
			None
		};
		let prepared = match apply::prepare(&self.held, records, timestamps) {
			Ok(prepared) => prepared,
			Err(error) => return Err(self.reject(error).await),
		};
		apply::apply(&mut self.held, prepared).await?;
		self.held.completion.complete();
		Ok(())
	}
	pub(crate) async fn keep(mut self) -> Result<(), RecordManyWriteError> {
		apply::cleanup(&mut self.held).await?;
		self.held.completion.complete();
		Ok(())
	}
	pub(crate) async fn reject(self, primary: RecordManyWriteError) -> RecordManyWriteError {
		match self.keep().await {
			Ok(()) => primary,
			Err(cleanup) => RecordManyWriteError::Cleanup {
				primary_error: Box::new(primary),
				cleanup_error: Box::new(cleanup),
			},
		}
	}
}

impl RecordManyHeldSet<'_, '_> {
	/// The keys for which this acquisition inserted a reservation row.
	fn inserted_reservations(&self) -> BTreeSet<usize> {
		self.rows
			.iter()
			.enumerate()
			.filter(|(_, row)| {
				matches!(
					row,
					HeldManyRow::Reservation(ReservationOwnership::Inserted)
						| HeldManyRow::SourceValue(ReservationOwnership::Inserted)
				)
			})
			.map(|(index, _)| index)
			.collect()
	}
	/// The keys for which the write moves a v10 value to native storage.
	fn source_values(&self) -> BTreeSet<usize> {
		self.rows
			.iter()
			.enumerate()
			.filter(|(_, row)| matches!(row, HeldManyRow::SourceValue(_)))
			.map(|(index, _)| index)
			.collect()
	}
	fn identify(&self, error: RecordManyWriteError, index: usize) -> RecordManyWriteError {
		error.with_identity(
			RecordIdentity::new(self.namespace.clone(), self.keys[index].clone()),
			index,
		)
	}
}
