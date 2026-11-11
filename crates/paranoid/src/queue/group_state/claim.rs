//! Claiming one selected due job under the same pause rules as the published worker.
use super::{
	BoundQueueGroup, JobId, PlanBuilder, PreparedQueueGroupOutcome, QueueGroupError,
	QueueGroupPreparation, QueueGroupSnapshot, QueuePauseScope, QueueSourceRole, at_record,
	preparation,
};
use crate::queue::record_transitions::JobAcquisition;
use crate::queue::{WorkerClaimToken, WorkerOwnerId};

/// How a claim learns whether the queue or the job's task is paused.
pub(in crate::queue) enum ClaimPauses {
	/// The group holds the pause records. During data retirement a pause can still be a v10
	/// row, which only the group reads.
	Held,
	/// The selection that found the candidate excluded jobs of a paused queue or task with a
	/// read that took no lock, as v10's claim does. No claim waits for another claim's pause
	/// read.
	ExcludedBySelection,
}

/// One candidate job and the fresh private claim material for it.
pub(in crate::queue) struct NativeClaimRequest {
	pub(in crate::queue) job_id: JobId,
	pub(in crate::queue) owner: WorkerOwnerId,
	pub(in crate::queue) token: WorkerClaimToken,
}

impl BoundQueueGroup<'_, '_, '_> {
	/// Claims the job when it is still pending and due and neither the queue nor its task is
	/// paused. Otherwise the plan changes nothing and reports that the candidate was skipped.
	pub(in crate::queue) fn prepare_claim(
		&self,
		snapshot: &QueueGroupSnapshot<'_>,
		request: &NativeClaimRequest,
		pauses: &ClaimPauses,
	) -> Result<QueueGroupPreparation, QueueGroupError> {
		preparation((|| {
			let Some((before, origin)) = self.read_job(snapshot, request.job_id)? else {
				return PlanBuilder::new()
					.finish(snapshot, PreparedQueueGroupOutcome::ClaimSkipped);
			};
			if matches!(pauses, ClaimPauses::Held)
				&& (self
					.read_pause(snapshot, &QueuePauseScope::Global)?
					.is_some() || self
					.read_pause(
						snapshot,
						&QueuePauseScope::Task(before.job().task_name.clone()),
					)?
					.is_some())
			{
				return PlanBuilder::new()
					.finish(snapshot, PreparedQueueGroupOutcome::ClaimSkipped);
			}
			let Some(after) = self
				.job_binding(request.job_id)?
				.prepare_claim(
					before.clone(),
					JobAcquisition {
						owner: request.owner.clone(),
						token: request.token.clone(),
					},
					snapshot.observed_at,
				)
				.map_err(|error| at_record(self.job_identity(request.job_id), error))?
			else {
				return PlanBuilder::new()
					.finish(snapshot, PreparedQueueGroupOutcome::ClaimSkipped);
			};
			let mut plan = PlanBuilder::new();
			self.reconcile_dedupe(snapshot, Some((&before, origin)), Some(&after), &mut plan)?;
			self.plan_job_write(snapshot, &after, &mut plan)?;
			plan.remove_source(
				self.job_identity(request.job_id),
				origin,
				QueueSourceRole::Job,
			);
			plan.finish(
				snapshot,
				PreparedQueueGroupOutcome::Claimed {
					job: Box::new(self.public_job(snapshot, &after)?),
				},
			)
		})())
	}
}
