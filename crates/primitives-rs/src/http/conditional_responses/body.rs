use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use http_body::{Body, Frame, SizeHint};

/// A response body that preserves data, trailers, and the inner body's error type.
///
/// A completed ETag read returns buffered data. An interrupted read returns its
/// buffered prefix followed by the remaining frames without losing data,
/// trailers, or errors or converting a body error into a different HTTP response.
pub struct EtagBody<B: Body<Data = Bytes>> {
	state: BodyState<B>,
}

enum BodyState<B: Body<Data = Bytes>> {
	Passthrough(Pin<Box<B>>),
	Buffered(Bytes),
	ReplayPrefix {
		prefix: Bytes,
		remainder: ReplayRemainder<B>,
	},
	ReplayRemainder(ReplayRemainder<B>),
	Ended {
		size_hint: SizeHint,
	},
}

enum ReplayRemainder<B: Body<Data = Bytes>> {
	Frame {
		frame: Result<Frame<Bytes>, B::Error>,
		body: Pin<Box<B>>,
	},
	Body(Pin<Box<B>>),
}

impl<B: Body<Data = Bytes>> ReplayRemainder<B> {
	fn is_end_stream(&self) -> bool {
		match self {
			Self::Frame { .. } => false,
			Self::Body(body) => body.is_end_stream(),
		}
	}
}

// Only the boxed inner body is pinned. Frames, bytes, and errors are never
// structurally pinned by this wrapper.
impl<B: Body<Data = Bytes>> Unpin for EtagBody<B> {}

impl<B: Body<Data = Bytes>> EtagBody<B> {
	pub(super) fn passthrough(body: B) -> Self {
		Self {
			state: BodyState::Passthrough(Box::pin(body)),
		}
	}

	pub(super) fn buffered(bytes: Bytes) -> Self {
		Self {
			state: BodyState::Buffered(bytes),
		}
	}
}

impl<B: Body<Data = Bytes>> Body for EtagBody<B> {
	type Data = Bytes;
	type Error = B::Error;

	fn poll_frame(
		self: Pin<&mut Self>,
		cx: &mut Context<'_>,
	) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
		let this = self.get_mut();
		loop {
			match &mut this.state {
				BodyState::Passthrough(body) => {
					let polled = body.as_mut().poll_frame(cx);
					if matches!(polled, Poll::Ready(None)) {
						this.state = BodyState::Ended {
							size_hint: SizeHint::with_exact(0),
						};
					}
					return polled;
				}
				BodyState::ReplayRemainder(ReplayRemainder::Body(body)) => {
					let polled = body.as_mut().poll_frame(cx);
					if matches!(polled, Poll::Ready(None)) {
						this.state = BodyState::Ended {
							size_hint: SizeHint::new(),
						};
					}
					return polled;
				}
				BodyState::Ended { .. } => return Poll::Ready(None),
				_ => {}
			}
			let state = std::mem::replace(
				&mut this.state,
				BodyState::Ended {
					size_hint: SizeHint::with_exact(0),
				},
			);
			match state {
				BodyState::Buffered(bytes) => {
					return Poll::Ready((!bytes.is_empty()).then(|| Ok(Frame::data(bytes))));
				}
				BodyState::ReplayPrefix { prefix, remainder } => {
					this.state = BodyState::ReplayRemainder(remainder);
					if !prefix.is_empty() {
						return Poll::Ready(Some(Ok(Frame::data(prefix))));
					}
				}
				BodyState::ReplayRemainder(ReplayRemainder::Frame { frame, body }) => {
					this.state = BodyState::ReplayRemainder(ReplayRemainder::Body(body));
					return Poll::Ready(Some(frame));
				}
				BodyState::Passthrough(_)
				| BodyState::ReplayRemainder(ReplayRemainder::Body(_))
				| BodyState::Ended { .. } => {
					unreachable!("polled body states return before ownership transfer")
				}
			}
		}
	}

	fn is_end_stream(&self) -> bool {
		match &self.state {
			BodyState::Passthrough(body) => body.is_end_stream(),
			BodyState::Buffered(bytes) => bytes.is_empty(),
			BodyState::ReplayPrefix { prefix, remainder } => {
				prefix.is_empty() && remainder.is_end_stream()
			}
			BodyState::ReplayRemainder(remainder) => remainder.is_end_stream(),
			BodyState::Ended { .. } => true,
		}
	}

	fn size_hint(&self) -> SizeHint {
		match &self.state {
			BodyState::Passthrough(body) => body.size_hint(),
			BodyState::Buffered(bytes) => SizeHint::with_exact(bytes.len() as u64),
			BodyState::ReplayPrefix { .. } | BodyState::ReplayRemainder(_) => SizeHint::new(),
			BodyState::Ended { size_hint } => *size_hint,
		}
	}
}

