//! Only a release attempt for a pending claimed release records the first claimed release.

use super::{
	AcquisitionCandidate, AttemptPurpose, DeliveryTargetState, OperationState, ReceiptHistory,
};
use crate::db::transaction_ending::{TestTransactionEnding, TransactionEndingReceipt};
use crate::fleet::{AcquisitionCapability, HolderId, SemaphoreClaim, SemaphoreKey};
use tokio::time::Instant;

fn claim() -> SemaphoreClaim {
	SemaphoreClaim {
		semaphore_key: SemaphoreKey::new("receipts").expect("a valid Semaphore key"),
		slot_suffix: "1".to_owned(),
		holder_id: HolderId::new("receipts.holder").expect("a valid holder"),
		acquisition_capability: AcquisitionCapability::generate().expect("a capability"),
	}
}

fn started(acquisition: &TransactionEndingReceipt) -> ReceiptHistory {
	ReceiptHistory::Started {
		acquisition: acquisition.clone(),
		first_release: std::sync::OnceLock::new(),
		latest: acquisition.clone(),
	}
}

fn first_release(history: &ReceiptHistory) -> Option<&TransactionEndingReceipt> {
	match history {
		ReceiptHistory::Unstarted => None,
		ReceiptHistory::Started { first_release, .. } => first_release.get(),
	}
}

fn latest(history: &ReceiptHistory) -> Option<&TransactionEndingReceipt> {
	match history {
		ReceiptHistory::Unstarted => None,
		ReceiptHistory::Started { latest, .. } => Some(latest),
	}
}

/// Each purpose in each state in which the operation registers a transaction with it.
fn registrations() -> Vec<(&'static str, AttemptPurpose, OperationState)> {
	vec![
		(
			"acquisition, new",
			AttemptPurpose::Acquisition,
			OperationState::New,
		),
		(
			"release, release pending",
			AttemptPurpose::Release,
			OperationState::ReleasePending(claim()),
		),
		(
			"release, cleanup pending",
			AttemptPurpose::Release,
			OperationState::CleanupPending(DeliveryTargetState::Unavailable),
		),
		(
			"proof, acquisition pending",
			AttemptPurpose::Proof,
			OperationState::AcquisitionPending {
				hold_until: Instant::now(),
				candidate: AcquisitionCandidate::Unprepared,
			},
		),
		(
			"proof, confirming held",
			AttemptPurpose::Proof,
			OperationState::ConfirmingHeld {
				claim: claim(),
				hold_until: Instant::now(),
				target: DeliveryTargetState::Unavailable,
			},
		),
		(
			"proof, release pending",
			AttemptPurpose::Proof,
			OperationState::ReleasePending(claim()),
		),
		(
			"proof, cleanup pending",
			AttemptPurpose::Proof,
			OperationState::CleanupPending(DeliveryTargetState::Unavailable),
		),
	]
}

#[test]
fn only_a_release_for_a_pending_claimed_release_records_the_first_claimed_release() {
	for (case, purpose, state) in registrations() {
		let records_first_release = matches!(
			(&purpose, &state),
			(AttemptPurpose::Release, OperationState::ReleasePending(_))
		);
		let acquisition = TestTransactionEnding::new().receipt();
		let attempt = TestTransactionEnding::new().receipt();
		let mut history = started(&acquisition);
		history
			.record(purpose, &state, &attempt)
			.unwrap_or_else(|error| panic!("{case}: record the attempt: {error:?}"));
		assert!(
			latest(&history).is_some_and(|latest| latest.shares_ending(&attempt)),
			"{case}: the attempt is the latest transaction"
		);
		assert_eq!(
			first_release(&history).is_some_and(|first| first.shares_ending(&attempt)),
			records_first_release,
			"{case}: whether the attempt is the first claimed release"
		);
	}
}

#[test]
fn a_later_release_attempt_keeps_the_first_claimed_release() {
	let acquisition = TestTransactionEnding::new().receipt();
	let first = TestTransactionEnding::new().receipt();
	let later = TestTransactionEnding::new().receipt();
	let state = OperationState::ReleasePending(claim());
	let mut history = started(&acquisition);
	history
		.record(AttemptPurpose::Release, &state, &first)
		.expect("record the first release attempt");
	history
		.record(AttemptPurpose::Release, &state, &later)
		.expect("record the later release attempt");
	assert!(first_release(&history).is_some_and(|recorded| recorded.shares_ending(&first)));
	assert!(latest(&history).is_some_and(|recorded| recorded.shares_ending(&later)));
}
