//! Owned subprocess execution with bounded output and cancellation.
//!
//! The child-process module is available on Linux, Android, Apple platforms, FreeBSD,
//! and Windows. Operating-system process creation can still be unavailable in a
//! platform sandbox and returns an I/O error.
//!
//! Each process has one owner. Dropping that owner requests immediate termination;
//! [`ChildProcess::terminate`] first allows the configured graceful shutdown period.
//! Completion includes reaping the direct child and requesting termination of any
//! remaining processes in the owned group. Unix groups do not contain descendants
//! that deliberately change their group or session. Windows groups use job objects.
//!
//! A Tokio runtime with I/O and time enabled must be active. Unix exit observation
//! installs Tokio's process-wide SIGCHLD handler. Applications must not reap these
//! children themselves. Unix cleanup retains the unreaped child until the final
//! group signal, so a reused process ID cannot receive that signal. Each Unix child
//! has a dedicated OS thread that reaps it, from before the child starts until the
//! child is reaped, so a thread or process limit can make spawning fail with an I/O
//! error before the child starts.
//!
//! Native arguments are passed without shell parsing. To run a shell program,
//! explicitly name the shell and its arguments. Command configuration and captured
//! bytes do not implement `Debug` because they can contain credentials.
//!
//! ```no_run
//! use std::{collections::BTreeMap, time::Duration};
//! use primitives_rs::processes::child_processes::{OutputHandling, ProcessEnvironment, CmdExecutionOptions, ProcessCmd, ChildProcessOptions, TerminationScope, TerminationOptions, execute_cmd};
//! use tokio_util::sync::CancellationToken;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let cmd = ProcessCmd {
//!     program: "application-tool".into(),
//!     args: vec!["inspect".into(), "a value with spaces".into()],
//!     current_dir: None,
//!     environment: ProcessEnvironment::Inherit { remove: Vec::new(), set: BTreeMap::new() },
//! };
//! let output = execute_cmd(&cmd, b"input", CmdExecutionOptions {
//!     process: ChildProcessOptions {
//!         termination_scope: TerminationScope::ProcessGroup,
//!         termination: TerminationOptions {
//!             grace_period: Duration::from_secs(2),
//!             reap_timeout: Duration::from_secs(5),
//!         },
//!     },
//!     stdout: OutputHandling::Capture { max_size: 64 * 1024 },
//!     stderr: OutputHandling::Inherit,
//! }, &CancellationToken::new()).await?;
//! if !output.status.success() {
//!     return Err(format!("application-tool exited with {}", output.status).into());
//! }
//! # Ok(())
//! # }
//! ```

use std::{
	collections::BTreeMap,
	ffi::OsString,
	io,
	path::PathBuf,
	process::{ExitStatus, Stdio},
	sync::Arc,
	time::Duration,
};
use tokio::{
	process::{ChildStderr, ChildStdin, ChildStdout},
	sync::watch,
};
use tokio_util::sync::CancellationToken;

mod execute_cmd;
#[cfg(unix)]
#[path = "child_processes/unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "child_processes/windows.rs"]
mod platform;
mod readiness;

pub use execute_cmd::{CmdExecutionOptions, CmdOutput, OutputHandling, execute_cmd};
#[cfg(feature = "child-process-http")]
pub use readiness::{LoopbackHttpProbe, LoopbackHttpProbeError};
pub use readiness::{ReadinessError, ReadinessOptions, wait_for_readiness};

/// The starting environment for a subprocess.
pub enum ProcessEnvironment {
	/// Inherits the environment, removes the listed names, and then applies values.
	Inherit {
		/// These variables are removed before explicit values are applied.
		remove: Vec<OsString>,
		/// These values override inherited variables.
		set: BTreeMap<OsString, OsString>,
	},
	/// Uses exactly these variables.
	Replace(BTreeMap<OsString, OsString>),
}

/// Native command configuration, without implicit shell parsing.
pub struct ProcessCmd {
	/// The executable name or path, with the platform's native search semantics.
	pub program: OsString,
	/// Each element is one argument.
	pub args: Vec<OsString>,
	/// `None` inherits the parent's working directory.
	pub current_dir: Option<PathBuf>,
	/// The subprocess environment policy.
	pub environment: ProcessEnvironment,
}

