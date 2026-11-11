//! Topic event point completions; publication allocation and workers belong to owning callers.

use super::{
	PayloadPointAction, PayloadPointCause, PayloadPointContext, PayloadPointError,
	PayloadPointFamily, RecordIdentity,
};
use crate::fleet::payload::scope::topic::{BoundTopicEventScopeReader, TopicEventScopeReadFields};
use crate::fleet::topic::event_state::{NativeTopicEventState, TopicEventIdentity};
use crate::records::PointRecordRead;
use crate::scope::payload::BoundPayloadReader;

pub(crate) struct TopicPointReadFields<'a> {
	pub(crate) identity: &'a RecordIdentity,
	pub(crate) topic_event_identity: &'a TopicEventIdentity,
	pub(crate) snapshot: &'a PointRecordRead,
}
pub(crate) struct TopicEventRead {
	pub(crate) state: NativeTopicEventState,
}
pub(crate) struct TopicValueRead<T> {
	pub(crate) event: TopicEventRead,
	pub(crate) value: T,
}
fn context(action: PayloadPointAction) -> PayloadPointContext {
	PayloadPointContext {
		family: PayloadPointFamily::TopicEvent,
		action,
	}
}
pub(crate) fn observe_topic_value<T, C>(
	fields: TopicPointReadFields<'_>,
	reader: &BoundTopicEventScopeReader<'_, '_>,
	payload: &BoundPayloadReader<'_, T, C>,
) -> Result<Option<TopicValueRead<T>>, PayloadPointError<C>> {
	let fail = |error| PayloadPointError::new(context(PayloadPointAction::ObserveValue), error);
	reader
		.require_payload_reader(payload)
		.map_err(|error| fail(PayloadPointCause::Envelope(error)))?;
	let Some(record) = fields.snapshot.live_record() else {
		return Ok(None);
	};
	let state = reader
		.decode_record(TopicEventScopeReadFields {
			identity: fields.identity,
			topic_event_identity: fields.topic_event_identity,
			record,
		})
		.map_err(|error| fail(PayloadPointCause::Envelope(error)))?;
	let value = payload
		.decode(state.payload())
		.map_err(|error| fail(PayloadPointCause::Payload(error)))?;
	Ok(Some(TopicValueRead {
		event: TopicEventRead { state },
		value,
	}))
}
