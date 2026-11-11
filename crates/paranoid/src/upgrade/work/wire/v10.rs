//! Strict fixed source procedures, actual key domains, and required cursor presence.

use std::borrow::Cow;

use primitives_rs::id::time_sortable::SORTABLE_ID_LEN;
use primitives_rs::serialization::json::deserialize_required_option;
use serde::{Deserialize, Serialize};

use crate::installation::contract::policy_wire::ComponentPolicyWire;
use crate::queue::{JobId, QueuePauseScope, TaskName, parse_source_pause_scope};
use crate::{
	fleet::lease, kv, scope::wire::ScopePolicySetWire,
	upgrade::policy::request::PolicyUpgradeRequestError,
};

use super::super::UpgradeWorkError;
use super::super::v10::source_checkpoint::{
	PgV10ConversionProcedure, SourceCheckpointError, SourceConversionCheckpoint,
	SourceConversionCursor, SourceConversionInvariant, SourceConversionInvariantFields,
	SourceConversionPauseKey, SourceScanProgress,
};

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ProcedureWire {
	RawBytesRelocationV1 {},
	FleetStateRelocationV1 {},
	LeaseAggregateRelocationV1 {},
	QueueJobRelocationV1 {},
	QueueDeadLetterRelocationV1 {},
	QueuePauseRelocationV1 {},
}
impl ProcedureWire {
	fn from_procedure(procedure: PgV10ConversionProcedure) -> Self {
		match procedure {
			PgV10ConversionProcedure::RawBytesRelocationV1 => Self::RawBytesRelocationV1 {},
			PgV10ConversionProcedure::FleetStateRelocationV1 => Self::FleetStateRelocationV1 {},
			PgV10ConversionProcedure::LeaseAggregateRelocationV1 => {
				Self::LeaseAggregateRelocationV1 {}
			}
			PgV10ConversionProcedure::QueueJobRelocationV1 => Self::QueueJobRelocationV1 {},
			PgV10ConversionProcedure::QueueDeadLetterRelocationV1 => {
				Self::QueueDeadLetterRelocationV1 {}
			}
			PgV10ConversionProcedure::QueuePauseRelocationV1 => Self::QueuePauseRelocationV1 {},
		}
	}
	fn into_procedure(self) -> PgV10ConversionProcedure {
		match self {
			Self::RawBytesRelocationV1 {} => PgV10ConversionProcedure::RawBytesRelocationV1,
			Self::FleetStateRelocationV1 {} => PgV10ConversionProcedure::FleetStateRelocationV1,
			Self::LeaseAggregateRelocationV1 {} => {
				PgV10ConversionProcedure::LeaseAggregateRelocationV1
			}
			Self::QueueJobRelocationV1 {} => PgV10ConversionProcedure::QueueJobRelocationV1,
			Self::QueueDeadLetterRelocationV1 {} => {
				PgV10ConversionProcedure::QueueDeadLetterRelocationV1
			}
			Self::QueuePauseRelocationV1 {} => PgV10ConversionProcedure::QueuePauseRelocationV1,
		}
	}
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum CursorWire<'a> {
	RawBytes {
		key: Cow<'a, str>,
	},
	FleetState {
		key: Cow<'a, str>,
	},
	LeaseAggregate {
		key: Cow<'a, str>,
	},
	QueueJob {
		id: [u8; SORTABLE_ID_LEN],
	},
	QueueDeadLetter {
		id: [u8; SORTABLE_ID_LEN],
	},
	QueuePause {
		key: Cow<'a, str>,
		#[serde(deserialize_with = "deserialize_required_option")]
		task: Option<Cow<'a, str>>,
	},
}
impl<'a> CursorWire<'a> {
	fn from_cursor(cursor: &'a SourceConversionCursor) -> Self {
		match cursor {
			SourceConversionCursor::RawBytes(key) => Self::RawBytes {
				key: Cow::Borrowed(key.as_str()),
			},
			SourceConversionCursor::FleetState(key) => Self::FleetState {
				key: Cow::Borrowed(key.as_str()),
			},
			SourceConversionCursor::LeaseAggregate(key) => Self::LeaseAggregate {
				key: Cow::Borrowed(key.as_str()),
			},
			SourceConversionCursor::QueueJob(id) => Self::QueueJob { id: *id.as_bytes() },
			SourceConversionCursor::QueueDeadLetter(id) => {
				Self::QueueDeadLetter { id: *id.as_bytes() }
			}
			SourceConversionCursor::QueuePause(key) => Self::QueuePause {
				key: Cow::Borrowed(key.as_str()),
				task: match key.scope() {
					QueuePauseScope::Global => None,
					QueuePauseScope::Task(task) => Some(Cow::Borrowed(task.as_str())),
				},
			},
		}
	}
	fn into_cursor(self) -> Result<SourceConversionCursor, SourceCheckpointError> {
		Ok(match self {
			Self::RawBytes { key } => SourceConversionCursor::RawBytes(
				kv::Key::from_persisted_text(key.into_owned())
					.map_err(SourceCheckpointError::KvCursor)?,
			),
			Self::FleetState { key } => SourceConversionCursor::FleetState(
				kv::Key::from_persisted_text(key.into_owned())
					.map_err(SourceCheckpointError::KvCursor)?,
			),
			Self::LeaseAggregate { key } => {
				let parts = key
					.strip_suffix(lease::LEASE_KEY_SEPARATOR)
					.ok_or(SourceCheckpointError::LeaseFinalSeparator)?;
				SourceConversionCursor::LeaseAggregate(
					lease::LeaseKey::from_parts(parts.split(lease::LEASE_KEY_SEPARATOR))
						.map_err(SourceCheckpointError::LeaseCursor)?,
				)
			}
			Self::QueueJob { id } => SourceConversionCursor::QueueJob(
				JobId::from_bytes(&id).map_err(SourceCheckpointError::IdCursor)?,
			),
			Self::QueueDeadLetter { id } => SourceConversionCursor::QueueDeadLetter(
				JobId::from_bytes(&id).map_err(SourceCheckpointError::IdCursor)?,
			),
			Self::QueuePause { key, task } => {
				let task = task
					.map(|task| {
						TaskName::new(task.as_ref()).map_err(SourceCheckpointError::TaskCursor)
					})
					.transpose()?;
				let scope = parse_source_pause_scope(&key, task.as_ref())
					.map_err(SourceCheckpointError::PauseCursor)?;
				SourceConversionCursor::QueuePause(SourceConversionPauseKey::from_scope(scope))
			}
		})
	}
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ProgressWire<'a> {
	Scanning {
		#[serde(deserialize_with = "deserialize_required_option")]
		after_key: Option<CursorWire<'a>>,
	},
	Scanned {
		#[serde(deserialize_with = "deserialize_required_option")]
		last_key: Option<CursorWire<'a>>,
	},
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InvariantWire<'a> {
	component_policies: Vec<ComponentPolicyWire<'a>>,
	scope_policies: ScopePolicySetWire<'a>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SourceCheckpointWire<'a> {
	procedure: ProcedureWire,
	invariant: InvariantWire<'a>,
	progress: ProgressWire<'a>,
}
impl<'a> SourceCheckpointWire<'a> {
	pub(super) fn from_checkpoint(checkpoint: &'a SourceConversionCheckpoint) -> Self {
		Self {
			procedure: ProcedureWire::from_procedure(checkpoint.procedure()),
			invariant: InvariantWire {
				component_policies: checkpoint
					.invariant()
					.component_policies()
					.iter()
					.map(ComponentPolicyWire::from_policy)
					.collect(),
				scope_policies: ScopePolicySetWire::from_policies(
					checkpoint.invariant().scope_policies(),
				),
			},
			progress: match checkpoint.progress() {
				SourceScanProgress::Scanning { after_key } => ProgressWire::Scanning {
					after_key: after_key.as_ref().map(CursorWire::from_cursor),
				},
				SourceScanProgress::Scanned { last_key } => ProgressWire::Scanned {
					last_key: last_key.as_ref().map(CursorWire::from_cursor),
				},
			},
		}
	}
	pub(super) fn into_checkpoint(self) -> Result<SourceConversionCheckpoint, UpgradeWorkError> {
		let policies = self
			.invariant
			.component_policies
			.into_iter()
			.map(|policy| policy.into_policy())
			.collect::<Result<Vec<_>, _>>()
			.map_err(PolicyUpgradeRequestError::from)?;
		Ok(SourceConversionCheckpoint {
			procedure: self.procedure.into_procedure(),
			invariant: SourceConversionInvariant::new(SourceConversionInvariantFields {
				component_policies: policies,
				scope_policies: self
					.invariant
					.scope_policies
					.into_policies()
					.map_err(PolicyUpgradeRequestError::from)?,
			})?,
			progress: match self.progress {
				ProgressWire::Scanning { after_key } => SourceScanProgress::Scanning {
					after_key: after_key.map(CursorWire::into_cursor).transpose()?,
				},
				ProgressWire::Scanned { last_key } => SourceScanProgress::Scanned {
					last_key: last_key.map(CursorWire::into_cursor).transpose()?,
				},
			},
		})
	}
}
