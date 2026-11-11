//! Explicit existing fixed-operation budgets, without whole-transaction timeout semantics.

use crate::upgrade::wait::require_valid_wait;
use std::{num::NonZeroU32, time::Duration};

use crate::upgrade::participants::ParticipantInspectionBudget;

/// Explicit budgets for fixed upgrade procedures; waits do not time out a whole COMMIT.
#[derive(Clone, Copy)]
pub(crate) struct NativeUpgradeExecutionLimitsFields {
	/// Maximum rows in a bounded document, payload, or v10 conversion page.
	pub limit: NonZeroU32,
	/// Limits for one complete participant compatibility inspection.
	pub participant_budget: ParticipantInspectionBudget,
	/// Wait budget supplied to a fixed protected-work procedure.
	pub protected_work_wait_timeout: Duration,
	/// Maximum wait for source exclusion where the fixed procedure requires it.
	pub source_lock_wait_timeout: Duration,
}

/// Parsed fixed-procedure budgets, without coverage or transaction authority.
pub(crate) struct NativeUpgradeExecutionLimits {
	fields: NativeUpgradeExecutionLimitsFields,
}

/// A rejected fixed-procedure wait.
#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeUpgradeExecutionLimitsError {
	/// The protected-work wait is zero or exceeds the local clock's range.
	#[error("upgrade protected work wait must be positive and fit the local clock")]
	ProtectedWork,
	/// The source-lock wait is zero or exceeds the local clock's range.
	#[error("upgrade source lock wait must be positive and fit the local clock")]
	SourceLock,
}

impl NativeUpgradeExecutionLimits {
	/// Parses the supplied budgets and checks each local wait.
	pub fn new(
		fields: NativeUpgradeExecutionLimitsFields,
	) -> Result<Self, NativeUpgradeExecutionLimitsError> {
		require_valid_wait(
			fields.protected_work_wait_timeout,
			NativeUpgradeExecutionLimitsError::ProtectedWork,
		)?;
		require_valid_wait(
			fields.source_lock_wait_timeout,
			NativeUpgradeExecutionLimitsError::SourceLock,
		)?;
		Ok(Self { fields })
	}
	/// The same budgets with `limit` in place of the configured one.
	pub(in crate::upgrade) fn with_limit(&self, limit: NonZeroU32) -> Self {
		Self {
			fields: NativeUpgradeExecutionLimitsFields {
				limit,
				..self.fields
			},
		}
	}
	pub(crate) fn limit(&self) -> NonZeroU32 {
		self.fields.limit
	}
	pub(crate) fn participant_budget(&self) -> ParticipantInspectionBudget {
		self.fields.participant_budget
	}
	pub(crate) fn protected_work_wait_timeout(&self) -> Duration {
		self.fields.protected_work_wait_timeout
	}
	pub(crate) fn source_lock_wait_timeout(&self) -> Duration {
		self.fields.source_lock_wait_timeout
	}
}
