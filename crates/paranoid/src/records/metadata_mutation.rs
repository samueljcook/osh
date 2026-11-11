//! Metadata-based record mutations retain storage authority without requiring stored bodies.

use crate::records::{
	EncodedMetadataUpdate, EncodedRecord, PointRecordLock, RecordMetadata, StateTimestampMicros,
};

pub(crate) struct RecordMutationCurrentMetadata {
	pub(crate) live_metadata: Option<RecordMetadata>,
	pub(crate) observed_at: StateTimestampMicros,
}

pub(crate) trait RecordMetadataMutationLock: Sized + Send {
	type Error: std::error::Error + Send + Sync + 'static;
	fn current(&self) -> RecordMutationCurrentMetadata;
	fn write(self, record: EncodedRecord) -> impl Future<Output = Result<(), Self::Error>> + Send;
	fn delete(self) -> impl Future<Output = Result<bool, Self::Error>> + Send;
	fn keep(self) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

pub(crate) struct PointRecordMetadataMutationLock<L>(L);

impl<L: PointRecordLock> PointRecordMetadataMutationLock<L> {
	pub(crate) fn new(lock: L) -> Self {
		Self(lock)
	}
}

impl<L: PointRecordLock> RecordMetadataMutationLock for PointRecordMetadataMutationLock<L> {
	type Error = L::Error;
	fn current(&self) -> RecordMutationCurrentMetadata {
		let snapshot = self.0.snapshot();
		RecordMutationCurrentMetadata {
			live_metadata: snapshot.live_record().map(|record| record.metadata()),
			observed_at: snapshot.observed_at(),
		}
	}
	fn write(self, record: EncodedRecord) -> impl Future<Output = Result<(), Self::Error>> + Send {
		self.0.write(record)
	}
	fn delete(self) -> impl Future<Output = Result<bool, Self::Error>> + Send {
		self.0.delete()
	}
	fn keep(self) -> impl Future<Output = Result<(), Self::Error>> + Send {
		self.0.keep()
	}
}

/// Updating an existing body in place requires a physical route that preserves that body.
pub(crate) trait RecordMetadataUpdateLock: RecordMetadataMutationLock {
	fn update_metadata(
		self,
		update: EncodedMetadataUpdate,
	) -> impl Future<Output = Result<(), Self::Error>> + Send;
}
