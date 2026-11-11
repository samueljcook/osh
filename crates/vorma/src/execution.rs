//! Phase execution. Task middleware completes before the endpoint or view
//! phase starts. Each phase runs every handler concurrently and commits
//! contributions in declaration order. A redirect or a failure eagerly stops
//! only later positions; earlier positions keep contributing. A handler that
//! panics fails at its own position with status 500.

use std::any::Any;
use std::collections::HashMap;

use futures_util::future::BoxFuture;
use http::StatusCode;
use serde_json::value::RawValue;
use tokio::task::{AbortHandle, JoinSet};
use tracing::Instrument;

use crate::outcome::{Contribution, HandlerKind, HandlerLabel, Outcome, PublicError, Redirection};

/// One handler's work in a phase.
pub(crate) struct PhaseInvocation {
	pub(crate) future: BoxFuture<'static, Outcome>,
	pub(crate) label: HandlerLabel,
}

/// A committed position.
pub(crate) enum Committed<PositionKey> {
	/// The handler succeeded; `output` is absent without output.
	Completed {
		position_key: PositionKey,
		output: Option<Box<RawValue>>,
	},
	/// The endpoint answered 304 without a body.
	NotModified { position_key: PositionKey },
	/// The position has no handler.
	NoHandler { position_key: PositionKey },
}

/// The first failed position of a phase.
pub(crate) struct FailedPosition {
	pub(crate) index: usize,
	pub(crate) status: StatusCode,
	pub(crate) public_error: Option<PublicError>,
}

pub(crate) enum PhaseResult<PositionKey> {
	Completed {
		contribution: Contribution,
		positions: Vec<Committed<PositionKey>>,
	},
	Redirect {
		contribution: Contribution,
		redirection: Redirection,
	},
	/// The failure's status, headers, and cookies are merged into the
	/// contribution. Later positions are stopped.
	Failed {
		contribution: Contribution,
		before: Vec<Committed<PositionKey>>,
		failed: FailedPosition,
	},
}

fn create_span(label: &HandlerLabel) -> tracing::Span {
	let pattern = &*label.pattern;
	match label.kind {
		HandlerKind::TaskMiddleware => {
			tracing::info_span!("vorma.task_middleware", vorma.pattern = pattern)
		}
		HandlerKind::ViewLoader => {
			tracing::info_span!("vorma.view_loader", vorma.pattern = pattern)
		}
		HandlerKind::ApiEndpoint => {
			tracing::info_span!("vorma.api_endpoint", vorma.pattern = pattern)
		}
	}
}

fn describe_panic(payload: &(dyn Any + Send)) -> &str {
	payload
		.downcast_ref::<&str>()
		.copied()
		.or_else(|| payload.downcast_ref::<String>().map(String::as_str))
		.unwrap_or("a non-text panic payload")
}

/// A panic is a defect: the position fails with status 500 and no payload.
fn panic_outcome(label: &HandlerLabel, payload: &(dyn Any + Send)) -> Outcome {
	tracing::error!(
		handler = label.kind.span_name(),
		pattern = &*label.pattern,
		path = &*label.request_path,
		panic = describe_panic(payload),
		"Vorma handler panicked",
	);
	Outcome::Failure {
		contribution: Contribution::default(),
		status: StatusCode::INTERNAL_SERVER_ERROR,
		public_error: None,
	}
}

/// The state of one position of a phase. A position's key is not part of
/// its state: positions commit in order, and each commit takes the next key.
enum PhasePosition {
	/// The position has no handler.
	NoHandler,
	/// The position's handler runs in a task.
	Running {
		label: HandlerLabel,
		abort: AbortHandle,
	},
	/// The position's handler finished and waits for the earlier positions
	/// to commit.
	Ready { outcome: Outcome },
	/// An earlier position's terminal outcome aborted the position's task.
	Cancelled,
	/// The position's contribution is merged.
	Committed,
}

