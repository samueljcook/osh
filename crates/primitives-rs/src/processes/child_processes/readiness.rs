use super::{ChildProcess, ProcessError};
use std::{future::Future, io, process::ExitStatus, time::Duration};
use tokio_util::sync::CancellationToken;

/// Time bounds for readiness probes.
#[derive(Clone, Copy, Debug)]
pub struct ReadinessOptions {
	/// The total deadline, including every probe and polling delay.
	pub timeout: Duration,
	/// The delay after a probe reports that the process is not ready.
	pub poll_interval: Duration,
	/// The maximum duration of one probe; an expired probe is retried.
	pub probe_timeout: Duration,
}
/// Readiness failure without captured subprocess output.
#[derive(Debug)]
pub enum ReadinessError {
	/// A duration is zero or exceeds the supported clock range.
	InvalidOptions,
	/// The direct child exited before readiness was accepted.
	ProcessExited(ExitStatus),
	/// Child process supervision failed.
	ChildProcess(ProcessError),
	/// A probe returned an operational error.
	Probe(io::Error),
	/// The caller cancelled the readiness wait.
	Cancelled,
	/// The total readiness deadline expired.
	Timeout,
}
impl std::fmt::Display for ReadinessError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::InvalidOptions => {
				f.write_str("readiness durations must be positive and fit the clock range")
			}
			Self::ProcessExited(status) => write!(f, "process exited before readiness: {status}"),
			Self::ChildProcess(error) => write!(f, "{error}"),
			Self::Probe(error) => write!(f, "readiness probe failed: {error}"),
			Self::Cancelled => f.write_str("readiness wait was cancelled"),
			Self::Timeout => f.write_str("readiness deadline expired"),
		}
	}
}
impl std::error::Error for ReadinessError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::ChildProcess(error) => Some(error),
			Self::Probe(error) => Some(error),
			_ => None,
		}
	}
}
/// Probes readiness while observing process exit, cancellation, and explicit deadlines.
///
/// The callback returns `false` for a retryable state and an error for a terminal
/// failure. An observed child exit takes priority over a successful probe. This
/// function borrows the process, so failure does not terminate it. Each probe must
/// be cancellation-safe because a deadline may drop its future.
pub async fn wait_for_readiness<F, Fut>(
	process: &ChildProcess,
	options: ReadinessOptions,
	cancellation: &CancellationToken,
	mut probe_fn: F,
) -> Result<(), ReadinessError>
where
	F: FnMut() -> Fut,
	Fut: Future<Output = io::Result<bool>>,
{
	let now = tokio::time::Instant::now();
	if options.timeout.is_zero()
		|| options.poll_interval.is_zero()
		|| options.probe_timeout.is_zero()
		|| now.checked_add(options.poll_interval).is_none()
		|| now.checked_add(options.probe_timeout).is_none()
	{
		return Err(ReadinessError::InvalidOptions);
	}
	let deadline = now
		.checked_add(options.timeout)
		.ok_or(ReadinessError::InvalidOptions)?;
	tokio::select! {
		biased;
		_ = cancellation.cancelled() => Err(ReadinessError::Cancelled),
		status = process.wait_for_exit() => match status { Ok(status) => Err(ReadinessError::ProcessExited(status)), Err(error) => Err(ReadinessError::ChildProcess(error)) },
		_ = tokio::time::sleep_until(deadline) => Err(ReadinessError::Timeout),
		result = async {
			loop {
				let ready = match tokio::time::timeout(options.probe_timeout, probe_fn()).await {
					Ok(result) => result.map_err(ReadinessError::Probe)?,
					Err(_) => false,
				};
				if let Some(status) = process.exit_status() { return Err(ReadinessError::ProcessExited(status)); }
				if ready { return Ok(()); }
				tokio::time::sleep(options.poll_interval).await;
			}
		} => result,
	}
}

/// A bounded HTTP/1 GET probe to an explicit loopback socket address.
///
/// A complete response with a 2xx status indicates readiness. Connection errors,
/// malformed responses, and other statuses return `false`. The probe does not use
/// DNS, proxies, TLS, redirects, or response bodies. Wrap it in
/// [`wait_for_readiness`] to enforce time bounds and process ownership.
#[cfg(feature = "child-process-http")]
pub struct LoopbackHttpProbe {
	address: std::net::SocketAddr,
	path: http::uri::PathAndQuery,
}
/// A loopback HTTP probe configuration is invalid.
#[cfg(feature = "child-process-http")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoopbackHttpProbeError {
	/// The socket address is not a loopback address with a nonzero port.
	InvalidAddress,
	/// The request target is not an origin-form target of visible ASCII
	/// characters without a fragment.
	InvalidRequestTarget,
}
#[cfg(feature = "child-process-http")]
impl std::fmt::Display for LoopbackHttpProbeError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(match self {
			Self::InvalidAddress => {
				"HTTP readiness requires a loopback socket address with a nonzero port"
			}
			Self::InvalidRequestTarget => "HTTP readiness requires an origin-form request target",
		})
	}
}
#[cfg(feature = "child-process-http")]
impl std::error::Error for LoopbackHttpProbeError {}
#[cfg(feature = "child-process-http")]
impl LoopbackHttpProbe {
	/// Parses a loopback address and an origin-form request target.
	pub fn new(address: std::net::SocketAddr, path: &str) -> Result<Self, LoopbackHttpProbeError> {
		if !address.ip().is_loopback() || address.port() == 0 {
			return Err(LoopbackHttpProbeError::InvalidAddress);
		}
		if !path.starts_with('/')
			|| path.starts_with("//")
			|| path.contains('#')
			|| !path.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
		{
			return Err(LoopbackHttpProbeError::InvalidRequestTarget);
		}
		let path = path
			.parse()
			.map_err(|_| LoopbackHttpProbeError::InvalidRequestTarget)?;
		Ok(Self { address, path })
	}
	/// Sends one GET request and reads response headers, with a 16 KiB parser buffer.
	pub async fn probe(&self) -> io::Result<bool> {
		let stream = match tokio::net::TcpStream::connect(self.address).await {
			Ok(stream) => stream,
			Err(_) => return Ok(false),
		};
		let handshake = hyper::client::conn::http1::Builder::new()
			.max_buf_size(16 * 1024)
			.handshake::<_, http_body_util::Empty<bytes::Bytes>>(hyper_util::rt::TokioIo::new(
				stream,
			))
			.await;
		let (mut sender, connection) = match handshake {
			Ok(value) => value,
			Err(_) => return Ok(false),
		};
		let request = http::Request::builder()
			.method(http::Method::GET)
			.uri(self.path.as_str())
			.header(http::header::HOST, self.address.to_string())
			.header(http::header::CONNECTION, "close")
			.body(http_body_util::Empty::new())
			.map_err(io::Error::other)?;
		let response = sender.send_request(request);
		tokio::pin!(response);
		let response = tokio::select! {
			biased;
			response = &mut response => response,
			_ = connection => response.await,
		};
		Ok(response.is_ok_and(|response| response.status().is_success()))
	}
}
