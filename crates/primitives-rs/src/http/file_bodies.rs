//! HTTP response bodies that read a byte range of an open file while the
//! server writes the response.
//!
//! Enable the `file-bodies` Cargo feature. A [`FileBody`] runs on a Tokio
//! runtime because Tokio's file reads run on its blocking thread pool.
//!
//! ```no_run
//! use std::num::NonZeroUsize;
//! use primitives_rs::http::file_bodies::{FileBody, FileBodyConfig};
//!
//! # async fn read() -> std::io::Result<()> {
//! let file = tokio::fs::File::open("video.mp4").await?;
//! let length = file.metadata().await?.len();
//! let body = FileBody::from_file(file, FileBodyConfig {
//!     start: 0,
//!     length,
//!     max_frame_size: NonZeroUsize::new(64 * 1024).unwrap(),
//! })
//! .await?;
//! // Send `body` as the body of an `http::Response`.
//! # Ok(())
//! # }
//! ```

use std::io;
use std::io::SeekFrom;
use std::num::{NonZeroU64, NonZeroUsize};
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncSeekExt, ReadBuf};

/// The bytes of a file that a [`FileBody`] reads, and the size of its frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileBodyConfig {
	/// The offset of the first byte that the body reads.
	pub start: u64,
	/// The number of bytes that the body reads.
	pub length: u64,
	/// The most bytes that one data frame holds. The body allocates one buffer
	/// of at most this size for each frame.
	pub max_frame_size: NonZeroUsize,
}

/// A response body that reads a byte range of an open file in data frames as
/// the server polls it, so the file is never held whole in memory.
///
/// The body reads exactly the configured number of bytes. Its size hint is
/// the exact number of bytes that remain. When the file ends before the
/// body's last byte, because the file became shorter after it was opened, the
/// body sends one error frame of the kind [`io::ErrorKind::UnexpectedEof`]
/// and ends. A read error also ends the body after one error frame. The body
/// closes the file when it ends. Dropping the body cancels the reading and
/// closes the file; a read that Tokio's blocking thread pool has started
/// finishes first.
#[derive(Debug)]
pub struct FileBody {
	max_frame_size: NonZeroUsize,
	state: FileBodyState,
}

#[derive(Debug)]
enum FileBodyState {
	/// The body reads `remaining` more bytes of `file`. `frame` is the buffer
	/// of the next frame, which a read fills: it is empty until the first poll
	/// of the frame, and it stays allocated while the read is pending.
	Reading {
		file: File,
		remaining: NonZeroU64,
		frame: Vec<u8>,
	},
	/// The body sent its last data frame or its error frame, or it reads no
	/// bytes, and it holds no file.
	Ended,
}

impl FileBody {
	/// Moves the position of `file` to `config.start` and returns a body that
	/// reads `config.length` bytes from there.
	///
	/// A body of zero bytes ends at once, and this function does not move the
	/// file's position for it. The function does not compare the range with
	/// the file's length: a range that reaches past the end of the file ends
	/// the body with an error frame when the reading reaches the end.
	///
	/// # Errors
	///
	/// The error of moving the file's position.
	pub async fn from_file(mut file: File, config: FileBodyConfig) -> io::Result<Self> {
		let Some(remaining) = NonZeroU64::new(config.length) else {
			return Ok(Self {
				max_frame_size: config.max_frame_size,
				state: FileBodyState::Ended,
			});
		};
		file.seek(SeekFrom::Start(config.start)).await?;
		Ok(Self {
			max_frame_size: config.max_frame_size,
			state: FileBodyState::Reading {
				file,
				remaining,
				frame: Vec::new(),
			},
		})
	}
}

impl Body for FileBody {
	type Data = Bytes;
	type Error = io::Error;

	fn poll_frame(
		self: Pin<&mut Self>,
		ctx: &mut Context<'_>,
	) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
		let body = self.get_mut();
		let FileBodyState::Reading {
			file,
			remaining,
			frame,
		} = &mut body.state
		else {
			return Poll::Ready(None);
		};
		let frame_len = usize::try_from(remaining.get())
			.map_or(body.max_frame_size.get(), |remaining| {
				remaining.min(body.max_frame_size.get())
			});
		if frame.len() != frame_len {
			frame.resize(frame_len, 0);
		}
		let mut read_buffer = ReadBuf::new(frame);
		match Pin::new(file).poll_read(ctx, &mut read_buffer) {
			Poll::Pending => return Poll::Pending,
			Poll::Ready(Err(error)) => {
				body.state = FileBodyState::Ended;
				return Poll::Ready(Some(Err(error)));
			}
			Poll::Ready(Ok(())) => {}
		}
		let read_bytes = read_buffer.filled().len();
		if read_bytes == 0 {
			body.state = FileBodyState::Ended;
			return Poll::Ready(Some(Err(io::Error::new(
				io::ErrorKind::UnexpectedEof,
				"the file ended before the last byte of the body's range",
			))));
		}
		let mut chunk = std::mem::take(frame);
		chunk.truncate(read_bytes);
		match NonZeroU64::new(remaining.get() - read_bytes as u64) {
			Some(next) => *remaining = next,
			None => body.state = FileBodyState::Ended,
		}
		Poll::Ready(Some(Ok(Frame::data(Bytes::from(chunk)))))
	}

	fn size_hint(&self) -> SizeHint {
		SizeHint::with_exact(match &self.state {
			FileBodyState::Reading { remaining, .. } => remaining.get(),
			FileBodyState::Ended => 0,
		})
	}
}
