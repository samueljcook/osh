use super::Duration;

/// The retry backoff was too large to bind as microseconds.
#[derive(Debug, Eq, PartialEq)]
pub struct RetryBackoffTooLargeError;

pub(super) fn retry_backoff_to_micros(
	duration: Duration,
) -> Result<i64, RetryBackoffTooLargeError> {
	duration
		.as_micros()
		.try_into()
		.map_err(|_| RetryBackoffTooLargeError)
}

/// Whether Postgres text can store the error message of a job.
pub fn is_error_msg_storable(error_msg: &str) -> bool {
	!error_msg.as_bytes().contains(&0)
}
