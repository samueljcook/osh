use super::*;
use crate::{
	fleet::{SemaphoreKey, ThrottlerKey},
	records::{RecordIdentity, RecordKey, RecordNamespace},
	runtime::local_admission::{
		RuntimeAdmissionStop,
		lease_work::{LeaseWork, RuntimeLeaseContinuation},
		semaphore_work::{
			RuntimeSemaphoreWorkIdentity, RuntimeSemaphoreWorkIdentityFields, SemaphoreWork,
		},
		throttler_work::{
			RuntimeThrottlerWorkIdentity, RuntimeThrottlerWorkIdentityFields, ThrottlerWork,
		},
		upgrade_work::{RuntimeUpgradeWorkIdentity, UpgradeWork},
	},
	scope::{ScopeIdentity, ScopeIdentityFields},
};
use primitives_rs::id::time_sortable::SortableId;
use std::time::Duration;
use tokio::{
	sync::oneshot,
	time::{Instant, timeout},
};

fn tracker() -> RuntimeAdmissionTracker {
	RuntimeAdmissionTracker::new(Instant::now() + Duration::from_secs(60))
}
fn scope() -> ScopeIdentity {
	ScopeIdentity::new(ScopeIdentityFields {
		namespace: RecordNamespace::new(b"ownership".to_vec()).unwrap(),
	})
}
fn lease_identity() -> RecordIdentity {
	RecordIdentity::new(
		scope().namespace().clone(),
		RecordKey::new(b"lease".to_vec()),
	)
}
fn semaphore_identity() -> RuntimeSemaphoreWorkIdentity {
	RuntimeSemaphoreWorkIdentity::new(RuntimeSemaphoreWorkIdentityFields {
		scope: &scope(),
		resource: &SemaphoreKey::new("ownership").unwrap(),
	})
}
fn throttler_identity() -> RuntimeThrottlerWorkIdentity {
	RuntimeThrottlerWorkIdentity::new(RuntimeThrottlerWorkIdentityFields {
		scope: &scope(),
		resource: &ThrottlerKey::new("ownership").unwrap(),
	})
}
fn upgrade_identity() -> RuntimeUpgradeWorkIdentity {
	RuntimeUpgradeWorkIdentity::from_parts(
		SortableId::min_at_unix_micros(0),
		SortableId::min_at_unix_micros(1),
	)
}

fn check_continuation_retention<K: RuntimeWorkKind>(identity: K::Identity) {
	let runtime = tracker();
	let mut owner = runtime.try_register_work::<K>(identity.clone()).unwrap();
	let id = owner.id();
	let transaction = {
		let mut state = runtime.0.lock();
		runtime
			.0
			.retain_work_transaction(&mut state, K::owned_id(id))
			.unwrap()
	};
	assert!(owner.complete_local(WorkCompletion::Finished).is_err());
	assert!(owner.snapshot().unwrap().continuation.is_retained());
	assert_eq!(runtime.snapshot().pending_transaction_count, 1);
	runtime.stop(RuntimeAdmissionStop::Shutdown);
	assert!(runtime.finish_local_lifetime().is_err());
	drop(transaction);
	owner.complete_local(WorkCompletion::Finished).unwrap();
	assert!(owner.snapshot().is_err());
	runtime.finish_local_lifetime().unwrap();
	assert!(runtime.try_register_work::<K>(identity).is_err());
}

#[test]
fn every_work_kind_retains_continuations_until_the_transaction_permit_ends() {
	check_continuation_retention::<LeaseWork>(lease_identity());
	check_continuation_retention::<SemaphoreWork>(semaphore_identity());
	check_continuation_retention::<ThrottlerWork>(throttler_identity());
	check_continuation_retention::<UpgradeWork>(upgrade_identity());
}

