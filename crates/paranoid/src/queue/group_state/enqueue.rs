//! Enqueue preserves the published pause and active dedupe behavior in one atomic group.
use super::{
	BoundQueueGroup, JobId, JobLifecycle, JobRunAtOrAfter, NativeJob, NativeJobMetadata,
	NativeJobParts, NativeJobPayload, PlanBuilder, PreparedQueueGroupOutcome, QueueGroupError,
	QueueGroupPreparation, QueueGroupSnapshot, QueuePauseScope, QueueRepresentationError,
	at_record, preparation,
};
use crate::queue::state::NativePayloadEncoding;
use crate::queue::{DedupeKey, StoredJobTimeout, TaskName};

/// One new job's complete stored meaning, already encoded by its task's producer.
pub(crate) struct NativeEnqueueRequest {
	pub(in crate::queue) job_id: JobId,
	pub(in crate::queue) task_name: TaskName,
	pub(in crate::queue) payload_json: String,
	pub(in crate::queue) payload_encoding: NativePayloadEncoding,
	pub(in crate::queue) run_at_or_after: Option<JobRunAtOrAfter>,
	pub(in crate::queue) max_retries: u32,
	pub(in crate::queue) timeout: StoredJobTimeout,
	pub(in crate::queue) dedupe_key: Option<DedupeKey>,
}

/// How an enqueue learns whether the queue or its task is paused.
pub(crate) enum EnqueuePauses {
	/// The group holds both pause records. During data retirement a pause can still be a v10
	/// row, which only the group reads.
	Held,
	/// One read that took no lock observed both pause records, as v10's enqueue reads its
	/// pauses. No enqueue waits for another enqueue's pause read.
	Observed {
		queue_paused: bool,
		task_paused: bool,
	},
}

/// Why an enqueue inserted no job, matching the published enqueue outcomes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeEnqueueRejection {
	QueuePaused,
	TaskPaused,
}

impl BoundQueueGroup<'_, '_, '_> {
	/// Inserts one pending job, or returns the active job that already owns its dedupe key.
	/// A paused queue or task inserts nothing.
	pub(in crate::queue) fn prepare_enqueue(
		&self,
		snapshot: &QueueGroupSnapshot<'_>,
		request: &NativeEnqueueRequest,
		pauses: &EnqueuePauses,
	) -> Result<QueueGroupPreparation, QueueGroupError> {
		preparation((|| {
			let task_scope = QueuePauseScope::Task(request.task_name.clone());
			let (queue_paused, task_paused) = match pauses {
				EnqueuePauses::Held => (
					self.read_pause(snapshot, &QueuePauseScope::Global)?
						.is_some(),
					self.read_pause(snapshot, &task_scope)?.is_some(),
				),
				EnqueuePauses::Observed {
					queue_paused,
					task_paused,
				} => (*queue_paused, *task_paused),
			};
			if queue_paused {
				return PlanBuilder::new().finish(
					snapshot,
					PreparedQueueGroupOutcome::EnqueueRejected(NativeEnqueueRejection::QueuePaused),
				);
			}
			if task_paused {
				return PlanBuilder::new().finish(
					snapshot,
					PreparedQueueGroupOutcome::EnqueueRejected(NativeEnqueueRejection::TaskPaused),
				);
			}
			if let Some(dedupe) = &request.dedupe_key {
				let binding = self.dedupe_binding(&request.task_name, dedupe)?;
				let (owner, _) =
					self.active_owner(snapshot, &binding, &request.task_name, dedupe)?;
				if let Some(owner) = owner {
					return PlanBuilder::new().finish(
						snapshot,
						PreparedQueueGroupOutcome::Enqueued {
							job_id: owner,
							deduplicated: true,
						},
					);
				}
			}
			if self.read_job(snapshot, request.job_id)?.is_some() {
				return Err(QueueGroupError::IdentityCollision);
			}
			let now = snapshot.observed_at.as_i64();
			let job = NativeJob::new(NativeJobParts {
				job: NativeJobMetadata {
					id: request.job_id,
					task_name: request.task_name.clone(),
					lifecycle: JobLifecycle::Pending,
					run_at_or_after_unix_micros: request
						.run_at_or_after
						.map_or(now, JobRunAtOrAfter::as_unix_micros),
					last_error_msg: None,
					retry_count: 0,
					max_retries: request.max_retries,
					timeout: request.timeout,
					dedupe_key: request.dedupe_key.clone(),
					created_at_unix_micros: now,
					updated_at_unix_micros: now,
				},
				payload: NativeJobPayload::InlineJson(request.payload_json.clone()),
				payload_encoding: request.payload_encoding.clone(),
			})
			.map_err(|error| {
				at_record(
					self.job_identity(request.job_id),
					QueueRepresentationError::from(error),
				)
			})?;
			let mut plan = PlanBuilder::new();
			self.reconcile_dedupe(snapshot, None, Some(&job), &mut plan)?;
			self.plan_job_write(snapshot, &job, &mut plan)?;
			plan.finish(
				snapshot,
				PreparedQueueGroupOutcome::Enqueued {
					job_id: request.job_id,
					deduplicated: false,
				},
			)
		})())
	}
}
