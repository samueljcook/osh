//! Fixed registered Throttler membership and fresh database authentication.
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
use crate::runtime::local_admission::throttler_work::{
	RuntimeThrottlerWorkIdentity, RuntimeThrottlerWorkPermit,
};

enum ThrottlerRegisteredPurpose {
	Acquisition,
	Outcome,
	ProbeExtension,
	OrderedOutcome(OwnedPrimitiveOrderedOrigins),
	Cleanup(OwnedPrimitiveOrderedOrigins),
}

/// This sealed pack retains its exact Throttler purpose and pre-BEGIN membership.
pub(crate) struct RegisteredThrottlerTransaction<'a> {
	registration: OwnedPrimitiveRegistration<'a>,
	purpose: ThrottlerRegisteredPurpose,
}
impl RegisteredThrottlerTransaction<'_> {
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
		let (ThrottlerRegisteredPurpose::Cleanup(origins)
		| ThrottlerRegisteredPurpose::OrderedOutcome(origins)) = &self.purpose
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
			ThrottlerRegisteredPurpose::Acquisition
			| ThrottlerRegisteredPurpose::Outcome
			| ThrottlerRegisteredPurpose::ProbeExtension => {
				self.registration.authenticate_current(tx).await?
			}
			ThrottlerRegisteredPurpose::Cleanup(origins)
			| ThrottlerRegisteredPurpose::OrderedOutcome(origins) => {
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
	pub(crate) fn register_throttler_work(
		&self,
		identity: RuntimeThrottlerWorkIdentity,
	) -> Result<RuntimeThrottlerWorkPermit, OwnedPrimitiveAdmissionError> {
		Ok(self
			.config
			.local_admission
			.try_register_throttler_work(identity)?)
	}
	pub(crate) fn register_throttler_acquisition<'a>(
		&'a self,
		owner: &RuntimeThrottlerWorkPermit,
		identity: &RuntimeThrottlerWorkIdentity,
	) -> Result<RegisteredThrottlerTransaction<'a>, OwnedPrimitiveAdmissionError> {
		let permit = self
			.config
			.local_admission
			.try_admit_throttler_acquisition(owner, identity)?;
		Ok(self.throttler_registration(permit, ThrottlerRegisteredPurpose::Acquisition))
	}
	pub(crate) fn register_throttler_outcome<'a>(
		&'a self,
		owner: &RuntimeThrottlerWorkPermit,
		identity: &RuntimeThrottlerWorkIdentity,
	) -> Result<RegisteredThrottlerTransaction<'a>, OwnedPrimitiveAdmissionError> {
		let permit = self
			.config
			.local_admission
			.try_admit_throttler_outcome(owner, identity)?;
		Ok(self.throttler_registration(permit, ThrottlerRegisteredPurpose::Outcome))
	}
	pub(crate) fn register_throttler_probe_extension<'a>(
		&'a self,
		owner: &RuntimeThrottlerWorkPermit,
		identity: &RuntimeThrottlerWorkIdentity,
	) -> Result<RegisteredThrottlerTransaction<'a>, OwnedPrimitiveAdmissionError> {
		let permit = self
			.config
			.local_admission
			.try_admit_throttler_probe_extension(owner, identity)?;
		Ok(self.throttler_registration(permit, ThrottlerRegisteredPurpose::ProbeExtension))
	}
	pub(crate) fn register_throttler_outcome_after_probe<'a>(
		&'a self,
		owner: &RuntimeThrottlerWorkPermit,
		identity: &RuntimeThrottlerWorkIdentity,
		origins: OwnedPrimitiveOriginEndings,
	) -> Result<RegisteredThrottlerTransaction<'a>, OwnedPrimitiveAdmissionError> {
		let origins = OwnedPrimitiveOrderedOrigins::new(origins)?;
		let permit = self
			.config
			.local_admission
			.try_admit_throttler_outcome(owner, identity)?;
		Ok(
			self.throttler_registration(
				permit,
				ThrottlerRegisteredPurpose::OrderedOutcome(origins),
			),
		)
	}
	pub(crate) fn register_throttler_cleanup<'a>(
		&'a self,
		owner: &RuntimeThrottlerWorkPermit,
		identity: &RuntimeThrottlerWorkIdentity,
		origins: OwnedPrimitiveOriginEndings,
	) -> Result<RegisteredThrottlerTransaction<'a>, OwnedPrimitiveAdmissionError> {
		let origins = OwnedPrimitiveOrderedOrigins::new(origins)?;
		let permit = self
			.config
			.local_admission
			.try_admit_throttler_cleanup(owner, identity)?;
		Ok(self.throttler_registration(permit, ThrottlerRegisteredPurpose::Cleanup(origins)))
	}
	fn throttler_registration(
		&self,
		permit: crate::runtime::local_admission::RuntimeTransactionPermit,
		purpose: ThrottlerRegisteredPurpose,
	) -> RegisteredThrottlerTransaction<'_> {
		RegisteredThrottlerTransaction {
			registration: OwnedPrimitiveRegistration::new(self, permit),
			purpose,
		}
	}
}
