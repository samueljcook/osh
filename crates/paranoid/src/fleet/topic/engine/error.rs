//! Ordinary Topic errors preserve borrowed codec causes and exact ending/cleanup failures.

use super::TopicPublicationError;
use crate::db::{DatabaseError, TransactionFailure};
use crate::fleet::TopicKey;
use crate::fleet::payload::point::PayloadPointError;
use crate::fleet::payload::point::factory::NativePayloadConstructionError;
use crate::fleet::payload::scope::PayloadScopeValueError;
use crate::fleet::scalar_codec::ScalarStateError;
use crate::installation::ProtocolError;
use crate::records::postgres::StateStorageError;
use crate::records::postgres::scan::RecordPageError;
use crate::runtime::admission::{NativeAdmissionError, NativeOperationError};
use crate::scope::{ScopeError, ScopeIdentity};
use std::fmt;

pub(crate) enum NativeTopicConstructionCause {
	Event(NativePayloadConstructionError),
	Association,
	OverlappingPlacement,
}
pub(crate) struct NativeTopicConstructionError {
	source: NativeTopicConstructionCause,
}
impl NativeTopicConstructionError {
	pub(crate) fn new(source: NativeTopicConstructionCause) -> Self {
		Self { source }
	}
}
impl fmt::Debug for NativeTopicConstructionError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("NativeTopicConstructionError { .. }")
	}
}
impl fmt::Display for NativeTopicConstructionError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("native Topic code or placement cannot be constructed")
	}
}
impl std::error::Error for NativeTopicConstructionError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match &self.source {
			NativeTopicConstructionCause::Event(source) => Some(source),
			_ => None,
		}
	}
}

pub(in crate::fleet::topic) enum NativeTopicCause<C> {
	Admission(NativeAdmissionError),
	Scope(ScopeError),
	Sequence(ScalarStateError),
	Payload(PayloadScopeValueError<C>),
	Publication(TopicPublicationError),
	/// A subscription cursor record could not be read or written in its representation.
	Cursor(ScalarStateError),
	/// Locking, reading, or writing a subscription cursor record failed.
	CursorStorage(StateStorageError),
	/// Reading a page of a Topic's events failed.
	EventPage(RecordPageError),
	/// A key in a Topic's event range is not an event key of the Topic.
	EventKey,
	/// The event with `sequence` could not be decoded, so the read delivered nothing and left
	/// the cursor in place.
	EventRead {
		sequence: i64,
		source: Box<PayloadPointError<C>>,
	},
	Database(DatabaseError),
	Protocol(ProtocolError),
	RollbackFailed {
		operation_error: Box<NativeTopicError<C>>,
		rollback_error: DatabaseError,
	},
}
struct TopicErrorContext {
	scope: ScopeIdentity,
	topic: TopicKey,
}
pub(crate) struct NativeTopicError<C> {
	context: Option<Box<TopicErrorContext>>,
	source: Box<NativeTopicCause<C>>,
}
impl<C> NativeTopicError<C> {
	pub(super) fn new(
		scope: &ScopeIdentity,
		topic: &TopicKey,
		source: NativeTopicCause<C>,
	) -> Self {
		Self {
			context: Some(Box::new(TopicErrorContext {
				scope: scope.clone(),
				topic: topic.clone(),
			})),
			source: Box::new(source),
		}
	}
	pub(super) fn with_context_if_absent(
		mut self,
		scope: &ScopeIdentity,
		topic: &TopicKey,
	) -> Self {
		if self.context.is_none() {
			self.context = Some(Box::new(TopicErrorContext {
				scope: scope.clone(),
				topic: topic.clone(),
			}));
		}
		self
	}
	pub(super) fn from_operation(source: NativeOperationError<Self>) -> Self {
		match source {
			NativeOperationError::Operation(source) => source,
			NativeOperationError::Admission(source) => Self {
				context: None,
				source: Box::new(NativeTopicCause::Admission(source)),
			},
		}
	}
	pub(in crate::fleet::topic) fn typed_source(&self) -> &NativeTopicCause<C> {
		&self.source
	}
}
impl<C> fmt::Debug for NativeTopicError<C> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("NativeTopicError { .. }")
	}
}
impl<C> fmt::Display for NativeTopicError<C> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self.source.as_ref() {
			NativeTopicCause::EventKey => {
				f.write_str("a key in the Topic's event range is not an event key of the Topic")
			}
			NativeTopicCause::EventRead { sequence, .. } => {
				write!(
					f,
					"the Topic event with sequence {sequence} could not be decoded"
				)
			}
			_ => f.write_str("native Topic operation failed"),
		}
	}
}

