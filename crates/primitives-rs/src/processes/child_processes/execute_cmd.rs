use super::*;
use crate::io::bounded_reads::{ReadError, ReadOptions, read_to_end_async};
use tokio::io::AsyncWriteExt;

/// Output handling for a single execution.
#[derive(Clone, Copy, Debug)]
pub enum OutputHandling {
	/// The child writes to the parent's corresponding stream.
	Inherit,
	/// The operating system discards this stream.
	Discard,
	/// Captures at most this many bytes and terminates the process on overflow.
	Capture {
		/// The maximum accepted output size in bytes.
		max_size: usize,
	},
}
impl OutputHandling {
	fn stdio(self) -> Stdio {
		match self {
			Self::Inherit => Stdio::inherit(),
			Self::Discard => Stdio::null(),
			Self::Capture { .. } => Stdio::piped(),
		}
	}
}
/// Configuration for bounded command execution.
#[derive(Clone, Copy, Debug)]
pub struct CmdExecutionOptions {
	/// Selects process ownership and termination behavior.
	pub process: ChildProcessOptions,
	/// Controls standard output.
	pub stdout: OutputHandling,
	/// Controls standard error.
	pub stderr: OutputHandling,
}
/// A completed execution. Nonzero exit codes are returned without interpretation.
pub struct CmdOutput {
	/// The direct child's exit status.
	pub status: ExitStatus,
	/// Captured standard output; absent when inherited or discarded.
	pub stdout: Option<Vec<u8>>,
	/// Captured standard error; absent when inherited or discarded.
	pub stderr: Option<Vec<u8>>,
}

/// Runs a command with byte input and concurrent, bounded output capture.
///
/// Standard input closes after all bytes have been written. If the child closes its
/// standard input or exits before it reads all bytes, the remaining bytes are
/// discarded, and the result reports the exit status and captured output. Use
/// [`ChildProcess::spawn`] for inherited or streaming input. Cancellation requests
/// graceful termination; dropping this future requests immediate termination.
/// Output overflow and I/O failures also request immediate termination and await
/// cleanup.
pub async fn execute_cmd(
	cmd: &ProcessCmd,
	input: &[u8],
	options: CmdExecutionOptions,
	cancellation: &CancellationToken,
) -> Result<CmdOutput, ProcessError> {
	if cancellation.is_cancelled() {
		return Err(ProcessError::Cancelled);
	}
	let mut process = ChildProcess::spawn(
		cmd,
		ChildProcessStdio {
			stdin: Stdio::piped(),
			stdout: options.stdout.stdio(),
			stderr: options.stderr.stdio(),
		},
		options.process,
	)?;
	let mut stdin = process.take_stdin().expect("execution creates piped input");
	let stdout = process.take_stdout();
	let stderr = process.take_stderr();
	let result = tokio::select! {
		biased;
		_ = cancellation.cancelled() => {
			return match process.terminate().await {
				Ok(_) => Err(ProcessError::Cancelled),
				Err(cleanup) => Err(ProcessError::CleanupFailed { cause: Box::new(ProcessError::Cancelled), cleanup: Box::new(cleanup) }),
			};
		},
		result = async {
			let write = async {
				match stdin.write_all(input).await {
					// A child can close its standard input or exit before it reads all input.
					Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {}
					result => result?,
				}
				drop(stdin);
				Ok::<(), ProcessError>(())
			};
			let (_, stdout, stderr, status) = tokio::try_join!(write,
				read_output(stdout, options.stdout, "stdout"),
				read_output(stderr, options.stderr, "stderr"), process.wait_for_completion())?;
			Ok(CmdOutput { status, stdout, stderr })
		} => result,
	};
	match result {
		Ok(output) => Ok(output),
		Err(cause) => match process.kill().await {
			Ok(_) => Err(cause),
			Err(cleanup) => Err(ProcessError::CleanupFailed {
				cause: Box::new(cause),
				cleanup: Box::new(cleanup),
			}),
		},
	}
}
async fn read_output<R: tokio::io::AsyncRead + Unpin>(
	reader: Option<R>,
	options: OutputHandling,
	stream: &'static str,
) -> Result<Option<Vec<u8>>, ProcessError> {
	let OutputHandling::Capture { max_size } = options else {
		return Ok(None);
	};
	let mut reader = reader.expect("captured output has a pipe");
	let mut output = Vec::new();
	// Keep the bounded reader's scratch buffer out of every enclosing execution future.
	Box::pin(read_to_end_async(
		&mut reader,
		&mut output,
		ReadOptions { max_size },
	))
	.await
	.map_err(|error| match error {
		ReadError::LimitExceeded { .. } => ProcessError::OutputLimitExceeded { stream, max_size },
		ReadError::AllocationFailed => ProcessError::OutputAllocationFailed,
		ReadError::Io(error) => ProcessError::from(error),
	})?;
	Ok(Some(output))
}
