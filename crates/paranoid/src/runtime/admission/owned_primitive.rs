//! Registered Primitive transaction ownership and exact causal ordering.
//!
//! Physical ordering supplies no semantic permission. Authentication uses the same
//! pinned control observation and retains private participant expiry checks.

use super::{
	Arc, DatabaseError, NativeAdmissionError, NativeAdmittedContracts, NativeSourceGatePhase,
	PgNativeAdmission, PointRecordRead, WriteTx,
};
use crate::db::OwnedTransactionRegistration;
use crate::db::transaction_ending::{
	TransactionEndingDisposition, TransactionEndingReceipt, TransactionEndingResult,
	TransactionEndingSnapshot,
};
use crate::installation::control_storage::ExclusiveControlOrderingProof;
use crate::runtime::local_admission::RuntimeTransactionPermit;

/// The admission failure of a Semaphore, Throttler, or lease cleanup transaction that a
/// runtime owns.
#[derive(Debug, thiserror::Error)]
pub(crate) enum OwnedPrimitiveAdmissionError {
	#[error("the runtime did not admit the transaction")]
	Admission(#[source] Box<NativeAdmissionError>),
	#[error("the operation does not own the ending of its transaction")]
	EndingMismatch,
	#[error("a preceding transaction of the operation has not ended in this process")]
	OriginPending,
	#[error("the wait for the preceding transactions to end in the database elapsed")]
	OrderingWaitElapsed,
}
impl From<NativeAdmissionError> for OwnedPrimitiveAdmissionError {
	fn from(error: NativeAdmissionError) -> Self {
		Self::Admission(Box::new(error))
	}
}
impl From<crate::runtime::local_admission::RuntimeAdmissionError> for OwnedPrimitiveAdmissionError {
	fn from(error: crate::runtime::local_admission::RuntimeAdmissionError) -> Self {
		NativeAdmissionError::Local(error).into()
	}
}
impl OwnedPrimitiveAdmissionError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Admission(error) => error.database_error(),
			_ => None,
		}
	}
	pub(crate) fn involves_database_failure(&self) -> bool {
		matches!(self, Self::Admission(error) if error.involves_database_failure())
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		matches!(self, Self::Admission(error) if error.commit_outcome_is_unknown())
	}
}

