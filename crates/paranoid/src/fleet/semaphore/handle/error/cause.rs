//! Closed native cause recursion preserves operation precedence over independent failures.

use super::PgSemaphoreErrorKind as Kind;
use crate::db::DatabaseError;
use crate::fleet::semaphore::engine::NativeSemaphoreError as Native;
use crate::records::postgres::StateStorageError;
use crate::runtime::local_admission::RuntimeAdmissionError;

pub(super) struct Details<'a> {
	pub(super) primary: &'a Native,
	pub(super) record: Option<&'a crate::records::RecordIdentity>,
	pub(super) cleanup: Vec<&'a StateStorageError>,
	pub(super) settlement: Vec<&'a Native>,
	pub(super) retention: Vec<&'a RuntimeAdmissionError>,
	pub(super) rollback: Vec<&'a DatabaseError>,
}
pub(super) fn details(error: &Native) -> Details<'_> {
	match error {
		Native::Record { identity, source } => {
			let mut value = details(source);
			value.record = Some(identity);
			value
		}
		Native::PreparationCleanup {
			preparation_error,
			cleanup_error,
		} => {
			let mut value = details(preparation_error);
			value.cleanup.insert(0, cleanup_error);
			value
		}
		Native::PhysicalSettlementFailed {
			operation_error,
			settlement_error,
		} => {
			let mut value = details(operation_error);
			value.settlement.insert(0, settlement_error);
			value
		}
		Native::DiagnosticRetention {
			operation_error,
			retention_error,
		} => {
			let mut value = details(operation_error);
			value.retention.insert(0, retention_error);
			value
		}
		Native::RollbackFailed {
			operation_error,
			rollback_error,
		} => {
			let mut value = details(operation_error);
			value.rollback.insert(0, rollback_error);
			value
		}
		_ => Details {
			primary: error,
			record: None,
			cleanup: Vec::new(),
			settlement: Vec::new(),
			retention: Vec::new(),
			rollback: Vec::new(),
		},
	}
}
pub(super) fn kind(error: &Native) -> Kind {
	use crate::fleet::semaphore::engine::NativeSemaphoreCancellationReason as Reason;
	use crate::fleet::semaphore::state::SemaphoreStateError;
	if crate::db::TransactionFailure::involves_database_failure(error) {
		return Kind::Database;
	}
	match details(error).primary {
		Native::Local(
			RuntimeAdmissionError::Stopped { .. } | RuntimeAdmissionError::LifetimeFinished,
		)
		| Native::AcquisitionCancelled {
			reason: Reason::RuntimeStopped(_),
		} => Kind::RuntimeStopped,
		Native::InvalidTiming => Kind::InvalidTiming,
		Native::ProtectedTimeout => Kind::Timeout,
		Native::RegisteredAdmission(_) | Native::Admission(_) | Native::Protocol(_) => {
			Kind::Admission
		}
		Native::Scope(_) => Kind::Scope,
		Native::State(SemaphoreStateError::SettingsMismatch) => Kind::SettingsMismatch,
		Native::State(_) | Native::Storage(_) => Kind::State,
		Native::Database(_) => Kind::Database,
		Native::Local(_)
		| Native::AcquisitionCancelled {
			reason: Reason::ClientClosed,
		}
		| Native::DeliveryRejected
		| Native::DriverClosed
		| Native::OperationState
		| Native::EndingPending => Kind::Internal,
		Native::Record { .. }
		| Native::PreparationCleanup { .. }
		| Native::PhysicalSettlementFailed { .. }
		| Native::DiagnosticRetention { .. }
		| Native::RollbackFailed { .. } => {
			unreachable!("primary cause recursion removes only aggregate contexts")
		}
	}
}
pub(super) fn database_error(error: &Native) -> Option<&DatabaseError> {
	match error {
		Native::RegisteredAdmission(e) => e.database_error(),
		Native::Admission(e) => e.database_error(),
		Native::Storage(e) => e.database_error(),
		Native::Database(e) => Some(e),
		Native::Record { source, .. } => database_error(source),
		Native::PreparationCleanup {
			preparation_error,
			cleanup_error,
		} => database_error(preparation_error).or_else(|| cleanup_error.database_error()),
		Native::PhysicalSettlementFailed {
			operation_error,
			settlement_error,
		} => database_error(operation_error).or_else(|| database_error(settlement_error)),
		Native::DiagnosticRetention {
			operation_error, ..
		} => database_error(operation_error),
		Native::RollbackFailed {
			operation_error,
			rollback_error,
		} => database_error(operation_error).or(Some(rollback_error)),
		_ => None,
	}
}
