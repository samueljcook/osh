//! Snapshots of a running upgrade: its phase, its last step, its prerequisites, the end of
//! its last transaction, and its last failure.

use std::{error::Error, fmt};

use crate::db::DatabaseError;
use crate::db::transaction_ending::{
	RequestedTransactionEnding, TransactionEndingDisposition, TransactionEndingSnapshot,
};
use crate::kv::document::schema::IndexContractIdentity;
use crate::scope::ScopeIdentity;
use crate::upgrade::engine::driver::observer::{NativeUpgradeDriverSnapshot, PgUpgradePhase};
use crate::upgrade::engine::selection::{
	NativeUpgradePrerequisiteError, NativeUpgradeScopePrerequisiteError, PgUpgradeStep,
};
use crate::upgrade::work::PgUpgradeStatus;
use crate::upgrade::work::v10::source_checkpoint::PgV10ConversionProcedure;

use super::error::PgUpgradeFailure;

/// The state of an upgrade task in this process when the snapshot was taken.
#[derive(Clone)]
pub struct PgUpgradeSnapshot(pub(super) NativeUpgradeDriverSnapshot);

impl PgUpgradeSnapshot {
	/// The task's phase in this process. The recorded status of the upgrade is
	/// `recorded_status`.
	pub fn phase(&self) -> PgUpgradePhase {
		self.0.phase
	}

	/// The upgrade's status in the installation's record, once the task has read the record.
	pub fn recorded_status(&self) -> Option<PgUpgradeStatus> {
		self.0.work.as_ref().map(|work| work.status())
	}

	/// The step that the task chose last, once it has chosen one.
	pub fn step(&self) -> Option<&PgUpgradeStep> {
		self.0.step.as_ref()
	}

	/// The conditions that must hold before the next step can run. The list is empty when
	/// nothing blocks the upgrade.
	pub fn prerequisites(&self) -> impl Iterator<Item = PgUpgradePrerequisite<'_>> {
		self.0.prerequisites.iter().map(PgUpgradePrerequisite)
	}

	/// How the task's most recent transaction ended, observed when this method is called.
	/// After the task checks a transaction with an unknown result, this is the ending of the
	/// checking transaction.
	pub fn ending(&self) -> Option<PgUpgradeEndingSnapshot> {
		self.0
			.ending
			.as_ref()
			.map(|receipt| PgUpgradeEndingSnapshot(receipt.snapshot()))
	}

	/// The task's most recent failure, if any. The task tries many failed steps again, so this
	/// can be a failure that a later attempt recovered from, even when the phase is
	/// `Completed`. When a failure ended the task, this is that failure.
	pub fn failure(&self) -> Option<PgUpgradeFailure<'_>> {
		self.0.failure.as_deref().map(PgUpgradeFailure::new)
	}
}

impl fmt::Debug for PgUpgradeSnapshot {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("PgUpgradeSnapshot")
			.field("phase", &self.phase())
			.field("recorded_status", &self.recorded_status())
			.finish_non_exhaustive()
	}
}

/// One condition that must hold before an upgrade's next step can run.
#[derive(Clone, Copy)]
pub struct PgUpgradePrerequisite<'a>(&'a NativeUpgradePrerequisiteError);

/// The class of a condition that blocks an upgrade.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgUpgradePrerequisiteKind {
	/// The installation has no scope with this identity, so the upgrade cannot change it.
	MissingCurrentScope,
	/// This runtime's code cannot add the declared scope to the installation.
	AdditiveScopeCode,
	/// The change from the installed scope to the target scope needs a data migration that an
	/// upgrade does not run.
	UnsupportedTransition,
	/// The scope's cache values or Topic payloads cannot be rewritten while the installation
	/// still uses v10 storage for them.
	PayloadsInV10Storage,
	/// The scope's new document writer version cannot be put into use yet, for example because
	/// a running deployment cannot read it.
	DocumentStage,
	/// The scope's documents cannot be finished yet, for example because some documents have
	/// not been rewritten.
	DocumentCompletion,
	/// The scope's index cannot be made available for queries yet, for example because a
	/// running deployment does not maintain it.
	DocumentReadiness,
	/// The scope's new payload writer version cannot be put into use yet, for example because
	/// a running deployment cannot read it.
	PayloadStage,
	/// The scope's cache values or Topic payloads cannot be finished yet.
	PayloadCompletion,
	/// An index cannot advance yet; `index` names it.
	Index,
	/// The installation records progress of an index check for an index that the target
	/// does not make available for queries. This indicates a defect or a changed target.
	VerificationTargetMissing,
	/// The installation records progress of an index check, and no record of the rewrite that
	/// must come first. This indicates a defect.
	VerificationConversionMissing,
	/// The scope's installed writer version conflicts with the recorded progress. This
	/// indicates a defect.
	StageProgressConflict,
	/// The installation records progress of a v10 move after it recorded that every v10
	/// record had moved. This indicates a defect.
	V10ProgressAfterRetirement,
}