pub(crate) struct ThrottlerOriginEndingsFields<'a> {
	pub(crate) original_operation: &'a TransactionEndingReceipt,
	pub(crate) preceding_operation: &'a TransactionEndingReceipt,
	pub(crate) pending_origins: &'a OwnedPrimitivePendingOrigins,
}
pub(crate) struct ThrottlerProbeOriginEndingsFields<'a> {
	pub(crate) outcome: ThrottlerOriginEndingsFields<'a>,
	pub(crate) acquisition: &'a TransactionEndingReceipt,
	pub(crate) heartbeat: &'a TransactionEndingReceipt,
}
pub(crate) struct SemaphoreOriginEndingsFields<'a> {
	pub(crate) acquisition: &'a TransactionEndingReceipt,
	pub(crate) first_claimed_release: Option<&'a TransactionEndingReceipt>,
	pub(crate) preceding_operation: &'a TransactionEndingReceipt,
	pub(crate) pending_origins: &'a OwnedPrimitivePendingOrigins,
}
/// Comparison-only exact placed Cache/Lease association; it supplies no mutation permission.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct CacheLeaseOriginIdentity {
	scope: crate::scope::ScopeIdentity,
	entry: crate::records::postgres::record_set::PgPlacedRecordIdentity,
	lease: crate::records::postgres::record_set::PgPlacedRecordIdentity,
}
pub(crate) struct CacheLeaseOriginIdentityFields {
	pub(crate) scope: crate::scope::ScopeIdentity,
	pub(crate) entry: crate::records::postgres::record_set::PgPlacedRecordIdentity,
	pub(crate) lease: crate::records::postgres::record_set::PgPlacedRecordIdentity,
}
impl CacheLeaseOriginIdentity {
	pub(crate) fn new(fields: CacheLeaseOriginIdentityFields) -> Self {
		Self {
			scope: fields.scope,
			entry: fields.entry,
			lease: fields.lease,
		}
	}
	pub(crate) fn lease(&self) -> &crate::records::postgres::record_set::PgPlacedRecordIdentity {
		&self.lease
	}
}
pub(crate) struct CacheLeaseOriginEndingsFields<'a> {
	pub(crate) identity: &'a CacheLeaseOriginIdentity,
	pub(crate) lease_operation: &'a TransactionEndingReceipt,
	pub(crate) publication: &'a TransactionEndingReceipt,
	pub(crate) preceding_operation: &'a TransactionEndingReceipt,
	pub(crate) pending_origins: &'a OwnedPrimitivePendingOrigins,
}
/// Comparison-only actual Once identity; it grants no claim or executable code.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct OnceLeaseOriginIdentity {
	key: crate::fleet::OnceKey,
	completion: crate::records::postgres::record_set::PgPlacedRecordIdentity,
	lease: crate::records::postgres::record_set::PgPlacedRecordIdentity,
}
pub(crate) struct OnceLeaseOriginIdentityFields {
	pub(crate) key: crate::fleet::OnceKey,
	pub(crate) completion: crate::records::postgres::record_set::PgPlacedRecordIdentity,
	pub(crate) lease: crate::records::postgres::record_set::PgPlacedRecordIdentity,
}
impl OnceLeaseOriginIdentity {
	pub(crate) fn new(fields: OnceLeaseOriginIdentityFields) -> Self {
		Self {
			key: fields.key,
			completion: fields.completion,
			lease: fields.lease,
		}
	}
	pub(crate) fn lease(&self) -> &crate::records::postgres::record_set::PgPlacedRecordIdentity {
		&self.lease
	}
}
pub(crate) struct OrdinaryOnceLeaseOriginEndingsFields<'a> {
	pub(crate) identity: &'a OnceLeaseOriginIdentity,
	pub(crate) lease_operation: &'a TransactionEndingReceipt,
	pub(crate) completion: &'a TransactionEndingReceipt,
	pub(crate) preceding_operation: &'a TransactionEndingReceipt,
	pub(crate) pending_origins: &'a OwnedPrimitivePendingOrigins,
}
/// Exact retained receipts provide no physical ending or mutation authority.
#[derive(Clone)]
pub(crate) struct OwnedPrimitiveOriginEndings {
	causal: OwnedPrimitiveCausalReceipts,
}
#[derive(Clone)]
enum OwnedPrimitiveCausalReceipts {
	OrdinaryOnceLease {
		identity: Box<OnceLeaseOriginIdentity>,
		lease_operation: TransactionEndingReceipt,
		completion: TransactionEndingReceipt,
		preceding_operation: TransactionEndingReceipt,
		unsettled_predecessor: Option<TransactionEndingReceipt>,
	},
	CacheLease {
		identity: Box<CacheLeaseOriginIdentity>,
		lease_operation: TransactionEndingReceipt,
		publication: TransactionEndingReceipt,
		preceding_operation: TransactionEndingReceipt,
		unsettled_predecessor: Option<TransactionEndingReceipt>,
	},
	Semaphore {
		acquisition: TransactionEndingReceipt,
		first_claimed_release: Option<TransactionEndingReceipt>,
		preceding_operation: TransactionEndingReceipt,
		unsettled_predecessor: Option<TransactionEndingReceipt>,
	},
	Throttler {
		original_operation: TransactionEndingReceipt,
		preceding_operation: TransactionEndingReceipt,
		probe: ThrottlerProbePredecessors,
		unsettled_predecessor: Option<TransactionEndingReceipt>,
	},
}

