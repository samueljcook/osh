use super::{Chunk, Chunker};
use std::{fmt, io};
use zeroize::Zeroizing;

/// Resource policy for one complete input stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadOptions {
	/// Maximum total input size in bytes. Zero permits only empty input.
	///
	/// A one-byte probe distinguishes exact-limit EOF from excess input. The
	/// probe consumes that byte on failure. This bound does not limit elapsed
	/// time, reader-owned memory, or caller-owned output.
	pub max_input_size: u64,
}

/// A reader could not allocate its buffer or complete its input.
#[derive(Debug)]
pub enum ReadError {
	/// The maximum-chunk-size buffer could not be allocated.
	AllocationFailed,
	/// Input continues beyond the configured total size bound.
	InputTooLarge {
		/// The configured maximum input size in bytes.
		max_input_size: u64,
	},
	/// The underlying reader failed for a reason other than interruption.
	Io(io::Error),
	/// An earlier read failed; this call performed no further input operation.
	AlreadyFailed,
}
impl fmt::Display for ReadError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::AllocationFailed => f.write_str("chunk buffer allocation failed"),
			Self::InputTooLarge { max_input_size } => {
				write!(f, "input exceeds {max_input_size} bytes")
			}
			Self::Io(error) => error.fmt(f),
			Self::AlreadyFailed => f.write_str("chunk reader has already failed"),
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

/// A synchronous reader that reuses one maximum-chunk-size buffer.
///
/// A chunk borrows this buffer until its next mutable use. Copy a chunk only
/// when its contents must outlive that borrow. No chunk bytes are copied or
/// allocated merely to return them.
/// The internal input buffer is zeroized on drop, including after failure.
///
/// The reader may read ahead by up to the configured maximum chunk size.
/// Previously returned chunks are partial results until [`Self::read_chunk`]
/// returns `Ok(None)`. A read or size failure does not establish complete input.
/// The first zero-byte read establishes final EOF; this reader does not follow
/// later file growth or establish that a source remained unchanged during use.
pub struct ChunkReader<'c, R> {
	chunker: &'c Chunker,
	reader: R,
	options: ReadOptions,
	buffer: Zeroizing<Vec<u8>>,
	start: usize,
	end: usize,
	bytes_read: u64,
	bytes_emitted: u64,
	state: ChunkReaderState,
}

enum ChunkReaderState {
	Reading,
	InputUnconfirmed,
	Draining,
	Finished,
	Failed,
}
impl<'c, R: io::Read> ChunkReader<'c, R> {
	pub(super) fn new(
		chunker: &'c Chunker,
		reader: R,
		options: ReadOptions,
	) -> Result<Self, ReadError> {
		let mut buffer = Vec::new();
		buffer
			.try_reserve_exact(chunker.options.max_size)
			.map_err(|_| ReadError::AllocationFailed)?;
		buffer.resize(chunker.options.max_size, 0);
		Ok(Self {
			chunker,
			reader,
			options,
			buffer: Zeroizing::new(buffer),
			start: 0,
			end: 0,
			bytes_read: 0,
			bytes_emitted: 0,
			state: ChunkReaderState::Reading,
		})
	}

	/// Returns the next chunk, or `None` after successful EOF.
	///
	/// Ordinary short reads do not finish the input. Interrupted reads retry;
	/// every other I/O error and an exceeded size bound permanently fail this
	/// reader. Calls after failure return [`ReadError::AlreadyFailed`] without reading.
	/// Calls after successful EOF keep returning `None` without reading.
	pub fn read_chunk(&mut self) -> Result<Option<Chunk<'_>>, ReadError> {
		match self.state {
			ChunkReaderState::InputUnconfirmed | ChunkReaderState::Failed => {
				self.state = ChunkReaderState::Failed;
				return Err(ReadError::AlreadyFailed);
			}
			ChunkReaderState::Finished => return Ok(None),
			ChunkReaderState::Reading | ChunkReaderState::Draining => {}
		}
		loop {
			let available = self.end - self.start;
			if available != 0 {
				let size = self.chunker.boundary(&self.buffer[self.start..self.end]);
				// A cut strictly inside a partial window cannot change when more
				// bytes arrive. Otherwise, only EOF or a full window fixes its end.
				if size < available
					|| matches!(self.state, ChunkReaderState::Draining)
					|| available == self.buffer.len()
				{
					let start = self.start;
					let offset = self.bytes_emitted;
					self.start += size;
					self.bytes_emitted += size as u64;
					return Ok(Some(Chunk {
						offset,
						bytes: &self.buffer[start..self.start],
					}));
				}
			} else if matches!(self.state, ChunkReaderState::Draining) {
				self.state = ChunkReaderState::Finished;
				return Ok(None);
			}
			// Caller-owned Read code can unwind after consuming unknown input.
			// Unconfirmed input cannot resume unless this fill completes.
			self.state = ChunkReaderState::InputUnconfirmed;
			if let Err(error) = self.fill_buffer() {
				self.state = ChunkReaderState::Failed;
				return Err(error);
			}
			if matches!(self.state, ChunkReaderState::InputUnconfirmed) {
				self.state = ChunkReaderState::Reading;
			}
		}
	}

	/// Returns the input reader, discarding any unread buffered bytes.
	///
	/// The returned reader has advanced by the read-ahead amount, which may
	/// exceed the number of bytes in returned chunks. An overflow probe also
	/// consumes one byte beyond the configured limit.
	pub fn into_inner(self) -> R {
		self.reader
	}

	fn fill_buffer(&mut self) -> Result<(), ReadError> {
		if self.start != 0 {
			self.buffer.copy_within(self.start..self.end, 0);
			self.end -= self.start;
			self.start = 0;
		}
		while self.end < self.buffer.len()
			&& matches!(self.state, ChunkReaderState::InputUnconfirmed)
		{
			let remaining = self.options.max_input_size - self.bytes_read;
			if remaining == 0 {
				let mut probe = Zeroizing::new([0]);
				match self.reader.read(probe.as_mut()) {
					Ok(0) => self.state = ChunkReaderState::Draining,
					Ok(_) => {
						return Err(ReadError::InputTooLarge {
							max_input_size: self.options.max_input_size,
						});
					}
					Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
					Err(error) => return Err(ReadError::Io(error)),
				}
				continue;
			}
			let capacity = self.buffer.len() - self.end;
			let capacity = remaining.min(capacity as u64) as usize;
			match self
				.reader
				.read(&mut self.buffer[self.end..self.end + capacity])
			{
				Ok(0) => self.state = ChunkReaderState::Draining,
				Ok(count) => {
					assert!(
						count <= capacity,
						"Read returned a size greater than its destination"
					);
					self.end += count;
					self.bytes_read += count as u64;
				}
				Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
				Err(error) => return Err(ReadError::Io(error)),
			}
		}
		Ok(())
	}
}
