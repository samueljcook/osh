//! Closed source-role requests, typed candidates, and sanitized physical failures.

use std::{fmt, num::NonZeroU32};

use primitives_rs::id::time_sortable::SortableIdParseError;

use crate::queue::{
	JobId, QueueAuxiliarySourceImportError, QueuePauseScope, TaskNameError, source_pause_key,
};
use crate::{fleet::lease, kv};

use crate::db::DatabaseError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SourceCandidateRole {
	RawKv,
	FleetState,
	LeaseAggregate,
	QueueJobs,
	QueueDeadLetters,
	QueuePauses,
}

/// Exact source identity and scope; no source row or mutation authority is retained.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct QueueSourcePauseKey {
	scope: QueuePauseScope,
	key: String,
}
impl QueueSourcePauseKey {
	pub(crate) fn from_scope(scope: QueuePauseScope) -> Self {
		let key = source_pause_key(&scope);
		Self { scope, key }
	}
	pub(crate) fn scope(&self) -> &QueuePauseScope {
		&self.scope
	}
	pub(crate) fn as_bytes(&self) -> &[u8] {
		self.key.as_bytes()
	}
}
impl fmt::Debug for QueueSourcePauseKey {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("QueueSourcePauseKey")
			.field("key_byte_len", &self.key.len())
			.finish_non_exhaustive()
	}
}

/// None begins before every key; each variant permits only its actual parsed cursor type.
#[derive(Clone)]
pub(crate) enum SourceCandidateSelection {
	RawKv { after: Option<kv::Key> },
	FleetState { after: Option<kv::Key> },
	LeaseAggregate { after: Option<lease::LeaseKey> },
	QueueJobs { after: Option<JobId> },
	QueueDeadLetters { after: Option<JobId> },
	QueuePauses { after: Option<QueueSourcePauseKey> },
}
impl SourceCandidateSelection {
	pub(crate) fn role(&self) -> SourceCandidateRole {
		match self {
			Self::RawKv { .. } => SourceCandidateRole::RawKv,
			Self::FleetState { .. } => SourceCandidateRole::FleetState,
			Self::LeaseAggregate { .. } => SourceCandidateRole::LeaseAggregate,
			Self::QueueJobs { .. } => SourceCandidateRole::QueueJobs,
			Self::QueueDeadLetters { .. } => SourceCandidateRole::QueueDeadLetters,
			Self::QueuePauses { .. } => SourceCandidateRole::QueuePauses,
		}
	}
	pub(crate) fn after_bytes(&self) -> Option<&[u8]> {
		match self {
			Self::RawKv { after } | Self::FleetState { after } => {
				after.as_ref().map(|key| key.as_str().as_bytes())
			}
			Self::LeaseAggregate { after } => after.as_ref().map(|key| key.as_str().as_bytes()),
			Self::QueueJobs { after } | Self::QueueDeadLetters { after } => {
				after.as_ref().map(|id| id.as_bytes().as_slice())
			}
			Self::QueuePauses { after } => after.as_ref().map(QueueSourcePauseKey::as_bytes),
		}
	}
}
impl fmt::Debug for SourceCandidateSelection {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("SourceCandidateSelection")
			.field("role", &self.role())
			.field("after_byte_len", &self.after_bytes().map(<[u8]>::len))
			.finish()
	}
}

pub(crate) struct SourceCandidatePageRequest {
	selection: SourceCandidateSelection,
	limit: NonZeroU32,
}
impl SourceCandidatePageRequest {
	pub(crate) fn new(selection: SourceCandidateSelection, limit: NonZeroU32) -> Self {
		Self { selection, limit }
	}
	pub(crate) fn selection(&self) -> &SourceCandidateSelection {
		&self.selection
	}
	pub(crate) fn limit(&self) -> NonZeroU32 {
		self.limit
	}
}

