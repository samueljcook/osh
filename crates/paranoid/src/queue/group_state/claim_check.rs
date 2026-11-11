//! Returns of claims that are identified by their owner label and private token.
use super::{
	BoundQueueGroup, JobId, JobStatus, OwnedJobMutation, PlanBuilder, PreparedQueueGroupOutcome,
	QueueGroupError, QueueGroupPreparation, QueueGroupSnapshot, QueueSourceRole, at_record,
	preparation,
};
use crate::queue::record_transitions::RequiredExecutionState;
use crate::queue::{WorkerClaimToken, WorkerOwnerId};

/// One job and the claim that is expected to hold it. The job returns to pending when the claim
/// holds it and its execution has not started.
pub(in crate::queue) struct NativeClaimCheckRequest {
	pub(in crate::queue) job_id: JobId,
	pub(in crate::queue) owner: WorkerOwnerId,
	pub(in crate::queue) token: WorkerClaimToken,
}

impl BoundQueueGroup<'_, '_, '_> {
	pub(in crate::queue) fn prepare_claim_check(
		&self,
		snapshot: &QueueGroupSnapshot<'_>,
		request: &NativeClaimCheckRequest,
	) -> Result<QueueGroupPreparation, QueueGroupError> {
		preparation((|| {
			let Some((before, origin)) = self.read_job(snapshot, request.job_id)? else {
				return PlanBuilder::new()
					.finish(snapshot, PreparedQueueGroupOutcome::ClaimNotReturned);
			};
			let held = before.job().status() == JobStatus::Running
				&& before.job().worker_owner_id() == Some(&request.owner)
				&& before.claim_token() == Some(&request.token);
			if !held {
				return PlanBuilder::new()
					.finish(snapshot, PreparedQueueGroupOutcome::ClaimNotReturned);
			}
			let authority = super::super::record_transitions::JobClaimCredentials {
				job_id: request.job_id,
				owner: &request.owner,
				token: &request.token,
			};
			let Some(after) = self
				.job_binding(request.job_id)?
				.prepare_owned_mutation(
					before.clone(),
					&authority,
					OwnedJobMutation::ReturnPending(RequiredExecutionState::Unstarted),
					snapshot.observed_at,
				)
				.map_err(|error| at_record(self.job_identity(request.job_id), error))?
			else {
				return PlanBuilder::new()
					.finish(snapshot, PreparedQueueGroupOutcome::ClaimNotReturned);
			};
			let mut plan = PlanBuilder::new();
			self.reconcile_dedupe(snapshot, Some((&before, origin)), Some(&after), &mut plan)?;
			self.plan_job_write(snapshot, &after, &mut plan)?;
			plan.remove_source(
				self.job_identity(request.job_id),
				origin,
				QueueSourceRole::Job,
			);
			plan.finish(snapshot, PreparedQueueGroupOutcome::ClaimReturned)
		})())
	}
}