#[derive(Clone)]
enum ThrottlerProbePredecessors {
	NoProbe,
	Probe {
		acquisition: TransactionEndingReceipt,
		heartbeat: TransactionEndingReceipt,
	},
}
impl OwnedPrimitiveOriginEndings {
	pub(crate) fn cache_lease(fields: CacheLeaseOriginEndingsFields<'_>) -> Self {
		Self {
			causal: OwnedPrimitiveCausalReceipts::CacheLease {
				identity: Box::new(fields.identity.clone()),
				lease_operation: fields.lease_operation.clone(),
				publication: fields.publication.clone(),
				preceding_operation: fields.preceding_operation.clone(),
				unsettled_predecessor: fields
					.pending_origins
					.retained()
					.map(|origins| origins.preceding().clone()),
			},
		}
	}
	pub(crate) fn ordinary_once_lease(fields: OrdinaryOnceLeaseOriginEndingsFields<'_>) -> Self {
		Self {
			causal: OwnedPrimitiveCausalReceipts::OrdinaryOnceLease {
				identity: Box::new(fields.identity.clone()),
				lease_operation: fields.lease_operation.clone(),
				completion: fields.completion.clone(),
				preceding_operation: fields.preceding_operation.clone(),
				unsettled_predecessor: fields
					.pending_origins
					.retained()
					.map(|origins| origins.preceding().clone()),
			},
		}
	}
	pub(super) fn lease_final_identity(
		&self,
	) -> Option<&crate::records::postgres::record_set::PgPlacedRecordIdentity> {
		match &self.causal {
			OwnedPrimitiveCausalReceipts::CacheLease { identity, .. } => Some(identity.lease()),
			OwnedPrimitiveCausalReceipts::OrdinaryOnceLease { identity, .. } => {
				Some(identity.lease())
			}
			_ => None,
		}
	}

	pub(crate) fn semaphore(fields: SemaphoreOriginEndingsFields<'_>) -> Self {
		Self {
			causal: OwnedPrimitiveCausalReceipts::Semaphore {
				acquisition: fields.acquisition.clone(),
				first_claimed_release: fields.first_claimed_release.cloned(),
				preceding_operation: fields.preceding_operation.clone(),
				unsettled_predecessor: fields
					.pending_origins
					.retained()
					.map(|origins| origins.preceding().clone()),
			},
		}
	}
	pub(crate) fn throttler(fields: ThrottlerOriginEndingsFields<'_>) -> Self {
		Self {
			causal: OwnedPrimitiveCausalReceipts::Throttler {
				original_operation: fields.original_operation.clone(),
				preceding_operation: fields.preceding_operation.clone(),
				probe: ThrottlerProbePredecessors::NoProbe,
				unsettled_predecessor: fields
					.pending_origins
					.retained()
					.map(|origins| origins.preceding().clone()),
			},
		}
	}
	pub(crate) fn throttler_after_probe(fields: ThrottlerProbeOriginEndingsFields<'_>) -> Self {
		Self {
			causal: OwnedPrimitiveCausalReceipts::Throttler {
				original_operation: fields.outcome.original_operation.clone(),
				preceding_operation: fields.outcome.preceding_operation.clone(),
				probe: ThrottlerProbePredecessors::Probe {
					acquisition: fields.acquisition.clone(),
					heartbeat: fields.heartbeat.clone(),
				},
				unsettled_predecessor: fields
					.outcome
					.pending_origins
					.retained()
					.map(|origins| origins.preceding().clone()),
			},
		}
	}
	fn preceding(&self) -> &TransactionEndingReceipt {
		match &self.causal {
			OwnedPrimitiveCausalReceipts::OrdinaryOnceLease {
				preceding_operation,
				..
			}
			| OwnedPrimitiveCausalReceipts::CacheLease {
				preceding_operation,
				..
			}
			| OwnedPrimitiveCausalReceipts::Semaphore {
				preceding_operation,
				..
			}
			| OwnedPrimitiveCausalReceipts::Throttler {
				preceding_operation,
				..
			} => preceding_operation,
		}
	}

