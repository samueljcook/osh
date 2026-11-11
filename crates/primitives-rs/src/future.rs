//! Future polling combinators.

use std::any::Any;
use std::future::Future;
use std::mem;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::task::{Context, Poll};

enum CatchUnwindState<F> {
	Polling(F),
	Completed,
}

/// Polls an [`Unpin`] future while catching unwinding panics from `poll` and `Drop`.
///
/// Callers that hold an unpinned future can pin it on the stack with [`std::pin::pin!`] or on the
/// heap with [`CatchUnwind::new_boxed`]. Returned panic payloads remain opaque so that callers do
/// not accidentally format secret values from panic messages.
pub struct CatchUnwind<F> {
	state: CatchUnwindState<F>,
}

impl<F> CatchUnwind<F> {
	/// Wraps an [`Unpin`] future so that polling [`CatchUnwind`] catches unwinding panics.
	pub const fn new(future: F) -> Self
	where
		F: Future + Unpin,
	{
		Self {
			state: CatchUnwindState::Polling(future),
		}
	}
}

impl<F: Future> CatchUnwind<Pin<Box<F>>> {
	/// Boxes and pins a future, returning a [`CatchUnwind`] wrapper around the boxed future.
	pub fn new_boxed(future: F) -> Self {
		Self {
			state: CatchUnwindState::Polling(Box::pin(future)),
		}
	}
}

impl<F: Future + Unpin> Future for CatchUnwind<F> {
	type Output = Result<F::Output, Box<dyn Any + Send + 'static>>;

	fn poll(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
		let this = self.as_mut().get_mut();
		let CatchUnwindState::Polling(mut future) =
			mem::replace(&mut this.state, CatchUnwindState::Completed)
		else {
			panic!("CatchUnwind polled after completion");
		};
		let result = catch_unwind(AssertUnwindSafe(move || {
			match Pin::new(&mut future).poll(ctx) {
				Poll::Pending => (Some(future), None),
				Poll::Ready(output) => {
					drop(future);
					(None, Some(output))
				}
			}
		}));
		match result {
			Ok((Some(future), None)) => {
				this.state = CatchUnwindState::Polling(future);
				Poll::Pending
			}
			Ok((None, Some(output))) => Poll::Ready(Ok(output)),
			Ok(_) => unreachable!("CatchUnwind poll returns either Pending with future or Ready"),
			Err(payload) => Poll::Ready(Err(payload)),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::CatchUnwind;
	use std::future::Future;
	use std::pin::Pin;
	use std::task::{Context, Poll, Waker};

	struct PanicInPollAndDrop {
		armed_drop_panic: bool,
	}

	impl Future for PanicInPollAndDrop {
		type Output = ();

		fn poll(mut self: Pin<&mut Self>, _ctx: &mut Context<'_>) -> Poll<Self::Output> {
			self.armed_drop_panic = true;
			panic!("poll failed");
		}
	}

	impl Drop for PanicInPollAndDrop {
		fn drop(&mut self) {
			if self.armed_drop_panic && !std::thread::panicking() {
				panic!("drop failed outside unwind");
			}
		}
	}

	#[test]
	fn catch_unwind_drops_panicking_future_inside_unwind_and_forbids_repoll() {
		let mut wrapped = CatchUnwind::new(PanicInPollAndDrop {
			armed_drop_panic: false,
		});
		let waker = Waker::noop();
		let mut cx = Context::from_waker(waker);
		let first = Pin::new(&mut wrapped).poll(&mut cx);
		assert!(matches!(first, Poll::Ready(Err(_))));
		let repoll = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
			let _ = Pin::new(&mut wrapped).poll(&mut cx);
		}));
		assert!(repoll.is_err());
	}
}