fn check_terminal_diagnostics<K: RuntimeWorkKind>(identity: K::Identity) {
	let runtime = tracker();
	let mut owner = runtime.try_register_work::<K>(identity.clone()).unwrap();
	let cause: Arc<dyn std::error::Error + Send + Sync> =
		Arc::new(std::io::Error::other("unresolved control operation"));
	owner.complete_unresolved(Arc::clone(&cause)).unwrap();
	drop(owner);
	let state = runtime.0.lock();
	let entry = K::registry(&state).entries.values().next().unwrap();
	assert_eq!(entry.state, K::State::UNRESOLVED);
	assert!(Arc::ptr_eq(
		&entry.terminal_diagnostic.as_ref().unwrap().0,
		&cause
	));
	assert!(!entry.is_pending());
	drop(state);
	runtime.stop(RuntimeAdmissionStop::Shutdown);
	runtime.finish_local_lifetime().unwrap();
	assert!(runtime.try_register_work::<K>(identity).is_err());
}
#[test]
fn unresolved_completion_retains_its_actual_cause_without_abandoning_ownership() {
	check_terminal_diagnostics::<LeaseWork>(lease_identity());
	check_terminal_diagnostics::<SemaphoreWork>(semaphore_identity());
	check_terminal_diagnostics::<ThrottlerWork>(throttler_identity());
	check_terminal_diagnostics::<UpgradeWork>(upgrade_identity());
}

async fn check_cancelled_join<K: RuntimeWorkKind>(identity: K::Identity) {
	let runtime = tracker();
	let owner = runtime.try_register_work::<K>(identity).unwrap();
	let id = K::owned_id(owner.id());
	let (ready_tx, ready_rx) = oneshot::channel();
	let (exit_tx, exit_rx) = oneshot::channel();
	owner
		.spawn_owned_task(&Handle::current(), |mut owner| async move {
			owner.complete_local(WorkCompletion::Finished).unwrap();
			ready_tx.send(()).unwrap();
			exit_rx.await.unwrap();
		})
		.unwrap();
	ready_rx.await.unwrap();
	runtime.stop(RuntimeAdmissionStop::Shutdown);
	let observer = runtime.clone();
	let waiter = tokio::spawn(async move { observer.wait_until_drained().await });
	timeout(Duration::from_secs(5), async {
		loop {
			if matches!(
				runtime.0.lock().local_work(id).unwrap().task(),
				RuntimeTaskOwnership::Joining
			) {
				break;
			}
			tokio::task::yield_now().await;
		}
	})
	.await
	.unwrap();
	waiter.abort();
	assert!(waiter.await.unwrap_err().is_cancelled());
	assert!(matches!(
		runtime.0.lock().local_work(id).unwrap().task(),
		RuntimeTaskOwnership::Running(_)
	));
	assert!(runtime.finish_local_lifetime().is_err());
	exit_tx.send(()).unwrap();
	timeout(Duration::from_secs(5), runtime.wait_until_drained())
		.await
		.unwrap()
		.unwrap();
	assert!(runtime.0.lock().local_work(id).is_none());
	runtime.finish_local_lifetime().unwrap();
}
#[tokio::test]
async fn a_cancelled_drain_waiter_preserves_the_actual_task_for_every_work_kind() {
	check_cancelled_join::<LeaseWork>(lease_identity()).await;
	check_cancelled_join::<SemaphoreWork>(semaphore_identity()).await;
	check_cancelled_join::<ThrottlerWork>(throttler_identity()).await;
	check_cancelled_join::<UpgradeWork>(upgrade_identity()).await;
}

async fn check_abandonment<K: RuntimeWorkKind>(identity: K::Identity) {
	let runtime = tracker();
	let owner = runtime.try_register_work::<K>(identity).unwrap();
	let id = K::owned_id(owner.id());
	let transaction = {
		let mut state = runtime.0.lock();
		runtime.0.retain_work_transaction(&mut state, id).unwrap()
	};
	drop(owner);
	assert_eq!(runtime.status().stop(), Some(RuntimeAdmissionStop::Fenced));
	assert!(runtime.wait_until_drained().await.is_err());
	drop(transaction);
	assert!(runtime.0.lock().local_work(id).is_some());
	assert!(runtime.finish_local_lifetime().is_err());
}
#[tokio::test]
async fn abandonment_fences_each_work_kind_and_retains_membership_after_continuation_drop() {
	check_abandonment::<LeaseWork>(lease_identity()).await;
	check_abandonment::<SemaphoreWork>(semaphore_identity()).await;
	check_abandonment::<ThrottlerWork>(throttler_identity()).await;
	check_abandonment::<UpgradeWork>(upgrade_identity()).await;
}

