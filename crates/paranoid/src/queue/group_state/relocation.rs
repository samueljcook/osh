//! Physical source movement preserves logical state and returns diagnostic placement only.

use super::snapshot::Origin;
use super::{
	BoundQueueGroup, DeadLetterOriginalState, JobId, PlanBuilder, PreparedQueueGroupOutcome,
	QueueGroupError, QueueGroupPreparation, QueueGroupSnapshot, QueuePauseScope, QueueSourceRole,
	RecordMetadata, RecordMutation, SourceRecord, StateTimestampMicros, encode_auxiliary,
	inconsistent, metadata, preparation,
};

/// One exact source record kind; no table, codec, or layout is selected by the caller.
pub(crate) enum QueueRelocationRecord {
	Job(JobId),
	DeadLetter(JobId),
	Pause(QueuePauseScope),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum QueueSourceRelocationOutcome {
	Moved,
	AlreadyNative,
	AbsentNow,
}

impl BoundQueueGroup<'_, '_, '_> {
	/// Prepares an unchanged logical value under actual source/native group observations.
	/// Existing native records must satisfy their group invariants; this is no repair API.
	pub(in crate::queue) fn prepare_source_relocation(
		&self,
		snapshot: &QueueGroupSnapshot<'_>,
		record: &QueueRelocationRecord,
	) -> Result<QueueGroupPreparation, QueueGroupError> {
		preparation(match record {
			QueueRelocationRecord::Job(id) => self.relocate_job(snapshot, *id),
			QueueRelocationRecord::DeadLetter(id) => self.relocate_dead_letter(snapshot, *id),
			QueueRelocationRecord::Pause(scope) => self.relocate_pause(snapshot, scope),
		})
	}

	fn relocate_job(
		&self,
		snapshot: &QueueGroupSnapshot<'_>,
		id: JobId,
	) -> Result<QueueGroupPreparation, QueueGroupError> {
		let mut plan = PlanBuilder::new();
		let Some((state, origin)) = self.read_job(snapshot, id)? else {
			return plan.finish(
				snapshot,
				relocation(QueueSourceRelocationOutcome::AbsentNow),
			);
		};
		// Unchanged membership still requires exact owner/probe validation. Only a source
		// owner may need a new native map; a missing native owner's map is an error.
		self.reconcile_dedupe(snapshot, Some((&state, origin)), Some(&state), &mut plan)?;
		let outcome = match origin {
			Origin::Native => QueueSourceRelocationOutcome::AlreadyNative,
			Origin::Source => {
				self.plan_job_write(snapshot, &state, &mut plan)?;
				plan.remove_source(self.job_identity(id), origin, QueueSourceRole::Job);
				QueueSourceRelocationOutcome::Moved
			}
		};
		plan.finish(snapshot, relocation(outcome))
	}

	fn relocate_dead_letter(
		&self,
		snapshot: &QueueGroupSnapshot<'_>,
		id: JobId,
	) -> Result<QueueGroupPreparation, QueueGroupError> {
		let mut plan = PlanBuilder::new();
		let Some((state, origin)) = self.read_dead_letter(snapshot, id)? else {
			return plan.finish(
				snapshot,
				relocation(QueueSourceRelocationOutcome::AbsentNow),
			);
		};
		let (original, has_map) = self.require_original_membership(snapshot, &state, origin)?;
		let outcome = match origin {
			Origin::Native => QueueSourceRelocationOutcome::AlreadyNative,
			Origin::Source => {
				if !has_map {
					plan.action(
						original.identity().clone(),
						RecordMutation::Write(encode_auxiliary(
							&original,
							&DeadLetterOriginalState {
								original_job_id: state.dead_letter().original_job_id,
								dead_letter_id: id,
							},
							metadata(snapshot.observed_at),
						)?),
					);
				}
				let binding = self.dead_letter_binding(id)?;
				let preserved = RecordMetadata {
					expires_at: None,
					updated_at: StateTimestampMicros::new(
						state.dead_letter().updated_at_unix_micros,
					),
				};
				plan.action(
					binding.identity().clone(),
					RecordMutation::Write(encode_auxiliary(&binding, &state, preserved)?),
				);
				plan.remove_source(
					binding.identity().clone(),
					origin,
					QueueSourceRole::DeadLetter,
				);
				QueueSourceRelocationOutcome::Moved
			}
		};
		plan.finish(snapshot, relocation(outcome))
	}

	fn relocate_pause(
		&self,
		snapshot: &QueueGroupSnapshot<'_>,
		scope: &QueuePauseScope,
	) -> Result<QueueGroupPreparation, QueueGroupError> {
		let mut plan = PlanBuilder::new();
		let Some((state, origin)) = self.read_pause(snapshot, scope)? else {
			return plan.finish(
				snapshot,
				relocation(QueueSourceRelocationOutcome::AbsentNow),
			);
		};
		let outcome = match origin {
			Origin::Native => QueueSourceRelocationOutcome::AlreadyNative,
			Origin::Source => {
				let binding = self.pause_binding(scope)?;
				let SourceRecord::Pause { metadata, .. } = snapshot.source(binding.identity())?
				else {
					return Err(inconsistent(
						"source pause has a different observed record kind",
					));
				};
				plan.action(
					binding.identity().clone(),
					RecordMutation::Write(encode_auxiliary(&binding, &state, *metadata)?),
				);
				plan.remove_source(binding.identity().clone(), origin, QueueSourceRole::Pause);
				QueueSourceRelocationOutcome::Moved
			}
		};
		plan.finish(snapshot, relocation(outcome))
	}
}

fn relocation(outcome: QueueSourceRelocationOutcome) -> PreparedQueueGroupOutcome {
	PreparedQueueGroupOutcome::SourceRelocation(outcome)
}
