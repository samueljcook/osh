//! Legal persisted job states and the private resources of a running claim.

use super::{QueueStateError, invalid};
use crate::queue::{JobStatus, StoredJobLifecycleFields, WorkerClaimToken, WorkerOwnerId};

#[derive(Clone, Eq, PartialEq)]
pub(in crate::queue) enum JobLifecycle {
	Pending,
	Running(RunningJob),
	Completed { finished_at: i64 },
	Failed { finished_at: i64 },
}

#[derive(Clone, Eq, PartialEq)]
pub(in crate::queue) struct RunningJob {
	pub(in crate::queue) owner: WorkerOwnerId,
	pub(in crate::queue) claimed_at: i64,
	pub(in crate::queue) execution: JobExecution,
	pub(in crate::queue) heartbeat_at: i64,
	pub(in crate::queue) claim: RunningJobClaim,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(in crate::queue) enum JobExecution {
	Unstarted,
	Started { at: i64 },
}

#[derive(Clone, Eq, PartialEq)]
pub(in crate::queue) enum RunningJobClaim {
	NativePrivate(WorkerClaimToken),
	RetiredPublished,
}

impl JobLifecycle {
	/// Parses untrusted stored lifecycle fields together with the claim that the record stores.
	/// A running job requires a claim, and every other job has none.
	pub(in crate::queue) fn parse(
		fields: StoredJobLifecycleFields,
		claim: Option<RunningJobClaim>,
	) -> Result<Self, QueueStateError> {
		use crate::queue::JobLifecycle as Observed;
		let observed = Observed::from_stored_fields(fields)
			.ok_or_else(|| invalid("job status, ownership, and lifecycle fields disagree"))?;
		match (observed, claim) {
			(Observed::Pending, None) => Ok(Self::Pending),
			(
				Observed::Completed {
					finished_at_unix_micros,
				},
				None,
			) => Ok(Self::Completed {
				finished_at: finished_at_unix_micros,
			}),
			(
				Observed::Failed {
					finished_at_unix_micros,
				},
				None,
			) => Ok(Self::Failed {
				finished_at: finished_at_unix_micros,
			}),
			(
				Observed::Running {
					worker_owner_id,
					claimed_at_unix_micros,
					execution,
					execution_heartbeat_at_unix_micros,
				},
				Some(claim),
			) => Ok(Self::Running(RunningJob {
				owner: worker_owner_id,
				claimed_at: claimed_at_unix_micros,
				execution: match execution {
					crate::queue::JobExecution::Started {
						started_at_unix_micros,
					} => JobExecution::Started {
						at: started_at_unix_micros,
					},
					crate::queue::JobExecution::Unstarted => JobExecution::Unstarted,
				},
				heartbeat_at: execution_heartbeat_at_unix_micros,
				claim,
			})),
			(Observed::Running { .. }, None) => {
				Err(invalid("running jobs require explicit ownership"))
			}
			_ => Err(invalid(
				"job status, ownership, and lifecycle fields disagree",
			)),
		}
	}

	/// The lifecycle that applications observe, without the claim's private material.
	pub(in crate::queue) fn observed(&self) -> crate::queue::JobLifecycle {
		use crate::queue::JobLifecycle as Observed;
		match self {
			Self::Pending => Observed::Pending,
			Self::Running(running) => Observed::Running {
				worker_owner_id: running.owner.clone(),
				claimed_at_unix_micros: running.claimed_at,
				execution: match running.execution {
					JobExecution::Unstarted => crate::queue::JobExecution::Unstarted,
					JobExecution::Started { at } => crate::queue::JobExecution::Started {
						started_at_unix_micros: at,
					},
				},
				execution_heartbeat_at_unix_micros: running.heartbeat_at,
			},
			Self::Completed { finished_at } => Observed::Completed {
				finished_at_unix_micros: *finished_at,
			},
			Self::Failed { finished_at } => Observed::Failed {
				finished_at_unix_micros: *finished_at,
			},
		}
	}

	pub(in crate::queue) fn status(&self) -> JobStatus {
		match self {
			Self::Pending => JobStatus::Pending,
			Self::Running(_) => JobStatus::Running,
			Self::Completed { .. } => JobStatus::Completed,
			Self::Failed { .. } => JobStatus::Failed,
		}
	}

	pub(in crate::queue) fn running(&self) -> Option<&RunningJob> {
		match self {
			Self::Running(running) => Some(running),
			Self::Pending | Self::Completed { .. } | Self::Failed { .. } => None,
		}
	}

	pub(in crate::queue) fn finished_at(&self) -> Option<i64> {
		match self {
			Self::Completed { finished_at } | Self::Failed { finished_at } => Some(*finished_at),
			Self::Pending | Self::Running(_) => None,
		}
	}
}

impl RunningJob {
	pub(in crate::queue) fn started_at(&self) -> Option<i64> {
		match self.execution {
			JobExecution::Unstarted => None,
			JobExecution::Started { at } => Some(at),
		}
	}
}
