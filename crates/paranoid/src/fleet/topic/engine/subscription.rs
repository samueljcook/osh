//! A subscription's cursor and the Topic events after it.
//!
//! A read locks the subscription's cursor record, reads one bounded page of the events after the
//! cursor, and advances the cursor past every event of the page in the same transaction so that
//! concurrent reads of one subscription deliver each event once. An event that expired before the
//! read is skipped.

use std::num::NonZeroU32;

use crate::db::{Database, WriteDatabase, run_transaction_read, run_transaction_write};
use crate::fleet::SubscriptionKey;
use crate::fleet::payload::identity::NativeFleetPayloadIdentity;
use crate::fleet::payload::point::topic::{TopicPointReadFields, observe_topic_value};
use crate::fleet::scalar::NativeFleetScalarResource;
use crate::fleet::topic::event_state::{TopicEventIdentity, TopicEventIdentityFields};
use crate::fleet::topic::state::SubscriptionCursorMutation;
use crate::records::postgres::scan::RecordVerificationPageRequest;
use crate::records::{
	NamespacePageRequest, PointRecordLock, PointRecordRead, RecordIdentity, RecordKey,
	RecordMetadata, StateTimestampMicros,
};
use crate::runtime::admission::NativeOperationError;
use crate::runtime::admission::payload::PayloadAdmissionFields;
use crate::scope::payload::PayloadSchemaDeclaration;

use super::{NativeTopicCause, NativeTopicError, PgNativeTopic};

pub(crate) struct NativeTopicReadFields<'a, 'scope, 'code, T, C> {
	pub(crate) payload: &'a PayloadSchemaDeclaration<'scope, 'code, T, C>,
	pub(crate) subscription: &'a SubscriptionKey,
	pub(crate) limit: NonZeroU32,
}

pub(crate) struct NativeTopicEvent<T> {
	pub(crate) sequence: i64,
	pub(crate) published_at: StateTimestampMicros,
	pub(crate) value: T,
}

pub(crate) struct NativeTopicReadResult<T> {
	pub(crate) events: Vec<NativeTopicEvent<T>>,
	/// Whether the page held fewer events than the limit, which means that no newer event
	/// existed when the read ran.
	pub(crate) reached_newest: bool,
}

impl PgNativeTopic<'_, '_> {
	fn cursor_identity(&self, subscription: &SubscriptionKey) -> RecordIdentity {
		RecordIdentity::new(
			self.placement.subscription_cursor().clone(),
			NativeFleetScalarResource::SubscriptionCursor {
				topic: &self.key,
				subscription,
			}
			.record_key(),
		)
	}

	fn event_key(&self, sequence: i64) -> Option<(TopicEventIdentity, RecordKey)> {
		let identity = TopicEventIdentity::new(TopicEventIdentityFields {
			topic: self.key.clone(),
			sequence,
		})
		.ok()?;
		let key = NativeFleetPayloadIdentity::TopicEvent(&identity).record_key();
		Some((identity, key))
	}

	/// The identity of the event that `key` names, when `key` is an event key of this Topic. An
	/// event key ends with the event's sequence as eight big-endian bytes.
	fn event_of_key(&self, key: &RecordKey) -> Option<TopicEventIdentity> {
		let bytes = key.as_bytes();
		let sequence = bytes
			.len()
			.checked_sub(8)
			.and_then(|start| <[u8; 8]>::try_from(&bytes[start..]).ok())
			.map(i64::from_be_bytes)?;
		let (identity, expected) = self.event_key(sequence)?;
		(&expected == key).then_some(identity)
	}

