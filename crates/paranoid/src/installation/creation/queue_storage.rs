//! Exact native Queue placement. Parsing grants no schema, route, or mutation authority.

use crate::records::RecordNamespace;
use primitives_rs::crypto::algorithms::blake3;

use super::{BindingRole, NativeInstallationError};
use crate::db::PgIdentifier;
use crate::queue::postgres::due::QueueSelectionIndexes;
use crate::records::postgres::PgPointStorage;

/// All Queue records share one configured container and use five distinct namespaces.
pub(crate) struct PgNativeQueueStorageFields {
	pub(crate) storage: PgPointStorage,
	pub(crate) jobs: RecordNamespace,
	pub(crate) active_dedupe: RecordNamespace,
	pub(crate) dead_letters: RecordNamespace,
	pub(crate) dead_letter_originals: RecordNamespace,
	pub(crate) pauses: RecordNamespace,
	pub(crate) due_jobs_index: PgIdentifier,
	pub(crate) running_jobs_index: PgIdentifier,
	pub(crate) finished_jobs_index: PgIdentifier,
	pub(crate) dead_letters_index: PgIdentifier,
}

#[derive(Clone)]
pub(crate) struct PgNativeQueueStorage {
	storage: PgPointStorage,
	jobs: RecordNamespace,
	active_dedupe: RecordNamespace,
	dead_letters: RecordNamespace,
	dead_letter_originals: RecordNamespace,
	pauses: RecordNamespace,
	due_jobs_index: PgIdentifier,
	running_jobs_index: PgIdentifier,
	finished_jobs_index: PgIdentifier,
	dead_letters_index: PgIdentifier,
}

impl PgNativeQueueStorage {
	pub(crate) fn new(fields: PgNativeQueueStorageFields) -> Result<Self, NativeInstallationError> {
		let placement = Self {
			storage: fields.storage,
			jobs: fields.jobs,
			active_dedupe: fields.active_dedupe,
			dead_letters: fields.dead_letters,
			dead_letter_originals: fields.dead_letter_originals,
			pauses: fields.pauses,
			due_jobs_index: fields.due_jobs_index,
			running_jobs_index: fields.running_jobs_index,
			finished_jobs_index: fields.finished_jobs_index,
			dead_letters_index: fields.dead_letters_index,
		};
		let indexes = placement.selection_indexes().all();
		for (position, index) in indexes.iter().enumerate() {
			if indexes[..position].contains(index) {
				return Err(NativeInstallationError::QueueIndexNamesEqual);
			}
		}
		let roles = placement.roles();
		for (index, role) in roles.iter().enumerate() {
			for prior in &roles[..index] {
				if role.namespace == prior.namespace {
					return Err(NativeInstallationError::OverlappingNamespaces {
						left: prior.name,
						right: role.name,
					});
				}
			}
		}
		Ok(placement)
	}
	pub(crate) fn storage(&self) -> &PgPointStorage {
		&self.storage
	}
	pub(crate) fn jobs(&self) -> &RecordNamespace {
		&self.jobs
	}
	pub(crate) fn active_dedupe(&self) -> &RecordNamespace {
		&self.active_dedupe
	}
	pub(crate) fn dead_letters(&self) -> &RecordNamespace {
		&self.dead_letters
	}
	pub(crate) fn dead_letter_originals(&self) -> &RecordNamespace {
		&self.dead_letter_originals
	}
	pub(crate) fn pauses(&self) -> &RecordNamespace {
		&self.pauses
	}
	pub(crate) fn selection_indexes(&self) -> QueueSelectionIndexes<'_> {
		QueueSelectionIndexes {
			due: &self.due_jobs_index,
			running: &self.running_jobs_index,
			finished: &self.finished_jobs_index,
			dead_letters: &self.dead_letters_index,
		}
	}
	pub(super) fn frame_index_fingerprint(&self, hasher: &mut blake3::Hasher) {
		super::frame_fingerprint_input(hasher, b"queue-selection-indexes.v1");
		super::frame_fingerprint_input(hasher, self.due_jobs_index.as_str().as_bytes());
		super::frame_fingerprint_input(hasher, self.running_jobs_index.as_str().as_bytes());
		super::frame_fingerprint_input(hasher, self.finished_jobs_index.as_str().as_bytes());
		super::frame_fingerprint_input(hasher, self.dead_letters_index.as_str().as_bytes());
	}
	pub(super) fn roles(&self) -> [BindingRole<'_>; 5] {
		[
			("Queue jobs", self.jobs()),
			("Queue active dedupe", self.active_dedupe()),
			("Queue dead letters", self.dead_letters()),
			("Queue dead-letter originals", self.dead_letter_originals()),
			("Queue pauses", self.pauses()),
		]
		.map(|(name, namespace)| BindingRole {
			name,
			storage: self.storage(),
			namespace,
			control_key: None,
		})
	}
}
