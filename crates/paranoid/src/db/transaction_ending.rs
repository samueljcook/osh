//! One actual transaction's local ending observation, without physical drainage proof.

use std::{
	fmt,
	sync::{
		Arc, Mutex,
		atomic::{AtomicU8, Ordering},
	},
};
use tokio::sync::Notify;

use super::{DatabaseError, TransactionPhase};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequestedTransactionEnding {
	Commit,
	Rollback,
}

impl RequestedTransactionEnding {
	pub(super) fn phase(self) -> TransactionPhase {
		match self {
			Self::Commit => TransactionPhase::Commit,
			Self::Rollback => TransactionPhase::Rollback,
		}
	}
}

#[derive(Clone, Copy)]
#[repr(u8)]
enum CommitAttemptState {
	NotAttempted,
	Attempted,
}

/// Records the driver COMMIT call; the receipt separately records its observed outcome.
#[derive(Clone)]
pub(super) struct CommitAttemptMarker(Arc<AtomicU8>);

impl CommitAttemptMarker {
	pub(super) fn new() -> Self {
		Self(Arc::new(AtomicU8::new(
			CommitAttemptState::NotAttempted as u8,
		)))
	}

	pub(super) fn mark_attempted(&self) {
		self.0
			.store(CommitAttemptState::Attempted as u8, Ordering::Release);
	}

	pub(super) fn was_attempted(&self) -> bool {
		match self.0.load(Ordering::Acquire) {
			value if value == CommitAttemptState::NotAttempted as u8 => false,
			value if value == CommitAttemptState::Attempted as u8 => true,
			_ => unreachable!("commit attempts have only NotAttempted and Attempted states"),
		}
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TransactionEndingDisposition {
	KnownCommit,
	KnownRollback,
	UnconfirmedDisposal,
	Unknown,
	TaskAbandoned,
}

/// The terminal outcome of one transaction's ending, with the data of each disposition.
pub(crate) enum TransactionEndingResult {
	/// The `COMMIT` that Paranoid sent succeeded.
	KnownCommit,
	/// The `ROLLBACK` succeeded. The error is the failure that the ending reports when the
	/// rollback replaced a requested commit.
	KnownRollback { error: Option<DatabaseError> },
	/// The transaction ended locally without a confirmed server outcome. The error is absent
	/// only when the owner dropped the transaction before any ending was requested.
	UnconfirmedDisposal { error: Option<DatabaseError> },
	/// Paranoid asked the driver to send the `COMMIT`, and that call failed, so the transaction
	/// may or may not have committed. The driver can return an earlier statement's unread error
	/// before it writes the `COMMIT`.
	Unknown { error: DatabaseError },
	/// The owner dropped the transaction while its requested ending had not reported.
	TaskAbandoned {
		commit_attempted: bool,
		error: DatabaseError,
	},
}
impl TransactionEndingResult {
	pub(crate) fn disposition(&self) -> TransactionEndingDisposition {
		match self {
			Self::KnownCommit => TransactionEndingDisposition::KnownCommit,
			Self::KnownRollback { .. } => TransactionEndingDisposition::KnownRollback,
			Self::UnconfirmedDisposal { .. } => TransactionEndingDisposition::UnconfirmedDisposal,
			Self::Unknown { .. } => TransactionEndingDisposition::Unknown,
			Self::TaskAbandoned { .. } => TransactionEndingDisposition::TaskAbandoned,
		}
	}
	pub(crate) fn commit_attempted(&self) -> bool {
		match self {
			Self::KnownCommit | Self::Unknown { .. } => true,
			Self::KnownRollback { .. } | Self::UnconfirmedDisposal { .. } => false,
			Self::TaskAbandoned {
				commit_attempted, ..
			} => *commit_attempted,
		}
	}
	pub(crate) fn error(&self) -> Option<&DatabaseError> {
		match self {
			Self::KnownCommit => None,
			Self::KnownRollback { error } | Self::UnconfirmedDisposal { error } => error.as_ref(),
			Self::Unknown { error } | Self::TaskAbandoned { error, .. } => Some(error),
		}
	}
}
impl fmt::Debug for TransactionEndingResult {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("TransactionEndingResult")
			.field("disposition", &self.disposition())
			.field("commit_attempted", &self.commit_attempted())
			.field("has_error", &self.error().is_some())
			.finish()
	}
}

#[derive(Clone, Debug)]
pub(crate) enum TransactionEndingSnapshot {
	Open,
	Pending {
		requested: RequestedTransactionEnding,
	},
	Terminal(Arc<TransactionEndingResult>),
}

pub(super) enum PendingTransactionEnding {
	Commit {
		attempted: CommitAttemptMarker,
		operation_label: &'static str,
	},
	RejectedCommit {
		primary: DatabaseError,
	},
	Rollback {
		operation_label: &'static str,
	},
}

impl PendingTransactionEnding {
	/// A rollback in place of a requested commit, which `primary` prevented.
	pub(super) fn rejected_commit(operation_label: &'static str, primary: sqlx::Error) -> Self {
		Self::RejectedCommit {
			primary: DatabaseError::for_transaction(
				operation_label,
				TransactionPhase::Commit,
				None,
				primary,
			)
			.with_commit_attempted(false),
		}
	}

