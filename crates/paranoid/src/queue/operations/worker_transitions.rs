use crate::queue::validation::RetryBackoffTooLargeError;
use crate::queue::{
	JobNotRunningCause, WorkerPersistenceError, is_error_msg_storable, retry_backoff_to_micros,
};
use std::time::Duration;

/// What an update of one job that a worker's claim owns reported.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OwnedJobUpdate<T> {
	/// The job moved, with the update's result.
	Applied(T),
	/// The job does not exist, is not running, or is not owned by the claim.
	NotRunning,
}

impl<T> OwnedJobUpdate<T> {
	/// The update's result, or the failure that the update reported.
	pub(in crate::queue) fn into_result<E: JobNotRunningCause>(self) -> Result<T, E> {
		match self {
			Self::Applied(value) => Ok(value),
			Self::NotRunning => Err(E::job_not_running()),
		}
	}
}

/// Why the arguments of a retry schedule cannot be stored.
#[derive(Debug, Eq, PartialEq)]
pub enum RetryScheduleInvalidError {
	/// The retry backoff was too large to bind as microseconds.
	RetryBackoffTooLarge,
	/// The error message contained a null byte.
	ErrorMsgContainsNullByte,
}

impl From<RetryScheduleInvalidError> for WorkerPersistenceError {
	fn from(invalid: RetryScheduleInvalidError) -> Self {
		match invalid {
			RetryScheduleInvalidError::RetryBackoffTooLarge => Self::RetryBackoffTooLarge,
			RetryScheduleInvalidError::ErrorMsgContainsNullByte => Self::ErrorMsgContainsNullByte,
		}
	}
}

/// Checks that the stored job can hold a retry schedule's backoff and error message.
pub(in crate::queue) fn validate_retry_schedule(
	retry_after: Duration,
	error_msg: &str,
) -> Result<(), RetryScheduleInvalidError> {
	retry_backoff_to_micros(retry_after)
		.map_err(|RetryBackoffTooLargeError| RetryScheduleInvalidError::RetryBackoffTooLarge)?;
	if !is_error_msg_storable(error_msg) {
		return Err(RetryScheduleInvalidError::ErrorMsgContainsNullByte);
	}
	Ok(())
}
