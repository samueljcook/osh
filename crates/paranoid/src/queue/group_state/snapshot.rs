//! Typed source observations remain distinct from unknown coverage and native physical values.
use super::{
	BTreeMap, BoundQueueGroup, JobId, NativeDeadLetterState, NativeJob, QueueGroupError,
	QueuePauseScope, QueuePauseState, RecordGroupRead, RecordIdentity, RecordMetadata,
	StateTimestampMicros, at_record, inconsistent,
};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Origin {
	Native,
	Source,
}

/// Actual decoded source facts; constructors provide no IO or source-retirement proof.
pub(in crate::queue) enum SourceRecord {
	Absent,
	Job(NativeJob),
	DeadLetter(NativeDeadLetterState),
	Pause {
		state: QueuePauseState,
		metadata: RecordMetadata,
	},
}
/// An exact at-most-one source unique-index observation, not a native-side emptiness proof.
pub(in crate::queue) enum SourceProbe {
	Absent,
	Present(JobId),
}

/// Missing entries are Unknown; no absence is inferred from a page, count, or native value.
pub(in crate::queue) struct QueueSourceFacts {
	pub(in crate::queue) records: BTreeMap<RecordIdentity, SourceRecord>,
	pub(in crate::queue) active_dedupe: BTreeMap<RecordIdentity, SourceProbe>,
	pub(in crate::queue) originals: BTreeMap<RecordIdentity, SourceProbe>,
}

/// The adapter must supply one final authoritative time after every native/source lock wait.
/// This data borrows observations; it does not prove admission, locks, or their current validity.
pub(in crate::queue) struct QueueGroupSnapshot<'a> {
	pub(in crate::queue) native: &'a RecordGroupRead,
	pub(in crate::queue) source: &'a QueueSourceFacts,
	pub(in crate::queue) observed_at: StateTimestampMicros,
}
impl QueueGroupSnapshot<'_> {
	pub(super) fn held(
		&self,
		identity: &RecordIdentity,
	) -> Result<&crate::records::PointRecordRead, QueueGroupError> {
		self.native
			.snapshots()
			.get(identity)
			.ok_or_else(|| QueueGroupError::RequiredIdentity {
				identity: identity.clone(),
			})
	}
	pub(super) fn source(
		&self,
		identity: &RecordIdentity,
	) -> Result<&SourceRecord, QueueGroupError> {
		self.held(identity)?;
		self.source
			.records
			.get(identity)
			.ok_or_else(|| QueueGroupError::SourceUnknown {
				identity: identity.clone(),
			})
	}
	pub(super) fn probe<'a>(
		&'a self,
		observations: &'a BTreeMap<RecordIdentity, SourceProbe>,
		identity: &RecordIdentity,
	) -> Result<Option<JobId>, QueueGroupError> {
		self.held(identity)?;
		match observations.get(identity) {
			Some(SourceProbe::Absent) => Ok(None),
			Some(SourceProbe::Present(id)) => Ok(Some(*id)),
			None => Err(QueueGroupError::SourceUnknown {
				identity: identity.clone(),
			}),
		}
	}
}
impl BoundQueueGroup<'_, '_, '_> {
	pub(super) fn read_job(
		&self,
		snapshot: &QueueGroupSnapshot<'_>,
		id: JobId,
	) -> Result<Option<(NativeJob, Origin)>, QueueGroupError> {
		let identity = self.job_identity(id);
		let binding = self.job_binding(id)?;
		binding.require_native_writer()?;
		let native = binding
			.decode_record(snapshot.held(&identity)?)
			.map_err(|error| at_record(identity.clone(), error))?;
		match (native, snapshot.source(&identity)?) {
			(Some(_), SourceRecord::Job(_)) => Err(inconsistent(
				"job has both native and source physical values",
			)),
			(Some(state), SourceRecord::Absent) => Ok(Some((state, Origin::Native))),
			(None, SourceRecord::Job(state)) if state.job().id == id => {
				Ok(Some((state.clone(), Origin::Source)))
			}
			(None, SourceRecord::Absent) => Ok(None),
			_ => Err(inconsistent(
				"source job observation has a different kind or identity",
			)),
		}
	}
	pub(super) fn read_dead_letter(
		&self,
		snapshot: &QueueGroupSnapshot<'_>,
		id: JobId,
	) -> Result<Option<(NativeDeadLetterState, Origin)>, QueueGroupError> {
		let binding = self.dead_letter_binding(id)?;
		let native = binding
			.decode_record(binding.identity(), snapshot.held(binding.identity())?)
			.map_err(|error| at_record(binding.identity().clone(), error))?;
		match (native, snapshot.source(binding.identity())?) {
			(Some(_), SourceRecord::DeadLetter(_)) => Err(inconsistent(
				"dead letter has both native and source physical values",
			)),
			(Some(state), SourceRecord::Absent) => Ok(Some((state, Origin::Native))),
			(None, SourceRecord::DeadLetter(state)) if state.dead_letter().id == id => {
				Ok(Some((state.clone(), Origin::Source)))
			}
			(None, SourceRecord::Absent) => Ok(None),
			_ => Err(inconsistent(
				"source dead letter observation has a different kind or identity",
			)),
		}
	}
	/// Whether a native pause record that a read without a lock observed is present.
	pub(in crate::queue) fn pause_is_present(
		&self,
		scope: &QueuePauseScope,
		read: &crate::records::PointRecordRead,
	) -> Result<bool, QueueGroupError> {
		let binding = self.pause_binding(scope)?;
		Ok(binding
			.decode_record(binding.identity(), read)
			.map_err(|error| at_record(binding.identity().clone(), error))?
			.is_some())
	}
	pub(super) fn read_pause(
		&self,
		snapshot: &QueueGroupSnapshot<'_>,
		scope: &QueuePauseScope,
	) -> Result<Option<(QueuePauseState, Origin)>, QueueGroupError> {
		let binding = self.pause_binding(scope)?;
		let native = binding
			.decode_record(binding.identity(), snapshot.held(binding.identity())?)
			.map_err(|error| at_record(binding.identity().clone(), error))?;
		match (native, snapshot.source(binding.identity())?) {
			(Some(_), SourceRecord::Pause { .. }) => Err(inconsistent(
				"pause has both native and source physical values",
			)),
			(Some(state), SourceRecord::Absent) => Ok(Some((state, Origin::Native))),
			(None, SourceRecord::Pause { state, metadata })
				if &state.scope == scope && metadata.expires_at.is_none() =>
			{
				Ok(Some((state.clone(), Origin::Source)))
			}
			(None, SourceRecord::Absent) => Ok(None),
			_ => Err(inconsistent(
				"source pause observation has a different kind, identity, or metadata",
			)),
		}
	}
}
