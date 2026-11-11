//! Pause presence changes are explicit group preparation, not worker selection policy.
use super::{
	BoundQueueGroup, PlanBuilder, PreparedQueueGroupOutcome, QueueGroupError,
	QueueGroupPreparation, QueueGroupSnapshot, QueuePauseScope, QueuePauseState, QueueSourceRole,
	RecordMutation, encode_auxiliary, metadata, preparation,
};
impl BoundQueueGroup<'_, '_, '_> {
	pub(in crate::queue) fn prepare_pause(
		&self,
		snapshot: &QueueGroupSnapshot<'_>,
		scope: QueuePauseScope,
		action: PauseAction,
	) -> Result<QueueGroupPreparation, QueueGroupError> {
		preparation((|| {
			let previous = self.read_pause(snapshot, &scope)?;
			// A pause of a paused target and a resume of a target that is not paused write
			// nothing.
			if previous.is_some() == matches!(action, PauseAction::Pause) {
				return Ok(QueueGroupPreparation::Unchanged);
			}
			let binding = self.pause_binding(&scope)?;
			let mut plan = PlanBuilder::new();
			match action {
				PauseAction::Pause => plan.action(
					binding.identity().clone(),
					RecordMutation::Write(encode_auxiliary(
						&binding,
						&QueuePauseState { scope },
						metadata(snapshot.observed_at),
					)?),
				),
				PauseAction::Unpause => {
					plan.action(binding.identity().clone(), RecordMutation::Delete)
				}
			}
			if let Some((_, origin)) = previous {
				plan.remove_source(binding.identity().clone(), origin, QueueSourceRole::Pause);
			}
			plan.finish(snapshot, PreparedQueueGroupOutcome::PauseChanged)
		})())
	}
}
#[derive(Clone, Copy)]
pub(in crate::queue) enum PauseAction {
	Pause,
	Unpause,
}
