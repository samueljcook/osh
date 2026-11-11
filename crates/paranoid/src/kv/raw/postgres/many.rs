//! Complete requested-key raw observations with fixed source/native placement layouts.

use crate::kv::raw::model::many::{RawManyReadRequest, RawManyReadResult, RawManySourceKeys};
use crate::records::{RecordIdentity, RecordKey, RecordNamespace, StateTimestampMicros};

use super::page::{PhysicalRecordObservation, RawPageStorageError, RawReadLayout};
use super::v10::relocating_point::PgRelocatingPointStorage;
use super::v10::source_point::PgSourcePointStorage;
use super::{DatabaseError, PgPointStorage, Tx};

mod read;

pub(crate) struct RawPhysicalManyRequest<'a> {
	pub(crate) namespace: &'a RecordNamespace,
	pub(crate) request: &'a RawManyReadRequest,
}
pub(crate) struct SourceRawPhysicalManyRequest<'a> {
	pub(crate) namespace: &'a RecordNamespace,
	pub(crate) keys: &'a RawManySourceKeys<'a>,
}
pub(super) struct RawPhysicalManyObservation {
	records: Vec<PhysicalRecordObservation>,
	observed_at: StateTimestampMicros,
}
pub(super) struct RawManyReadFields<'a> {
	namespace: &'a RecordNamespace,
	request: &'a RawManyReadRequest,
	source_texts: Option<&'a [String]>,
}

pub(super) struct PhysicalManyReadFields<'a> {
	namespace: &'a RecordNamespace,
	keys: &'a [RecordKey],
	source_texts: Option<&'a [String]>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RawManyStorageError {
	#[error("raw multi-get database work failed")]
	Database(#[from] DatabaseError),
	#[error("raw multi-get physical record is invalid")]
	Physical(#[from] RawPageStorageError),
	#[error("raw multi-get physical request must contain at least one key")]
	EmptyRequest,
	#[error("raw multi-get source layout does not match its parsed key binding")]
	SourceBindingMismatch,
	#[error("raw multi-get returned unknown, repeated, missing, or mismatched request markers")]
	RequestCoverage,
	#[error("raw multi-get does not share one authoritative clock")]
	InconsistentClock,
	#[error("raw multi-get record observation failed")]
	Record {
		identity: RecordIdentity,
		input_index: Option<usize>,
		#[source]
		source: Box<Self>,
	},
}
impl RawManyStorageError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Database(error) => Some(error),
			Self::Physical(error) => error.database_error(),
			Self::Record { source, .. } => source.database_error(),
			_ => None,
		}
	}
	pub(crate) fn identity(&self) -> Option<&RecordIdentity> {
		match self {
			Self::Record { identity, .. } => Some(identity),
			_ => None,
		}
	}
	pub(crate) fn input_index(&self) -> Option<usize> {
		match self {
			Self::Record { input_index, .. } => *input_index,
			_ => None,
		}
	}
	fn with_identity(self, identity: RecordIdentity, input_index: Option<usize>) -> Self {
		Self::Record {
			identity,
			input_index,
			source: Box::new(self),
		}
	}
}

impl PgPointStorage {
	pub(crate) async fn read_raw_many(
		&self,
		tx: &mut Tx<'_>,
		fields: RawPhysicalManyRequest<'_>,
	) -> Result<RawManyReadResult, RawManyStorageError> {
		read::read(
			tx,
			RawManyReadFields {
				namespace: fields.namespace,
				request: fields.request,
				source_texts: None,
			},
			RawReadLayout::Native(self),
		)
		.await
	}
}
impl PgSourcePointStorage {
	pub(crate) async fn read_raw_many(
		&self,
		tx: &mut Tx<'_>,
		fields: SourceRawPhysicalManyRequest<'_>,
	) -> Result<RawManyReadResult, RawManyStorageError> {
		read::read(
			tx,
			RawManyReadFields {
				namespace: fields.namespace,
				request: fields.keys.request(),
				source_texts: Some(fields.keys.texts()),
			},
			RawReadLayout::Source(self),
		)
		.await
	}
}
impl PgRelocatingPointStorage {
	pub(crate) async fn read_raw_many(
		&self,
		tx: &mut Tx<'_>,
		fields: SourceRawPhysicalManyRequest<'_>,
	) -> Result<RawManyReadResult, RawManyStorageError> {
		read::read(
			tx,
			RawManyReadFields {
				namespace: fields.namespace,
				request: fields.keys.request(),
				source_texts: Some(fields.keys.texts()),
			},
			RawReadLayout::Relocating(self),
		)
		.await
	}
}
