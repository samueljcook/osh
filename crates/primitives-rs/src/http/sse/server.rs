use super::{EncodeError, Event, encode_event};
use bytes::Bytes;
use http::{
	Response,
	header::{CACHE_CONTROL, CONTENT_TYPE, HeaderValue},
};
use http_body::{Body as HttpBody, Frame};
use std::{
	fmt,
	future::Future,
	num::NonZeroUsize,
	pin::Pin,
	task::{Context, Poll},
	time::Duration,
};
use tokio::{
	sync::mpsc,
	task::{JoinError, JoinHandle},
	time::{Instant, Sleep},
};
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

/// Explicit buffering, idle keepalive, and cancellation choices for one response.
pub struct ServerOptions {
	/// Maximum queued frames. Producers wait for a slot before encoding a frame.
	pub buffer_capacity: NonZeroUsize,
	/// Maximum encoded bytes per event, including framing.
	pub max_frame_size: NonZeroUsize,
	/// Idle comment interval, or no keepalives. Zero is invalid.
	pub keepalive_interval: Option<Duration>,
	/// Parent cancellation. The response owns a child token and never cancels the parent.
	pub cancellation_token: CancellationToken,
}
/// Invalid channel capacity, keepalive duration, or keepalive frame size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerOptionsError;
impl fmt::Display for ServerOptionsError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("SSE channel capacity or keepalive configuration is invalid")
	}
}
impl std::error::Error for ServerOptionsError {}

/// A failed or cancelled send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendError {
	/// The response was dropped, cancelled, or completed.
	Closed,
	/// Event fields or encoded size were invalid.
	Encode(EncodeError),
}
impl fmt::Display for SendError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Closed => f.write_str("SSE response closed"),
			Self::Encode(e) => e.fmt(f),
		}
	}
}
impl std::error::Error for SendError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Encode(error) => Some(error),
			Self::Closed => None,
		}
	}
}

/// A bounded event sender. Cloning it does not extend the producer's lifetime.
#[derive(Clone)]
pub struct EventSender {
	sender: mpsc::Sender<Bytes>,
	max_size: usize,
	cancel: CancellationToken,
}
impl EventSender {
	/// Waits for capacity, then checks and encodes one complete event.
	/// Cancellation before enqueue emits no partial event.
	pub async fn send(&self, event: &Event) -> Result<(), SendError> {
		let permit = tokio::select! { biased;
			_ = self.cancel.cancelled() => return Err(SendError::Closed),
			permit = self.sender.reserve() => permit.map_err(|_| SendError::Closed)?,
		};
		let frame = encode_event(event, self.max_size).map_err(SendError::Encode)?;
		if self.cancel.is_cancelled() {
			return Err(SendError::Closed);
		}
		permit.send(Bytes::from(frame));
		Ok(())
	}
}

/// Failure after an SSE response opened. The HTTP body returns it after queued frames.
#[derive(Debug)]
pub enum BodyError<E> {
	/// The producer returned an application error.
	Producer(E),
	/// The producer panicked or its Tokio task failed.
	Task(JoinError),
}
impl<E: fmt::Display> fmt::Display for BodyError<E> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Producer(e) => write!(f, "SSE producer: {e}"),
			Self::Task(e) => write!(f, "SSE producer task: {e}"),
		}
	}
}
impl<E: std::error::Error + 'static> std::error::Error for BodyError<E> {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Producer(error) => Some(error),
			Self::Task(error) => Some(error),
		}
	}
}

/// An owned HTTP body that cancels and aborts its producer when dropped.
pub struct Body<E> {
	state: BodyState<E>,
	cancel: CancellationToken,
}

enum BodyState<E> {
	Streaming {
		delivery: BodyDelivery,
		producer: ProducerState<E>,
	},
	Ended,
}

struct BodyDelivery {
	receiver: mpsc::Receiver<Bytes>,
	cancelled: Pin<Box<WaitForCancellationFutureOwned>>,
	keepalive: Option<(Duration, Pin<Box<Sleep>>)>,
}

enum ProducerState<E> {
	Running(JoinHandle<Result<(), E>>),
	Completed(Result<(), BodyError<E>>),
}