	pub(super) fn shared(&self) -> Self {
		match self {
			Self::Commit {
				attempted,
				operation_label,
			} => Self::Commit {
				attempted: attempted.clone(),
				operation_label,
			},
			Self::RejectedCommit { primary } => Self::RejectedCommit {
				primary: primary.shared(),
			},
			Self::Rollback { operation_label } => Self::Rollback { operation_label },
		}
	}

	pub(super) fn requested(&self) -> RequestedTransactionEnding {
		match self {
			Self::Commit { .. } | Self::RejectedCommit { .. } => RequestedTransactionEnding::Commit,
			Self::Rollback { .. } => RequestedTransactionEnding::Rollback,
		}
	}

	pub(super) fn operation_label(&self) -> &'static str {
		match self {
			Self::Commit {
				operation_label, ..
			}
			| Self::Rollback { operation_label } => operation_label,
			Self::RejectedCommit { primary } => primary.operation_label(),
		}
	}

	/// Whether Paranoid asked the driver to send the `COMMIT` of this ending.
	pub(super) fn commit_attempted(&self) -> bool {
		match self {
			Self::Commit { attempted, .. } => attempted.was_attempted(),
			Self::RejectedCommit { .. } | Self::Rollback { .. } => false,
		}
	}

	/// The failure that replaced a requested commit with a rollback.
	pub(super) fn primary(&self) -> Option<&DatabaseError> {
		match self {
			Self::RejectedCommit { primary } => Some(primary),
			Self::Commit { .. } | Self::Rollback { .. } => None,
		}
	}

	pub(super) fn abandoned_error(&self, driver_error: sqlx::Error) -> (bool, DatabaseError) {
		match self {
			Self::RejectedCommit { primary, .. } => (
				false,
				primary
					.shared()
					.with_rollback_error(DatabaseError::for_transaction(
						"db.tx.rollback",
						TransactionPhase::Rollback,
						None,
						driver_error,
					)),
			),
			Self::Commit {
				attempted,
				operation_label,
			} => {
				let attempted = attempted.was_attempted();
				(
					attempted,
					DatabaseError::for_transaction(
						operation_label,
						TransactionPhase::Commit,
						None,
						driver_error,
					)
					.with_commit_attempted(attempted),
				)
			}
			Self::Rollback { operation_label } => (
				false,
				DatabaseError::for_transaction(
					operation_label,
					TransactionPhase::Rollback,
					None,
					driver_error,
				),
			),
		}
	}
}

enum EndingState {
	Open,
	Pending(PendingTransactionEnding),
	Terminal(Arc<TransactionEndingResult>),
}
struct SharedEnding {
	state: Mutex<EndingState>,
	changed: Notify,
}

/// A receipt grants no transaction, SQL, mutation, or cleanup authority.
#[derive(Clone)]
pub(crate) struct TransactionEndingReceipt {
	shared: Arc<SharedEnding>,
}
impl TransactionEndingReceipt {
	pub(crate) fn shares_ending(&self, other: &Self) -> bool {
		Arc::ptr_eq(&self.shared, &other.shared)
	}

