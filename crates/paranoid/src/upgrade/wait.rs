//! Checks of the local monotonic waits and deadlines of upgrade operations.

use std::time::Duration;

use primitives_rs::time::Deadline;
use tokio::time::Instant;

/// Returns a `Deadline` at `wait` after the current monotonic time, or `invalid` when `wait` is
/// zero or exceeds the local monotonic clock's range.
pub(super) fn deadline_after_positive_wait<E>(wait: Duration, invalid: E) -> Result<Deadline, E> {
	if wait.is_zero() {
		return Err(invalid);
	}
	Deadline::new_from_base(Instant::now().into_std(), wait).map_err(|_| invalid)
}

/// Returns the error `invalid` unless `wait` is positive and a deadline `wait` after the
/// current monotonic time fits the local clock.
pub(super) fn require_valid_wait<E>(wait: Duration, invalid: E) -> Result<(), E> {
	deadline_after_positive_wait(wait, invalid).map(|_| ())
}

/// Returns the error `elapsed` once the monotonic clock has reached `deadline`.
pub(super) fn require_before_deadline<E>(deadline: Deadline, elapsed: E) -> Result<(), E> {
	if Instant::now().into_std() >= deadline.instant() {
		return Err(elapsed);
	}
	Ok(())
}

/// Converts a `Deadline` to a Tokio `Instant` for Tokio timer primitives.
pub(super) fn tokio_instant(deadline: Deadline) -> Instant {
	Instant::from_std(deadline.instant())
}
