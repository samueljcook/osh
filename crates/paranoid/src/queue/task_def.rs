//! The definition of a task, which producers and workers share.

use super::{Duration, JobPolicyError, MAX_RETRIES, PhantomData, TaskName, fmt};

/// How long a worker lets a job run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobTimeout {
	/// Run without a queue-enforced timeout.
	NoTimeout,
	/// Stop the job after this duration. The duration must be positive.
	ExpiresAfter(Duration),
}

/// The attempt budget and the timeout that every job of a task records when it is enqueued.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JobPolicy {
	/// Maximum number of retries after the first attempt.
	pub max_retries: u32,
	/// Execution timeout.
	pub timeout: JobTimeout,
}

impl JobPolicy {
	/// Checks that a stored job can hold the policy: the retry count as a 32-bit integer and a
	/// nonzero timeout as 64-bit nanoseconds.
	pub(super) fn require_storable(self) -> Result<(), JobPolicyError> {
		if self.max_retries > MAX_RETRIES {
			return Err(JobPolicyError::MaxRetriesTooLarge);
		}
		if let JobTimeout::ExpiresAfter(duration) = self.timeout
			&& (duration.is_zero() || i64::try_from(duration.as_nanos()).is_err())
		{
			return Err(JobPolicyError::InvalidTimeout);
		}
		Ok(())
	}
}

/// The name, the payload type, and the [`JobPolicy`] of one task.
///
/// A producer enqueues jobs with a definition and a worker registers its handler with the same
/// definition, so both agree on the task's name, on its payload type, and on the attempt budget
/// and timeout that each job records. A definition can be a `const`:
///
/// ```rust
/// use paranoid::queue::{JobPolicy, JobTimeout, TaskDef, TaskName};
/// use std::time::Duration;
///
/// struct SendEmail;
///
/// const SEND_EMAIL: TaskDef<SendEmail> = TaskDef::new(
///     TaskName::from_static("send-email"),
///     JobPolicy {
///         max_retries: 5,
///         timeout: JobTimeout::ExpiresAfter(Duration::from_secs(60)),
///     },
/// );
/// ```
pub struct TaskDef<T> {
	name: TaskName,
	policy: JobPolicy,
	payload_type: PhantomData<fn() -> T>,
}

impl<T> TaskDef<T> {
	/// Defines a task.
	pub const fn new(name: TaskName, policy: JobPolicy) -> Self {
		Self {
			name,
			policy,
			payload_type: PhantomData,
		}
	}

	/// Returns the task name.
	pub fn name(&self) -> &TaskName {
		&self.name
	}

	/// Returns the job policy.
	pub fn policy(&self) -> JobPolicy {
		self.policy
	}
}

impl<T> Clone for TaskDef<T> {
	fn clone(&self) -> Self {
		Self {
			name: self.name.clone(),
			policy: self.policy,
			payload_type: PhantomData,
		}
	}
}

impl<T> fmt::Debug for TaskDef<T> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("TaskDef")
			.field("name", &self.name)
			.field("policy", &self.policy)
			.finish()
	}
}
