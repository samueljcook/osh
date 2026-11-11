//! Fixed plaintext decisions compose the unchanged admitted Raw mutation engine.

use primitives_rs::secret_bytes::SecretBytes;

use crate::db::WriteDatabase;
use crate::kv::raw::model::encryption::{
	DecryptedRawLiveMutationCurrent, DecryptedRawLiveOrInitMutationResult,
	DecryptedRawMutationCurrent, DecryptedRawMutationResult, EncryptedRawMutation,
	PublishedItemEncryption, PublishedItemEncryptionFields, prepare_published_item_key,
};
use crate::kv::{AtomicMutation, RawLiveOrInitValue, StateTimestampMicros, Ttl};
use crate::records::RecordKey;

use super::PgEncryptedRaw;
mod error;
use error::{
	EncryptedRawMutationCause, EncryptedRawMutationDecisionError, PgEncryptedRawMutationErrorFields,
};
pub use error::{PgEncryptedRawMutationError, PgEncryptedRawMutationErrorKind};

impl PgEncryptedRaw<'_, '_, '_> {
	/// Locks the record, decrypts its live value, and calls `decide_mutation_fn` once with the
	/// value, or with `None` when the record is absent or expired. Paranoid encrypts only a
	/// replacement value that the decision chooses. Paranoid zeroes the previous plaintext when
	/// the result is dropped. A change inside the caller's transaction becomes durable only when
	/// the caller commits.
	pub async fn mutate<C>(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		decide_mutation_fn: impl for<'current> FnOnce(
			DecryptedRawMutationCurrent<'current>,
		) -> Result<EncryptedRawMutation, C>,
	) -> Result<DecryptedRawMutationResult, PgEncryptedRawMutationError<C>> {
		let parsed = prepare_published_item_key(key)
			.map_err(|error| self.mutation_failure(key, EncryptedRawMutationCause::Input(error)))?;
		let encryption = PublishedItemEncryption::new(PublishedItemEncryptionFields {
			keyset: self.keyset,
			scope: self.raw.scope(),
			key: &parsed,
		});
		let mut decision = DecryptedDecision::Awaiting;
		let result = self
			.raw
			.mutate(database, key, |current| {
				let previous_live_value = current
					.live_value()
					.map(|value| encryption.decrypt(value))
					.transpose()
					.map_err(EncryptedRawMutationDecisionError::Decryption)?;
				let mutation = decide_mutation_fn(DecryptedRawMutationCurrent::new(
					previous_live_value.as_ref(),
					current.observed_at(),
				))
				.map_err(EncryptedRawMutationDecisionError::Callback)?;
				let mutation = encode_mutation(&encryption, mutation, None)?;
				decision = DecryptedDecision::Prepared {
					value: previous_live_value,
					observed_at: current.observed_at(),
				};
				Ok(mutation)
			})
			.await
			.map_err(|error| self.mutation_failure(key, EncryptedRawMutationCause::Raw(error)))?;
		let (previous_live_value, observed_at) = decision.into_prepared();
		Ok(DecryptedRawMutationResult::new(
			previous_live_value,
			observed_at,
			result.outcome,
		))
	}

	/// Locks the record, decrypts its live value, and calls `decide_mutation_fn` once with it.
	/// An absent or expired record fails with the Raw kind `NotLive`, and a value that does not
	/// decrypt fails with `Decryption`; in both cases the decision does not run. Paranoid encrypts
	/// only a replacement value that the decision chooses.
	pub async fn mutate_live<C>(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		decide_mutation_fn: impl for<'current> FnOnce(
			DecryptedRawLiveMutationCurrent<'current>,
		) -> Result<EncryptedRawMutation, C>,
	) -> Result<DecryptedRawMutationResult, PgEncryptedRawMutationError<C>> {
		let parsed = prepare_published_item_key(key)
			.map_err(|error| self.mutation_failure(key, EncryptedRawMutationCause::Input(error)))?;
		let encryption = PublishedItemEncryption::new(PublishedItemEncryptionFields {
			keyset: self.keyset,
			scope: self.raw.scope(),
			key: &parsed,
		});
		let mut decision = DecryptedDecision::Awaiting;
		let result = self
			.raw
			.mutate_live(database, key, |current| {
				let value = encryption
					.decrypt(current.live_value())
					.map_err(EncryptedRawMutationDecisionError::Decryption)?;
				let mutation = decide_mutation_fn(DecryptedRawLiveMutationCurrent::new(
					&value,
					current.observed_at(),
				))
				.map_err(EncryptedRawMutationDecisionError::Callback)?;
				let mutation = encode_mutation(&encryption, mutation, None)?;
				decision = DecryptedDecision::Prepared {
					value,
					observed_at: current.observed_at(),
				};
				Ok(mutation)
			})
			.await
			.map_err(|error| self.mutation_failure(key, EncryptedRawMutationCause::Raw(error)))?;
		let (previous_live_value, observed_at) = decision.into_prepared();
		Ok(DecryptedRawMutationResult::new(
			Some(previous_live_value),
			observed_at,
			result.outcome,
		))
	}

	/// Locks the record and calls `decide_mutation_fn` once with its decrypted live value. When
	/// the record is absent or expired, Paranoid first calls `initialize_value_fn` for a value and
	/// a TTL and passes that value to the decision without storing it first.
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
			DecryptedRawLiveMutationCurrent<'current>,
		) -> Result<EncryptedRawMutation, C>,
	) -> Result<DecryptedRawLiveOrInitMutationResult, PgEncryptedRawMutationError<C>> {
		let parsed = prepare_published_item_key(key)
			.map_err(|error| self.mutation_failure(key, EncryptedRawMutationCause::Input(error)))?;
		let encryption = PublishedItemEncryption::new(PublishedItemEncryptionFields {
			keyset: self.keyset,
			scope: self.raw.scope(),
			key: &parsed,
		});
		let mut decision = DecryptedDecision::Awaiting;
		let result = self
			.raw
			.mutate(database, key, |current| {
				let input = match current.live_value() {
					Some(value) => LiveOrInitializedInput::Existing(
						encryption
							.decrypt(value)
							.map_err(EncryptedRawMutationDecisionError::Decryption)?,
					),
					None => {
						let (value, ttl) = initialize_value_fn(current.observed_at())
							.map_err(EncryptedRawMutationDecisionError::Callback)?;
						LiveOrInitializedInput::Initialized {
							value: SecretBytes::from_vec(value),
							ttl,
						}
					}
				};
				let mutation = decide_mutation_fn(DecryptedRawLiveMutationCurrent::new(
					input.value(),
					current.observed_at(),
				))
				.map_err(EncryptedRawMutationDecisionError::Callback)?;
				let mutation = encode_mutation(&encryption, mutation, input.initialized_value())?;
				decision = DecryptedDecision::Prepared {
					value: input,
					observed_at: current.observed_at(),
				};
				Ok(mutation)
			})
			.await
			.map_err(|error| self.mutation_failure(key, EncryptedRawMutationCause::Raw(error)))?;
		let (input, observed_at) = decision.into_prepared();
		let value_seen = match input {
			LiveOrInitializedInput::Existing(value) => RawLiveOrInitValue::Existing { value },
			LiveOrInitializedInput::Initialized { value, .. } => {
				RawLiveOrInitValue::Initialized { value }
			}
		};
		Ok(DecryptedRawLiveOrInitMutationResult::new(
			value_seen,
			observed_at,
			result.outcome,
		))
	}

	fn mutation_failure<C>(
		&self,
		key: &RecordKey,
		source: EncryptedRawMutationCause<C>,
	) -> PgEncryptedRawMutationError<C> {
		PgEncryptedRawMutationError::new(PgEncryptedRawMutationErrorFields {
			raw: self.raw,
			key,
			source,
		})
	}
}

