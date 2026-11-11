//! Fixed registered Semaphore membership and fresh database authentication.
//!
//! Local continuation after stop does not bypass private registration expiry. Physical
//! origin ordering supplies no semantic permission, and the actual registration precedes BEGIN.

use super::owned_primitive::{
	OriginOrderingProgress, OwnedPrimitiveAdmissionError, OwnedPrimitiveAuthenticationFields,
	OwnedPrimitiveCleanupPhase, OwnedPrimitiveOrderedOrigins, OwnedPrimitiveOrderingFields,
	OwnedPrimitiveOriginEndings, OwnedPrimitiveOriginOrdering, OwnedPrimitiveRegistration,
};
use super::{NativeAdmittedContracts, PgNativeAdmission, WriteTx};
use crate::db::OwnedTransactionRegistration;
use crate::db::transaction_ending::TransactionEndingReceipt;
use crate::runtime::local_admission::semaphore_work::{
	RuntimeSemaphoreWorkIdentity, RuntimeSemaphoreWorkPermit,
};

enum SemaphoreRegisteredPurpose {
	Acquisition,
	Release(OwnedPrimitiveOrderedOrigins),
	Cleanup(OwnedPrimitiveOrderedOrigins),
}

/// This sealed pack retains its exact Semaphore purpose and pre-BEGIN membership.
pub(crate) struct RegisteredSemaphoreTransaction<'a> {
	registration: OwnedPrimitiveRegistration<'a>,
	purpose: SemaphoreRegisteredPurpose,
}
impl RegisteredSemaphoreTransaction<'_> {
	pub(crate) fn receipt(&self) -> TransactionEndingReceipt {
		self.registration.receipt()
	}
	pub(crate) fn take_registration(&mut self) -> OwnedTransactionRegistration {
		self.registration.take_registration()
	}
	/// Physical ordering is separate from participant authentication and accounting.
	pub(crate) async fn prove_origins(
		&self,
		tx: &mut WriteTx<'_>,
		ordering: &mut OriginOrderingProgress<OwnedPrimitiveOriginOrdering>,
	) -> Result<(), OwnedPrimitiveAdmissionError> {
		self.registration.require_ending(tx)?;
		let mut completion = tx.track_state_mutation();
		let (SemaphoreRegisteredPurpose::Cleanup(origins)
		| SemaphoreRegisteredPurpose::Release(origins)) = &self.purpose
		else {
			return Err(OwnedPrimitiveAdmissionError::OriginPending);
		};
		self.registration
			.observe_origins(tx, OwnedPrimitiveOrderingFields { origins, ordering })
			.await?;
		completion.complete();
		Ok(())
	}
	/// No new permit is attached after BEGIN; the actual Tx already owns this receipt.
	pub(crate) async fn authenticate(
		&self,
		tx: &mut WriteTx<'_>,
		ordering: &mut OriginOrderingProgress<OwnedPrimitiveOriginOrdering>,
		phase: Option<(
			&tokio::sync::watch::Sender<OwnedPrimitiveCleanupPhase>,
			tokio::time::Instant,
		)>,
	) -> Result<NativeAdmittedContracts, OwnedPrimitiveAdmissionError> {
		Ok(match &self.purpose {
			SemaphoreRegisteredPurpose::Acquisition => {
				self.registration.authenticate_current(tx).await?
			}
			SemaphoreRegisteredPurpose::Cleanup(origins)
			| SemaphoreRegisteredPurpose::Release(origins) => {
				self.registration
					.authenticate_ordered(
						tx,
						OwnedPrimitiveAuthenticationFields {
							ordered: OwnedPrimitiveOrderingFields { origins, ordering },
							phase,
						},
					)
					.await?
			}
		})
	}
}
impl PgNativeAdmission {
	pub(crate) fn register_semaphore_work(
		&self,
		identity: RuntimeSemaphoreWorkIdentity,
	) -> Result<RuntimeSemaphoreWorkPermit, OwnedPrimitiveAdmissionError> {
		Ok(self
			.config
			.local_admission
			.try_register_semaphore_work(identity)?)
	}
	pub(crate) fn register_semaphore_acquisition<'a>(
		&'a self,
		owner: &RuntimeSemaphoreWorkPermit,
		identity: &RuntimeSemaphoreWorkIdentity,
	) -> Result<RegisteredSemaphoreTransaction<'a>, OwnedPrimitiveAdmissionError> {
		let permit = self
			.config
			.local_admission
			.try_admit_semaphore_acquisition(owner, identity)?;
		Ok(self.semaphore_registration(permit, SemaphoreRegisteredPurpose::Acquisition))
	}
	pub(crate) fn register_semaphore_release<'a>(
		&'a self,
		owner: &RuntimeSemaphoreWorkPermit,
		identity: &RuntimeSemaphoreWorkIdentity,
		origins: OwnedPrimitiveOriginEndings,
	) -> Result<RegisteredSemaphoreTransaction<'a>, OwnedPrimitiveAdmissionError> {
		let origins = OwnedPrimitiveOrderedOrigins::new(origins)?;
		let permit = self
			.config
			.local_admission
			.try_admit_semaphore_release(owner, identity)?;
		Ok(self.semaphore_registration(permit, SemaphoreRegisteredPurpose::Release(origins)))
	}

	pub(crate) fn register_semaphore_cleanup<'a>(
		&'a self,
		owner: &RuntimeSemaphoreWorkPermit,
		identity: &RuntimeSemaphoreWorkIdentity,
		origins: OwnedPrimitiveOriginEndings,
	) -> Result<RegisteredSemaphoreTransaction<'a>, OwnedPrimitiveAdmissionError> {
		let origins = OwnedPrimitiveOrderedOrigins::new(origins)?;
		let permit = self
			.config
			.local_admission
			.try_admit_semaphore_cleanup(owner, identity)?;
		Ok(self.semaphore_registration(permit, SemaphoreRegisteredPurpose::Cleanup(origins)))
	}
	fn semaphore_registration(
		&self,
		permit: crate::runtime::local_admission::RuntimeTransactionPermit,
		purpose: SemaphoreRegisteredPurpose,
	) -> RegisteredSemaphoreTransaction<'_> {
		RegisteredSemaphoreTransaction {
			registration: OwnedPrimitiveRegistration::new(self, permit),
			purpose,
		}
	}
}
