//! Throttler failures, with their complete cause chains. Formatting omits keys and scope data.

mod cause;

use crate::db::{DatabaseError, TransactionFailure};
use crate::fleet::RandomnessError;
use crate::fleet::throttler::engine::{
	NativeThrottlerCancellationReason, NativeThrottlerError, NativeThrottlerPointError,
};
use crate::runtime::local_admission::RuntimeAdmissionError;
use crate::runtime::throttler::NativeRuntimeThrottlerHandleError;
use std::{error::Error, fmt, sync::Arc};

enum Cause {
	Construction(NativeRuntimeThrottlerHandleError),
	Point(NativeThrottlerPointError),
	Native(Arc<NativeThrottlerError>),
	Randomness(RandomnessError),
	RetryInterval,
}

/// A failed Throttler operation, with its complete cause chain.
pub struct PgThrottlerError(Box<Cause>);

/// The class of a Throttler failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgThrottlerErrorKind {
	/// The runtime declares no Throttler code for the scope, the scope declares another family,
	/// or the installation does not select the scope.
	Scope,
	/// The probe heartbeat choice does not match the Throttler's circuit settings.
	InvalidProbeHeartbeat,
	/// The requested token cost requires a rate limit with sufficient capacity.
	InvalidRateLimitCost,
	/// A duration is zero or exceeds the local monotonic clock domain.
	InvalidTiming,
	/// A database statement or the transaction's own control failed. The error's source chain
	/// holds the database error.
	Database,
	/// Paranoid's own work on the Throttler exceeded its time limit.
	Timeout,
	/// The runtime stopped accepting work.
	RuntimeStopped,
	/// The runtime cannot operate on the Throttler under the installation's recorded policy.
	Admission,
	/// The stored Throttler could not be interpreted or rejected the operation, or the handle's
	/// settings have no circuit for a circuit operation.
	State,
	/// A local failure that the app cannot correct, such as a failed random source.
	Internal,
}

/// The fine classification of a cause, before it is reduced to a public kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FineKind {
	ScopeCode,
	WrongFamily,
	Admission,
	RegisteredAdmission,
	LocalOwnership,
	AcquisitionCancelled,
	DeliveryRejected,
	DriverClosed,
	InvalidProbeHeartbeatPolicy,
	InvalidRateLimitCost,
	ProbeExtensionLost,
	InvalidTiming,
	OperationState,
	ProtectedTimeout,
	EndingPending,
	ScopeBinding,
	State,
	GroupRejected,
	Group,
	Mutation,
	Storage,
	Database,
	Protocol,
}

impl PgThrottlerError {
	pub(super) fn construction(source: NativeRuntimeThrottlerHandleError) -> Self {
		Self(Box::new(Cause::Construction(source)))
	}
	pub(super) fn point(source: NativeThrottlerPointError) -> Self {
		Self(Box::new(Cause::Point(source)))
	}
	pub(super) fn operation(source: NativeThrottlerError) -> Self {
		Self::shared(Arc::new(source))
	}
	pub(super) fn shared(source: Arc<NativeThrottlerError>) -> Self {
		Self(Box::new(Cause::Native(source)))
	}
	pub(super) fn randomness(source: RandomnessError) -> Self {
		Self(Box::new(Cause::Randomness(source)))
	}
	pub(super) fn retry_interval() -> Self {
		Self(Box::new(Cause::RetryInterval))
	}

