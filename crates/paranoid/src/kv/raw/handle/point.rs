//! Point calls delegate once through the actual closed owner without additional database work.

use std::convert::Infallible;

use crate::db::{Database, WriteDatabase};
use crate::kv::{
	AtomicLiveMutationCurrent, AtomicMutation, AtomicMutationCurrent, AtomicMutationResult,
	ConditionalRawWriteResult, KeyLifetime, RawLiveOrInitMutationResult, RawValueObservation,
	RawWriteResult, StateTimestampMicros, Ttl,
};
use crate::records::RecordKey;

use super::{PgRaw, PgRawError, RawHandleOwner};

impl PgRaw<'_> {
	/// Reads one record's live value, or returns `None` when the record is absent or expired.
	pub async fn get_bytes(
		&self,
		database: impl Database,
		key: &RecordKey,
	) -> Result<Option<Vec<u8>>, PgRawError<Infallible>> {
		match &self.owner {
			RawHandleOwner::Core(store) => store.get_bytes(database, key).await,
			RawHandleOwner::Scope(store) => store.get_bytes(database, key).await,
		}
		.map_err(|error| self.point_error(key, error))
	}

	/// Reads one record's live value and metadata, and the database time of the read, which the
	/// result reports also when the record is absent or expired.
	pub async fn get_bytes_observation(
		&self,
		database: impl Database,
		key: &RecordKey,
	) -> Result<RawValueObservation, PgRawError<Infallible>> {
		match &self.owner {
			RawHandleOwner::Core(store) => store.get_bytes_observation(database, key).await,
			RawHandleOwner::Scope(store) => store.get_bytes_observation(database, key).await,
		}
		.map_err(|error| self.point_error(key, error))
	}

	/// Reads a record while retaining a lock that blocks updates and deletes until transaction
	/// end. Native records use shared locks, so concurrent locked reads can proceed together.
	/// Core records in v10 coexistence or relocation use their existing exclusive mutation locks.
	/// App Raw scopes require the Reader role; PostgreSQL also requires UPDATE privileges for
	/// row locking, and the supplied transaction must be writable.
	///
	/// Time and liveness are observed after lock acquisition. An absent result does not prevent
	/// a later insert. Expiry can pass while a lock is held. Rollback to an earlier savepoint
	/// releases locks acquired after it; callers must then repeat any dependent authorization.
	/// Cancelling this operation marks the transaction's statement as interrupted, so commit
	/// or rollback discards the connection without waiting for that statement to finish.
	pub async fn get_bytes_observation_with_lock(
		&self,
		tx: &mut crate::db::WriteTx<'_>,
		key: &RecordKey,
	) -> Result<RawValueObservation, PgRawError<Infallible>> {
		let mut interruption = tx.track_statement_interruption();
		let result = match &self.owner {
			RawHandleOwner::Core(store) => store.get_bytes_observation_with_lock(tx, key).await,
			RawHandleOwner::Scope(store) => store.get_bytes_observation_with_lock(tx, key).await,
		}
		.map_err(|error| self.point_error(key, error));
		interruption.complete();
		result
	}

	/// Whether the record is live, without reading its value.
	pub async fn exists(
		&self,
		database: impl Database,
		key: &RecordKey,
	) -> Result<bool, PgRawError<Infallible>> {
		match &self.owner {
			RawHandleOwner::Core(store) => store.exists(database, key).await,
			RawHandleOwner::Scope(store) => store.exists(database, key).await,
		}
		.map_err(|error| self.point_error(key, error))
	}

	/// The live record's remaining lifetime, or `None` when the record is absent or expired.
	pub async fn lifetime(
		&self,
		database: impl Database,
		key: &RecordKey,
	) -> Result<Option<KeyLifetime>, PgRawError<Infallible>> {
		match &self.owner {
			RawHandleOwner::Core(store) => store.lifetime(database, key).await,
			RawHandleOwner::Scope(store) => store.lifetime(database, key).await,
		}
		.map_err(|error| self.point_error(key, error))
	}

	/// Replaces the record's value. The TTL counts from the database time of the write. A
	/// write inside the caller's transaction becomes durable only when the caller commits.
	pub async fn set_bytes(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		value: Vec<u8>,
		ttl: Ttl,
	) -> Result<RawWriteResult, PgRawError<Infallible>> {
		match &self.owner {
			RawHandleOwner::Core(store) => store.set_bytes(database, key, value, ttl).await,
			RawHandleOwner::Scope(store) => store.set_bytes(database, key, value, ttl).await,
		}
		.map_err(|error| self.point_error(key, error))
	}

	/// Writes the value only when the record is absent or expired. The result for a live record
	/// does not include its value or metadata.
	pub async fn set_bytes_if_absent(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		value: Vec<u8>,
		ttl: Ttl,
	) -> Result<ConditionalRawWriteResult, PgRawError<Infallible>> {
		match &self.owner {
			RawHandleOwner::Core(store) => {
				store.set_bytes_if_absent(database, key, value, ttl).await
			}
			RawHandleOwner::Scope(store) => {
				store.set_bytes_if_absent(database, key, value, ttl).await
			}
		}
		.map_err(|error| self.point_error(key, error))
	}

	/// Deletes a live record. An absent or expired record fails with the kind `NotLive`.
	pub async fn delete_live(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
	) -> Result<(), PgRawError<Infallible>> {
		match &self.owner {
			RawHandleOwner::Core(store) => store.delete_live(database, key).await,
			RawHandleOwner::Scope(store) => store.delete_live(database, key).await,
		}
		.map_err(|error| self.point_error(key, error))
	}

	/// Locks the record and calls `decide_mutation_fn` once with its live value, or with no value
	/// when the record is absent or expired, and then applies the decision's change. The
	/// operation needs both the reader and the writer role in an app Raw scope.
	pub async fn mutate<C>(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		decide_mutation_fn: impl for<'current> FnOnce(
			AtomicMutationCurrent<'current>,
		) -> Result<AtomicMutation, C>,
	) -> Result<AtomicMutationResult, PgRawError<C>> {
		match &self.owner {
			RawHandleOwner::Core(store) => store.mutate(database, key, decide_mutation_fn).await,
			RawHandleOwner::Scope(store) => store.mutate(database, key, decide_mutation_fn).await,
		}
		.map_err(|error| self.point_error(key, error))
	}

	/// Locks the record and calls `decide_mutation_fn` once with its live value. An absent or
	/// expired record fails with the kind `NotLive`, and the decision does not run.
	pub async fn mutate_live<C>(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		decide_mutation_fn: impl for<'current> FnOnce(
			AtomicLiveMutationCurrent<'current>,
		) -> Result<AtomicMutation, C>,
	) -> Result<AtomicMutationResult, PgRawError<C>> {
		match &self.owner {
			RawHandleOwner::Core(store) => {
				store.mutate_live(database, key, decide_mutation_fn).await
			}
			RawHandleOwner::Scope(store) => {
				store.mutate_live(database, key, decide_mutation_fn).await
			}
		}
		.map_err(|error| self.point_error(key, error))
	}

	/// Locks the record and calls `decide_mutation_fn` once with its live value. When the record
	/// is absent or expired, Paranoid first calls `initialize_value_fn` for a value and a TTL and
	/// passes that value to the decision without storing it first.
	///
	/// After initialization, `KeepExisting` stores the initial value with the initial TTL,
	/// `SetBytesPreservingExpiration` stores the new value with the initial TTL, `SetBytes` uses
	/// its own TTL, and `Delete` stores nothing.
	pub async fn mutate_live_or_init<C>(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		initialize_value_fn: impl FnOnce(StateTimestampMicros) -> Result<(Vec<u8>, Ttl), C>,
		decide_mutation_fn: impl for<'current> FnOnce(
			AtomicLiveMutationCurrent<'current>,
		) -> Result<AtomicMutation, C>,
	) -> Result<RawLiveOrInitMutationResult, PgRawError<C>> {
		match &self.owner {
			RawHandleOwner::Core(store) => {
				store
					.mutate_live_or_init(database, key, initialize_value_fn, decide_mutation_fn)
					.await
			}
			RawHandleOwner::Scope(store) => {
				store
					.mutate_live_or_init(database, key, initialize_value_fn, decide_mutation_fn)
					.await
			}
		}
		.map_err(|error| self.point_error(key, error))
	}

	/// Sets a live record's modification time to the database time and keeps its value and
	/// expiry. An absent or expired record fails with the kind `NotLive`.
	pub async fn touch(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
	) -> Result<(), PgRawError<Infallible>> {
		match &self.owner {
			RawHandleOwner::Core(store) => store.touch(database, key).await,
			RawHandleOwner::Scope(store) => store.touch(database, key).await,
		}
		.map_err(|error| self.point_error(key, error))
	}

	/// Replaces a live record's expiry with `ttl`, counted from the database time. An absent or
	/// expired record fails with the kind `NotLive`.
	pub async fn set_ttl(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		ttl: Ttl,
	) -> Result<(), PgRawError<Infallible>> {
		match &self.owner {
			RawHandleOwner::Core(store) => store.set_ttl(database, key, ttl).await,
			RawHandleOwner::Scope(store) => store.set_ttl(database, key, ttl).await,
		}
		.map_err(|error| self.point_error(key, error))
	}

	/// Makes a live record expire at the database time and keeps its stored value until a
	/// deletion or a cleanup removes it. An absent or expired record fails with the kind
	/// `NotLive`.
	pub async fn expire(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
	) -> Result<(), PgRawError<Infallible>> {
		match &self.owner {
			RawHandleOwner::Core(store) => store.expire(database, key).await,
			RawHandleOwner::Scope(store) => store.expire(database, key).await,
		}
		.map_err(|error| self.point_error(key, error))
	}
}
