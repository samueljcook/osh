//! Public Topic operations of the native runtime.

use std::error::Error;
use std::fmt;
use std::num::NonZeroU32;

use crate::db::{Database, WriteDatabase};
use crate::fleet::topic::engine::{
	NativeTopicCause, NativeTopicConstructionError, NativeTopicError, NativeTopicPublishFields,
	NativeTopicReadFields, PgNativeTopic,
};
use crate::fleet::{SubscriptionKey, TopicKey};
use crate::kv::Ttl;
use crate::scope::TopicScope;
use crate::scope::payload::PayloadSchemaDeclaration;

/// The inputs of a Topic handle.
pub struct PgTopicFields<'a, T, C> {
	/// The Topic's key.
	pub key: TopicKey,
	/// The declared Topic scope that holds the Topic's events.
	pub scope: &'a TopicScope<T, C>,
	/// How long a published event stays readable.
	pub event_ttl: Ttl,
}

/// One Topic of the runtime's installation, with events of type `T`.
///
/// Each publication stores one event under the Topic's next sequence, starting at 1. A
/// subscription reads the events after its cursor and advances the cursor past them in the
/// same transaction so that the reads of one subscription deliver each event once, whichever
/// process runs them. Independent subscriptions of one Topic each receive every event.
pub struct PgTopic<'a, T, C> {
	native: PgNativeTopic<'a, 'a>,
	payload: PayloadSchemaDeclaration<'a, 'a, T, C>,
}

/// A published event's sequence and publication time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct PgTopicPublication {
	/// The event's sequence in the Topic.
	pub sequence: i64,
	/// When the event was published, in Unix microseconds by the database clock.
	pub published_at_unix_micros: i64,
}

impl<'a, T, C> PgTopic<'a, T, C> {
	pub(crate) fn new(
		native: PgNativeTopic<'a, 'a>,
		payload: PayloadSchemaDeclaration<'a, 'a, T, C>,
	) -> Self {
		Self { native, payload }
	}

	/// The Topic's key.
	pub fn key(&self) -> &TopicKey {
		self.native.key()
	}

	/// Publishes `value` as the Topic's next event, in its own transaction or in the caller's.
	pub async fn publish(
		&self,
		database: impl WriteDatabase,
		value: &T,
	) -> Result<PgTopicPublication, PgTopicError<C>> {
		self.native
			.publish_value(
				database,
				NativeTopicPublishFields {
					payload: &self.payload,
					value,
				},
			)
			.await
			.map(|publication| PgTopicPublication {
				sequence: publication.sequence,
				published_at_unix_micros: publication.published_at.as_i64(),
			})
			.map_err(|error| PgTopicError::new(Cause::Operation(error)))
	}

	/// A subscription of the Topic. It does no database work and grants nothing.
	pub fn subscription(
		&self,
		fields: PgTopicSubscriptionFields,
	) -> PgTopicSubscription<'_, 'a, T, C> {
		PgTopicSubscription {
			topic: self,
			key: fields.key,
		}
	}
}

/// The inputs of a Topic subscription.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PgTopicSubscriptionFields {
	/// The subscription's key. Each key has its own cursor in the Topic.
	pub key: SubscriptionKey,
}

/// One subscription of a Topic.
///
/// A read advances the cursor when its transaction commits. Read in the transaction that
/// processes the events so that the processing and the cursor commit together; a read in a
/// transaction of its own commits the cursor before the app processes the events, and events
/// that the app then fails to process are not delivered again.
pub struct PgTopicSubscription<'t, 'a, T, C> {
	topic: &'t PgTopic<'a, T, C>,
	key: SubscriptionKey,
}

/// The bounds of one subscription read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgTopicReadOptions {
	/// The maximum number of stored events that the read examines. It must be positive.
	pub limit: NonZeroU32,
}

/// One event that a subscription read delivered.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct PgTopicEvent<T> {
	/// The event's sequence in the Topic.
	pub sequence: i64,
	/// When the event was published, in Unix microseconds by the database clock.
	pub published_at_unix_micros: i64,
	/// The event's value.
	pub value: T,
}

