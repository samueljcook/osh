//! Operator plans preserve exact dedupe occupancy under the existing whole-set snapshot.

use super::{
	BoundQueueGroup, JobId, JobLifecycle, PlanBuilder, PreparedQueueGroupOutcome, QueueGroupError,
	QueueGroupPreparation, QueueGroupSnapshot, QueueSourceRole, at_record, older_than, preparation,
};
use crate::queue::state::{
	QueueJobOperatorAction, QueueJobOperatorOutcome, QueueJobOperatorPreparation,
};

impl BoundQueueGroup<'_, '_, '_> {
	/// Deletes a completed or failed job that finished more than `retention_micros` before the
	/// snapshot's database time. Any other job is unchanged.
	pub(in crate::queue) fn prepare_delete_finished_job(
		&self,
		snapshot: &QueueGroupSnapshot<'_>,
		id: JobId,
		retention_micros: i64,
	) -> Result<QueueGroupPreparation, QueueGroupError> {
		preparation((|| {
			let Some((before, origin)) = self.read_job(snapshot, id)? else {
				return Ok(QueueGroupPreparation::Unchanged);
			};
			let finished_at = match before.job().lifecycle {
				JobLifecycle::Completed { finished_at } | JobLifecycle::Failed { finished_at } => {
					finished_at
				}
				JobLifecycle::Pending | JobLifecycle::Running(_) => {
					return Ok(QueueGroupPreparation::Unchanged);
				}
			};
			if !older_than(finished_at, snapshot.observed_at, retention_micros) {
				return Ok(QueueGroupPreparation::Unchanged);
			}
			let mut plan = PlanBuilder::new();
			self.reconcile_dedupe(snapshot, Some((&before, origin)), None, &mut plan)?;
			self.plan_job_delete(snapshot, &before, &mut plan)?;
			plan.remove_source(self.job_identity(id), origin, QueueSourceRole::Job);
			plan.finish(snapshot, PreparedQueueGroupOutcome::JobDeleted)
		})())
	}

	pub(in crate::queue) fn prepare_job_operator_action(
		&self,
		snapshot: &QueueGroupSnapshot<'_>,
		id: JobId,
		action: QueueJobOperatorAction,
	) -> Result<QueueGroupPreparation, QueueGroupError> {
		preparation((|| {
			let mut plan = PlanBuilder::new();
			let Some((before, origin)) = self.read_job(snapshot, id)? else {
				return plan.finish(
					snapshot,
					PreparedQueueGroupOutcome::JobOperator(QueueJobOperatorOutcome::NotFound),
				);
			};
			let prepared = self
				.job_binding(id)?
				.prepare_operator_action(before.clone(), action, snapshot.observed_at)
				.map_err(|error| at_record(self.job_identity(id), error))?;
			match prepared {
				QueueJobOperatorPreparation::StateMismatch(outcome) => {
					return plan.finish(snapshot, PreparedQueueGroupOutcome::JobOperator(outcome));
				}
				QueueJobOperatorPreparation::Delete => {
					self.reconcile_dedupe(snapshot, Some((&before, origin)), None, &mut plan)?;
					self.plan_job_delete(snapshot, &before, &mut plan)?;
				}
				QueueJobOperatorPreparation::Write(after) => {
					self.reconcile_dedupe(
						snapshot,
						Some((&before, origin)),
						Some(&after),
						&mut plan,
					)?;
					self.plan_job_write(snapshot, &after, &mut plan)?;
				}
			}
			plan.remove_source(self.job_identity(id), origin, QueueSourceRole::Job);
			plan.finish(
				snapshot,
				PreparedQueueGroupOutcome::JobOperator(QueueJobOperatorOutcome::Applied),
			)
		})())
	}
}
