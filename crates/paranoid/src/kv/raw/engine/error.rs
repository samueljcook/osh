//! The failure of a Raw operation on several records or on a selection: the operation's own
//! failure, its admission failure, or a failure of the transaction that it owns.

use std::{error::Error, fmt};

use crate::db::{DatabaseError, TransactionFailure};
use crate::installation::ProtocolError;
use crate::kv::raw::postgres::slot::RawRejectionSettlement;
use crate::records::RecordIdentity;
use crate::runtime::admission::{NativeAdmissionError, NativeOperationError};

/// The operation's own failure. [`NativeRawError`] classifies it with these methods.
pub(in crate::kv) trait RawOperationFailure: Error + 'static {
	/// The operation's name at the start of the error messages, such as `raw deletion`.
	const OPERATION: &'static str;
	fn database_error(&self) -> Option<&DatabaseError>;
	fn involves_database_failure(&self) -> bool;
	fn commit_outcome_is_unknown(&self) -> bool;
	fn record_identity(&self) -> Option<&RecordIdentity>;
	fn input_index(&self) -> Option<usize>;
}

/// The own failure of a deletion that returns its typed failure inside the admitted result, so
/// that Paranoid's admission tracking finishes before the failure reaches the caller.
pub(in crate::kv) trait RawDeletionFailure {
	/// Whether the caller's transaction can still commit after this failure.
	fn settlement(&self) -> RawRejectionSettlement;
}

#[derive(Debug)]
pub(in crate::kv) enum NativeRawError<E> {
	Admission(NativeAdmissionError),
	Operation(E),
	Database(DatabaseError),
	Protocol(ProtocolError),
	RollbackFailed {
		operation_error: Box<Self>,
		rollback_error: DatabaseError,
	},
}

impl<E: RawOperationFailure> NativeRawError<E> {
	pub(super) fn from_operation(error: NativeOperationError<E>) -> Self {
		match error {
			NativeOperationError::Admission(error) => Self::Admission(error),
			NativeOperationError::Operation(error) => Self::Operation(error),
		}
	}
	/// The operation's own failure, also when rolling back its transaction failed afterward.
	pub(crate) fn operation_error(&self) -> Option<&E> {
		match self {
			Self::Operation(error) => Some(error),
			Self::RollbackFailed {
				operation_error, ..
			} => operation_error.operation_error(),
			Self::Admission(_) | Self::Database(_) | Self::Protocol(_) => None,
		}
	}
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Admission(error) => error.database_error(),
			Self::Operation(error) => error.database_error(),
			Self::Database(error) => Some(error),
			Self::Protocol(_) => None,
			Self::RollbackFailed {
				operation_error,
				rollback_error,
			} => operation_error.database_error().or(Some(rollback_error)),
		}
	}
	pub(crate) fn record_identity(&self) -> Option<&RecordIdentity> {
		self.operation_error().and_then(E::record_identity)
	}
	pub(crate) fn input_index(&self) -> Option<usize> {
		self.operation_error().and_then(E::input_index)
	}
}

impl<E: RawDeletionFailure> NativeRawError<E> {
	/// A deletion returns its own failure as `Operation` only after Paranoid finished its
	/// admission tracking, so only that failure can leave the caller's transaction committable.
	/// An admission failure leaves the tracking unfinished, and the other failures happen only
	/// in a transaction that Paranoid owns.
	pub(in crate::kv) fn rejection_settlement(&self) -> RawRejectionSettlement {
		match self {
			Self::Operation(error) => error.settlement(),
			Self::Admission(_)
			| Self::Database(_)
			| Self::Protocol(_)
			| Self::RollbackFailed { .. } => RawRejectionSettlement::WholeRollbackRequired,
		}
	}
}

impl<E: RawOperationFailure> fmt::Display for NativeRawError<E> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Admission(_) => write!(formatter, "{} admission failed", E::OPERATION),
			Self::Operation(error) => fmt::Display::fmt(error, formatter),
			Self::Database(_) => {
				write!(
					formatter,
					"{} transaction database work failed",
					E::OPERATION
				)
			}
			Self::Protocol(_) => write!(formatter, "{} transaction protocol failed", E::OPERATION),
			Self::RollbackFailed { .. } => {
				write!(
					formatter,
					"{} failed and rollback also failed",
					E::OPERATION
				)
			}
		}
	}
}

impl<E: RawOperationFailure> Error for NativeRawError<E> {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		match self {
			Self::Admission(error) => Some(error),
			Self::Operation(error) => error.source(),
			Self::Database(error) => Some(error),
			Self::Protocol(error) => Some(error),
			Self::RollbackFailed {
				operation_error, ..
			} => Some(operation_error.as_ref()),
		}
	}
}

impl<E: RawOperationFailure> TransactionFailure for NativeRawError<E> {
	fn from_protocol_error(error: ProtocolError) -> Self {
		Self::Protocol(error)
	}
	fn from_database_error(error: DatabaseError) -> Self {
		Self::Database(error)
	}
	fn from_rollback_failure(operation_error: Self, rollback_error: DatabaseError) -> Self {
		Self::RollbackFailed {
			operation_error: Box::new(operation_error),
			rollback_error,
		}
	}
	fn involves_database_failure(&self) -> bool {
		match self {
			Self::Admission(error) => error.involves_database_failure(),
			Self::Operation(error) => error.involves_database_failure(),
			Self::Database(_) | Self::RollbackFailed { .. } => true,
			Self::Protocol(_) => false,
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Admission(error) => error.commit_outcome_is_unknown(),
			Self::Operation(error) => error.commit_outcome_is_unknown(),
			Self::Database(error) => error.commit_outcome_is_unknown(),
			Self::Protocol(_) => false,
			Self::RollbackFailed {
				operation_error,
				rollback_error,
			} => {
				operation_error.commit_outcome_is_unknown()
					|| rollback_error.commit_outcome_is_unknown()
			}
		}
	}
}