impl ProcessCmd {
	fn to_std_cmd(&self) -> std::process::Command {
		let mut cmd = std::process::Command::new(&self.program);
		cmd.args(&self.args);
		if let Some(dir) = &self.current_dir {
			cmd.current_dir(dir);
		}
		match &self.environment {
			ProcessEnvironment::Inherit { remove, set } => {
				for name in remove {
					cmd.env_remove(name);
				}
				cmd.envs(set);
			}
			ProcessEnvironment::Replace(values) => {
				cmd.env_clear().envs(values);
			}
		}
		cmd
	}
}

/// The processes that the owner may terminate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminationScope {
	/// Owns the direct child, preserving its inherited terminal process group.
	ChildOnly,
	/// Owns a new Unix process group or Windows job object.
	ProcessGroup,
}

/// Explicit termination policy.
#[derive(Clone, Copy, Debug)]
pub struct TerminationOptions {
	/// Unix sends SIGTERM, waits this duration, and then sends SIGKILL.
	/// Group ownership uses the full grace period so descendants can finish even
	/// after the leader exits. Direct-child ownership stops waiting at child exit.
	/// Windows termination is immediate because job objects have no equivalent
	/// cooperative signal. Zero requests immediate termination on either platform.
	pub grace_period: Duration,
	/// Bounds the wait for direct-child reaping after forced termination.
	/// A timeout reports incomplete cleanup; Unix reaping continues independently.
	pub reap_timeout: Duration,
}

/// Stream ownership for a started process.
pub struct ChildProcessStdio {
	/// The child's standard input.
	pub stdin: Stdio,
	/// The child's standard output.
	pub stdout: Stdio,
	/// The child's standard error.
	pub stderr: Stdio,
}

/// Lifecycle configuration for a subprocess.
#[derive(Clone, Copy, Debug)]
pub struct ChildProcessOptions {
	/// Selects direct-child or group ownership.
	pub termination_scope: TerminationScope,
	/// Controls graceful termination and the reaping deadline.
	pub termination: TerminationOptions,
}

/// A subprocess lifecycle failure. Captured bytes and command values are omitted.
#[derive(Clone, Debug)]
pub enum ProcessError {
	/// A termination duration exceeds the clock range, or `reap_timeout` is zero.
	InvalidTerminationOptions,
	/// No Tokio runtime is active in the calling context.
	MissingTokioRuntime,
	/// An operating-system operation failed.
	Io(Arc<io::Error>),
	/// Cancellation was requested.
	Cancelled,
	/// Captured output exceeded its configured size.
	OutputLimitExceeded {
		/// The affected output stream.
		stream: &'static str,
		/// The maximum accepted size in bytes.
		max_size: usize,
	},
	/// Output allocation failed.
	OutputAllocationFailed,
	/// Forced termination did not complete within the reaping deadline.
	ReapTimeout,
	/// The runtime stopped before the supervisor could report its result.
	SupervisorStopped,
	/// Cleanup failed after an execution failure.
	CleanupFailed {
		/// The original execution failure.
		cause: Box<ProcessError>,
		/// The cleanup failure.
		cleanup: Box<ProcessError>,
	},
}
impl std::fmt::Display for ProcessError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::InvalidTerminationOptions => f.write_str(
				"process durations must fit the clock range and reap_timeout must be positive",
			),
			Self::MissingTokioRuntime => f.write_str("an active Tokio runtime is required"),
			Self::Io(error) => write!(f, "process I/O failed: {error}"),
			Self::Cancelled => f.write_str("process execution was cancelled"),
			Self::OutputLimitExceeded { stream, max_size } => {
				write!(f, "{stream} exceeds {max_size} bytes")
			}
			Self::OutputAllocationFailed => f.write_str("process output allocation failed"),
			Self::ReapTimeout => f.write_str("process reaping deadline expired"),
			Self::SupervisorStopped => f.write_str("process supervisor stopped without a result"),
			Self::CleanupFailed { cause, cleanup } => {
				write!(f, "{cause}; cleanup failed: {cleanup}")
			}
		}
	}
}
impl std::error::Error for ProcessError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Io(error) => Some(error.as_ref()),
			Self::CleanupFailed { cause, .. } => Some(cause.as_ref()),
			_ => None,
		}
	}
}
impl From<io::Error> for ProcessError {
	fn from(value: io::Error) -> Self {
		Self::Io(Arc::new(value))
	}
}

