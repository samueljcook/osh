//! Job metadata used by transitions without loading the application's payload.

use super::lifecycle::JobLifecycle;
use crate::queue::{DedupeKey, Job, JobId, JobStatus, StoredJobTimeout, TaskName, WorkerOwnerId};

#[derive(Clone, Eq, PartialEq)]
pub(in crate::queue) struct NativeJobMetadata {
	pub(in crate::queue) id: JobId,
	pub(in crate::queue) task_name: TaskName,
	pub(in crate::queue) lifecycle: JobLifecycle,
	pub(in crate::queue) run_at_or_after_unix_micros: i64,
	pub(in crate::queue) last_error_msg: Option<String>,
	pub(in crate::queue) retry_count: u32,
	pub(in crate::queue) max_retries: u32,
	pub(in crate::queue) timeout: StoredJobTimeout,
	pub(in crate::queue) dedupe_key: Option<DedupeKey>,
	pub(in crate::queue) created_at_unix_micros: i64,
	pub(in crate::queue) updated_at_unix_micros: i64,
}

impl NativeJobMetadata {
	pub(in crate::queue) fn status(&self) -> JobStatus {
		self.lifecycle.status()
	}
	pub(in crate::queue) fn worker_owner_id(&self) -> Option<&WorkerOwnerId> {
		self.lifecycle.running().map(|running| &running.owner)
	}
	pub(in crate::queue) fn claimed_by_worker_at_unix_micros(&self) -> Option<i64> {
		self.lifecycle.running().map(|running| running.claimed_at)
	}
	pub(in crate::queue) fn execution_started_at_unix_micros(&self) -> Option<i64> {
		self.lifecycle
			.running()
			.and_then(|running| running.started_at())
	}
	pub(in crate::queue) fn execution_heartbeat_at_unix_micros(&self) -> Option<i64> {
		self.lifecycle.running().map(|running| running.heartbeat_at)
	}
	pub(in crate::queue) fn finished_at_unix_micros(&self) -> Option<i64> {
		self.lifecycle.finished_at()
	}

	pub(super) fn with_payload(&self, payload_json: String) -> Job {
		Job {
			id: self.id,
			task_name: self.task_name.clone(),
			payload_json,
			lifecycle: self.lifecycle.observed(),
			run_at_or_after_unix_micros: self.run_at_or_after_unix_micros,
			last_error_msg: self.last_error_msg.clone(),
			retry_count: self.retry_count,
			max_retries: self.max_retries,
			timeout: self.timeout,
			dedupe_key: self.dedupe_key.clone(),
			created_at_unix_micros: self.created_at_unix_micros,
			updated_at_unix_micros: self.updated_at_unix_micros,
		}
	}
}