impl<'a> PgUpgradePrerequisite<'a> {
	/// The class of the condition.
	pub fn kind(&self) -> PgUpgradePrerequisiteKind {
		use PgUpgradePrerequisiteKind as Kind;
		match self.0 {
			NativeUpgradePrerequisiteError::SourceProgressAfterRetirement { .. } => {
				Kind::V10ProgressAfterRetirement
			}
			NativeUpgradePrerequisiteError::Scope { source, .. } => match source.as_ref() {
				NativeUpgradeScopePrerequisiteError::MissingCurrentScope(_) => {
					Kind::MissingCurrentScope
				}
				NativeUpgradeScopePrerequisiteError::AdditiveScopeCode(_) => {
					Kind::AdditiveScopeCode
				}
				NativeUpgradeScopePrerequisiteError::UnsupportedTransition => {
					Kind::UnsupportedTransition
				}
				NativeUpgradeScopePrerequisiteError::NativePayloadRoute { .. } => {
					Kind::PayloadsInV10Storage
				}
				NativeUpgradeScopePrerequisiteError::DocumentStage(_) => Kind::DocumentStage,
				NativeUpgradeScopePrerequisiteError::DocumentCompletion(_) => {
					Kind::DocumentCompletion
				}
				NativeUpgradeScopePrerequisiteError::DocumentReadiness(_) => {
					Kind::DocumentReadiness
				}
				NativeUpgradeScopePrerequisiteError::PayloadStage(_) => Kind::PayloadStage,
				NativeUpgradeScopePrerequisiteError::PayloadCompletion(_) => {
					Kind::PayloadCompletion
				}
				NativeUpgradeScopePrerequisiteError::Index { .. } => Kind::Index,
				NativeUpgradeScopePrerequisiteError::VerificationTargetMissing => {
					Kind::VerificationTargetMissing
				}
				NativeUpgradeScopePrerequisiteError::VerificationConversionMissing => {
					Kind::VerificationConversionMissing
				}
				NativeUpgradeScopePrerequisiteError::StageProgressConflict => {
					Kind::StageProgressConflict
				}
			},
		}
	}

	/// The condition as an error, with its complete source chain.
	pub fn error(&self) -> &'a (dyn Error + Send + Sync + 'static) {
		self.0
	}

	/// The scope of the condition, when it concerns one scope.
	pub fn scope(&self) -> Option<&'a ScopeIdentity> {
		self.0.scope()
	}

	/// The index, when the condition concerns one index.
	pub fn index(&self) -> Option<&'a IndexContractIdentity> {
		match self.0.scope_source() {
			Some(NativeUpgradeScopePrerequisiteError::Index { index, .. }) => Some(index),
			_ => None,
		}
	}

	/// The v10 move procedure, when the kind is `V10ProgressAfterRetirement`.
	pub fn v10_procedure(&self) -> Option<PgV10ConversionProcedure> {
		match self.0 {
			NativeUpgradePrerequisiteError::SourceProgressAfterRetirement { procedure } => {
				Some(*procedure)
			}
			_ => None,
		}
	}
}

impl fmt::Debug for PgUpgradePrerequisite<'_> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("PgUpgradePrerequisite")
			.field("kind", &self.kind())
			.finish_non_exhaustive()
	}
}

/// How an upgrade task's last transaction ended.
#[derive(Clone)]
pub struct PgUpgradeEndingSnapshot(TransactionEndingSnapshot);

/// The state of an upgrade task's last transaction. A known ending in this process does not
/// show that the database server has finished the transaction's work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgUpgradeEndingKind {
	/// The transaction has not been asked to end.
	Open,
	/// The task sent `COMMIT`, and the result has not arrived.
	CommitPending,
	/// The task sent `ROLLBACK`, and the result has not arrived.
	RollbackPending,
	/// The transaction committed.
	KnownCommit,
	/// The transaction rolled back.
	KnownRollback,
	/// The connection was dropped without a confirmed end of the transaction.
	UnconfirmedDisposal,
	/// The task asked the transaction to end, and the result is unknown.
	Unknown,
	/// The task that ran the transaction stopped before the transaction ended.
	TaskAbandoned,
}

impl PgUpgradeEndingSnapshot {
	/// The state of the transaction.
	pub fn kind(&self) -> PgUpgradeEndingKind {
		match &self.0 {
			TransactionEndingSnapshot::Open => PgUpgradeEndingKind::Open,
			TransactionEndingSnapshot::Pending { requested } => match requested {
				RequestedTransactionEnding::Commit => PgUpgradeEndingKind::CommitPending,
				RequestedTransactionEnding::Rollback => PgUpgradeEndingKind::RollbackPending,
			},
			TransactionEndingSnapshot::Terminal(outcome) => match outcome.disposition() {
				TransactionEndingDisposition::KnownCommit => PgUpgradeEndingKind::KnownCommit,
				TransactionEndingDisposition::KnownRollback => PgUpgradeEndingKind::KnownRollback,
				TransactionEndingDisposition::UnconfirmedDisposal => {
					PgUpgradeEndingKind::UnconfirmedDisposal
				}
				TransactionEndingDisposition::Unknown => PgUpgradeEndingKind::Unknown,
				TransactionEndingDisposition::TaskAbandoned => PgUpgradeEndingKind::TaskAbandoned,
			},
		}
	}

	/// Whether the task sent `COMMIT`, once the transaction has ended. A sent `COMMIT` can
	/// still have failed.
	pub fn commit_attempted(&self) -> Option<bool> {
		match &self.0 {
			TransactionEndingSnapshot::Terminal(outcome) => Some(outcome.commit_attempted()),
			_ => None,
		}
	}

	/// The database error of the transaction's end, once the transaction has ended with a
	/// database failure.
	pub fn database_error(&self) -> Option<&DatabaseError> {
		match &self.0 {
			TransactionEndingSnapshot::Terminal(outcome) => outcome.error(),
			_ => None,
		}
	}
}

impl fmt::Debug for PgUpgradeEndingSnapshot {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_tuple("PgUpgradeEndingSnapshot")
			.field(&self.kind())
			.finish()
	}
}
