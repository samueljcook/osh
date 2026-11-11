//! Metadata-based mutations use a validated header and the same storage completion operations.

use super::v10::relocating_point::{PgRelocatingPointRecordHeaderLock, RelocatingPointError};
use super::v10::source_point::{PgSourcePointRecordHeaderLock, SourcePointError};
use crate::records::metadata_mutation::{
	RecordMetadataMutationLock, RecordMetadataUpdateLock, RecordMutationCurrentMetadata,
};
use crate::records::postgres::{PgPointRecordHeaderLock, StateStorageError};
use crate::records::{EncodedMetadataUpdate, EncodedRecord};

impl RecordMetadataMutationLock for PgPointRecordHeaderLock<'_, '_> {
	type Error = StateStorageError;

	fn current(&self) -> RecordMutationCurrentMetadata {
		let header = self.header();
		RecordMutationCurrentMetadata {
			live_metadata: header.metadata().filter(|_| header.is_live()),
			observed_at: header.observed_at(),
		}
	}

	async fn write(self, record: EncodedRecord) -> Result<(), Self::Error> {
		self.write_value(record).await
	}

	async fn delete(self) -> Result<bool, Self::Error> {
		self.delete().await
	}

	async fn keep(self) -> Result<(), Self::Error> {
		self.keep_value().await
	}
}

impl RecordMetadataMutationLock for PgSourcePointRecordHeaderLock<'_, '_> {
	type Error = SourcePointError;
	fn current(&self) -> RecordMutationCurrentMetadata {
		let header = self.header();
		RecordMutationCurrentMetadata {
			live_metadata: header.metadata().filter(|_| header.is_live()),
			observed_at: header.observed_at(),
		}
	}
	async fn write(self, record: EncodedRecord) -> Result<(), Self::Error> {
		self.write_value(record).await
	}
	async fn delete(self) -> Result<bool, Self::Error> {
		self.delete().await
	}
	async fn keep(self) -> Result<(), Self::Error> {
		self.keep_value().await
	}
}

impl RecordMetadataMutationLock for PgRelocatingPointRecordHeaderLock<'_, '_> {
	type Error = RelocatingPointError;
	fn current(&self) -> RecordMutationCurrentMetadata {
		let header = self.header();
		RecordMutationCurrentMetadata {
			live_metadata: header.metadata().filter(|_| header.is_live()),
			observed_at: header.observed_at(),
		}
	}
	async fn write(self, record: EncodedRecord) -> Result<(), Self::Error> {
		self.write_value(record).await
	}
	async fn delete(self) -> Result<bool, Self::Error> {
		self.delete().await
	}
	async fn keep(self) -> Result<(), Self::Error> {
		self.keep_value().await
	}
}

impl RecordMetadataUpdateLock for PgPointRecordHeaderLock<'_, '_> {
	async fn update_metadata(self, update: EncodedMetadataUpdate) -> Result<(), Self::Error> {
		self.update_value_metadata(update).await
	}
}
impl RecordMetadataUpdateLock for PgSourcePointRecordHeaderLock<'_, '_> {
	async fn update_metadata(self, update: EncodedMetadataUpdate) -> Result<(), Self::Error> {
		self.update_value_metadata(update).await
	}
}

impl RecordMetadataUpdateLock for PgRelocatingPointRecordHeaderLock<'_, '_> {
	async fn update_metadata(self, update: EncodedMetadataUpdate) -> Result<(), Self::Error> {
		self.update_value_metadata(update).await
	}
}