/// What one subscription read delivered.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct PgTopicReadResult<T> {
	/// The delivered events, in sequence order. Events that expired before the read are left
	/// out, and the cursor moves past them too.
	pub events: Vec<PgTopicEvent<T>>,
	/// Whether the read examined fewer stored events than its limit, which means that no newer
	/// event existed when it ran. When this is `false`, another read can deliver more events at
	/// once.
	pub reached_newest: bool,
}

/// The inputs of a cursor setting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PgTopicCursorFields {
	/// The non-negative sequence after which the next read starts.
	pub sequence: i64,
}

impl<T, C> PgTopicSubscription<'_, '_, T, C> {
	/// The subscription's key.
	pub fn key(&self) -> &SubscriptionKey {
		&self.key
	}

	/// Reads the events after the subscription's cursor, examining at most `options.limit`
	/// stored events, and advances the cursor past the examined events in the same
	/// transaction. A cursor that no read has advanced is 0, so the first read starts at the
	/// Topic's first retained event.
	///
	/// When an event's value cannot be decoded, the read fails with
	/// [`PgTopicErrorKind::UndecodableEvent`], delivers nothing, and leaves the cursor in
	/// place; [`PgTopicError::undecodable_sequence`] names the event, and `set_cursor` can move
	/// the cursor past it.
	pub async fn read_new_events_and_advance_cursor(
		&self,
		database: impl WriteDatabase,
		options: PgTopicReadOptions,
	) -> Result<PgTopicReadResult<T>, PgTopicError<C>> {
		self.topic
			.native
			.read_events_and_advance_cursor(
				database,
				NativeTopicReadFields {
					payload: &self.topic.payload,
					subscription: &self.key,
					limit: options.limit,
				},
			)
			.await
			.map(|read| PgTopicReadResult {
				events: read
					.events
					.into_iter()
					.map(|event| PgTopicEvent {
						sequence: event.sequence,
						published_at_unix_micros: event.published_at.as_i64(),
						value: event.value,
					})
					.collect(),
				reached_newest: read.reached_newest,
			})
			.map_err(|error| PgTopicError::new(Cause::Operation(error)))
	}

	/// Reads the subscription's cursor: the sequence after which the next read starts. A read
	/// moves the cursor to the newest event that it delivered or moved past, `set_cursor` sets
	/// it, and a cursor that neither has changed is 0. The read grants nothing.
	pub async fn fetch_cursor(&self, database: impl Database) -> Result<i64, PgTopicError<C>> {
		self.topic
			.native
			.fetch_cursor(database, &self.key)
			.await
			.map_err(|error| PgTopicError::new(Cause::Operation(error)))
	}

	/// Sets the subscription's cursor to `fields.sequence`, which can be lower or higher than
	/// its current value. The next read starts after that sequence.
	pub async fn set_cursor(
		&self,
		database: impl WriteDatabase,
		fields: PgTopicCursorFields,
	) -> Result<(), PgTopicError<C>> {
		self.topic
			.native
			.set_cursor(database, &self.key, fields.sequence)
			.await
			.map_err(|error| PgTopicError::new(Cause::Operation(error)))
	}
}

/// The class of a failed Topic operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PgTopicErrorKind {
	/// The installation does not declare the scope as a Topic scope with these value and codec
	/// failure types, or the runtime could not build the Topic from the declaration.
	Scope,
	/// The app's payload codec could not encode the published value. The error's source chain
	/// holds the codec error.
	Codec,
	/// An event's value could not be decoded. `undecodable_sequence` names the event.
	UndecodableEvent,
	/// A cursor setting named a negative sequence.
	InvalidCursor,
	/// A database statement or the transaction's own control failed. The error's source chain
	/// holds the database error.
	Database,
	/// The runtime stopped accepting work.
	RuntimeStopped,
	/// The runtime cannot operate on the Topic under the installation's recorded policy.
	Admission,
	/// The stored sequence, event, or cursor could not be interpreted or changed.
	State,
	/// A local failure that the app cannot correct.
	Internal,
}

