use super::{Frame, InitialEventIdError, ParseError, Parser};
use bytes::Bytes;
use futures_util::Stream;
use std::{
	fmt,
	num::NonZeroUsize,
	pin::Pin,
	task::{Context, Poll},
};

/// Failure from the byte transport or bounded SSE parser.
#[derive(Debug)]
pub enum ReadError<E> {
	/// The byte stream failed.
	Transport(E),
	/// The input exceeded the parser's frame bound.
	Parse(ParseError),
}
impl<E: fmt::Display> fmt::Display for ReadError<E> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Transport(e) => write!(f, "SSE transport: {e}"),
			Self::Parse(e) => e.fmt(f),
		}
	}
}
impl<E: std::error::Error + 'static> std::error::Error for ReadError<E> {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Transport(error) => Some(error),
			Self::Parse(error) => Some(error),
		}
	}
}

/// A pull-driven SSE decoder. Dropping it drops its transport immediately.
/// No background task or parsed-event queue exists. A transport-owned input
/// chunk can exceed the frame limit; the parser never copies that whole chunk.
pub struct DecoderStream<S> {
	state: DecoderState<S>,
	parser: Parser,
}

enum DecoderState<S> {
	Reading {
		source: Pin<Box<S>>,
		remaining: Bytes,
	},
	Ended,
}
impl<S> DecoderStream<S> {
	/// Wraps a byte stream with an explicit frame bound.
	pub fn new(source: S, max_frame_size: NonZeroUsize) -> Self {
		Self::from_parser(source, Parser::new(max_frame_size))
	}
	/// Wraps a resumed connection's byte stream with its previously committed cursor.
	/// The cursor has the same bounds as [`Parser::new_with_event_id`].
	pub fn new_with_event_id(
		source: S,
		max_frame_size: NonZeroUsize,
		initial_event_id: &str,
	) -> Result<Self, InitialEventIdError> {
		Ok(Self::from_parser(
			source,
			Parser::new_with_event_id(max_frame_size, initial_event_id)?,
		))
	}
	fn from_parser(source: S, parser: Parser) -> Self {
		Self {
			state: DecoderState::Reading {
				source: Box::pin(source),
				remaining: Bytes::new(),
			},
			parser,
		}
	}
	/// Returns the last complete cursor, for application-controlled reconnects.
	pub fn last_event_id(&self) -> &str {
		self.parser.last_event_id()
	}
	/// Returns the server's most recent valid retry delay.
	pub fn retry_millis(&self) -> Option<u64> {
		self.parser.retry_millis()
	}
}
impl<S, E> Stream for DecoderStream<S>
where
	S: Stream<Item = Result<Bytes, E>>,
{
	type Item = Result<Frame, ReadError<E>>;
	fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		let this = self.get_mut();
		let DecoderState::Reading { source, remaining } = &mut this.state else {
			return Poll::Ready(None);
		};
		// Yield to the executor even when a source supplies endless empty chunks.
		for _ in 0..32 {
			if !remaining.is_empty() {
				let mut input = remaining.as_ref();
				let result = this.parser.next_frame(&mut input);
				let consumed = remaining.len() - input.len();
				*remaining = remaining.slice(consumed..);
				match result {
					Ok(Some(frame)) => return Poll::Ready(Some(Ok(frame))),
					Ok(None) => {}
					Err(error) => {
						this.state = DecoderState::Ended;
						return Poll::Ready(Some(Err(ReadError::Parse(error))));
					}
				}
			}
			match source.as_mut().poll_next(cx) {
				Poll::Pending => return Poll::Pending,
				Poll::Ready(None) => {
					this.state = DecoderState::Ended;
					return Poll::Ready(None);
				}
				Poll::Ready(Some(Err(error))) => {
					this.state = DecoderState::Ended;
					return Poll::Ready(Some(Err(ReadError::Transport(error))));
				}
				Poll::Ready(Some(Ok(bytes))) => *remaining = bytes,
			}
		}
		cx.waker().wake_by_ref();
		Poll::Pending
	}
}