impl<E> Body<E> {
	fn end(&mut self) -> Option<BodyError<E>> {
		let BodyState::Streaming {
			mut delivery,
			producer,
		} = std::mem::replace(&mut self.state, BodyState::Ended)
		else {
			return None;
		};
		self.cancel.cancel();
		delivery.receiver.close();
		match producer {
			ProducerState::Running(producer) => {
				producer.abort();
				None
			}
			ProducerState::Completed(result) => result.err(),
		}
	}
}
// The potentially !Unpin application error is never pinned or projected.
impl<E> Unpin for Body<E> {}
impl<E> Drop for Body<E> {
	fn drop(&mut self) {
		self.cancel.cancel();
		if let BodyState::Streaming {
			producer: ProducerState::Running(producer),
			..
		} = &self.state
		{
			producer.abort();
		}
	}
}
impl<E> HttpBody for Body<E> {
	type Data = Bytes;
	type Error = BodyError<E>;
	fn poll_frame(
		self: Pin<&mut Self>,
		cx: &mut Context<'_>,
	) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
		let this = self.get_mut();
		let BodyState::Streaming { delivery, producer } = &mut this.state else {
			return Poll::Ready(None);
		};
		if delivery.cancelled.as_mut().poll(cx).is_ready() {
			this.end();
			return Poll::Ready(None);
		}
		if let ProducerState::Running(task) = producer
			&& let Poll::Ready(result) = Pin::new(task).poll(cx)
		{
			*producer = ProducerState::Completed(match result {
				Ok(result) => result.map_err(BodyError::Producer),
				Err(error) => Err(BodyError::Task(error)),
			});
			// Escaped sender clones must not hold the response open after completion.
			delivery.receiver.close();
		}
		match delivery.receiver.poll_recv(cx) {
			Poll::Ready(Some(bytes)) => {
				if let Some((interval, timer)) = &mut delivery.keepalive {
					timer.as_mut().reset(Instant::now() + *interval);
				}
				return Poll::Ready(Some(Ok(Frame::data(bytes))));
			}
			Poll::Ready(None) if matches!(producer, ProducerState::Completed(_)) => {
				return Poll::Ready(this.end().map(Err));
			}
			_ => {}
		}
		if let Some((interval, timer)) = &mut delivery.keepalive
			&& timer.as_mut().poll(cx).is_ready()
		{
			timer.as_mut().reset(Instant::now() + *interval);
			return Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b":\n\n")))));
		}
		Poll::Pending
	}
	fn is_end_stream(&self) -> bool {
		matches!(self.state, BodyState::Ended)
	}
}

/// Starts a producer and returns a standard SSE response.
///
/// The producer owns its asynchronous work; returning completes the stream after
/// queued frames. Application terminal events must be sent explicitly. A body
/// error is a transport failure, not an encoded application event. The caller
/// must run this operation inside Tokio. Dropping the response cancels its child
/// token and aborts the producer, including a producer that ignores cancellation.
pub fn serve<F, Fut, E>(
	options: ServerOptions,
	producer_fn: F,
) -> Result<Response<Body<E>>, ServerOptionsError>
where
	F: FnOnce(EventSender, CancellationToken) -> Fut + Send + 'static,
	Fut: Future<Output = Result<(), E>> + Send + 'static,
	E: Send + 'static,
{
	if options.buffer_capacity.get() > tokio::sync::Semaphore::MAX_PERMITS
		|| options.keepalive_interval.is_some_and(|interval| {
			interval.is_zero()
				|| Instant::now().checked_add(interval).is_none()
				|| options.max_frame_size.get() < 3
		}) {
		return Err(ServerOptionsError);
	}
	let cancel = options.cancellation_token.child_token();
	let (sender, receiver) = mpsc::channel(options.buffer_capacity.get());
	let sink = EventSender {
		sender,
		max_size: options.max_frame_size.get(),
		cancel: cancel.clone(),
	};
	let producer_cancel = cancel.clone();
	let producer = tokio::spawn(async move {
		tokio::select! { biased;
			_ = producer_cancel.cancelled() => Ok(()),
			result = async { producer_fn(sink, producer_cancel.clone()).await } => result,
		}
	});
	let body = Body {
		state: BodyState::Streaming {
			delivery: BodyDelivery {
				receiver,
				cancelled: Box::pin(cancel.clone().cancelled_owned()),
				keepalive: options
					.keepalive_interval
					.map(|interval| (interval, Box::pin(tokio::time::sleep(interval)))),
			},
			producer: ProducerState::Running(producer),
		},
		cancel,
	};
	let mut response = Response::new(body);
	response
		.headers_mut()
		.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
	response
		.headers_mut()
		.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
	Ok(response)
}
