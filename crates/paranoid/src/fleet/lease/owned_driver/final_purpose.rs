//! Closed terminal purposes share only actual ending and cleanup ownership.

use super::cache_publication::CachePublicationOrigin;
use super::once_completion::OnceCompletionOrigin;
use super::{
	Arc, DeliveryAuthority, DriverShared, Instant, LeaseCommittedCompletion, LeaseDeliveryDeadline,
	LeaseDriver, NativeLeaseDriverError, NativeLeaseWorkPhase, OwnedPrimitivePendingOrigins,
	sleep_until,
};
use crate::db::transaction_ending::{TransactionEndingReceipt, TransactionEndingResult};

pub(super) enum LeaseFinalPurposeOrigin {
	CachePublication(CachePublicationOrigin),
	OnceCompletion(OnceCompletionOrigin),
}
pub(super) struct FinalPurposeCleanupOriginFields<'a> {
	pub(super) lease_operation: &'a TransactionEndingReceipt,
	pub(super) preceding_operation: &'a TransactionEndingReceipt,
	pub(super) pending_origins: &'a OwnedPrimitivePendingOrigins,
}
impl LeaseFinalPurposeOrigin {
	pub(super) fn receipt(&self) -> &TransactionEndingReceipt {
		match self {
			Self::CachePublication(origin) => origin.receipt(),
			Self::OnceCompletion(origin) => origin.receipt(),
		}
	}
	pub(super) async fn wait_until_terminal(&self) {
		self.receipt().wait_until_terminal().await;
	}
	pub(super) fn ending_failure(&self) -> Option<Arc<TransactionEndingResult>> {
		match self {
			Self::CachePublication(origin) => origin.ending_failure(),
			Self::OnceCompletion(origin) => origin.ending_failure(),
		}
	}
	pub(super) fn cleanup_origins(
		&self,
		fields: FinalPurposeCleanupOriginFields<'_>,
	) -> Option<crate::runtime::admission::owned_primitive::OwnedPrimitiveOriginEndings> {
		match self {
			Self::CachePublication(origin) => origin.cleanup_origins(fields),
			Self::OnceCompletion(origin) => origin.cleanup_origins(fields),
		}
	}
	pub(super) fn committed_completion(&self) -> Option<LeaseCommittedCompletion> {
		match self {
			Self::CachePublication(_) => None,
			Self::OnceCompletion(origin) => origin.committed_completion(),
		}
	}
}
impl LeaseDriver {
	pub(super) async fn wait_final_purpose(
		&mut self,
		deadline: Instant,
		phase: NativeLeaseWorkPhase,
	) {
		self.drain_pending_cmds().await;
		loop {
			let runtime = self.owner.runtime_view();
			let current = match Self::current_operation_deadline(
				&self.owner,
				&self.shared,
				Some(deadline),
				phase,
			) {
				Ok((_, until)) => until,
				Err(error) => {
					self.remember_failure(error);
					self.shared.close();
					return;
				}
			};
			let receipt = self
				.ownership
				.final_purpose()
				.expect("issued final purpose retains origin")
				.receipt();
			tokio::select! { biased;
				_ = receipt.wait_until_terminal() => return,
				_ = self.shared.wait_closed() => return,
				_ = self.owner.wait_for_runtime_change(&runtime) => {},
				_ = sleep_until(current) => {},
			}
		}
	}
}

pub(super) fn require_final_purpose_deadline(
	shared: &DriverShared,
	deadline: Instant,
) -> Result<Instant, Arc<NativeLeaseDriverError>> {
	shared
		.delivery_deadline(
			DeliveryAuthority::HeldRenewal,
			Some(LeaseDeliveryDeadline::OriginalOperationDeadline(deadline)),
		)
		.map_err(Arc::new)
}
/// Bounds only protected work; fixed consumers await their actual owned ending separately.
pub(super) async fn wait_final_purpose_work<T>(
	shared: &DriverShared,
	deadline: Instant,
	work: impl Future<Output = T>,
) -> Result<T, Arc<NativeLeaseDriverError>> {
	tokio::pin!(work);
	loop {
		let runtime = shared.runtime.work_view(shared.work_id);
		let until = require_final_purpose_deadline(shared, deadline)?;
		tokio::select! { biased;
			result = &mut work => return Ok(result),
			_ = shared.wait_closed() => {},
			_ = shared.runtime.wait_for_work_change(shared.work_id, &runtime) => {},
			_ = sleep_until(until) => {},
		}
	}
}
