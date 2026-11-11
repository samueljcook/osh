//! Ordinary Once marker and exact live Lease release use one actual placed pair.

use super::super::DriverShared;
use super::super::final_purpose::{require_final_purpose_deadline, wait_final_purpose_work};
use super::super::once_pair::{OncePairPrepareFields, PreparedOncePair, prepare_once_pair};
use super::error::{
	NativeOnceCompletionCause as Cause, NativeOnceCompletionError as Error,
	NativeOnceCompletionErrorWithObserver,
};
use super::{
	Instant, NativeLeaseCloseObserver, NativeLeaseOnceCompletion, NativeOncePreparedCompletion,
	PgRecordTarget, PgRecordTargetFields,
};
use crate::db::{
	TransactionFailure, WriteTx, finish_pool_owned_write_transaction_and_preserve_rollback_error,
};
use crate::records::RecordMutation;
use crate::records::postgres::record_set::{PgRecordSetLock, PgRecordSetLockFields};

pub(crate) struct NativeOnceCompletionResult {
	pub(crate) completion: NativeOncePreparedCompletion,
	pub(crate) observer: NativeLeaseCloseObserver,
}
impl NativeLeaseOnceCompletion {
	pub(crate) async fn complete_and_release(
		mut self,
	) -> Result<NativeOnceCompletionResult, NativeOnceCompletionErrorWithObserver> {
		let observer = self.close_observer();
		let result = self.execute().await;
		match result {
			Ok(completion) => Ok(NativeOnceCompletionResult {
				completion,
				observer,
			}),
			Err(error) => Err(NativeOnceCompletionErrorWithObserver { error, observer }),
		}
	}
	async fn execute(&mut self) -> Result<NativeOncePreparedCompletion, Error> {
		require_before_deadline(&self.client.shared, self.prepared.deadline)?;
		let registration = self
			.prepared
			.registered
			.take_registration()
			.map_err(|error| Error::new(Cause::Registration(error)))?;
		self.prepared.progress.registration_transferred();
		let shared = &self.client.shared;
		let deadline = self.prepared.deadline;
		let bindings = &self.prepared.once.lease().bindings;
		let mut tx = wait_final_purpose_work(
			shared,
			deadline,
			bindings.control_pool.begin_registered_transaction(
				registration,
				"fleet.native_once.complete_and_release",
			),
		)
		.await
		.map_err(|error| Error::new(Cause::Authority(error)))?
		.map_err(Error::from_database_error)?;
		let marker = tx.statement_interruption_marker();
		let result = match wait_final_purpose_work(
			shared,
			deadline,
			self.prepare_and_apply(&mut tx),
		)
		.await
		{
			Ok(result) => result,
			Err(source) => {
				marker.mark_interrupted();
				Err(Error::new(Cause::Authority(source)))
			}
		};
		finish_pool_owned_write_transaction_and_preserve_rollback_error(
			"fleet.native_once.complete_and_release",
			tx,
			result,
			Error::from_database_error,
			|_, operation, rollback| Error::from_rollback_failure(operation, rollback),
		)
		.await
	}
	async fn prepare_and_apply(
		&self,
		tx: &mut WriteTx<'_>,
	) -> Result<NativeOncePreparedCompletion, Error> {
		let prepared = &self.prepared;
		let once = &prepared.once;
		let contracts = prepared
			.registered
			.authenticate_once_completion(tx, once)
			.await
			.map_err(|error| Error::new(Cause::Registration(error)))?;
		require_before_deadline(&self.client.shared, prepared.deadline)?;
		let once_declaration = once.component().declaration();
		let once_contract = contracts
			.bind(&once_declaration)
			.map_err(|error| Error::new(Cause::Admission(error)))?;
		let once_schema = once
			.component()
			.bind_schema(&once_contract)
			.map_err(|error| Error::new(Cause::Completion(error)))?;
		let bindings = &once.lease().bindings;
		let lease_declaration = bindings.component.declaration();
		let lease_contract = contracts
			.bind(&lease_declaration)
			.map_err(|error| Error::new(Cause::Admission(error)))?;
		let lease_schema = lease_contract
			.transitions()
			.bind_schema(&lease_contract, once.lease().key().clone())
			.map_err(|error| Error::new(Cause::LeaseBinding(error)))?;
		let lease_target = PgRecordTarget::new(PgRecordTargetFields {
			storage: &bindings.storage,
			record: once.lease().identity().clone(),
		});
		let completion_target = PgRecordTarget::new(PgRecordTargetFields {
			storage: once.completion_storage(),
			record: once.completion_identity().clone(),
		});
		let lease_placed = lease_target.identity().clone();
		let completion_placed = completion_target.identity().clone();
		if lease_placed == completion_placed {
			return Err(Error::new(Cause::AliasedRecords));
		}
		let group = PgRecordSetLock::acquire(PgRecordSetLockFields {
			tx,
			targets: vec![lease_target, completion_target],
			conflict_attempts: bindings.conflict_attempts,
		})
		.await
		.map_err(|error| Error::new(Cause::Records(error)))?;
		require_before_deadline(&self.client.shared, prepared.deadline)?;
		let plan = prepare_once_pair(OncePairPrepareFields {
			once,
			once_schema: &once_schema,
			lease_schema: &lease_schema,
			authority: &prepared.claim,
			group: &group,
			lease_identity: &lease_placed,
			completion_identity: &completion_placed,
		})
		.map_err(|error| Error::new(Cause::Pair(error)))
		.map(|plan| match plan {
			PreparedOncePair::NoMatch => (
				RecordMutation::Keep,
				RecordMutation::Keep,
				NativeOncePreparedCompletion::Refused,
			),
			PreparedOncePair::ExistingCompletion { completion, lease } => (
				RecordMutation::Keep,
				lease,
				NativeOncePreparedCompletion::AlreadyRecorded(completion),
			),
			PreparedOncePair::NewCompletion {
				completion,
				lease,
				completion_mutation,
			} => (
				completion_mutation,
				lease,
				NativeOncePreparedCompletion::Recorded(completion),
			),
		});
		let (completion_mutation, lease_mutation, intended) = match plan {
			Ok(plan) => plan,
			Err(operation) => {
				return match group.keep().await {
					Ok(()) => Err(operation),
					Err(cleanup) => Err(Error::new(Cause::PreparationCleanup {
						operation_error: Box::new(operation),
						cleanup_error: cleanup,
					})),
				};
			}
		};
		require_before_deadline(&self.client.shared, prepared.deadline)?;
		prepared.progress.prepare(intended.clone());
		if intended.released() {
			group
				.apply(vec![
					(lease_placed, lease_mutation),
					(completion_placed, completion_mutation),
				])
				.await
				.map_err(|error| Error::new(Cause::Records(error)))?
				.map_err(|error| Error::new(Cause::Mutation(error)))?;
		} else {
			group
				.keep()
				.await
				.map_err(|error| Error::new(Cause::Records(error)))?;
		}
		require_before_deadline(&self.client.shared, prepared.deadline)?;
		Ok(intended)
	}
}
fn require_before_deadline(shared: &DriverShared, deadline: Instant) -> Result<Instant, Error> {
	require_final_purpose_deadline(shared, deadline)
		.map_err(|error| Error::new(Cause::Authority(error)))
}
