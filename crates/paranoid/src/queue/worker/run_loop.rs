use super::job::{
	JoinedJob, WorkerJobs, abort_and_collect_in_flight_jobs,
	return_claimed_jobs_after_worker_task_failure,
	worker_runtime_error_after_in_flight_abort_and_claimed_job_cleanup,
	worker_runtime_shutdown_timeout_result_after_in_flight_abort_and_claimed_job_cleanup,
};
use super::model::{
	RuntimeCancellationSignal, WorkerRunLoopSummary, WorkerRuntime, WorkerShutdownOutcome,
};
use crate::queue::{
	TaskName, WorkerClaimError, WorkerRunError, WorkerStepError,
	claim_available_jobs_for_worker_with_database_operation_timeout,
};
use primitives_rs::random::jitter::generate_random_unit_f64;
use primitives_rs::random::secure_bytes as random;
use primitives_rs::time::duration_from_clamped_secs_f64;
use std::time::Duration;

pub(super) async fn run_queue_worker_loop(
	runtime: WorkerRuntime,
) -> Result<WorkerRunLoopSummary, WorkerRunError> {
	if !wait_for_queue_worker_startup_jitter_before_run_loop(
		&runtime.worker_shutdown_signal,
		runtime.config.startup_jitter_max_delay,
	)
	.await
	.map_err(WorkerRunError::StartupJitterRandom)?
	{
		return Ok(WorkerRunLoopSummary::default());
	}

	let registered_task_names = runtime.task_registry.registered_task_names();
	let mut in_flight_jobs = WorkerJobs::new();
	let mut summary = WorkerRunLoopSummary::default();
	let mut claim_error_backoff = runtime.config.claim_error_min_backoff;

	loop {
		if runtime.worker_shutdown_signal.is_cancellation_requested() {
			break;
		}

		match claim_and_spawn_jobs_up_to_worker_capacity(
			&runtime,
			&registered_task_names,
			&mut in_flight_jobs,
		)
		.await
		{
			Ok(claimed_count) => {
				claim_error_backoff = runtime.config.claim_error_min_backoff;
				summary.record_claimed_count(claimed_count)?;
			}
			Err(error) if error.is_protocol_failure() => {
				let (in_flight_errors, cleanup_result) =
					handle_queue_worker_runtime_error(&runtime, &mut in_flight_jobs).await;
				return Err(
					worker_runtime_error_after_in_flight_abort_and_claimed_job_cleanup(
						WorkerRunError::Claim(error),
						in_flight_errors,
						cleanup_result,
					),
				);
			}
			Err(_) => {
				wait_for_worker_claim_retry_backoff_or_shutdown(
					&runtime.worker_shutdown_signal,
					claim_error_backoff,
				)
				.await;
				claim_error_backoff = claim_error_backoff
					.saturating_mul(2)
					.min(runtime.config.claim_error_max_backoff);
				if runtime.worker_shutdown_signal.is_cancellation_requested() {
					break;
				}
				continue;
			}
		}

		if in_flight_jobs.running_count() == 0 {
			tokio::select! {
				_ = runtime.worker_shutdown_signal.wait_until_cancellation_requested() => break,
				_ = tokio::time::sleep(runtime.config.poll_interval) => {}
			}
			continue;
		}

		tokio::select! {
			joined = in_flight_jobs.join_next() => {
				if let Some(joined) = joined {
					handle_queue_worker_joined_job(
						&runtime,
						&mut in_flight_jobs,
						&mut summary,
						joined,
					)
					.await?;
				}
			}
			_ = runtime.worker_shutdown_signal.wait_until_cancellation_requested() => break,
			_ = tokio::time::sleep(runtime.config.poll_interval) => {}
		}
	}

	runtime.worker_shutdown_signal.request_cancellation();
	finish_queue_worker_shutdown(runtime, in_flight_jobs, summary).await
}

pub(super) async fn claim_and_spawn_jobs_up_to_worker_capacity(
	runtime: &WorkerRuntime,
	registered_task_names: &[TaskName],
	in_flight_jobs: &mut WorkerJobs,
) -> Result<usize, WorkerClaimError> {
	let in_flight_count = u32::try_from(in_flight_jobs.running_count()).unwrap_or(u32::MAX);
	let available_capacity = runtime.config.concurrency.saturating_sub(in_flight_count);
	if available_capacity == 0 || registered_task_names.is_empty() {
		return Ok(0);
	}

	let worker_owner_id = runtime.claim_owner.owner_for_next_batch()?;
	let jobs = claim_available_jobs_for_worker_with_database_operation_timeout(
		&runtime.queue,
		&runtime.pool,
		registered_task_names,
		available_capacity,
		&worker_owner_id,
		runtime.config.database_operation_timeout,
	)
	.await?;
	let claimed_count = jobs.len();
	for claimed_job in jobs {
		in_flight_jobs.spawn(runtime, claimed_job);
	}
	Ok(claimed_count)
}

#[doc(hidden)]
pub async fn wait_for_queue_worker_startup_jitter_before_run_loop(
	worker_shutdown_signal: &RuntimeCancellationSignal,
	startup_jitter_max_delay: Duration,
) -> Result<bool, random::RandomError> {
	let startup_jitter_delay = compute_queue_worker_startup_jitter_delay(startup_jitter_max_delay)?;
	wait_for_queue_worker_startup_jitter_delay_or_shutdown(
		worker_shutdown_signal,
		startup_jitter_delay,
	)
	.await
}

