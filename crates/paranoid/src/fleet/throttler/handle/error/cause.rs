//! Closed cause recursion retains exact primary and independent native failures.

use super::FineKind as Kind;
use crate::db::DatabaseError;
use crate::fleet::throttler::engine::{
	NativeThrottlerError as Native, NativeThrottlerPointError as Point,
};
use crate::records::RecordIdentity;
use crate::records::postgres::group_attempt::GroupAttemptError;
use crate::runtime::local_admission::RuntimeAdmissionError;
use crate::runtime::throttler::NativeRuntimeThrottlerHandleError as ConstructionError;
use std::error::Error;

pub(super) struct Details<'a> {
	pub(super) kind: Kind,
	pub(super) primary: &'a (dyn Error + 'static),
	pub(super) record: Option<&'a RecordIdentity>,
	pub(super) cleanup: Vec<&'a (dyn Error + 'static)>,
	pub(super) settlement: Vec<&'a Native>,
	pub(super) retention: Vec<&'a RuntimeAdmissionError>,
	pub(super) rollback: Vec<&'a DatabaseError>,
}
fn leaf<'a>(kind: Kind, primary: &'a (dyn Error + 'static)) -> Details<'a> {
	Details {
		kind,
		primary,
		record: None,
		cleanup: Vec::new(),
		settlement: Vec::new(),
		retention: Vec::new(),
		rollback: Vec::new(),
	}
}
pub(super) fn construction(error: &ConstructionError) -> Details<'_> {
	match error {
		ConstructionError::ScopeCode(_) => leaf(Kind::ScopeCode, error),
		ConstructionError::WrongFamily { .. } => leaf(Kind::WrongFamily, error),
		ConstructionError::Throttler(error) => native(error),
	}
}
pub(super) fn native(error: &Native) -> Details<'_> {
	match error {
		Native::Record { identity, source } => {
			let mut value = native(source);
			value.record = Some(identity);
			value
		}
		Native::PreparationCleanup {
			preparation_error,
			cleanup_error,
		} => {
			let mut value = leaf(Kind::ScopeBinding, preparation_error.as_ref());
			value.cleanup.push(cleanup_error.as_ref());
			value
		}
		Native::PointPreparationCleanup {
			preparation_error,
			cleanup_error,
		} => {
			let mut value = leaf(Kind::ScopeBinding, preparation_error.as_ref());
			value.cleanup.push(cleanup_error.as_ref());
			value
		}
		Native::PhysicalSettlementFailed {
			operation_error,
			settlement_error,
		} => {
			let mut value = native(operation_error);
			value.settlement.insert(0, settlement_error);
			value
		}
		Native::DiagnosticRetention {
			operation_error,
			retention_error,
		} => {
			let mut value = native(operation_error);
			value.retention.insert(0, retention_error);
			value
		}
		Native::RollbackFailed {
			operation_error,
			rollback_error,
		} => {
			let mut value = native(operation_error);
			value.rollback.insert(0, rollback_error);
			value
		}
		Native::AcquisitionCancelled { .. } => leaf(Kind::AcquisitionCancelled, error),
		Native::DriverClosed => leaf(Kind::DriverClosed, error),
		Native::DeliveryRejected => leaf(Kind::DeliveryRejected, error),
		Native::InvalidProbeHeartbeatPolicy => leaf(Kind::InvalidProbeHeartbeatPolicy, error),
		Native::InvalidRateLimitCost => leaf(Kind::InvalidRateLimitCost, error),
		Native::ProbeExtensionLost => leaf(Kind::ProbeExtensionLost, error),
		Native::InvalidTiming => leaf(Kind::InvalidTiming, error),
		Native::OperationState => leaf(Kind::OperationState, error),
		Native::ProtectedTimeout => leaf(Kind::ProtectedTimeout, error),
		Native::EndingPending => leaf(Kind::EndingPending, error),
		Native::Admission(_) => leaf(Kind::RegisteredAdmission, error),
		Native::Local(_) => leaf(Kind::LocalOwnership, error),
		Native::Preparation(_) => leaf(Kind::ScopeBinding, error),
		Native::GroupRejected(_) => leaf(Kind::GroupRejected, error),
		Native::Group(error) => group(error),
		Native::Mutation(_) => leaf(Kind::Mutation, error),
		Native::Storage(_) => leaf(Kind::Storage, error),
		Native::Database(_) => leaf(Kind::Database, error),
		Native::Protocol(_) => leaf(Kind::Protocol, error),
	}
}
fn group(error: &GroupAttemptError) -> Details<'_> {
	match error {
		GroupAttemptError::RejectionCleanup {
			rejection_error,
			cleanup_error,
		} => {
			let mut value = leaf(Kind::Mutation, rejection_error);
			value.cleanup.push(cleanup_error.as_ref());
			value
		}
		GroupAttemptError::ReacquisitionCleanup {
			rejection_error,
			cleanup_error,
		} => {
			let mut value = leaf(Kind::GroupRejected, rejection_error);
			value.cleanup.push(cleanup_error.as_ref());
			value
		}
		GroupAttemptError::Storage(_) | GroupAttemptError::Boundary { .. } => {
			leaf(Kind::Group, error)
		}
	}
}
pub(super) fn point(error: &Point) -> Details<'_> {
	match error {
		Point::Record { identity, source } => {
			let mut value = point(source);
			value.record = Some(identity);
			value
		}
		Point::RollbackFailed {
			operation_error,
			rollback_error,
		} => {
			let mut value = point(operation_error);
			value.rollback.insert(0, rollback_error);
			value
		}
		Point::CircuitAbsent => leaf(Kind::State, error),
		Point::Admission(_) => leaf(Kind::Admission, error),
		Point::Scope(_) => leaf(Kind::ScopeBinding, error),
		Point::State(_) => leaf(Kind::State, error),
		Point::Storage(_) => leaf(Kind::Storage, error),
		Point::Database(_) => leaf(Kind::Database, error),
		Point::Protocol(_) => leaf(Kind::Protocol, error),
	}
}
pub(super) fn native_database_error(error: &Native) -> Option<&DatabaseError> {
	match error {
		Native::Record { source, .. } => native_database_error(source),
		Native::Admission(e) => e.database_error(),
		Native::Group(e)
		| Native::PreparationCleanup {
			cleanup_error: e, ..
		} => e.database_error(),
		Native::Storage(e)
		| Native::PointPreparationCleanup {
			cleanup_error: e, ..
		} => e.database_error(),
		Native::Database(e) => Some(e),
		Native::PhysicalSettlementFailed {
			operation_error,
			settlement_error,
		} => native_database_error(operation_error)
			.or_else(|| native_database_error(settlement_error)),
		Native::DiagnosticRetention {
			operation_error, ..
		} => native_database_error(operation_error),
		Native::RollbackFailed {
			operation_error,
			rollback_error,
		} => native_database_error(operation_error).or(Some(rollback_error)),
		_ => None,
	}
}
pub(super) fn point_database_error(error: &Point) -> Option<&DatabaseError> {
	match error {
		Point::Record { source, .. } => point_database_error(source),
		Point::Admission(e) => e.database_error(),
		Point::Storage(e) => e.database_error(),
		Point::Database(e) => Some(e),
		Point::RollbackFailed {
			operation_error,
			rollback_error,
		} => point_database_error(operation_error).or(Some(rollback_error)),
		Point::CircuitAbsent | Point::Scope(_) | Point::State(_) | Point::Protocol(_) => None,
	}
}
