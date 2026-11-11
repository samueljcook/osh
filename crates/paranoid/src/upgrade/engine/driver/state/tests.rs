use super::*;
use crate::db::transaction_ending::{TestTransactionEnding, TransactionEndingDisposition};
use crate::runtime::local_admission::upgrade_work::RuntimeUpgradeWorkIdentity;
use crate::upgrade::handle::error::{PgUpgradeFailure, PgUpgradeFailureKind};
use crate::upgrade::requests::work::recovery_observation::{
	NativeUpgradeRecoveryOrigin, NativeUpgradeRecoveryOriginFields,
};
use primitives_rs::id::time_sortable::SortableId;

fn identity() -> RuntimeUpgradeWorkIdentity {
	RuntimeUpgradeWorkIdentity {
		installation_id: SortableId::from_bytes(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
			.unwrap(),
		request_id: SortableId::from_bytes(&[2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
			.unwrap(),
	}
}

#[test]
fn invalidating_an_unaccepted_observation_preserves_its_rejection_history() {
	let mut observation = DriverObservation::new();
	assert!(observation.request_reobservation());
	observation.invalidate();
	assert!(!observation.request_reobservation());
	observation.defer_failure(Arc::new(NativeUpgradeDriverError::WorkAbsent));
	observation.invalidate();
	assert!(!observation.request_reobservation());
	assert!(matches!(
		observation.take_deferred_failure().unwrap().as_ref(),
		NativeUpgradeDriverError::WorkAbsent
	));
}

#[test]
fn a_terminal_receipt_does_not_permit_dispatch_before_owner_settlement() {
	let publisher = TestTransactionEnding::new();
	let original = publisher.receipt();
	let mut transaction = DriverTransaction::Idle;
	transaction.begin_dispatch(DispatchKind::Mutation).unwrap();
	*transaction.slot_mut() = Some(original.clone());
	assert!(transaction.finish_dispatch());
	publisher.complete_with_unknown_commit();
	assert!(transaction.begin_dispatch(DispatchKind::Read).is_err());
	let recovery = NativeUpgradeRecoveryState::Unordered(
		NativeUpgradeRecoveryOrigin::new(NativeUpgradeRecoveryOriginFields {
			identity: identity(),
			receipt: transaction.receipt().unwrap().clone(),
		})
		.unwrap(),
	);
	transaction.record_settled();
	transaction.begin_dispatch(DispatchKind::Read).unwrap();
	let later = TestTransactionEnding::new();
	*transaction.slot_mut() = Some(later.receipt());
	assert!(transaction.finish_dispatch());
	assert!(!transaction.receipt().unwrap().shares_ending(&original));
	assert_eq!(
		recovery.origin().ending().disposition(),
		TransactionEndingDisposition::Unknown
	);
	assert!(recovery.origin().ending().commit_attempted());
}

#[test]
fn unresolved_recovery_requires_a_failure_even_without_a_prior_operation_error() {
	let publisher = TestTransactionEnding::new();
	publisher.complete_with_unknown_commit();
	let recovery = NativeUpgradeRecoveryState::Unordered(
		NativeUpgradeRecoveryOrigin::new(NativeUpgradeRecoveryOriginFields {
			identity: identity(),
			receipt: publisher.receipt(),
		})
		.unwrap(),
	);
	let completion =
		DriverCompletion::Succeeded(DriverExit::Stopped).unresolved_recovery(&recovery, None);
	assert_eq!(completion.phase(), PgUpgradePhase::Unresolved);
	let source = completion
		.failure()
		.expect("unresolved recovery retains its actual cause");
	let failure = PgUpgradeFailure::new(source);
	assert_eq!(failure.kind(), PgUpgradeFailureKind::UncertainMutation);
	assert!(failure.commit_outcome_is_unknown());
	assert!(
		failure
			.database_error()
			.is_some_and(|error| error.commit_outcome_is_unknown())
	);
	let preceding = Arc::new(NativeUpgradeDriverError::WorkAbsent);
	let with_source = DriverCompletion::Failed(preceding).unresolved_recovery(&recovery, None);
	drop(recovery);
	let failure = PgUpgradeFailure::new(with_source.failure().unwrap());
	assert_eq!(failure.kind(), PgUpgradeFailureKind::UncertainMutation);
	assert_eq!(
		failure.source_failure().unwrap().kind(),
		PgUpgradeFailureKind::WorkAbsent
	);
	assert!(failure.commit_outcome_is_unknown());
}