	fn slots(&self) -> [Option<&TransactionEndingReceipt>; 5] {
		match &self.causal {
			OwnedPrimitiveCausalReceipts::OrdinaryOnceLease {
				lease_operation,
				completion,
				preceding_operation,
				unsettled_predecessor,
				..
			} => [
				Some(lease_operation),
				Some(completion),
				Some(preceding_operation),
				unsettled_predecessor.as_ref(),
				None,
			],
			OwnedPrimitiveCausalReceipts::CacheLease {
				lease_operation,
				publication,
				preceding_operation,
				unsettled_predecessor,
				..
			} => [
				Some(lease_operation),
				Some(publication),
				Some(preceding_operation),
				unsettled_predecessor.as_ref(),
				None,
			],
			OwnedPrimitiveCausalReceipts::Semaphore {
				acquisition,
				first_claimed_release,
				preceding_operation,
				unsettled_predecessor,
			} => [
				Some(acquisition),
				Some(preceding_operation),
				first_claimed_release.as_ref(),
				unsettled_predecessor.as_ref(),
				None,
			],
			OwnedPrimitiveCausalReceipts::Throttler {
				original_operation,
				preceding_operation,
				probe,
				unsettled_predecessor,
			} => {
				let (acquisition, heartbeat) = match probe {
					ThrottlerProbePredecessors::NoProbe => (None, None),
					ThrottlerProbePredecessors::Probe {
						acquisition,
						heartbeat,
					} => (Some(acquisition), Some(heartbeat)),
				};
				[
					Some(original_operation),
					Some(preceding_operation),
					acquisition,
					heartbeat,
					unsettled_predecessor.as_ref(),
				]
			}
		}
	}
	fn receipts(&self) -> impl Iterator<Item = &TransactionEndingReceipt> {
		self.slots().into_iter().flatten()
	}
	pub(crate) async fn wait_until_terminal(&self) {
		for receipt in self.receipts() {
			receipt.wait_until_terminal().await;
		}
	}
	/// Fresh classification grants no participant or mutation permission.
	pub(crate) fn all_known(&self) -> Result<bool, OwnedPrimitiveAdmissionError> {
		Ok(self.dispositions()?.all_known)
	}
	fn dispositions(
		&self,
	) -> Result<OwnedPrimitiveOriginDispositions, OwnedPrimitiveAdmissionError> {
		let mut all_known = true;
		for receipt in self.receipts() {
			let TransactionEndingSnapshot::Terminal(ending) = receipt.snapshot() else {
				return Err(OwnedPrimitiveAdmissionError::OriginPending);
			};
			all_known &= matches!(
				ending.disposition(),
				TransactionEndingDisposition::KnownCommit
					| TransactionEndingDisposition::KnownRollback
			);
		}
		Ok(OwnedPrimitiveOriginDispositions { all_known })
	}
	fn same_kind_identity(&self, other: &Self) -> bool {
		match (&self.causal, &other.causal) {
			(
				OwnedPrimitiveCausalReceipts::OrdinaryOnceLease {
					identity: expected, ..
				},
				OwnedPrimitiveCausalReceipts::OrdinaryOnceLease {
					identity: actual, ..
				},
			) => expected == actual,
			(
				OwnedPrimitiveCausalReceipts::CacheLease {
					identity: expected, ..
				},
				OwnedPrimitiveCausalReceipts::CacheLease {
					identity: actual, ..
				},
			) => expected == actual,
			_ => std::mem::discriminant(&self.causal) == std::mem::discriminant(&other.causal),
		}
	}
	fn matches(&self, other: &Self) -> bool {
		if !self.same_kind_identity(other) {
			return false;
		}
		self.slots()
			.into_iter()
			.zip(other.slots())
			.all(|(expected, actual)| match (expected, actual) {
				(Some(expected), Some(actual)) => expected.shares_ending(actual),
				(None, None) => true,
				_ => false,
			})
	}
}
/// Retains one exact uncertain predecessor across retries that fail before physical observation.
/// An intervening attempt cannot mutate resource state before ordering this association.
pub(crate) struct OwnedPrimitivePendingOrigins {
	state: PendingOriginState,
}

enum PendingOriginState {
	Vacant,
	Retained(Box<OwnedPrimitiveOriginEndings>),
}
impl OwnedPrimitivePendingOrigins {
	pub(crate) fn new() -> Self {
		Self {
			state: PendingOriginState::Vacant,
		}
	}
	fn retained(&self) -> Option<&OwnedPrimitiveOriginEndings> {
		match &self.state {
			PendingOriginState::Vacant => None,
			PendingOriginState::Retained(origins) => Some(origins),
		}
	}
	pub(crate) fn retain(
		&mut self,
		origins: &OwnedPrimitiveOriginEndings,
	) -> Result<(), OwnedPrimitiveAdmissionError> {
		let TransactionEndingSnapshot::Terminal(ending) = origins.preceding().snapshot() else {
			return Err(OwnedPrimitiveAdmissionError::OriginPending);
		};
		if matches!(self.state, PendingOriginState::Vacant)
			&& !matches!(
				ending.disposition(),
				TransactionEndingDisposition::KnownCommit
					| TransactionEndingDisposition::KnownRollback
			) {
			self.state = PendingOriginState::Retained(Box::new(origins.clone()));
		}
		Ok(())
	}
	/// Only an actual full exclusive observation covering every retained receipt can clear it.
	pub(crate) fn clear_observed(&mut self, ordering: &OwnedPrimitiveOriginOrdering) {
		if let PendingOriginState::Retained(retained) = &self.state
			&& matches!(
				ordering._kind,
				OwnedPrimitiveOriginOrderingKind::Exclusive { .. }
			) && retained.same_kind_identity(&ordering.origins)
			&& retained.receipts().all(|required| {
				ordering
					.origins
					.receipts()
					.any(|actual| actual.shares_ending(required))
			}) {
			self.state = PendingOriginState::Vacant;
		}
	}
	pub(crate) fn ending(&self) -> Option<Arc<TransactionEndingResult>> {
		match self.retained()?.preceding().snapshot() {
			TransactionEndingSnapshot::Terminal(ending) => Some(ending),
			_ => None,
		}
	}
}

