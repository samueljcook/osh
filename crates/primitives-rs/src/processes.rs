//! Child-process management, local locks, and shutdown notifications.
#[cfg(all(
	feature = "child-process",
	any(
		windows,
		target_os = "linux",
		target_os = "android",
		target_os = "freebsd",
		target_vendor = "apple"
	)
))]
pub mod child_processes;
#[cfg(all(feature = "process-lock", any(unix, windows)))]
pub mod local_locks;
#[cfg(all(feature = "shutdown-signals", any(unix, windows)))]
pub mod shutdown_signals;