/// The supervised lifecycle of one child process.
#[derive(Clone)]
enum ChildLifecycle {
	/// Supervision has not observed the direct child's exit.
	Running,
	/// The direct child exited, and cleanup continues.
	Exited(ExitStatus),
	/// Supervision ended.
	Completed {
		/// The direct child's exit status, if supervision observed its exit.
		exit: Option<ExitStatus>,
		/// The exit status after complete cleanup, or the supervision failure.
		result: Result<ExitStatus, ProcessError>,
	},
}

impl ChildLifecycle {
	fn complete(&mut self, result: Result<ExitStatus, ProcessError>) {
		let exit = match self {
			Self::Running => None,
			Self::Exited(status) => Some(*status),
			Self::Completed { .. } => unreachable!("supervision completes once"),
		};
		*self = Self::Completed { exit, result };
	}
}

/// Publishes lifecycle transitions. Dropping the publisher before completion
/// completes the lifecycle with `SupervisorStopped`.
struct LifecyclePublisher(watch::Sender<ChildLifecycle>);

impl LifecyclePublisher {
	fn publish_exit(&self, status: ExitStatus) {
		self.0.send_replace(ChildLifecycle::Exited(status));
	}

	fn finish(self, result: Result<ExitStatus, ProcessError>) {
		self.0.send_modify(|lifecycle| lifecycle.complete(result));
	}
}

impl Drop for LifecyclePublisher {
	fn drop(&mut self) {
		self.0.send_if_modified(|lifecycle| {
			if matches!(lifecycle, ChildLifecycle::Completed { .. }) {
				false
			} else {
				lifecycle.complete(Err(ProcessError::SupervisorStopped));
				true
			}
		});
	}
}

#[derive(Clone, Copy)]
enum TerminationRequest {
	None,
	Graceful,
	Immediate,
}

enum SupervisorAction {
	Graceful,
	Force,
}

/// Owns a subprocess and requests immediate cleanup when dropped.
///
/// Pipe handles may be taken for streaming. Callers must drain piped output while
/// waiting because a full pipe blocks its writer. [`execute_cmd`] handles concurrent
/// input and output for bounded capture.
pub struct ChildProcess {
	id: u32,
	stdin: Option<ChildStdin>,
	stdout: Option<ChildStdout>,
	stderr: Option<ChildStderr>,
	termination_request: watch::Sender<TerminationRequest>,
	lifecycle: watch::Receiver<ChildLifecycle>,
}
impl ChildProcess {
	/// Starts a command and supervises it independently of the caller's future.
	pub fn spawn(
		cmd: &ProcessCmd,
		stdio: ChildProcessStdio,
		options: ChildProcessOptions,
	) -> Result<Self, ProcessError> {
		let now = tokio::time::Instant::now();
		if options.termination.reap_timeout.is_zero()
			|| now.checked_add(options.termination.reap_timeout).is_none()
			|| now.checked_add(options.termination.grace_period).is_none()
		{
			return Err(ProcessError::InvalidTerminationOptions);
		}
		let runtime =
			tokio::runtime::Handle::try_current().map_err(|_| ProcessError::MissingTokioRuntime)?;
		let mut native = cmd.to_std_cmd();
		native
			.stdin(stdio.stdin)
			.stdout(stdio.stdout)
			.stderr(stdio.stderr);
		let (mut backend, stdin, stdout, stderr) =
			platform::Backend::spawn(native, options.termination_scope)?;
		let id = backend.id();
		let (termination_request, mut requests) = watch::channel(TerminationRequest::None);
		let (lifecycle_tx, lifecycle) = watch::channel(ChildLifecycle::Running);
		let publisher = LifecyclePublisher(lifecycle_tx);
		let process = Self {
			id,
			stdin,
			stdout,
			stderr,
			termination_request,
			lifecycle,
		};
		runtime.spawn(async move {
            let result = async {
                let action = tokio::select! {
                    biased;
                    request = requests.wait_for(|request| !matches!(request, TerminationRequest::None)) => {
                        match *request.map_err(|_| ProcessError::SupervisorStopped)? {
                            TerminationRequest::Graceful => SupervisorAction::Graceful,
                            TerminationRequest::Immediate => SupervisorAction::Force,
                            TerminationRequest::None => unreachable!("termination waits require a request"),
                        }
                    },
                    status = backend.wait_for_exit() => {
                        publisher.publish_exit(status?);
                        SupervisorAction::Force
                    },
                };
                if matches!(action, SupervisorAction::Graceful) && !options.termination.grace_period.is_zero() {
                    // Windows job objects have no cooperative termination signal, so
                    // Windows termination is immediate.
                    #[cfg(unix)]
                    {
                        backend.request_termination()?;
                        tokio::select! {
                            biased;
                            request = requests.wait_for(|request| matches!(request, TerminationRequest::Immediate)) => { request.map_err(|_| ProcessError::SupervisorStopped)?; },
                            _ = tokio::time::sleep(options.termination.grace_period) => {},
                            status = backend.wait_for_exit(), if options.termination_scope == TerminationScope::ChildOnly => { publisher.publish_exit(status?); },
                        }
                    }
                }
                backend.force_termination()?;
                let status = tokio::time::timeout(options.termination.reap_timeout, backend.reap()).await.map_err(|_| ProcessError::ReapTimeout)??;
                publisher.publish_exit(status);
                Ok(status)
            }.await;
			publisher.finish(result);
        });
		Ok(process)
	}