	fn details(&self) -> Option<cause::Details<'_>> {
		Some(match self.0.as_ref() {
			Cause::Native(e) => cause::native(e),
			Cause::Point(e) => cause::point(e),
			Cause::Construction(NativeRuntimeThrottlerHandleError::Throttler(e)) => {
				cause::native(e)
			}
			Cause::Construction(e) => cause::construction(e),
			Cause::Randomness(_) | Cause::RetryInterval => return None,
		})
	}

	/// The class of the failure.
	pub fn kind(&self) -> PgThrottlerErrorKind {
		use PgThrottlerErrorKind as Kind;
		if matches!(self.0.as_ref(), Cause::RetryInterval) {
			return Kind::InvalidTiming;
		}
		if self.database_error().is_some() {
			return Kind::Database;
		}
		let Some(details) = self.details() else {
			return Kind::Internal;
		};
		let primary = details.primary.downcast_ref::<NativeThrottlerError>();
		match details.kind {
			FineKind::ScopeCode | FineKind::WrongFamily | FineKind::ScopeBinding => Kind::Scope,
			FineKind::Admission | FineKind::RegisteredAdmission | FineKind::Protocol => {
				Kind::Admission
			}
			FineKind::LocalOwnership => match primary {
				Some(NativeThrottlerError::Local(
					RuntimeAdmissionError::Stopped { .. } | RuntimeAdmissionError::LifetimeFinished,
				)) => Kind::RuntimeStopped,
				_ => Kind::Internal,
			},
			FineKind::AcquisitionCancelled => match primary {
				Some(NativeThrottlerError::AcquisitionCancelled {
					reason: NativeThrottlerCancellationReason::RuntimeStopped,
				}) => Kind::RuntimeStopped,
				_ => Kind::Internal,
			},
			FineKind::InvalidProbeHeartbeatPolicy => Kind::InvalidProbeHeartbeat,
			FineKind::InvalidRateLimitCost => Kind::InvalidRateLimitCost,
			FineKind::InvalidTiming => Kind::InvalidTiming,
			FineKind::ProtectedTimeout => Kind::Timeout,
			FineKind::State
			| FineKind::GroupRejected
			| FineKind::Group
			| FineKind::Mutation
			| FineKind::Storage => Kind::State,
			FineKind::Database => Kind::Database,
			FineKind::DeliveryRejected
			| FineKind::DriverClosed
			| FineKind::ProbeExtensionLost
			| FineKind::OperationState
			| FineKind::EndingPending => Kind::Internal,
		}
	}

	/// The database error, when a database statement or the transaction's own control failed.
	pub fn database_error(&self) -> Option<&DatabaseError> {
		match self.0.as_ref() {
			Cause::Native(e) => cause::native_database_error(e),
			Cause::Point(e) => cause::point_database_error(e),
			Cause::Construction(NativeRuntimeThrottlerHandleError::Throttler(e)) => {
				cause::native_database_error(e)
			}
			Cause::Construction(_) | Cause::Randomness(_) | Cause::RetryInterval => None,
		}
	}

	/// Whether a transaction's own `COMMIT` failed, which means that its change may or may not
	/// have committed.
	pub fn commit_outcome_is_unknown(&self) -> bool {
		match self.0.as_ref() {
			Cause::Native(e) => e.commit_outcome_is_unknown(),
			Cause::Point(e) => e.commit_outcome_is_unknown(),
			Cause::Construction(NativeRuntimeThrottlerHandleError::Throttler(e)) => {
				e.commit_outcome_is_unknown()
			}
			Cause::Construction(_) | Cause::Randomness(_) | Cause::RetryInterval => false,
		}
	}
}

impl fmt::Debug for PgThrottlerError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "PgThrottlerError({:?}): {self}", self.kind())?;
		let mut source = self.source();
		while let Some(error) = source {
			write!(f, ": {error}")?;
			source = error.source();
		}
		Ok(())
	}
}
impl fmt::Display for PgThrottlerError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self.0.as_ref() {
			Cause::RetryInterval => f.write_str(
				"the Throttler's acquire retry interval must be positive and within the local \
				 monotonic clock domain",
			),
			_ => f.write_str("Throttler operation failed"),
		}
	}
}
impl Error for PgThrottlerError {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		Some(match self.0.as_ref() {
			Cause::Construction(source) => source,
			Cause::Point(source) => source,
			Cause::Native(source) => source.as_ref(),
			Cause::Randomness(source) => source,
			Cause::RetryInterval => return None,
		})
	}
}