enum ChunkBuffer {
	Empty,
	Single(Bytes),
	Joined(BytesMut),
}

impl ChunkBuffer {
	fn len(&self) -> usize {
		match self {
			Self::Empty => 0,
			Self::Single(bytes) => bytes.len(),
			Self::Joined(bytes) => bytes.len(),
		}
	}
	fn push(&mut self, data: Bytes, expected_size: usize) {
		if data.is_empty() {
			return;
		}
		*self = match std::mem::replace(self, Self::Empty) {
			Self::Empty => Self::Single(data),
			Self::Single(first) => {
				let mut joined =
					BytesMut::with_capacity(expected_size.max(first.len() + data.len()));
				joined.extend_from_slice(&first);
				joined.extend_from_slice(&data);
				Self::Joined(joined)
			}
			Self::Joined(mut joined) => {
				joined.extend_from_slice(&data);
				Self::Joined(joined)
			}
		};
	}
	fn into_bytes(self) -> Bytes {
		match self {
			Self::Empty => Bytes::new(),
			Self::Single(bytes) => bytes,
			Self::Joined(bytes) => bytes.freeze(),
		}
	}
}

enum BufferingEnd<E> {
	Completed,
	TimedOut,
	Frame(Result<Frame<Bytes>, E>),
}

pub(super) enum BufferingOutcome<B: Body<Data = Bytes>> {
	Completed(Bytes),
	Interrupted(EtagBody<B>),
}

pub(super) async fn buffer_body<B: Body<Data = Bytes>>(
	body: B,
	expected_size: usize,
	max_buffer_size: usize,
	buffer_timeout: Duration,
) -> BufferingOutcome<B> {
	let mut body = Box::pin(body);
	let mut timer = Box::pin(tokio::time::sleep(buffer_timeout));
	let mut chunks = ChunkBuffer::Empty;
	let ended = poll_fn(|cx| {
		// Yield between batches even for an immediately ready stream of empty
		// frames so that the runtime can advance timers and serve other work.
		for _ in 0..32 {
			if timer.as_mut().poll(cx).is_ready() {
				return Poll::Ready(BufferingEnd::TimedOut);
			}
			match body.as_mut().poll_frame(cx) {
				Poll::Pending => return Poll::Pending,
				Poll::Ready(None) => return Poll::Ready(BufferingEnd::Completed),
				Poll::Ready(Some(Err(error))) => {
					return Poll::Ready(BufferingEnd::Frame(Err(error)));
				}
				Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
					Ok(data) if data.len() <= max_buffer_size - chunks.len() => {
						chunks.push(data, expected_size)
					}
					Ok(data) => return Poll::Ready(BufferingEnd::Frame(Ok(Frame::data(data)))),
					Err(frame) => return Poll::Ready(BufferingEnd::Frame(Ok(frame))),
				},
			}
		}
		cx.waker().wake_by_ref();
		Poll::Pending
	})
	.await;
	let prefix = chunks.into_bytes();
	let remainder = match ended {
		BufferingEnd::Completed => return BufferingOutcome::Completed(prefix),
		BufferingEnd::TimedOut => ReplayRemainder::Body(body),
		BufferingEnd::Frame(frame) => ReplayRemainder::Frame { frame, body },
	};
	BufferingOutcome::Interrupted(EtagBody {
		state: BodyState::ReplayPrefix { prefix, remainder },
	})
}
