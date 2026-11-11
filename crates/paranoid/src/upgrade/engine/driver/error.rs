//! Actual typed causes retained by an owned upgrade continuation.

use std::sync::Arc;

use crate::runtime::admission::upgrade::NativeUpgradeAdmissionError;
use crate::runtime::admission::upgrade::ending::NativeUpgradeEndingWaitError;
use crate::runtime::local_admission::RuntimeAdmissionError;
use crate::runtime::targets::NativeRuntimeTargetError;
use crate::upgrade::participants::NativeUpgradeParticipantsError;
use crate::upgrade::requests::NativeUpgradeRequestError;
use crate::upgrade::requests::work::recovery_observation::NativeUpgradeRecoveryOrigin;

use super::super::execution::error::NativeUpgradeStepExecutionError;
use super::super::selection::{NativeUpgradePrerequisiteError, NativeUpgradeSelectionError};

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeUpgradeDriverError {
	#[error("upgrade continuation enrollment failed")]
	Admission(#[from] NativeUpgradeAdmissionError),
	#[error("upgrade local ownership failed")]
	Ownership(#[from] RuntimeAdmissionError),
	#[error("upgrade mutation remains unresolved after its uncertain ending")]
	UncertainMutation {
		origin: NativeUpgradeRecoveryOrigin,
		#[source]
		source: Option<Arc<Self>>,
	},
	#[error("upgrade transaction ending or continuation release remains unresolved")]
	Ending(#[source] Arc<NativeUpgradeEndingWaitError>),
	#[error("upgrade creation or observation failed")]
	Request(#[source] Arc<NativeUpgradeRequestError>),
	#[error("upgrade selected operation failed")]
	Step(#[source] Arc<NativeUpgradeStepExecutionError>),
	#[error("upgrade deletion of expired participant registrations failed")]
	ExpiredParticipantDeletion(#[source] Arc<NativeUpgradeParticipantsError>),
	#[error("upgrade target resolution failed")]
	Target(#[from] NativeRuntimeTargetError),
	#[error("upgrade observation cannot select a procedure")]
	Selection(#[from] NativeUpgradeSelectionError),
	#[error("upgrade requested Work is absent")]
	WorkAbsent,
	#[error("upgrade recorded target differs from this owner's complete target")]
	TargetChanged,
	#[error("upgrade has no eligible procedure and has identified prerequisites")]
	Prerequisites {
		prerequisites: Arc<Vec<NativeUpgradePrerequisiteError>>,
	},
	#[error("upgrade has no eligible procedure or identified terminal outcome")]
	NoProcedure,
	#[error("upgrade scheduling exceeds the local monotonic clock domain")]
	LocalClockDomain,
	#[error("upgrade reached its maximum of consecutive transient failures")]
	MaxConsecutiveTransientFailuresReached {
		#[source]
		source: Arc<Self>,
	},
	#[error("upgrade freshly observed unchanged Work rejected the same procedure")]
	RepeatedContext {
		#[source]
		source: Arc<Self>,
	},
	#[error("upgrade failed while resolving an earlier failure")]
	FollowingFailure {
		#[source]
		primary_error: Arc<Self>,
		following_error: Arc<Self>,
	},
}