/// Diagnostic candidate identity only. Consuming storage must reacquire fresh record authority.
#[derive(Clone, Eq, PartialEq)]
pub(crate) enum SourceCandidateKey {
	RawKv(kv::Key),
	FleetState(kv::Key),
	LeaseAggregate(lease::LeaseKey),
	QueueJobs(JobId),
	QueueDeadLetters(JobId),
	QueuePauses(QueueSourcePauseKey),
}
impl SourceCandidateKey {
	pub(crate) fn role(&self) -> SourceCandidateRole {
		match self {
			Self::RawKv(_) => SourceCandidateRole::RawKv,
			Self::FleetState(_) => SourceCandidateRole::FleetState,
			Self::LeaseAggregate(_) => SourceCandidateRole::LeaseAggregate,
			Self::QueueJobs(_) => SourceCandidateRole::QueueJobs,
			Self::QueueDeadLetters(_) => SourceCandidateRole::QueueDeadLetters,
			Self::QueuePauses(_) => SourceCandidateRole::QueuePauses,
		}
	}
	pub(crate) fn as_bytes(&self) -> &[u8] {
		match self {
			Self::RawKv(key) | Self::FleetState(key) => key.as_str().as_bytes(),
			Self::LeaseAggregate(key) => key.as_str().as_bytes(),
			Self::QueueJobs(id) | Self::QueueDeadLetters(id) => id.as_bytes(),
			Self::QueuePauses(key) => key.as_bytes(),
		}
	}
}
impl fmt::Debug for SourceCandidateKey {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("SourceCandidateKey")
			.field("role", &self.role())
			.field("key_byte_len", &self.as_bytes().len())
			.finish()
	}
}

/// One statement's bounded key observation. A page with fewer keys than the row budget grants
/// no stable coverage or retirement.
pub(crate) struct SourceCandidatePage {
	pub(super) role: SourceCandidateRole,
	pub(super) keys: Vec<SourceCandidateKey>,
}
impl SourceCandidatePage {
	pub(crate) fn keys(&self) -> &[SourceCandidateKey] {
		&self.keys
	}
}
impl fmt::Debug for SourceCandidatePage {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("SourceCandidatePage")
			.field("role", &self.role)
			.field("count", &self.keys.len())
			.finish()
	}
}

/// The exact invalid or out-of-bound key remains available only through deliberate accessors.
pub(crate) struct SourceCandidateFailureContext {
	pub(super) raw_key: Vec<u8>,
}
impl fmt::Debug for SourceCandidateFailureContext {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("SourceCandidateFailureContext")
			.field("key_byte_len", &self.raw_key.len())
			.finish()
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SourceCandidateKeyError {
	#[error("source candidate key is not UTF8")]
	KeyUtf8(#[source] std::str::Utf8Error),
	#[error("source candidate pause task is not UTF8")]
	TaskUtf8(#[source] std::str::Utf8Error),
	#[error("source candidate KV key is not canonical")]
	Kv(#[source] kv::KeyError),
	#[error("source candidate lease key lacks its final separator")]
	LeaseFinalSeparator,
	#[error("source candidate lease key is not canonical")]
	Lease(#[source] lease::CoordinationKeyError),
	#[error("source candidate Queue ID is invalid")]
	Id(#[source] SortableIdParseError),
	#[error("source candidate pause task is invalid")]
	Task(#[source] TaskNameError),
	#[error("source candidate pause key/task association is invalid")]
	Pause(#[source] QueueAuxiliarySourceImportError),
	#[error("source candidate non-pause observation has a task field")]
	UnexpectedTask,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SourceCandidateBoundsError {
	#[error("source candidate page exceeds its requested row limit")]
	LimitExceeded,
	#[error("source candidate key does not belong to the requested role")]
	RoleMismatch,
	#[error("source candidate key is outside the exclusive lower bound")]
	LowerBoundViolation,
	#[error("source candidate keys are not strictly increasing bytewise")]
	KeyOrderViolation,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SourceCandidatePageError {
	#[error("source candidate page database work failed")]
	Database(#[from] DatabaseError),
	#[error("source candidate page has an invalid typed key")]
	Key {
		context: Box<SourceCandidateFailureContext>,
		#[source]
		source: Box<SourceCandidateKeyError>,
	},
	#[error("source candidate page violates its request bounds")]
	Bounds {
		context: Box<SourceCandidateFailureContext>,
		#[source]
		source: SourceCandidateBoundsError,
	},
}
impl SourceCandidatePageError {
	/// The stored bytes of the key that the page could not accept, when the page failed at one
	/// row.
	pub(crate) fn stored_key(&self) -> Option<&[u8]> {
		match self {
			Self::Key { context, .. } | Self::Bounds { context, .. } => Some(&context.raw_key),
			Self::Database(_) => None,
		}
	}
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Database(error) => Some(error),
			Self::Key { .. } | Self::Bounds { .. } => None,
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		self.database_error()
			.is_some_and(DatabaseError::commit_outcome_is_unknown)
	}
}