#[doc(hidden)]
pub async fn wait_for_queue_worker_startup_jitter_delay_or_shutdown(
	worker_shutdown_signal: &RuntimeCancellationSignal,
	startup_jitter_delay: Duration,
) -> Result<bool, random::RandomError> {
	if startup_jitter_delay.is_zero() {
		return Ok(!worker_shutdown_signal.is_cancellation_requested());
	}
	tokio::select! {
		_ = worker_shutdown_signal.wait_until_cancellation_requested() => Ok(false),
		_ = tokio::time::sleep(startup_jitter_delay) => Ok(true),
	}
}

#[doc(hidden)]
pub fn compute_queue_worker_startup_jitter_delay(
	startup_jitter_max_delay: Duration,
) -> Result<Duration, random::RandomError> {
	if startup_jitter_max_delay.is_zero() {
		return Ok(Duration::ZERO);
	}
	let unit = generate_random_unit_f64()?;
	Ok(duration_from_clamped_secs_f64(
		startup_jitter_max_delay.as_secs_f64() * unit,
		startup_jitter_max_delay,
	))
}

#[doc(hidden)]
pub async fn wait_for_worker_claim_retry_backoff_or_shutdown(
	worker_shutdown_signal: &RuntimeCancellationSignal,
	backoff: Duration,
) {
	tokio::select! {
		_ = worker_shutdown_signal.wait_until_cancellation_requested() => {}
		_ = tokio::time::sleep(backoff) => {}
	}
}

pub(super) async fn handle_queue_worker_joined_job(
	runtime: &WorkerRuntime,
	in_flight_jobs: &mut WorkerJobs,
	summary: &mut WorkerRunLoopSummary,
	joined: JoinedJob,
) -> Result<(), WorkerRunError> {
	let worker_error = match record_joined_job_outcome(summary, joined) {
		Ok(()) => return Ok(()),
		Err(error) => error,
	};
	let (in_flight_errors, cleanup_result) =
		handle_queue_worker_runtime_error(runtime, in_flight_jobs).await;
	Err(
		worker_runtime_error_after_in_flight_abort_and_claimed_job_cleanup(
			worker_error,
			in_flight_errors,
			cleanup_result,
		),
	)
}

fn record_joined_job_outcome(
	summary: &mut WorkerRunLoopSummary,
	joined: JoinedJob,
) -> Result<(), WorkerRunError> {
	match joined {
		Ok(Ok(outcome)) => summary.record_processed_job_outcome(outcome),
		Ok(Err(error)) => Err(WorkerRunError::Job(error)),
		Err(error) => Err(WorkerRunError::JobTaskJoinFailed(error)),
	}
}

pub(super) async fn handle_queue_worker_runtime_error(
	runtime: &WorkerRuntime,
	in_flight_jobs: &mut WorkerJobs,
) -> (Vec<WorkerRunError>, Result<(), WorkerStepError>) {
	runtime.worker_shutdown_signal.request_cancellation();
	let aborted = abort_and_collect_in_flight_jobs(in_flight_jobs, None).await;
	let cleanup_result = return_claimed_jobs_after_worker_task_failure(
		&runtime.queue,
		&runtime.pool,
		in_flight_jobs.unsettled(),
		runtime.config.database_operation_timeout,
	)
	.await;
	(aborted.errors, cleanup_result)
}

pub(super) async fn finish_queue_worker_shutdown(
	runtime: WorkerRuntime,
	mut in_flight_jobs: WorkerJobs,
	mut summary: WorkerRunLoopSummary,
) -> Result<WorkerRunLoopSummary, WorkerRunError> {
	let drain = async {
		while let Some(joined) = in_flight_jobs.join_next().await {
			record_joined_job_outcome(&mut summary, joined)?;
		}
		Ok::<(), WorkerRunError>(())
	};

	match tokio::time::timeout(runtime.config.shutdown_grace_period, drain).await {
		Ok(Ok(())) => {}
		Ok(Err(worker_error)) => {
			// Cleanup must retain the observed failure even if the grace period expires.
			let (in_flight_errors, cleanup_result) =
				handle_queue_worker_runtime_error(&runtime, &mut in_flight_jobs).await;
			return Err(
				worker_runtime_error_after_in_flight_abort_and_claimed_job_cleanup(
					worker_error,
					in_flight_errors,
					cleanup_result,
				),
			);
		}
		Err(_) => {
			let aborted =
				abort_and_collect_in_flight_jobs(&mut in_flight_jobs, Some(&mut summary)).await;
			summary.shutdown = WorkerShutdownOutcome::GracePeriodExpired {
				cancelled_job_count: aborted.cancelled_job_count,
			};
			let cleanup_result = return_claimed_jobs_after_worker_task_failure(
				&runtime.queue,
				&runtime.pool,
				in_flight_jobs.unsettled(),
				runtime.config.database_operation_timeout,
			)
			.await;
			return worker_runtime_shutdown_timeout_result_after_in_flight_abort_and_claimed_job_cleanup(
                summary,
                aborted.errors,
                cleanup_result,
            );
		}
	};

	return_claimed_jobs_after_worker_task_failure(
		&runtime.queue,
		&runtime.pool,
		in_flight_jobs.unsettled(),
		runtime.config.database_operation_timeout,
	)
	.await
	.map_err(WorkerRunError::ClaimedJobCleanup)?;
	Ok(summary)
}
