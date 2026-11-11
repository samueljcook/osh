//! Ordinary Once caller effects remain local through exact completion and close accounting.

mod acquire;
pub(crate) mod error;
pub(crate) mod options;
pub(crate) mod result;
pub(super) mod start;

use std::sync::Arc;
use tokio::time::Instant;

use super::PgNativeOnce;
use crate::db::WritePool;
use crate::fleet::lease::owned_driver::{
	NativeOwnedLeaseClient,
	cache_publication::NativeLeaseCallerScope,
	loss::{NativeLeaseAuthorityObservation, NativeLeaseCallerCtx},
	once_completion::NativeLeaseOnceCompletionFields,
};
use error::NativeOnceManagedError;
use options::NativeOnceManagedOptions;
use result::{
	NativeOnceCompletionNotRequested, NativeOnceManagedCompletion, NativeOnceManagedRan,
	NativeOnceManagedResult, NativeOnceReleaseAccounting,
};
use start::{NativeOnceStartFields, NativeOnceStarted, start};

pub(crate) struct NativeOnceManagedFields<'a, TaskFn> {
	pub(crate) pool: &'a WritePool,
	pub(crate) options: &'a NativeOnceManagedOptions,
	pub(crate) task_fn: TaskFn,
}
impl PgNativeOnce {
	/// An error means that the task function did not run; task output never becomes a library
	/// error.
	pub(crate) async fn run_task<T, E, TaskFn>(
		self: &Arc<Self>,
		fields: NativeOnceManagedFields<'_, TaskFn>,
	) -> Result<NativeOnceManagedResult<T, E>, NativeOnceManagedError>
	where
		TaskFn: AsyncFnOnce(NativeLeaseCallerCtx) -> Result<T, E>,
	{
		let (client, caller, ctx) = match start(NativeOnceStartFields {
			once: self,
			pool: fields.pool,
			options: fields.options,
		})
		.await?
		{
			NativeOnceStarted::Ready {
				client,
				caller,
				ctx,
			} => (client, caller, ctx),
			NativeOnceStarted::AlreadyDone { completion } => {
				return Ok(NativeOnceManagedResult::AlreadyDone { completion });
			}
			NativeOnceStarted::AlreadyRunning => {
				return Ok(NativeOnceManagedResult::AlreadyRunning);
			}
		};
		let loss = ctx.loss.clone();
		let task = {
			let future = (fields.task_fn)(ctx);
			future.await
		};
		let authority_at_completion = loss.observe();
		let not_requested = if task.is_err() {
			Some(NativeOnceCompletionNotRequested::AppError)
		} else if matches!(
			authority_at_completion,
			NativeLeaseAuthorityObservation::Lost { .. }
		) {
			Some(NativeOnceCompletionNotRequested::AuthorityLost)
		} else {
			None
		};
		let (completion, observer) = if let Some(reason) = not_requested {
			drop(caller);
			(
				NativeOnceManagedCompletion::NotRequested(reason),
				client.close(),
			)
		} else {
			match client
				.into_once_completion(NativeLeaseOnceCompletionFields {
					once: Arc::clone(self),
					caller,
					requested_at: Instant::now(),
				})
				.await
			{
				Ok(completion) => match completion.complete_and_release().await {
					Ok(result) => (
						NativeOnceManagedCompletion::Committed(result.completion),
						result.observer,
					),
					Err(failure) => (
						NativeOnceManagedCompletion::Failed(failure.error),
						failure.observer,
					),
				},
				Err(failure) => (
					NativeOnceManagedCompletion::HandoffFailed(failure.source),
					failure.observer,
				),
			}
		};
		let release =
			NativeOnceReleaseAccounting::observe(observer, &fields.options.close_wait).await;
		Ok(NativeOnceManagedResult::Ran(NativeOnceManagedRan {
			task,
			completion,
			release,
		}))
	}
}
async fn close_caller(
	client: NativeOwnedLeaseClient,
	caller: NativeLeaseCallerScope,
	options: &NativeOnceManagedOptions,
) -> NativeOnceReleaseAccounting {
	drop(caller);
	NativeOnceReleaseAccounting::observe(client.close(), &options.close_wait).await
}