enum DecryptedDecision<Value> {
	Awaiting,
	Prepared {
		value: Value,
		observed_at: StateTimestampMicros,
	},
}

impl<Value> DecryptedDecision<Value> {
	fn into_prepared(self) -> (Value, StateTimestampMicros) {
		match self {
			Self::Prepared { value, observed_at } => (value, observed_at),
			Self::Awaiting => unreachable!("a successful Raw mutation ran its decision once"),
		}
	}
}

enum LiveOrInitializedInput {
	Existing(SecretBytes),
	Initialized { value: SecretBytes, ttl: Ttl },
}

impl LiveOrInitializedInput {
	fn value(&self) -> &SecretBytes {
		match self {
			Self::Existing(value) | Self::Initialized { value, .. } => value,
		}
	}
	fn initialized_value(&self) -> Option<InitializedValue<'_>> {
		match self {
			Self::Existing(_) => None,
			Self::Initialized { value, ttl } => Some(InitializedValue { value, ttl: *ttl }),
		}
	}
}

struct InitializedValue<'a> {
	value: &'a SecretBytes,
	ttl: Ttl,
}
fn encode_mutation<C>(
	encryption: &PublishedItemEncryption<'_>,
	mutation: EncryptedRawMutation,
	initialized: Option<InitializedValue<'_>>,
) -> Result<AtomicMutation, EncryptedRawMutationDecisionError<C>> {
	Ok(match mutation {
		EncryptedRawMutation::KeepExisting => match initialized {
			Some(initial) => AtomicMutation::SetBytes {
				value: encryption
					.encrypt(initial.value.expose_secret())
					.map_err(EncryptedRawMutationDecisionError::Encryption)?,
				ttl: initial.ttl,
			},
			None => AtomicMutation::KeepExisting,
		},
		EncryptedRawMutation::SetBytes { value, ttl } => {
			let plaintext = SecretBytes::from_vec(value);
			AtomicMutation::SetBytes {
				value: encryption
					.encrypt(plaintext.expose_secret())
					.map_err(EncryptedRawMutationDecisionError::Encryption)?,
				ttl,
			}
		}
		EncryptedRawMutation::SetBytesPreservingExpiration { value } => {
			let plaintext = SecretBytes::from_vec(value);
			let value = encryption
				.encrypt(plaintext.expose_secret())
				.map_err(EncryptedRawMutationDecisionError::Encryption)?;
			match initialized {
				Some(initial) => AtomicMutation::SetBytes {
					value,
					ttl: initial.ttl,
				},
				None => AtomicMutation::SetBytesPreservingExpiration { value },
			}
		}
		EncryptedRawMutation::Delete => AtomicMutation::Delete,
	})
}