/// Runs one phase. `initial` holds the contributions committed before it.
pub(crate) async fn run_phase<PositionKey>(
	positions: Vec<(PositionKey, Option<PhaseInvocation>)>,
	initial: Contribution,
) -> PhaseResult<PositionKey> {
	let count = positions.len();
	let mut tasks = JoinSet::new();
	let mut task_indexes = HashMap::new();
	let mut keys = Vec::with_capacity(count);
	// Every handler runs in its own Tokio task so that no handler sees the
	// host's task-local values, whatever the number of handlers in the phase.
	let mut positions: Vec<_> = positions
		.into_iter()
		.enumerate()
		.map(|(index, (key, invocation))| {
			keys.push(key);
			match invocation {
				None => PhasePosition::NoHandler,
				Some(invocation) => {
					let span = create_span(&invocation.label);
					let abort = tasks.spawn(invocation.future.instrument(span));
					task_indexes.insert(abort.id(), index);
					PhasePosition::Running {
						label: invocation.label,
						abort,
					}
				}
			}
		})
		.collect();
	let mut keys = keys.into_iter();

	let mut contribution = initial;
	let mut committed = Vec::with_capacity(count);
	let mut next = 0;
	loop {
		while next < count {
			if matches!(positions[next], PhasePosition::Running { .. }) {
				break;
			}
			let outcome = match std::mem::replace(&mut positions[next], PhasePosition::Committed) {
				PhasePosition::NoHandler => None,
				PhasePosition::Ready { outcome } => Some(outcome),
				PhasePosition::Running { .. } => unreachable!("a running position waits above"),
				PhasePosition::Cancelled => {
					unreachable!("a cancelled position follows an earlier terminal outcome")
				}
				PhasePosition::Committed => unreachable!("each position commits once"),
			};
			let position_key = keys.next().expect("each position has a key");
			match outcome {
				None => committed.push(Committed::NoHandler { position_key }),
				Some(Outcome::Continue {
					contribution: later,
					output,
				}) => {
					contribution.merge(later);
					committed.push(Committed::Completed {
						position_key,
						output,
					});
				}
				Some(Outcome::NotModified {
					contribution: later,
				}) => {
					contribution.merge(later);
					committed.push(Committed::NotModified { position_key });
				}
				Some(Outcome::Redirect {
					contribution: later,
					redirection,
				}) => {
					contribution.merge(later);
					return PhaseResult::Redirect {
						contribution,
						redirection,
					};
				}
				Some(Outcome::Failure {
					contribution: mut later,
					status,
					public_error,
				}) => {
					later.status = Some(status);
					contribution.merge(later);
					return PhaseResult::Failed {
						contribution,
						before: committed,
						failed: FailedPosition {
							index: next,
							status,
							public_error,
						},
					};
				}
			}
			next += 1;
		}
		if next == count {
			return PhaseResult::Completed {
				contribution,
				positions: committed,
			};
		}
		let Some(joined) = tasks.join_next_with_id().await else {
			unreachable!("an uncommitted position with a handler has a running task");
		};
		let (id, outcome) = match joined {
			Ok((id, outcome)) => (id, Ok(outcome)),
			Err(error) => (error.id(), Err(error)),
		};
		let index = task_indexes
			.remove(&id)
			.expect("each joined task has a position");
		let label = match &positions[index] {
			PhasePosition::Cancelled => continue,
			PhasePosition::Running { label, .. } => label,
			PhasePosition::NoHandler | PhasePosition::Ready { .. } | PhasePosition::Committed => {
				unreachable!("each task completes its running position once")
			}
		};
		let outcome = match outcome {
			Ok(outcome) => outcome,
			Err(error) if error.is_cancelled() => {
				unreachable!("only positions after a terminal outcome are cancelled");
			}
			Err(error) => panic_outcome(label, &*error.into_panic()),
		};
		if outcome.is_terminal() {
			// Aborting later handlers drops their task participation; earlier
			// positions retain their resources and still contribute in order.
			for position in &mut positions[index + 1..] {
				if let PhasePosition::Running { abort, .. } = position {
					abort.abort();
					*position = PhasePosition::Cancelled;
				}
			}
		}
		positions[index] = PhasePosition::Ready { outcome };
	}
}
