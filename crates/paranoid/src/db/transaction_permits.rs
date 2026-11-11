//! Local transaction and caller permit traits retained across owned transaction execution.

/// Ownership of a transaction's local runtime registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TransactionWorkOwner {
	Lease(u64),
	Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum LeaseCallerTransactionAssociationError {
	#[error("Lease caller and transaction belong to different runtimes")]
	RuntimeMismatch,
	#[error("transaction is associated with a different work owner")]
	WorkMismatch,
}

/// Local transaction membership retained until the transaction's control statements finish.
pub(crate) trait TransactionPermitHandle: Send + Sync + 'static {
	fn runtime_identity(&self) -> usize;
	fn work_owner(&self) -> Option<TransactionWorkOwner>;
}

/// Local Lease caller membership retained until the transaction's control statements finish.
pub(crate) trait LeaseCallerPermitHandle: Send + Sync + 'static {
	fn runtime_identity(&self) -> usize;
	fn lease_work_id(&self) -> u64;

	fn check_transaction_association(
		&self,
		transaction: &dyn TransactionPermitHandle,
	) -> Result<(), LeaseCallerTransactionAssociationError> {
		if self.runtime_identity() != transaction.runtime_identity() {
			return Err(LeaseCallerTransactionAssociationError::RuntimeMismatch);
		}
		match transaction.work_owner() {
			None => Ok(()),
			Some(TransactionWorkOwner::Lease(id)) if id == self.lease_work_id() => Ok(()),
			Some(_) => Err(LeaseCallerTransactionAssociationError::WorkMismatch),
		}
	}
}
