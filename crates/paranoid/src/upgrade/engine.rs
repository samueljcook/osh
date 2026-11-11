//! Library-owned continuation of explicit installation upgrade work.

use std::sync::Arc;

use super::participants::PgNativeUpgradeParticipants;
use super::requests::PgNativeUpgradeRequests;
use crate::runtime::admission::PgNativeAdmission;

pub(crate) mod driver;
pub(crate) mod execution;
pub(crate) mod selection;

/// Constructed from one runtime's actual associations without retaining its lifecycle owner.
/// Every fixed operation independently checks its exact enrollment and current authority.
pub(crate) struct NativeUpgradeExecutionServices {
	pub(crate) admission: Arc<PgNativeAdmission>,
	pub(crate) requests: Arc<PgNativeUpgradeRequests>,
	pub(crate) participants: Arc<PgNativeUpgradeParticipants>,
}
