//! Fixed private attempt boundaries and pure whole-set progress shared by storage owners.
//!
//! Each owner acquires its mutation token before the first boundary await and completes it
//! only after its entire operation is known restored or applied and released.

use std::collections::BTreeSet;
use std::num::{NonZeroU32, NonZeroUsize};

use super::{
	DatabaseError, DatabaseOperationKind, StateStorageError, WriteTx, owned_portable_query,
};
use crate::records::InvalidGroupMutationError;

const CREATE_ATTEMPT_SQL: &str = "SAVEPOINT \"paranoid_record_group_attempt\"";
const RESTORE_ATTEMPT_SQL: &str = "ROLLBACK TO SAVEPOINT \"paranoid_record_group_attempt\"; RELEASE SAVEPOINT \"paranoid_record_group_attempt\"";
const RELEASE_ATTEMPT_SQL: &str = "RELEASE SAVEPOINT \"paranoid_record_group_attempt\"";

/// Explicit materialization and acquisition budgets; database waits remain interruptible.
#[derive(Clone, Copy)]
pub(crate) struct PgRecordGroupAttemptBudget {
	pub(crate) conflict_attempts: NonZeroU32,
	pub(crate) identity_count: NonZeroUsize,
	pub(crate) whole_set_attempts: NonZeroU32,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum GroupAttemptRejectionError {
	#[error("native group identity count exceeds the configured budget")]
	IdentityBudgetExhausted,
	#[error("native group whole-set acquisition exceeds the configured budget")]
	AttemptBudgetExhausted,
	#[error("native group reacquisition requires at least one new identity")]
	NoNewIdentity,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum GroupAttemptBoundaryPhase {
	Create,
	RestoreAndRelease,
	Release,
}
impl GroupAttemptBoundaryPhase {
	fn statement(self) -> &'static str {
		match self {
			Self::Create => CREATE_ATTEMPT_SQL,
			Self::RestoreAndRelease => RESTORE_ATTEMPT_SQL,
			Self::Release => RELEASE_ATTEMPT_SQL,
		}
	}
	fn operation(self) -> &'static str {
		match self {
			Self::Create => "db.state_storage.group_attempt.create",
			Self::RestoreAndRelease => "db.state_storage.group_attempt.restore_and_release",
			Self::Release => "db.state_storage.group_attempt.release",
		}
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum GroupAttemptError {
	#[error("native group storage work failed")]
	Storage(#[from] StateStorageError),
	#[error("native group attempt boundary failed during {phase:?}")]
	Boundary {
		phase: GroupAttemptBoundaryPhase,
		#[source]
		error: DatabaseError,
	},
	#[error("native group plan was rejected and attempt restoration failed")]
	RejectionCleanup {
		rejection_error: InvalidGroupMutationError,
		#[source]
		cleanup_error: Box<Self>,
	},
	#[error("native group reacquisition was rejected and attempt restoration failed")]
	ReacquisitionCleanup {
		rejection_error: GroupAttemptRejectionError,
		#[source]
		cleanup_error: Box<Self>,
	},
}

impl GroupAttemptError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Storage(error) => error.database_error(),
			Self::Boundary { error, .. } => Some(error),
			Self::RejectionCleanup {
				cleanup_error: cleanup,
				..
			}
			| Self::ReacquisitionCleanup {
				cleanup_error: cleanup,
				..
			} => cleanup.database_error(),
		}
	}
	pub(crate) fn involves_database_failure(&self) -> bool {
		self.database_error().is_some()
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		self.database_error()
			.is_some_and(DatabaseError::commit_outcome_is_unknown)
	}
}

pub(crate) async fn execute_boundary(
	tx: &mut WriteTx<'_>,
	phase: GroupAttemptBoundaryPhase,
) -> Result<(), GroupAttemptError> {
	let operation = phase.operation();
	let statement = phase.statement();
	tx.record_database_operation(DatabaseOperationKind::Execute, operation, Some(statement));
	owned_portable_query(statement)
		.execute(tx.inner.as_mut())
		.await
		.map(|_| ())
		.map_err(|error| GroupAttemptError::Boundary {
			phase,
			error: error.with_operation_label(operation),
		})
}

/// This state counts complete acquisitions, including the initial discovery set.
pub(crate) struct RecordAttemptProgress<I> {
	budget: PgRecordGroupAttemptBudget,
	attempts: u32,
	identities: BTreeSet<I>,
}
impl<I: Ord> RecordAttemptProgress<I> {
	pub(crate) fn new(
		identities: BTreeSet<I>,
		budget: PgRecordGroupAttemptBudget,
	) -> Result<Self, GroupAttemptRejectionError> {
		if identities.len() > budget.identity_count.get() {
			return Err(GroupAttemptRejectionError::IdentityBudgetExhausted);
		}
		Ok(Self {
			budget,
			attempts: 1,
			identities,
		})
	}
	pub(crate) fn identities(&self) -> &BTreeSet<I> {
		&self.identities
	}
	pub(crate) fn conflict_attempts(&self) -> NonZeroU32 {
		self.budget.conflict_attempts
	}

	/// The pure rejection is fixed before restoration and retained if cleanup fails.
	pub(crate) async fn restore_and_advance(
		&mut self,
		tx: &mut WriteTx<'_>,
		required: BTreeSet<I>,
	) -> Result<Result<(), GroupAttemptRejectionError>, GroupAttemptError> {
		let old_count = self.identities.len();
		self.identities.extend(required);
		let rejection = if self.identities.len() == old_count {
			Some(GroupAttemptRejectionError::NoNewIdentity)
		} else if self.identities.len() > self.budget.identity_count.get() {
			Some(GroupAttemptRejectionError::IdentityBudgetExhausted)
		} else if self.attempts >= self.budget.whole_set_attempts.get() {
			Some(GroupAttemptRejectionError::AttemptBudgetExhausted)
		} else {
			None
		};
		if let Err(cleanup) =
			execute_boundary(tx, GroupAttemptBoundaryPhase::RestoreAndRelease).await
		{
			return Err(match rejection {
				Some(rejection) => GroupAttemptError::ReacquisitionCleanup {
					rejection_error: rejection,
					cleanup_error: Box::new(cleanup),
				},
				None => cleanup,
			});
		}
		if let Some(rejection) = rejection {
			return Ok(Err(rejection));
		}
		self.attempts += 1;
		execute_boundary(tx, GroupAttemptBoundaryPhase::Create).await?;
		Ok(Ok(()))
	}
}