fn page_database_error(error: &RecordPageError) -> Option<&DatabaseError> {
	match error {
		RecordPageError::Database(error) => Some(error),
		RecordPageError::Record(error) => error.database_error(),
		_ => None,
	}
}
impl<C: std::error::Error + 'static> std::error::Error for NativeTopicError<C> {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self.source.as_ref() {
			NativeTopicCause::Admission(source) => Some(source),
			NativeTopicCause::Scope(source) => Some(source),
			NativeTopicCause::Sequence(source) => Some(source),
			NativeTopicCause::Payload(source) => Some(source),

			NativeTopicCause::Publication(source) => Some(source),
			NativeTopicCause::Cursor(source) => Some(source),
			NativeTopicCause::CursorStorage(source) => Some(source),
			NativeTopicCause::EventPage(source) => Some(source),
			NativeTopicCause::EventKey => None,
			NativeTopicCause::EventRead { source, .. } => Some(source.as_ref()),
			NativeTopicCause::Database(source) => Some(source),
			NativeTopicCause::Protocol(source) => Some(source),
			NativeTopicCause::RollbackFailed {
				operation_error, ..
			} => Some(operation_error.as_ref()),
		}
	}
}
impl<C> TransactionFailure for NativeTopicError<C> {
	fn from_protocol_error(source: ProtocolError) -> Self {
		Self {
			context: None,
			source: Box::new(NativeTopicCause::Protocol(source)),
		}
	}
	fn from_database_error(source: DatabaseError) -> Self {
		Self {
			context: None,
			source: Box::new(NativeTopicCause::Database(source)),
		}
	}
	fn from_rollback_failure(operation_error: Self, rollback_error: DatabaseError) -> Self {
		let context = operation_error.context.as_ref().map(|context| {
			Box::new(TopicErrorContext {
				scope: context.scope.clone(),
				topic: context.topic.clone(),
			})
		});
		Self {
			context,
			source: Box::new(NativeTopicCause::RollbackFailed {
				operation_error: Box::new(operation_error),
				rollback_error,
			}),
		}
	}
	fn involves_database_failure(&self) -> bool {
		match self.source.as_ref() {
			NativeTopicCause::Admission(source) => source.involves_database_failure(),
			NativeTopicCause::Publication(source) => source.database_error().is_some(),
			NativeTopicCause::CursorStorage(source) => source.database_error().is_some(),
			NativeTopicCause::EventPage(source) => page_database_error(source).is_some(),
			NativeTopicCause::Database(_) | NativeTopicCause::RollbackFailed { .. } => true,
			NativeTopicCause::Scope(_)
			| NativeTopicCause::Sequence(_)
			| NativeTopicCause::Payload(_)
			| NativeTopicCause::Cursor(_)
			| NativeTopicCause::EventKey
			| NativeTopicCause::EventRead { .. }
			| NativeTopicCause::Protocol(_) => false,
		}
	}
	fn commit_outcome_is_unknown(&self) -> bool {
		match self.source.as_ref() {
			NativeTopicCause::Admission(source) => source.commit_outcome_is_unknown(),
			NativeTopicCause::Publication(source) => source.commit_outcome_is_unknown(),
			NativeTopicCause::CursorStorage(source) => source
				.database_error()
				.is_some_and(DatabaseError::commit_outcome_is_unknown),
			NativeTopicCause::EventPage(source) => {
				page_database_error(source).is_some_and(DatabaseError::commit_outcome_is_unknown)
			}
			NativeTopicCause::Database(source) => source.commit_outcome_is_unknown(),
			NativeTopicCause::RollbackFailed {
				operation_error,
				rollback_error,
			} => {
				operation_error.commit_outcome_is_unknown()
					|| rollback_error.commit_outcome_is_unknown()
			}
			NativeTopicCause::Scope(_)
			| NativeTopicCause::Sequence(_)
			| NativeTopicCause::Payload(_)
			| NativeTopicCause::Cursor(_)
			| NativeTopicCause::EventKey
			| NativeTopicCause::EventRead { .. }
			| NativeTopicCause::Protocol(_) => false,
		}
	}
}