#[test]
fn lease_callers_share_ownership_and_preserve_shutdown_renewal_and_final_purpose_rules() {
	let runtime = tracker();
	let identity = lease_identity();
	let mut owner = runtime.try_register_lease_work(identity.clone()).unwrap();
	owner.confirm_held().unwrap();
	let first = owner.try_start_caller_work().unwrap();
	let second = owner.try_start_caller_work().unwrap();
	assert_eq!(owner.snapshot().unwrap().pending_caller_work_count, 2);
	runtime.stop(RuntimeAdmissionStop::Shutdown);
	let transaction = runtime
		.try_admit_lease_continuation(&owner, &identity, RuntimeLeaseContinuation::RenewHeld)
		.unwrap();
	first.require_transaction_association(&transaction).unwrap();
	assert!(
		runtime
			.try_admit_lease_once_completion(&owner, &identity, &first)
			.is_err()
	);
	drop(transaction);
	drop(
		runtime
			.try_admit_lease_once_completion(&owner, &identity, &first)
			.unwrap(),
	);
	owner.begin_closing().unwrap();
	assert!(owner.complete_local(WorkCompletion::Finished).is_err());
	drop(first);
	assert!(
		runtime
			.try_admit_lease_continuation(
				&owner,
				&identity,
				RuntimeLeaseContinuation::ReleaseCleanup
			)
			.is_err()
	);
	drop(second);
	drop(
		runtime
			.try_admit_lease_continuation(
				&owner,
				&identity,
				RuntimeLeaseContinuation::ReleaseCleanup,
			)
			.unwrap(),
	);
	owner.complete_local(WorkCompletion::Finished).unwrap();
	runtime.finish_local_lifetime().unwrap();
}

#[test]
fn semaphore_callers_are_exclusive_and_release_survives_shutdown() {
	let runtime = tracker();
	let identity = semaphore_identity();
	let mut owner = runtime
		.try_register_semaphore_work(identity.clone())
		.unwrap();
	drop(
		runtime
			.try_admit_semaphore_acquisition(&owner, &identity)
			.unwrap(),
	);
	owner.confirm_held().unwrap();
	let caller = owner.try_start_caller_work().unwrap();
	assert!(owner.try_start_caller_work().is_err());
	assert!(owner.begin_closing().is_err());
	assert!(
		runtime
			.try_admit_semaphore_release(&owner, &identity)
			.is_err()
	);
	runtime.stop(RuntimeAdmissionStop::Shutdown);
	drop(caller);
	let transaction = runtime
		.try_admit_semaphore_release(&owner, &identity)
		.unwrap();
	assert_eq!(
		owner.snapshot().unwrap().state,
		RuntimeResourceWorkState::Closing
	);
	assert!(owner.complete().is_err());
	drop(transaction);
	owner.complete().unwrap();
	runtime.finish_local_lifetime().unwrap();
}

#[test]
fn throttler_probes_can_overlap_callers_until_fencing_and_outcomes_retain_their_own_permission() {
	let runtime = tracker();
	let identity = throttler_identity();
	let mut owner = runtime
		.try_register_throttler_work(identity.clone())
		.unwrap();
	owner.confirm_held().unwrap();
	let caller = owner.try_start_caller_work().unwrap();
	assert!(owner.try_start_caller_work().is_err());
	runtime.stop(RuntimeAdmissionStop::Shutdown);
	drop(
		runtime
			.try_admit_throttler_probe_extension(&owner, &identity)
			.unwrap(),
	);
	assert!(
		runtime
			.try_admit_throttler_outcome(&owner, &identity)
			.is_err()
	);
	runtime.stop(RuntimeAdmissionStop::Fenced);
	assert!(
		runtime
			.try_admit_throttler_probe_extension(&owner, &identity)
			.is_err()
	);
	drop(caller);
	drop(
		runtime
			.try_admit_throttler_outcome(&owner, &identity)
			.unwrap(),
	);
	drop(
		runtime
			.try_admit_throttler_cleanup(&owner, &identity)
			.unwrap(),
	);
	owner.complete().unwrap();
	runtime.finish_local_lifetime().unwrap();
}

