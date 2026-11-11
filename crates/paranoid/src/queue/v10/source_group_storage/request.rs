//! Complete source requests derive from the actual held Queue identity set.
//!
//! A discovered probe owner expands the whole canonical set before its source row is locked.

use super::{
	BTreeMap, BTreeSet, DedupeKey, JobId, PgNativeQueueStorage, QueuePauseScope,
	QueueSourceGroupReaders, QueueSourceGroupStorageError, QueueSourceNativeRecords,
	RecordGroupRead, RecordIdentity, RecordKey, RecordNamespace, TaskName,
};
use crate::queue::auxiliary_state::{parse_active_dedupe_key, parse_pause_key};

pub(crate) struct QueueSourceGroupRequest {
	requested: RequestedSources,
}
impl QueueSourceGroupRequest {
	pub(in crate::queue) fn for_pause(
		storage: &PgNativeQueueStorage,
		scope: QueuePauseScope,
	) -> Self {
		let mut requested = RequestedSources::new();
		requested.pauses.insert(
			RecordIdentity::new(storage.pauses().clone(), scope.key()),
			scope,
		);
		Self { requested }
	}
	pub(in crate::queue) fn from_held(
		native: &RecordGroupRead,
		storage: &PgNativeQueueStorage,
	) -> Result<Self, QueueSourceGroupStorageError> {
		let mut requested = RequestedSources::new();
		for identity in native.snapshots().keys() {
			let namespace = identity.namespace();
			if namespace == storage.jobs() {
				requested.jobs.insert(identity.clone(), parse_id(identity)?);
			} else if namespace == storage.dead_letters() {
				requested
					.dead_letters
					.insert(identity.clone(), parse_id(identity)?);
			} else if namespace == storage.active_dedupe() {
				requested.active_dedupe.insert(
					identity.clone(),
					parse_active_dedupe_key(identity.key())
						.map_err(|e| QueueSourceGroupStorageError::AuxiliaryBinding(e.into()))?,
				);
			} else if namespace == storage.dead_letter_originals() {
				requested
					.originals
					.insert(identity.clone(), parse_id(identity)?);
			} else if namespace == storage.pauses() {
				requested.pauses.insert(
					identity.clone(),
					parse_pause_key(identity.key())
						.map_err(|e| QueueSourceGroupStorageError::AuxiliaryBinding(e.into()))?,
				);
			} else {
				return Err(QueueSourceGroupStorageError::ForeignIdentity {
					identity: identity.clone(),
				});
			}
		}
		Ok(Self { requested })
	}
}
fn parse_id(identity: &RecordIdentity) -> Result<JobId, QueueSourceGroupStorageError> {
	JobId::from_bytes(identity.key().as_bytes()).map_err(|_| {
		QueueSourceGroupStorageError::InvalidIdentity {
			identity: identity.clone(),
		}
	})
}
pub(super) struct RequestedSources {
	pub(super) jobs: BTreeMap<RecordIdentity, JobId>,
	pub(super) dead_letters: BTreeMap<RecordIdentity, JobId>,
	pub(super) pauses: BTreeMap<RecordIdentity, QueuePauseScope>,
	pub(super) active_dedupe: BTreeMap<RecordIdentity, (TaskName, DedupeKey)>,
	pub(super) originals: BTreeMap<RecordIdentity, JobId>,
}
pub(super) fn id_identity(namespace: &RecordNamespace, id: JobId) -> RecordIdentity {
	RecordIdentity::new(namespace.clone(), RecordKey::new(id.as_bytes().to_vec()))
}
impl RequestedSources {
	fn new() -> Self {
		Self {
			jobs: BTreeMap::new(),
			dead_letters: BTreeMap::new(),
			pauses: BTreeMap::new(),
			active_dedupe: BTreeMap::new(),
			originals: BTreeMap::new(),
		}
	}
	pub(super) fn parse(
		request: QueueSourceGroupRequest,
		storage: &PgNativeQueueStorage,
		readers: &QueueSourceGroupReaders<'_, '_, '_>,
	) -> Result<Self, QueueSourceGroupStorageError> {
		let parsed = request.requested;
		for (identity, (task, dedupe)) in &parsed.active_dedupe {
			let binding = readers.auxiliary.schemas().bind_active_dedupe(
				readers.auxiliary_contract,
				storage.active_dedupe().clone(),
				task,
				dedupe,
			)?;
			if binding.identity() != identity {
				return Err(QueueSourceGroupStorageError::InvalidIdentity {
					identity: identity.clone(),
				});
			}
		}
		for (identity, id) in &parsed.originals {
			let binding = readers.auxiliary.schemas().bind_original(
				readers.auxiliary_contract,
				storage.dead_letter_originals().clone(),
				*id,
			)?;
			if binding.identity() != identity {
				return Err(QueueSourceGroupStorageError::InvalidIdentity {
					identity: identity.clone(),
				});
			}
		}
		Ok(parsed)
	}
	pub(super) fn missing(&self, native: QueueSourceNativeRecords<'_>) -> BTreeSet<RecordIdentity> {
		self.jobs
			.keys()
			.chain(self.dead_letters.keys())
			.chain(self.pauses.keys())
			.chain(self.active_dedupe.keys())
			.chain(self.originals.keys())
			.filter(|identity| !native.contains_key(identity))
			.cloned()
			.collect()
	}
	pub(super) fn include_job(&mut self, storage: &PgNativeQueueStorage, id: JobId) {
		self.jobs.insert(id_identity(storage.jobs(), id), id);
	}
	pub(super) fn include_dead_letter(&mut self, storage: &PgNativeQueueStorage, id: JobId) {
		self.dead_letters
			.insert(id_identity(storage.dead_letters(), id), id);
	}
}
