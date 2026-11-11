//! Reads that distinguish complete input from input that exceeds a byte limit.
//!
//! The limit applies to the final destination length, including existing bytes.
//! At the limit, a one-byte read distinguishes EOF from overflow. An overflow
//! consumes that extra byte without appending it. Earlier bytes remain available
//! in the destination after any failure.
//!
//! ```
//! use primitives_rs::io::bounded_reads::{read_to_end, ReadOptions};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mut source = &b"hello"[..];
//! let mut bytes = Vec::new();
//! read_to_end(&mut source, &mut bytes, ReadOptions { max_size: 5 })?;
//! assert_eq!(bytes, b"hello");
//! # Ok(())
//! # }
//! ```

use std::{fmt, io};

/// Limits for a read into an application-owned byte buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadOptions {
	/// Maximum final destination length, in bytes. Zero permits only empty input.
	pub max_size: usize,
}

/// A bounded read failed. Previously appended bytes remain in the destination.
#[derive(Debug)]
pub enum ReadError {
	/// The destination or remaining input exceeds the configured byte limit.
	LimitExceeded {
		/// The configured maximum destination length.
		max: usize,
	},
	/// The destination could not reserve memory before the next read.
	AllocationFailed,
	/// The reader failed for a reason other than interruption.
	Io(io::Error),
}
impl fmt::Display for ReadError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::LimitExceeded { max } => write!(f, "input exceeds {max} bytes"),
			Self::AllocationFailed => f.write_str("read buffer allocation failed"),
			Self::Io(error) => error.fmt(f),
		}
	}
}
impl std::error::Error for ReadError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Io(error) => Some(error),
			_ => None,
		}
	}
}

const READ_CHUNK_SIZE: usize = 8192;

/// Appends input through EOF and returns the number of appended bytes.
///
/// Interrupted reads are retried. At the limit this operation still reads one
/// byte, so an exact-limit input must reach EOF before this operation succeeds.
/// An initially oversized destination fails without reading or changing it.
pub fn read_to_end<R: io::Read + ?Sized>(
	reader: &mut R,
	output: &mut Vec<u8>,
	options: ReadOptions,
) -> Result<usize, ReadError> {
	let initial_size = output.len();
	let mut chunk = [0; READ_CHUNK_SIZE];
	loop {
		let available = prepare_read(output, options)?;
		let count = match reader.read(&mut chunk[..available]) {
			Ok(0) => return Ok(output.len() - initial_size),
			Ok(count) => count,
			Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
			Err(error) => return Err(ReadError::Io(error)),
		};
		append_read(output, &chunk[..count], options)?;
	}
}

/// Appends Tokio asynchronous input with the same semantics as [`read_to_end`].
///
/// Cancelling the future retains bytes already appended to the destination.
/// The function creates no tasks or timeouts; callers control its lifetime.
#[cfg(feature = "io-async")]
pub async fn read_to_end_async<R: tokio::io::AsyncRead + Unpin + ?Sized>(
	reader: &mut R,
	output: &mut Vec<u8>,
	options: ReadOptions,
) -> Result<usize, ReadError> {
	use tokio::io::AsyncReadExt;
	let initial_size = output.len();
	let mut chunk = [0; READ_CHUNK_SIZE];
	loop {
		let available = prepare_read(output, options)?;
		let count = match reader.read(&mut chunk[..available]).await {
			Ok(0) => return Ok(output.len() - initial_size),
			Ok(count) => count,
			Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
			Err(error) => return Err(ReadError::Io(error)),
		};
		append_read(output, &chunk[..count], options)?;
	}
}

fn prepare_read(output: &mut Vec<u8>, options: ReadOptions) -> Result<usize, ReadError> {
	let remaining = options
		.max_size
		.checked_sub(output.len())
		.ok_or(ReadError::LimitExceeded {
			max: options.max_size,
		})?;
	let available = remaining.min(READ_CHUNK_SIZE);
	if output.capacity() - output.len() < available {
		let capacity = output
			.capacity()
			.saturating_mul(2)
			.max(output.len() + available)
			.min(options.max_size);
		output
			.try_reserve_exact(capacity - output.len())
			.map_err(|_| ReadError::AllocationFailed)?;
	}
	Ok(available.max(1))
}
fn append_read(output: &mut Vec<u8>, bytes: &[u8], options: ReadOptions) -> Result<(), ReadError> {
	if output.len() == options.max_size {
		return Err(ReadError::LimitExceeded {
			max: options.max_size,
		});
	}
	output.extend_from_slice(bytes);
	Ok(())
}