enum Cause<C> {
	Construction(NativeTopicConstructionError),
	ScopeHandle(crate::runtime::PgScopeHandleError),
	Operation(NativeTopicError<C>),
}

/// A failed Topic operation, with its complete cause chain. `C` is the error type of the app's
/// payload codec.
pub struct PgTopicError<C> {
	source: Box<Cause<C>>,
}

impl<C> PgTopicError<C> {
	fn new(source: Cause<C>) -> Self {
		Self {
			source: Box::new(source),
		}
	}

	pub(crate) fn construction(source: NativeTopicConstructionError) -> Self {
		Self::new(Cause::Construction(source))
	}

	pub(crate) fn scope_handle(source: crate::runtime::PgScopeHandleError) -> Self {
		Self::new(Cause::ScopeHandle(source))
	}

	/// The sequence of the event that could not be decoded, when the kind is
	/// `UndecodableEvent`.
	pub fn undecodable_sequence(&self) -> Option<i64> {
		match self.source.as_ref() {
			Cause::Operation(error) => match error.typed_source() {
				NativeTopicCause::EventRead { sequence, .. } => Some(*sequence),
				_ => None,
			},
			_ => None,
		}
	}

	/// Whether the operation's own `COMMIT` failed, which means that a publication or a cursor
	/// change may or may not have committed.
	pub fn commit_outcome_is_unknown(&self) -> bool {
		match self.source.as_ref() {
			Cause::Operation(error) => {
				crate::db::TransactionFailure::commit_outcome_is_unknown(error)
			}
			Cause::Construction(_) | Cause::ScopeHandle(_) => false,
		}
	}
}

impl<C: Error + 'static> PgTopicError<C> {
	/// The class of the failure.
	pub fn kind(&self) -> PgTopicErrorKind {
		use crate::fleet::lease::handle::HandleFailureClass as Class;
		let error = match self.source.as_ref() {
			Cause::Construction(_) | Cause::ScopeHandle(_) => return PgTopicErrorKind::Scope,
			Cause::Operation(error) => error,
		};
		match error.typed_source() {
			NativeTopicCause::EventRead { .. } => return PgTopicErrorKind::UndecodableEvent,
			NativeTopicCause::Payload(_) => return PgTopicErrorKind::Codec,
			NativeTopicCause::Scope(_) => return PgTopicErrorKind::Scope,
			NativeTopicCause::Cursor(crate::fleet::scalar_codec::ScalarStateError::Domain(_)) => {
				return PgTopicErrorKind::InvalidCursor;
			}
			_ => {}
		}
		match crate::fleet::lease::handle::classify_handle_failure(self.source_error()) {
			Class::Database => PgTopicErrorKind::Database,
			Class::RuntimeStopped => PgTopicErrorKind::RuntimeStopped,
			Class::Admission => PgTopicErrorKind::Admission,
			Class::Internal => PgTopicErrorKind::Internal,
			// A Topic's cause chain holds no lease driver error, which alone yields these.
			Class::Timeout | Class::LeadershipLost | Class::State => PgTopicErrorKind::State,
		}
	}

	fn source_error(&self) -> &(dyn Error + 'static) {
		match self.source.as_ref() {
			Cause::Construction(source) => source,
			Cause::ScopeHandle(source) => source,
			Cause::Operation(source) => source,
		}
	}
}

impl<C: Error + 'static> fmt::Display for PgTopicError<C> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("Topic operation failed")
	}
}

impl<C: Error + 'static> fmt::Debug for PgTopicError<C> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(formatter, "PgTopicError: {self}")?;
		let mut source = self.source();
		while let Some(error) = source {
			write!(formatter, ": {error}")?;
			source = error.source();
		}
		Ok(())
	}
}

impl<C: Error + 'static> Error for PgTopicError<C> {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		Some(self.source_error())
	}
}
