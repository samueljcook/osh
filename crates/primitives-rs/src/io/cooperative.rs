//! Cooperative cancellation and byte progress for synchronous I/O.
//!
//! An [`IoOperation`] owns one operation's lifetime. Its cloneable cancellation
//! handle can be used from another thread. Reads check cancellation between
//! bounded calls; a blocking syscall cannot be interrupted by this facility.
//! Progress counts source bytes consumed, including any size-limit probe byte.
//! It does not indicate that parsing, writing, or publication has completed.
//!
//! [`CooperativeReader`] borrows an operation so that cancellation can cover work
//! after EOF. Call [`IoOperation::finish_cancellable_work`] when cancellable work ends,
//! before any subsequent non-cancellable commit. This resolves cancellation but
//! does not perform or attest to that commit. Atomic file publication consumes the operation
//! through `fs::WriteFromReaderOptions` when the
//! `fs` feature is enabled and resolves cancellation at its publication boundary.
//!
//! ```
//! use primitives_rs::io::cooperative::{CooperativeReader, IoOperation,
//!     IoOperationOptions, ReaderOptions};
//! use std::io::Read;
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mut operation = IoOperation::new(IoOperationOptions { on_progress_fn: None });
//! let cancellation = operation.cancellation_handle();
//! // Another thread can call cancellation.cancel().
//! let mut output = Vec::new();
//! {
//!     let mut reader = CooperativeReader::new(&b"input"[..], &mut operation,
//!         ReaderOptions { max_read_size: 8192.try_into()? });
//!     reader.read_to_end(&mut output)?;
//! }
//! operation.finish_cancellable_work()?;
//! # drop(cancellation);
//! # Ok(())
//! # }
//! ```

use std::fmt;
use std::io::{self, Read};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy)]
#[repr(u8)]
enum IoOperationState {
	Active,
	Cancelled,
	Closed,
}

impl IoOperationState {
	fn from_atomic(value: u8) -> Self {
		match value {
			value if value == Self::Active as u8 => Self::Active,
			value if value == Self::Cancelled as u8 => Self::Cancelled,
			value if value == Self::Closed as u8 => Self::Closed,
			_ => unreachable!("I/O operation states are Active, Cancelled, and Closed"),
		}
	}
}

/// Cumulative source consumption for one operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadProgress {
	/// Number of source bytes consumed, excluding comparison reads of stored files.
	pub bytes_read: u64,
}

/// Optional observation of source consumption.
pub struct IoOperationOptions<'a> {
	/// Called synchronously after each nonempty read, with cumulative consumption.
	/// Callback errors stop the operation, even when their kind is `Interrupted`.
	/// A callback may request cancellation through a cancellation handle. Callbacks
	/// must return promptly; cancellation cannot interrupt a running callback.
	pub on_progress_fn: Option<Box<dyn FnMut(ReadProgress) -> io::Result<()> + Send + 'a>>,
}

/// The result of requesting cancellation of one operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelOutcome {
	/// Cancellation was accepted, or had already been accepted. An atomic writer
	/// using this operation will not publish its temporary file.
	Accepted,
	/// The operation already finished, failed, was dropped, or began publication.
	/// Its actual result determines whether publication succeeded.
	TooLate,
}

/// A cloneable handle for requesting cancellation from any thread.
#[derive(Clone, Debug)]
pub struct CancellationHandle(Arc<AtomicU8>);

impl CancellationHandle {
	/// Atomically resolves cancellation against completion or publication.
	/// Acceptance prevents publication but does not undo reads or interrupt a
	/// pending syscall. A concurrent I/O failure may still be the returned error.
	#[must_use]
	pub fn cancel(&self) -> CancelOutcome {
		match self.0.compare_exchange(
			IoOperationState::Active as u8,
			IoOperationState::Cancelled as u8,
			Ordering::AcqRel,
			Ordering::Acquire,
		) {
			Ok(_) => CancelOutcome::Accepted,
			Err(value) => match IoOperationState::from_atomic(value) {
				IoOperationState::Cancelled => CancelOutcome::Accepted,
				IoOperationState::Closed => CancelOutcome::TooLate,
				IoOperationState::Active => {
					unreachable!("a failed compare_exchange observed a different state")
				}
			},
		}
	}
}

/// A failure in cooperative I/O control.
#[derive(Debug)]
pub enum IoControlError {
	/// Cancellation was accepted before completion or publication.
	Cancelled,
	/// A progress callback failed after the reported bytes had been consumed.
	ProgressCallback(io::Error),
	/// The cumulative source byte count would exceed `u64::MAX`.
	ProgressOverflow,
	/// A previous control failure stopped this operation.
	Stopped,
}

impl fmt::Display for IoControlError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Cancelled => f.write_str("I/O operation cancelled"),
			Self::ProgressCallback(error) => write!(f, "I/O progress callback failed: {error}"),
			Self::ProgressOverflow => f.write_str("I/O progress exceeds u64::MAX bytes"),
			Self::Stopped => f.write_str("I/O operation already stopped"),
		}
	}
}

impl std::error::Error for IoControlError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::ProgressCallback(error) => Some(error),
			_ => None,
		}
	}
}

/// One I/O operation's cancellation state and progress callback.
///
/// This value cannot be cloned or reset. Dropping it closes cancellation without
/// claiming success. Create a new operation for each independent attempt.
pub struct IoOperation<'a> {
	state: Arc<AtomicU8>,
	options: IoOperationOptions<'a>,
	bytes_read: u64,
}

impl<'a> IoOperation<'a> {
	/// Creates an operation with zero consumed bytes and no cancellation request.
	pub fn new(options: IoOperationOptions<'a>) -> Self {
		Self {
			state: Arc::new(AtomicU8::new(IoOperationState::Active as u8)),
			options,
			bytes_read: 0,
		}
	}