	pub(crate) fn snapshot(&self) -> TransactionEndingSnapshot {
		match &*self
			.shared
			.state
			.lock()
			.expect("ending state contains no caller code")
		{
			EndingState::Open => TransactionEndingSnapshot::Open,
			EndingState::Pending(ending) => TransactionEndingSnapshot::Pending {
				requested: ending.requested(),
			},
			EndingState::Terminal(outcome) => {
				TransactionEndingSnapshot::Terminal(Arc::clone(outcome))
			}
		}
	}
	/// Registers its waiter before observing the level, so terminal publication cannot be missed.
	pub(crate) async fn wait_until_terminal(&self) -> Arc<TransactionEndingResult> {
		loop {
			let changed = self.shared.changed.notified();
			tokio::pin!(changed);
			changed.as_mut().enable();
			if let TransactionEndingSnapshot::Terminal(outcome) = self.snapshot() {
				return outcome;
			}
			changed.await;
		}
	}
}
impl fmt::Debug for TransactionEndingReceipt {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("TransactionEndingReceipt")
			.field("observation", &self.snapshot())
			.finish()
	}
}

/// The actual transaction owns the only publisher through its local and background lifetimes.
pub(super) struct TransactionEndingPublisher {
	receipt: TransactionEndingReceipt,
}
impl TransactionEndingPublisher {
	pub(super) fn new() -> Self {
		Self {
			receipt: TransactionEndingReceipt {
				shared: Arc::new(SharedEnding {
					state: Mutex::new(EndingState::Open),
					changed: Notify::new(),
				}),
			},
		}
	}
	pub(super) fn receipt(&self) -> TransactionEndingReceipt {
		self.receipt.clone()
	}
	pub(super) fn mark_pending(&self, ending: PendingTransactionEnding) {
		let mut state = self
			.receipt
			.shared
			.state
			.lock()
			.expect("ending state contains no caller code");
		if matches!(*state, EndingState::Open) {
			*state = EndingState::Pending(ending);
		}
		drop(state);
		self.receipt.shared.changed.notify_waiters();
	}
	pub(super) fn record_primary_before_rollback(&self, primary: DatabaseError) {
		let mut state = self
			.receipt
			.shared
			.state
			.lock()
			.expect("ending state contains no caller code");
		if matches!(
			&*state,
			EndingState::Pending(PendingTransactionEnding::Commit { .. })
		) {
			*state = EndingState::Pending(PendingTransactionEnding::RejectedCommit { primary });
		}
	}
	pub(super) fn complete(&self, result: TransactionEndingResult) {
		let mut state = self
			.receipt
			.shared
			.state
			.lock()
			.expect("ending state contains no caller code");
		if !matches!(*state, EndingState::Terminal(_)) {
			*state = EndingState::Terminal(Arc::new(result));
		}
		drop(state);
		self.receipt.shared.changed.notify_waiters();
	}

	/// Publishes the local end of a transaction with an unconfirmed server outcome.
	pub(super) fn complete_unconfirmed(&self, error: &DatabaseError) {
		self.complete(TransactionEndingResult::UnconfirmedDisposal {
			error: Some(error.shared()),
		});
	}
	pub(super) fn mark_owner_dropped(&self) {
		let mut state = self
			.receipt
			.shared
			.state
			.lock()
			.expect("ending state contains no caller code");
		let result = match &*state {
			EndingState::Terminal(_) => return,
			EndingState::Open => TransactionEndingResult::UnconfirmedDisposal { error: None },
			EndingState::Pending(ending) => {
				let (commit_attempted, error) = ending.abandoned_error(sqlx::Error::WorkerCrashed);
				TransactionEndingResult::TaskAbandoned {
					commit_attempted,
					error,
				}
			}
		};
		*state = EndingState::Terminal(Arc::new(result));
		drop(state);
		self.receipt.shared.changed.notify_waiters();
	}
}

/// A publisher that drops before its transaction's ending is terminal publishes that its owner
/// dropped the transaction.
impl Drop for TransactionEndingPublisher {
	fn drop(&mut self) {
		self.mark_owner_dropped();
	}
}

/// Lets a unit test outside `db` control a transaction ending.
#[cfg(test)]
pub(crate) struct TestTransactionEnding(TransactionEndingPublisher);
#[cfg(test)]
impl TestTransactionEnding {
	pub(crate) fn new() -> Self {
		Self(TransactionEndingPublisher::new())
	}
	pub(crate) fn receipt(&self) -> TransactionEndingReceipt {
		self.0.receipt()
	}
	/// Publishes a failed `COMMIT` with an unknown outcome.
	pub(crate) fn complete_with_unknown_commit(&self) {
		self.0.complete(TransactionEndingResult::Unknown {
			error: DatabaseError::for_transaction(
				"db.tx.commit",
				TransactionPhase::Commit,
				None,
				sqlx::Error::Io(std::io::Error::other("test")),
			),
		});
	}
}