	/// Returns the process ID for diagnostics; it does not grant ownership.
	pub fn id(&self) -> u32 {
		self.id
	}
	/// Takes the piped standard input once.
	pub fn take_stdin(&mut self) -> Option<ChildStdin> {
		self.stdin.take()
	}
	/// Takes the piped standard output once.
	pub fn take_stdout(&mut self) -> Option<ChildStdout> {
		self.stdout.take()
	}
	/// Takes the piped standard error once.
	pub fn take_stderr(&mut self) -> Option<ChildStderr> {
		self.stderr.take()
	}
	/// Returns the observed direct-child exit status, possibly before cleanup ends.
	pub fn exit_status(&self) -> Option<ExitStatus> {
		match &*self.lifecycle.borrow() {
			ChildLifecycle::Running | ChildLifecycle::Completed { exit: None, .. } => None,
			ChildLifecycle::Exited(status)
			| ChildLifecycle::Completed {
				exit: Some(status), ..
			} => Some(*status),
		}
	}
	/// Waits for direct-child exit or a supervisor failure, without cancelling work.
	/// A supervisor failure before an observed exit returns that failure.
	pub async fn wait_for_exit(&self) -> Result<ExitStatus, ProcessError> {
		let mut lifecycle = self.lifecycle.clone();
		let observed = lifecycle
			.wait_for(|state| !matches!(state, ChildLifecycle::Running))
			.await
			.map_err(|_| ProcessError::SupervisorStopped)?;
		match &*observed {
			ChildLifecycle::Exited(status)
			| ChildLifecycle::Completed {
				exit: Some(status), ..
			} => Ok(*status),
			ChildLifecycle::Completed { exit: None, result } => result.clone(),
			ChildLifecycle::Running => {
				unreachable!("exit observation waits for an exit or a terminal result")
			}
		}
	}
	/// Waits for completion. Cancelling this wait preserves process ownership.
	pub async fn wait_for_completion(&self) -> Result<ExitStatus, ProcessError> {
		let mut lifecycle = self.lifecycle.clone();
		let completed = lifecycle
			.wait_for(|state| matches!(state, ChildLifecycle::Completed { .. }))
			.await
			.map_err(|_| ProcessError::SupervisorStopped)?;
		match &*completed {
			ChildLifecycle::Completed { result, .. } => result.clone(),
			ChildLifecycle::Running | ChildLifecycle::Exited(_) => {
				unreachable!("completion observation waits for a terminal result")
			}
		}
	}
	/// Requests graceful termination and waits for cleanup. The request survives cancellation.
	pub async fn terminate(&self) -> Result<ExitStatus, ProcessError> {
		self.termination_request.send_if_modified(|request| {
			if matches!(request, TerminationRequest::None) {
				*request = TerminationRequest::Graceful;
				true
			} else {
				false
			}
		});
		self.wait_for_completion().await
	}
	/// Requests immediate termination and waits for cleanup. The request survives cancellation.
	pub async fn kill(&self) -> Result<ExitStatus, ProcessError> {
		self.request_immediate_termination();
		self.wait_for_completion().await
	}

	fn request_immediate_termination(&self) {
		self.termination_request.send_if_modified(|request| {
			if matches!(request, TerminationRequest::Immediate) {
				false
			} else {
				*request = TerminationRequest::Immediate;
				true
			}
		});
	}
}
impl Drop for ChildProcess {
	fn drop(&mut self) {
		self.request_immediate_termination();
	}
}