struct OwnedPrimitiveOriginDispositions {
	all_known: bool,
}
/// Classification is captured before the fixed family admits its local continuation.
pub(super) struct OwnedPrimitiveOrderedOrigins {
	origins: OwnedPrimitiveOriginEndings,
	dispositions: OwnedPrimitiveOriginDispositions,
}
impl OwnedPrimitiveOrderedOrigins {
	pub(super) fn new(
		origins: OwnedPrimitiveOriginEndings,
	) -> Result<Self, OwnedPrimitiveAdmissionError> {
		let dispositions = origins.dispositions()?;
		Ok(Self {
			origins,
			dispositions,
		})
	}
}
/// Ordering covers every exact predecessor and grants no resource mutation permission.
pub(crate) struct OwnedPrimitiveOriginOrdering {
	origins: OwnedPrimitiveOriginEndings,
	_kind: OwnedPrimitiveOriginOrderingKind,
}
enum OwnedPrimitiveOriginOrderingKind {
	KnownEndings,
	Exclusive {
		_proof: ExclusiveControlOrderingProof,
	},
}
impl OwnedPrimitiveOriginOrdering {
	pub(crate) fn matches(&self, origins: &OwnedPrimitiveOriginEndings) -> bool {
		self.origins.matches(origins)
	}
}

pub(crate) enum OriginOrderingProgress<Proof> {
	Awaiting,
	Proven(Proof),
}

impl<Proof> OriginOrderingProgress<Proof> {
	pub(crate) fn proof(&self) -> Option<&Proof> {
		match self {
			Self::Awaiting => None,
			Self::Proven(proof) => Some(proof),
		}
	}

	pub(crate) fn is_proven(&self) -> bool {
		matches!(self, Self::Proven(_))
	}

	pub(crate) fn take_proof(&mut self) -> Option<Proof> {
		match std::mem::replace(self, Self::Awaiting) {
			Self::Awaiting => None,
			Self::Proven(proof) => Some(proof),
		}
	}
}

#[derive(Clone, Copy)]
pub(crate) enum OwnedPrimitiveCleanupPhase {
	OriginOrdering,
	AuthenticationAndMutation(tokio::time::Instant),
}

pub(super) struct OwnedPrimitiveOrderingFields<'a> {
	pub(super) origins: &'a OwnedPrimitiveOrderedOrigins,
	pub(super) ordering: &'a mut OriginOrderingProgress<OwnedPrimitiveOriginOrdering>,
}
pub(super) struct OwnedPrimitiveAuthenticationFields<'a> {
	pub(super) ordered: OwnedPrimitiveOrderingFields<'a>,
	pub(super) phase: Option<(
		&'a tokio::sync::watch::Sender<OwnedPrimitiveCleanupPhase>,
		tokio::time::Instant,
	)>,
}

