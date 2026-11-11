use super::model::WorkerConfig;
use crate::queue::{
	MAX_WORKER_DATABASE_OPERATION_TIMEOUT, PgWorkerConfig, QueueRecoveryPolicy, WorkerConfigError,
	resolve_queue_worker_concurrency, validate_positive_worker_duration, validate_worker_timing,
};
use std::time::Duration;

impl WorkerConfig {
	/// Validates the settings of a runtime worker and combines them with the installation's Queue
	/// recovery settings.
	pub fn new(
		config: PgWorkerConfig,
		recovery: &QueueRecoveryPolicy,
	) -> Result<Self, WorkerConfigError> {
		let poll_interval = validate_positive_worker_duration(
			config.poll_interval,
			WorkerConfigError::PollIntervalIsZero,
		)?;
		let startup_jitter_max_delay = config.startup_jitter_max_delay;
		let concurrency = resolve_queue_worker_concurrency(config.concurrency)?;
		let stale_threshold = validate_positive_worker_duration(
			Duration::from_micros(
				u64::try_from(recovery.stale_threshold_micros())
					.expect("a recorded stale threshold is positive"),
			),
			WorkerConfigError::StaleThresholdIsZero,
		)?;
		let execution_heartbeat_interval = validate_positive_worker_duration(
			config.execution_heartbeat_interval,
			WorkerConfigError::ExecutionHeartbeatIntervalIsZero,
		)?;
		validate_worker_timing(stale_threshold, execution_heartbeat_interval)?;
		let shutdown_grace_period = config.shutdown_grace_period;
		let database_operation_timeout = validate_positive_worker_duration(
			config.database_operation_timeout,
			WorkerConfigError::DatabaseOperationTimeoutIsZero,
		)?;
		if database_operation_timeout > MAX_WORKER_DATABASE_OPERATION_TIMEOUT {
			return Err(WorkerConfigError::DatabaseOperationTimeoutTooLarge {
				actual: database_operation_timeout,
				max: MAX_WORKER_DATABASE_OPERATION_TIMEOUT,
			});
		}
		let claim_error_min_backoff = validate_positive_worker_duration(
			config.claim_error_min_backoff,
			WorkerConfigError::ClaimErrorMinBackoffIsZero,
		)?;
		if config.claim_error_max_backoff < claim_error_min_backoff {
			return Err(WorkerConfigError::ClaimErrorMaxBackoffBelowMin);
		}
		let claim_error_max_backoff = config.claim_error_max_backoff;
		Ok(Self {
			poll_interval,
			startup_jitter_max_delay,
			concurrency,
			stale_threshold,
			execution_heartbeat_interval,
			dead_letters: recovery.dead_letters(),
			shutdown_grace_period,
			database_operation_timeout,
			claim_error_min_backoff,
			claim_error_max_backoff,
		})
	}
}
