//! Exact operator changes retain payload interpretation and create no worker authority.

use super::{JobLifecycle, NativeJob, QueueJobSchemaBinding, QueueRepresentationError};
use crate::queue::JobStatus;
use crate::queue::group_state::QueueRequeueSchedule;
use crate::records::StateTimestampMicros;

/// An explicit operator change, independent of private worker acquisition material.
#[derive(Clone, Copy)]
pub(in crate::queue) enum QueueJobOperatorAction {
	CancelPending,
	RetryFailed { schedule: QueueRequeueSchedule },
}

impl QueueJobOperatorAction {
	pub(super) fn expected_status(self) -> JobStatus {
		match self {
			Self::CancelPending => JobStatus::Pending,
			Self::RetryFailed { .. } => JobStatus::Failed,
		}
	}

	pub(in crate::queue) fn operation_name(self) -> &'static str {
		match self {
			Self::CancelPending => "queue.native_group.cancel_pending",

			Self::RetryFailed { .. } => "queue.native_group.retry_failed",
		}
	}
}

/// An observed protected result; no variant claims nonblocking acquisition or known commit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::queue) enum QueueJobOperatorOutcome {
	Applied,
	NotFound,
	StateMismatch {
		expected: JobStatus,
		actual: JobStatus,
	},
}

/// A pure prepared change under the actual bound job owner, without persistence authority.
pub(in crate::queue) enum QueueJobOperatorPreparation {
	StateMismatch(QueueJobOperatorOutcome),
	Delete,
	Write(Box<NativeJob>),
}

impl QueueJobSchemaBinding<'_> {
	pub(in crate::queue) fn prepare_operator_action(
		&self,
		state: NativeJob,
		action: QueueJobOperatorAction,
		observed_at: StateTimestampMicros,
	) -> Result<QueueJobOperatorPreparation, QueueRepresentationError> {
		self.require_native_writer()?;
		self.check_identity(&state)?;
		let expected = action.expected_status();
		let actual = state.job().status();
		if expected != actual {
			return Ok(QueueJobOperatorPreparation::StateMismatch(
				QueueJobOperatorOutcome::StateMismatch { expected, actual },
			));
		}
		if matches!(action, QueueJobOperatorAction::CancelPending) {
			return Ok(QueueJobOperatorPreparation::Delete);
		}
		let mut parts = state.into_parts();
		let job = &mut parts.job;
		match action {
			QueueJobOperatorAction::RetryFailed { schedule } => {
				job.lifecycle = JobLifecycle::Pending;
				job.retry_count = 0;
				job.last_error_msg = None;
				job.run_at_or_after_unix_micros = match schedule {
					QueueRequeueSchedule::ObservedNow => observed_at.as_i64(),
					QueueRequeueSchedule::At(at) => at.as_unix_micros(),
				};
			}

			QueueJobOperatorAction::CancelPending => {
				unreachable!("the selected cancellation returned its delete preparation")
			}
		}
		job.updated_at_unix_micros = observed_at.as_i64();
		Ok(QueueJobOperatorPreparation::Write(Box::new(
			NativeJob::new(parts)?,
		)))
	}
}
