//! Waits for the actual ending and local continuation release without database IO.

use std::{sync::Arc, time::Duration};

use crate::db::transaction_ending::{TransactionEndingReceipt, TransactionEndingResult};
use crate::runtime::local_admission::RuntimeAdmissionError;
use crate::runtime::local_admission::upgrade_work::RuntimeUpgradeWorkPermit;

pub(crate) struct NativeUpgradeEndingWait<'a> {
	pub(crate) owner: &'a RuntimeUpgradeWorkPermit,
	pub(crate) receipt: &'a TransactionEndingReceipt,
	pub(crate) wait_timeout: Duration,
}

/// A terminal receipt may be published before the transaction drops its permit.
/// Both observations are required before retry or explicit membership resolution.
/// The result certifies no server drainage, durable Work state or promotion authority.
pub(crate) async fn wait_upgrade_transaction_ending(
	fields: NativeUpgradeEndingWait<'_>,
) -> Result<Arc<TransactionEndingResult>, NativeUpgradeEndingWaitError> {
	if fields.wait_timeout.is_zero() {
		return Err(NativeUpgradeEndingWaitError::InvalidWaitTimeout);
	}
	let deadline = tokio::time::Instant::now()
		.checked_add(fields.wait_timeout)
		.ok_or(NativeUpgradeEndingWaitError::InvalidWaitTimeout)?;
	tokio::time::timeout_at(deadline, async {
		let outcome = fields.receipt.wait_until_terminal().await;
		fields
			.owner
			.wait_until_continuations_end()
			.await
			.map_err(|error| NativeUpgradeEndingWaitError::Local {
				error,
				ending: fields.receipt.clone(),
			})?;
		Ok(outcome)
	})
	.await
	.map_err(|_| NativeUpgradeEndingWaitError::DeadlineElapsed {
		ending: fields.receipt.clone(),
	})?
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeUpgradeEndingWaitError {
	#[error("upgrade ending wait must be positive and fit the local clock")]
	InvalidWaitTimeout,
	#[error("upgrade ending or local continuation release remains unresolved at its deadline")]
	DeadlineElapsed { ending: TransactionEndingReceipt },
	#[error("upgrade local ownership failed while resolving its transaction ending")]
	Local {
		#[source]
		error: RuntimeAdmissionError,
		ending: TransactionEndingReceipt,
	},
}