	/// Reads at most `fields.limit` events after the subscription's cursor and advances the
	/// cursor past them, in one transaction.
	pub(crate) async fn read_events_and_advance_cursor<T, C>(
		&self,
		database: impl WriteDatabase,
		fields: NativeTopicReadFields<'_, '_, '_, T, C>,
	) -> Result<NativeTopicReadResult<T>, NativeTopicError<C>> {
		run_transaction_write(
			database,
			"fleet.native_topic.read_events_and_advance_cursor",
			async |tx| {
				self.admission
					.execute_write_with_contracts(tx, async |contracts, tx| {
						let operation = |error: NativeTopicCause<C>| {
							NativeOperationError::Operation(self.failure(error))
						};
						let reader = contracts
							.bind_topic_event_reader(self.event.declaration())
							.map_err(NativeOperationError::Admission)?;
						self.event
							.require_payload_declaration(fields.payload)
							.map_err(|error| operation(NativeTopicCause::Scope(error)))?;
						let payload = contracts
							.bind_payload_reader(PayloadAdmissionFields {
								actual_scope: self.event.scope(),
								declaration: fields.payload,
							})
							.map_err(NativeOperationError::Admission)?;
						payload
							.require_declaration(fields.payload)
							.map_err(|error| operation(NativeTopicCause::Scope(error)))?;
						let declaration = self.code.topic.declaration();
						let component = contracts
							.bind(&declaration)
							.map_err(NativeOperationError::Admission)?;
						let cursor_schema = self
							.code
							.topic
							.bind_cursor(&component)
							.map_err(|error| operation(NativeTopicCause::Cursor(error)))?;
						let cursor_identity = self.cursor_identity(fields.subscription);
						let mut lock = self
							.placement
							.storage()
							.lock(tx, &cursor_identity, self.budget.conflict_attempts)
							.await
							.map_err(|error| operation(NativeTopicCause::CursorStorage(error)))?;
						let current = lock
							.snapshot()
							.live_record()
							.map(|record| cursor_schema.decode_record(record))
							.transpose()
							.map_err(|error| operation(NativeTopicCause::Cursor(error)))?;
						let cursor = self
							.code
							.topic
							.cursor(current)
							.map_err(|error| operation(NativeTopicCause::Cursor(error)))?;
						// No event follows the largest sequence, so a cursor there reads no page.
						let (events, last, reached_newest) = if cursor == i64::MAX {
							(Vec::new(), cursor, true)
						} else {
							let (Some((_, after)), Some((_, upper))) =
								(self.event_key(cursor), self.event_key(i64::MAX))
							else {
								unreachable!(
									"a non-negative cursor and the largest sequence name events"
								)
							};
							let request = RecordVerificationPageRequest::new(
								NamespacePageRequest::new(
									self.event.scope().namespace().clone(),
									Some(after),
									fields.limit,
								),
								upper,
							)
							.map_err(|error| operation(NativeTopicCause::EventPage(error)))?;
							let page = self
								.event
								.storage()
								.read_verification_page(lock.as_read_tx(), &request)
								.await
								.map_err(|error| operation(NativeTopicCause::EventPage(error)))?;
							let mut events = Vec::with_capacity(page.entries().len());
							let mut last = cursor;
							for entry in page.entries() {
								let key = self
									.event_of_key(entry.identity().key())
									.ok_or_else(|| operation(NativeTopicCause::EventKey))?;
								let snapshot = PointRecordRead::new(
									Some(entry.record().clone()),
									page.observed_at(),
								);
								let read = observe_topic_value(
									TopicPointReadFields {
										identity: entry.identity(),
										topic_event_identity: &key,
										snapshot: &snapshot,
									},
									&reader,
									&payload,
								)
								.map_err(|error| {
									operation(NativeTopicCause::EventRead {
										sequence: key.sequence(),
										source: Box::new(error),
									})
								})?;
								last = key.sequence();
								if let Some(read) = read {
									events.push(NativeTopicEvent {
										sequence: key.sequence(),
										published_at: read.event.state.published_at(),
										value: read.value,
									});
								}
							}
							let reached_newest = page.entries().len() < fields.limit.get() as usize;
							(events, last, reached_newest)
						};
						let mutation = self
							.code
							.topic
							.advance_cursor(current, last)
							.map_err(|error| operation(NativeTopicCause::Cursor(error)))?;
						match mutation {
							SubscriptionCursorMutation::Set(value) => {
								let record = cursor_schema
									.encode_record(
										&value,
										RecordMetadata {
											expires_at: None,
											updated_at: lock.snapshot().observed_at(),
										},
									)
									.map_err(|error| operation(NativeTopicCause::Cursor(error)))?;
								lock.write(record).await
							}
							SubscriptionCursorMutation::KeepExisting => lock.keep().await,
						}
						.map_err(|error| operation(NativeTopicCause::CursorStorage(error)))?;
						Ok(Ok(NativeTopicReadResult {
							events,
							reached_newest,
						}))
					})
					.await
					.map_err(NativeTopicError::from_operation)?
			},
		)
		.await
		.map_err(|error: NativeTopicError<C>| {
			error.with_context_if_absent(self.event.scope(), &self.key)
		})
	}