/// The fixed family supplies one actual local permit before BEGIN.
pub(super) struct OwnedPrimitiveRegistration<'a> {
	admission: &'a PgNativeAdmission,
	/// The registration until the dispatch of its transaction takes it.
	registration: Option<OwnedTransactionRegistration>,
	receipt: TransactionEndingReceipt,
}
impl<'a> OwnedPrimitiveRegistration<'a> {
	pub(super) fn new(admission: &'a PgNativeAdmission, permit: RuntimeTransactionPermit) -> Self {
		let registration = OwnedTransactionRegistration::new(permit);
		let receipt = registration.receipt();
		Self {
			admission,
			registration: Some(registration),
			receipt,
		}
	}
	pub(super) fn receipt(&self) -> TransactionEndingReceipt {
		self.receipt.clone()
	}
	pub(super) fn take_registration(&mut self) -> OwnedTransactionRegistration {
		self.registration
			.take()
			.expect("fixed registered operation dispatches once")
	}
	pub(super) fn require_ending(
		&self,
		tx: &mut WriteTx<'_>,
	) -> Result<(), OwnedPrimitiveAdmissionError> {
		if !tx.ending_receipt().shares_ending(&self.receipt) {
			return Err(OwnedPrimitiveAdmissionError::EndingMismatch);
		}
		Ok(())
	}
	pub(super) async fn observe_origins(
		&self,
		tx: &mut WriteTx<'_>,
		fields: OwnedPrimitiveOrderingFields<'_>,
	) -> Result<(Option<NativeSourceGatePhase>, PointRecordRead), OwnedPrimitiveAdmissionError> {
		self.admission
			.observe_owned_primitive_origins(tx, fields)
			.await
	}

	pub(super) async fn authenticate_current(
		&self,
		tx: &mut WriteTx<'_>,
	) -> Result<NativeAdmittedContracts, OwnedPrimitiveAdmissionError> {
		self.require_ending(tx)?;
		let mut completion = tx.track_state_mutation();
		let observation = self.admission.observe(tx).await?;
		let contracts = NativeAdmittedContracts { observation };
		completion.complete();
		Ok(contracts)
	}
	pub(super) async fn authenticate_ordered(
		&self,
		tx: &mut WriteTx<'_>,
		fields: OwnedPrimitiveAuthenticationFields<'_>,
	) -> Result<NativeAdmittedContracts, OwnedPrimitiveAdmissionError> {
		self.require_ending(tx)?;
		let mut completion = tx.track_state_mutation();
		let (source_phase, record) = self.observe_origins(tx, fields.ordered).await?;
		begin_cleanup_phase(fields.phase)?;
		let observation = self
			.admission
			.authenticate_observation(tx, source_phase, record)
			.await?;
		let contracts = NativeAdmittedContracts { observation };
		completion.complete();
		Ok(contracts)
	}
}
impl PgNativeAdmission {
	pub(super) async fn observe_owned_primitive_origins(
		&self,
		tx: &mut WriteTx<'_>,
		fields: OwnedPrimitiveOrderingFields<'_>,
	) -> Result<(Option<NativeSourceGatePhase>, PointRecordRead), OwnedPrimitiveAdmissionError> {
		let source_phase = self
			.pin_source(tx)
			.await
			.map_err(NativeAdmissionError::from)?;
		let record = if fields.origins.dispositions.all_known {
			*fields.ordering = OriginOrderingProgress::Proven(OwnedPrimitiveOriginOrdering {
				origins: fields.origins.origins.clone(),
				_kind: OwnedPrimitiveOriginOrderingKind::KnownEndings,
			});
			self.config
				.control_storage
				.read_pinned_shared(tx)
				.await
				.map_err(NativeAdmissionError::from)?
		} else {
			let physical = self
				.config
				.control_storage
				.read_pinned_exclusive_physical(tx)
				.await
				.map_err(NativeAdmissionError::from)?;
			let (proof, record) = physical.into_parts();
			*fields.ordering = OriginOrderingProgress::Proven(OwnedPrimitiveOriginOrdering {
				origins: fields.origins.origins.clone(),
				_kind: OwnedPrimitiveOriginOrderingKind::Exclusive { _proof: proof },
			});
			record.map_err(NativeAdmissionError::from)?
		};
		Ok((source_phase, record))
	}
}

fn begin_cleanup_phase(
	phase: Option<(
		&tokio::sync::watch::Sender<OwnedPrimitiveCleanupPhase>,
		tokio::time::Instant,
	)>,
) -> Result<(), OwnedPrimitiveAdmissionError> {
	if let Some((phase, proof_deadline)) = phase {
		let ordering_pending =
			matches!(*phase.borrow(), OwnedPrimitiveCleanupPhase::OriginOrdering);
		if ordering_pending {
			let start = tokio::time::Instant::now();
			if start >= proof_deadline {
				return Err(OwnedPrimitiveAdmissionError::OrderingWaitElapsed);
			}
			phase.send_replace(OwnedPrimitiveCleanupPhase::AuthenticationAndMutation(start));
		}
	}
	Ok(())
}
