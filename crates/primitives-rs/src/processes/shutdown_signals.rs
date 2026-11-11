//! Explicit, opt-in operating-system shutdown signal subscriptions.
//!
//! Applications compose [`ShutdownSignalListener::recv`] with their own cleanup and
//! subprocess termination. This module never exits the process or creates a
//! runtime. Tokio's signal handlers remain installed after subscriptions are
//! dropped; dropping a listener does not restore the operating-system default.
//!
//! ```no_run
//! use primitives_rs::processes::shutdown_signals::{ShutdownSignal, ShutdownSignalListener};
//! use tokio_util::sync::CancellationToken;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let cancellation = CancellationToken::new();
//! let mut signals = ShutdownSignalListener::new(&[ShutdownSignal::Interrupt])?;
//! let reason = signals.recv(&cancellation).await?;
//! // The application uses the reason to start its own cleanup.
//! # let _ = reason;
//! # Ok(())
//! # }
//! ```

use std::io;
use tokio_util::sync::CancellationToken;

/// A supported operating-system shutdown signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownSignal {
	/// Unix SIGINT or Windows console Ctrl+C.
	Interrupt,
	/// Unix SIGTERM.
	#[cfg(unix)]
	Terminate,
	/// Unix SIGHUP.
	#[cfg(unix)]
	Hangup,
	/// Unix SIGQUIT.
	#[cfg(unix)]
	Quit,
	/// Windows console Ctrl+Break.
	#[cfg(windows)]
	Break,
}
/// The event that requested application shutdown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownEvent {
	/// The application cancellation token was cancelled.
	Cancelled,
	/// An explicitly subscribed operating-system signal arrived.
	Signal(ShutdownSignal),
}
/// Subscribing to shutdown signals failed.
#[derive(Debug)]
pub enum ShutdownSignalError {
	/// The requested signal list was empty.
	EmptySignalList,
	/// The requested signal list contained this signal more than once.
	DuplicateSignal(ShutdownSignal),
	/// Registering a signal listener failed.
	Registration(io::Error),
}
impl std::fmt::Display for ShutdownSignalError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::EmptySignalList => f.write_str("shutdown signals must be nonempty"),
			Self::DuplicateSignal(signal) => {
				write!(f, "shutdown signal {signal:?} was requested more than once")
			}
			Self::Registration(error) => write!(f, "shutdown signal registration failed: {error}"),
		}
	}
}
impl std::error::Error for ShutdownSignalError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Registration(error) => Some(error),
			Self::EmptySignalList | Self::DuplicateSignal(_) => None,
		}
	}
}
/// Owns subscriptions to an explicit, nonempty set of distinct shutdown signals.
pub struct ShutdownSignalListener {
	listeners: Vec<Listener>,
}
struct Listener {
	signal: ShutdownSignal,
	native: NativeListener,
}
#[cfg(unix)]
type NativeListener = tokio::signal::unix::Signal;
#[cfg(windows)]
enum NativeListener {
	Interrupt(tokio::signal::windows::CtrlC),
	Break(tokio::signal::windows::CtrlBreak),
}
impl ShutdownSignalListener {
	/// Installs every requested listener on the active Tokio I/O runtime.
	///
	/// Registration errors are returned without substituting a different signal.
	/// If a later registration fails, Tokio may retain a process-wide handler for
	/// a signal that was registered earlier in this call.
	pub fn new(signals: &[ShutdownSignal]) -> Result<Self, ShutdownSignalError> {
		if signals.is_empty() {
			return Err(ShutdownSignalError::EmptySignalList);
		}
		if let Some(&duplicate) = signals
			.iter()
			.enumerate()
			.find_map(|(index, signal)| signals[..index].contains(signal).then_some(signal))
		{
			return Err(ShutdownSignalError::DuplicateSignal(duplicate));
		}
		let listeners = signals
			.iter()
			.map(|&signal| {
				Ok(Listener {
					signal,
					native: register(signal)?,
				})
			})
			.collect::<io::Result<_>>()
			.map_err(ShutdownSignalError::Registration)?;
		Ok(Self { listeners })
	}
	/// Waits for an explicit signal or cancellation. This wait is cancellation-safe.
	pub async fn recv(&mut self, cancellation: &CancellationToken) -> io::Result<ShutdownEvent> {
		let futures = self
			.listeners
			.iter_mut()
			.map(|listener| Box::pin(listener.recv()));
		tokio::select! {
			biased;
			_ = cancellation.cancelled() => Ok(ShutdownEvent::Cancelled),
			(result, _, _) = futures_util::future::select_all(futures) => result.map(ShutdownEvent::Signal),
		}
	}
}
impl Listener {
	async fn recv(&mut self) -> io::Result<ShutdownSignal> {
		#[cfg(unix)]
		let received = self.native.recv().await;
		#[cfg(windows)]
		let received = match &mut self.native {
			NativeListener::Interrupt(signal) => signal.recv().await,
			NativeListener::Break(signal) => signal.recv().await,
		};
		received
			.map(|()| self.signal)
			.ok_or_else(|| io::Error::other("shutdown signal listener closed"))
	}
}
#[cfg(unix)]
fn register(signal: ShutdownSignal) -> io::Result<NativeListener> {
	use tokio::signal::unix::{SignalKind, signal as register};
	register(match signal {
		ShutdownSignal::Interrupt => SignalKind::interrupt(),
		ShutdownSignal::Terminate => SignalKind::terminate(),
		ShutdownSignal::Hangup => SignalKind::hangup(),
		ShutdownSignal::Quit => SignalKind::quit(),
	})
}
#[cfg(windows)]
fn register(signal: ShutdownSignal) -> io::Result<NativeListener> {
	match signal {
		ShutdownSignal::Interrupt => {
			tokio::signal::windows::ctrl_c().map(NativeListener::Interrupt)
		}
		ShutdownSignal::Break => tokio::signal::windows::ctrl_break().map(NativeListener::Break),
	}
}