	/// Reads the subscription's cursor: the sequence after which the next read starts. A read
	/// moves the cursor to the newest event that it delivered or skipped, `set_cursor` sets it,
	/// and a cursor that neither has changed is 0.
	pub(crate) async fn fetch_cursor<C>(
		&self,
		database: impl Database,
		subscription: &SubscriptionKey,
	) -> Result<i64, NativeTopicError<C>> {
		run_transaction_read(database, "fleet.native_topic.fetch_cursor", async |tx| {
			self.admission
				.execute_read_with_contracts(tx, async |contracts, tx| {
					let operation = |error: NativeTopicCause<C>| {
						NativeOperationError::Operation(self.failure(error))
					};
					let declaration = self.code.topic.declaration();
					let component = contracts
						.bind(&declaration)
						.map_err(NativeOperationError::Admission)?;
					let cursor_schema = self
						.code
						.topic
						.bind_cursor(&component)
						.map_err(|error| operation(NativeTopicCause::Cursor(error)))?;
					let snapshot = self
						.placement
						.storage()
						.read(tx, &self.cursor_identity(subscription))
						.await
						.map_err(|error| operation(NativeTopicCause::CursorStorage(error)))?;
					let current = snapshot
						.live_record()
						.map(|record| cursor_schema.decode_record(record))
						.transpose()
						.map_err(|error| operation(NativeTopicCause::Cursor(error)))?;
					self.code
						.topic
						.cursor(current)
						.map_err(|error| operation(NativeTopicCause::Cursor(error)))
				})
				.await
				.map_err(NativeTopicError::from_operation)
		})
		.await
		.map_err(|error: NativeTopicError<C>| {
			error.with_context_if_absent(self.event.scope(), &self.key)
		})
	}

	/// Sets the subscription's cursor to `sequence`, which may be lower or higher than the
	/// cursor: the next read delivers the events after `sequence`.
	pub(crate) async fn set_cursor<C>(
		&self,
		database: impl WriteDatabase,
		subscription: &SubscriptionKey,
		sequence: i64,
	) -> Result<(), NativeTopicError<C>> {
		run_transaction_write(database, "fleet.native_topic.set_cursor", async |tx| {
			self.admission
				.execute_write_with_contracts(tx, async |contracts, tx| {
					let operation = |error: NativeTopicCause<C>| {
						NativeOperationError::Operation(self.failure(error))
					};
					let declaration = self.code.topic.declaration();
					let component = contracts
						.bind(&declaration)
						.map_err(NativeOperationError::Admission)?;
					let cursor_schema = self
						.code
						.topic
						.bind_cursor(&component)
						.map_err(|error| operation(NativeTopicCause::Cursor(error)))?;
					let SubscriptionCursorMutation::Set(value) = self
						.code
						.topic
						.set_cursor(sequence)
						.map_err(|error| operation(NativeTopicCause::Cursor(error)))?
					else {
						unreachable!("an explicit cursor setting always sets the cursor")
					};
					let lock = self
						.placement
						.storage()
						.lock(
							tx,
							&self.cursor_identity(subscription),
							self.budget.conflict_attempts,
						)
						.await
						.map_err(|error| operation(NativeTopicCause::CursorStorage(error)))?;
					let record = cursor_schema
						.encode_record(
							&value,
							RecordMetadata {
								expires_at: None,
								updated_at: lock.snapshot().observed_at(),
							},
						)
						.map_err(|error| operation(NativeTopicCause::Cursor(error)))?;
					lock.write(record)
						.await
						.map_err(|error| operation(NativeTopicCause::CursorStorage(error)))
				})
				.await
				.map_err(NativeTopicError::from_operation)
		})
		.await
		.map_err(|error: NativeTopicError<C>| {
			error.with_context_if_absent(self.event.scope(), &self.key)
		})
	}
}