#[test]
fn upgrade_stopping_and_shutdown_both_prevent_new_continuations() {
	let runtime = tracker();
	let identity = upgrade_identity();
	let mut owner = runtime.try_register_upgrade_work(identity).unwrap();
	let transaction = runtime
		.try_admit_upgrade_continuation(&owner, identity)
		.unwrap();
	assert!(
		runtime
			.try_admit_upgrade_continuation(&owner, identity)
			.is_err()
	);
	owner.begin_stopping().unwrap();
	assert!(owner.complete().is_err());
	drop(transaction);
	assert!(
		runtime
			.try_admit_upgrade_continuation(&owner, identity)
			.is_err()
	);
	owner.complete().unwrap();
	let mut owner = runtime.try_register_upgrade_work(identity).unwrap();
	runtime.stop(RuntimeAdmissionStop::Shutdown);
	assert!(
		runtime
			.try_admit_upgrade_continuation(&owner, identity)
			.is_err()
	);
	owner.complete().unwrap();
	runtime.finish_local_lifetime().unwrap();
}

async fn check_failed_task_cause<K: RuntimeWorkKind>(identity: K::Identity) {
	let runtime = tracker();
	let owner = runtime.try_register_work::<K>(identity).unwrap();
	let id = K::owned_id(owner.id());
	let cause: Arc<dyn std::error::Error + Send + Sync> =
		Arc::new(std::io::Error::other("the original unresolved operation"));
	let original = Arc::clone(&cause);
	let (ready_tx, ready_rx) = oneshot::channel();
	owner
		.spawn_owned_task(&Handle::current(), |mut owner| async move {
			owner.complete_unresolved(cause).unwrap();
			ready_tx.send(()).unwrap();
			panic!("the owned task failed after local unresolved completion");
		})
		.unwrap();
	ready_rx.await.unwrap();
	timeout(Duration::from_secs(5), async {
		loop {
			runtime.status();
			if matches!(
				runtime.0.lock().local_work(id).unwrap().task(),
				RuntimeTaskOwnership::Joined
			) {
				break;
			}
			tokio::task::yield_now().await;
		}
	})
	.await
	.unwrap();
	let retained = runtime
		.0
		.lock()
		.local_work(id)
		.unwrap()
		.diagnostic()
		.unwrap();
	let failed_join = retained
		.error()
		.downcast_ref::<super::super::task::RuntimeTaskJoinError>()
		.unwrap();
	assert!(failed_join.join_error().is_panic());
	assert!(Arc::ptr_eq(
		&failed_join.prior_error().unwrap().0,
		&original
	));
	assert_eq!(runtime.status().stop(), Some(RuntimeAdmissionStop::Fenced));
	assert!(runtime.wait_until_drained().await.is_err());
}
#[tokio::test]
async fn every_work_kind_retains_both_the_original_cause_and_a_later_task_failure() {
	check_failed_task_cause::<LeaseWork>(lease_identity()).await;
	check_failed_task_cause::<SemaphoreWork>(semaphore_identity()).await;
	check_failed_task_cause::<ThrottlerWork>(throttler_identity()).await;
	check_failed_task_cause::<UpgradeWork>(upgrade_identity()).await;
}

#[test]
fn exhausted_transaction_capacity_does_not_consume_a_held_resource() {
	let runtime = tracker();
	let identity = semaphore_identity();
	let mut owner = runtime
		.try_register_semaphore_work(identity.clone())
		.unwrap();
	owner.confirm_held().unwrap();
	runtime.0.lock().pending_transaction_count = usize::MAX;
	assert!(matches!(
		runtime.try_admit_semaphore_release(&owner, &identity),
		Err(RuntimeAdmissionError::CountExhausted)
	));
	let retained = owner.snapshot().unwrap();
	assert_eq!(retained.state, RuntimeResourceWorkState::Held);
	assert!(!retained.continuation.is_retained());
	runtime.0.lock().pending_transaction_count = 0;
	drop(
		runtime
			.try_admit_semaphore_release(&owner, &identity)
			.unwrap(),
	);
	owner.complete().unwrap();
	runtime.stop(RuntimeAdmissionStop::Shutdown);
	runtime.finish_local_lifetime().unwrap();
}