	/// Returns a handle that can request cancellation of this operation.
	pub fn cancellation_handle(&self) -> CancellationHandle {
		CancellationHandle(Arc::clone(&self.state))
	}

	/// Returns the cumulative number of consumed source bytes.
	pub fn bytes_read(&self) -> u64 {
		self.bytes_read
	}

	/// Checks whether work may continue. This check does not reserve publication;
	/// a later cancellation request can still be accepted.
	pub fn check_cancellation(&self) -> Result<(), IoControlError> {
		match IoOperationState::from_atomic(self.state.load(Ordering::Acquire)) {
			IoOperationState::Active => Ok(()),
			IoOperationState::Cancelled => Err(IoControlError::Cancelled),
			IoOperationState::Closed => Err(IoControlError::Stopped),
		}
	}

	/// Ends cancellable work and consumes this operation. An accepted cancellation
	/// returns `Cancelled`; otherwise, later cancellation requests return `TooLate`.
	/// Call this after reading and processing, before any non-cancellable commit.
	/// Success permits that commit; it does not perform the commit or establish its
	/// result. The caller must still await and handle the actual commit result.
	pub fn finish_cancellable_work(self) -> Result<(), IoControlError> {
		self.begin_commit()
	}

	pub(crate) fn begin_commit(&self) -> Result<(), IoControlError> {
		match self.state.compare_exchange(
			IoOperationState::Active as u8,
			IoOperationState::Closed as u8,
			Ordering::AcqRel,
			Ordering::Acquire,
		) {
			Ok(_) => Ok(()),
			Err(value) => match IoOperationState::from_atomic(value) {
				IoOperationState::Cancelled => Err(IoControlError::Cancelled),
				IoOperationState::Closed => Err(IoControlError::Stopped),
				IoOperationState::Active => {
					unreachable!("a failed compare_exchange observed a different state")
				}
			},
		}
	}

	pub(crate) fn read(
		&mut self,
		reader: &mut (impl Read + ?Sized),
		buffer: &mut [u8],
	) -> io::Result<usize> {
		self.check_cancellation().map_err(io::Error::other)?;
		let count = reader.read(buffer)?;
		let Some(total) = self.bytes_read.checked_add(count as u64) else {
			self.close();
			return Err(io::Error::other(IoControlError::ProgressOverflow));
		};
		self.bytes_read = total;
		self.check_cancellation().map_err(io::Error::other)?;
		if count != 0
			&& let Some(callback) = self.options.on_progress_fn.as_mut()
			&& let Err(error) = callback(ReadProgress { bytes_read: total })
		{
			self.close();
			return Err(io::Error::other(IoControlError::ProgressCallback(error)));
		}
		self.check_cancellation().map_err(io::Error::other)?;
		Ok(count)
	}

	fn close(&self) {
		let _ = self.state.compare_exchange(
			IoOperationState::Active as u8,
			IoOperationState::Closed as u8,
			Ordering::AcqRel,
			Ordering::Acquire,
		);
	}
}

impl Drop for IoOperation<'_> {
	fn drop(&mut self) {
		self.close();
	}
}

/// The maximum source read size between progress and cancellation checks.
#[derive(Clone, Copy, Debug)]
pub struct ReaderOptions {
	/// Maximum buffer length passed to each source read. This bounds byte work,
	/// not elapsed time in a source read or a callback.
	pub max_read_size: NonZeroUsize,
}

/// A synchronous reader that bounds each source read and observes an operation.
///
/// Control failures use `io::ErrorKind::Other` with an [`IoControlError`] payload,
/// so cancellation cannot be confused with EOF or retried as `Interrupted`.
/// Ordinary source errors retain their kind and payload. A read that fails after
/// the source returns may have consumed bytes and changed its caller's buffer.
/// Repeated reads after cancellation fail without reading the source.
pub struct CooperativeReader<'operation, 'callback, R> {
	reader: R,
	operation: &'operation mut IoOperation<'callback>,
	options: ReaderOptions,
}

impl<'operation, 'callback, R: Read> CooperativeReader<'operation, 'callback, R> {
	/// Borrows one operation for the lifetime of this adapter.
	pub fn new(
		reader: R,
		operation: &'operation mut IoOperation<'callback>,
		options: ReaderOptions,
	) -> Self {
		Self {
			reader,
			operation,
			options,
		}
	}

	/// Returns the source reader and releases the operation borrow.
	pub fn into_inner(self) -> R {
		self.reader
	}
}

impl<R: Read> Read for CooperativeReader<'_, '_, R> {
	fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
		let size = buffer.len().min(self.options.max_read_size.get());
		self.operation.read(&mut self.reader, &mut buffer[..size])
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn progress_overflow_stops_without_wrapping_or_reporting_a_false_count() {
		let mut operation = IoOperation::new(IoOperationOptions {
			on_progress_fn: Some(Box::new(|_| panic!("overflow must not report progress"))),
		});
		operation.bytes_read = u64::MAX - 1;
		let cancellation = operation.cancellation_handle();
		let mut source = &b"abc"[..];
		let error = operation.read(&mut source, &mut [0; 2]).unwrap_err();
		assert!(matches!(
			error.get_ref().unwrap().downcast_ref::<IoControlError>(),
			Some(IoControlError::ProgressOverflow)
		));
		assert_eq!(operation.bytes_read(), u64::MAX - 1);
		assert!(matches!(
			operation.check_cancellation(),
			Err(IoControlError::Stopped)
		));
		assert_eq!(source, b"c");
		assert_eq!(cancellation.cancel(), CancelOutcome::TooLate);
	}
}
