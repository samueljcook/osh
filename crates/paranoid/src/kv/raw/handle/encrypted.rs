//! Fixed published Item encryption borrows the actual Raw handle and its caller-chosen keyset.

use primitives_rs::crypto::keysets::Keyset;

use crate::db::{Database, WriteDatabase};
use crate::kv::raw::model::encryption::{
	DecryptedRawObservation, DecryptedRawValue, PublishedItemEncryption,
	PublishedItemEncryptionFields, prepare_published_item_key,
};
use crate::kv::{ConditionalRawWriteResult, RawWriteResult, Ttl};
use crate::records::RecordKey;

use super::PgRaw;
mod error;
use error::{EncryptedRawCause, PgEncryptedRawErrorFields};
pub use error::{PgEncryptedRawError, PgEncryptedRawErrorKind};
mod mutation;
pub use mutation::{PgEncryptedRawMutationError, PgEncryptedRawMutationErrorKind};

/// The inputs of an encrypted view of a Raw handle.
pub struct PgEncryptedRawFields<'keyset> {
	/// The keyset that encrypts and decrypts values. The app chooses and rotates the keyset.
	pub keyset: &'keyset Keyset,
}

/// `PgEncryptedRaw` provides an encrypted view of a [`PgRaw`] handle. Each stored value is a
/// `primitives_rs::crypto` envelope authenticated with associated data that contains the domain
/// header `paranoid.kv.raw.v1\0`, the handle's Raw domain (either the core Raw namespace or the
/// declared [`ScopeIdentity`](crate::scope::ScopeIdentity)), and the length-prefixed key.
///
/// Every key, including its final separator, must be a valid v10 key because the associated
/// data encodes the canonical key text.
///
/// Because the associated data encodes the scope identity alongside the key, if a caller or
/// attacker copies a ciphertext to another Raw scope, to another key, or between the core Raw
/// namespace and an app Raw scope, decryption fails authentication even when both handles use
/// the same keyset.
pub struct PgEncryptedRaw<'handle, 'scope, 'keyset> {
	raw: &'handle PgRaw<'scope>,
	keyset: &'keyset Keyset,
}
impl<'scope> PgRaw<'scope> {
	/// Builds an encrypted view of this handle without database work.
	pub fn encrypted<'handle, 'keyset>(
		&'handle self,
		fields: PgEncryptedRawFields<'keyset>,
	) -> PgEncryptedRaw<'handle, 'scope, 'keyset> {
		PgEncryptedRaw {
			raw: self,
			keyset: fields.keyset,
		}
	}
}
impl PgEncryptedRaw<'_, '_, '_> {
	/// Reads one record and decrypts its live value. An absent or expired record returns no
	/// value, and the result reports the database time of the read in either case.
	pub async fn get_bytes_observation(
		&self,
		database: impl Database,
		key: &RecordKey,
	) -> Result<DecryptedRawObservation, PgEncryptedRawError> {
		let parsed = prepare_published_item_key(key)
			.map_err(|error| self.failure(key, EncryptedRawCause::Input(error)))?;
		let observed = self
			.raw
			.get_bytes_observation(database, key)
			.await
			.map_err(|error| self.failure(key, EncryptedRawCause::Raw(error)))?;
		let (value, observed_at) = observed.into_parts();
		let value = value
			.map(|value| -> Result<_, PgEncryptedRawError> {
				let (envelope, metadata) = value.into_parts();
				let plaintext = PublishedItemEncryption::new(PublishedItemEncryptionFields {
					keyset: self.keyset,
					scope: self.raw.scope(),
					key: &parsed,
				})
				.decrypt(&envelope)
				.map_err(|error| self.failure(key, EncryptedRawCause::Decryption(error)))?;
				Ok(DecryptedRawValue::new(plaintext, metadata))
			})
			.transpose()?;
		Ok(DecryptedRawObservation::new(value, observed_at))
	}

	/// Encrypts `plaintext` for `key` and replaces the record's value. Paranoid checks the key
	/// and encrypts the value before any database work. A write inside the caller's transaction
	/// becomes durable only when the caller commits.
	pub async fn set_bytes(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		plaintext: &[u8],
		ttl: Ttl,
	) -> Result<RawWriteResult, PgEncryptedRawError> {
		let parsed = prepare_published_item_key(key)
			.map_err(|error| self.failure(key, EncryptedRawCause::Input(error)))?;
		let envelope = PublishedItemEncryption::new(PublishedItemEncryptionFields {
			keyset: self.keyset,
			scope: self.raw.scope(),
			key: &parsed,
		})
		.encrypt(plaintext)
		.map_err(|error| self.failure(key, EncryptedRawCause::Encryption(error)))?;
		self.raw
			.set_bytes(database, key, envelope, ttl)
			.await
			.map_err(|error| self.failure(key, EncryptedRawCause::Raw(error)))
	}

	/// Encrypts `plaintext` for `key` and writes it only when the record is absent or expired.
	/// Paranoid encrypts the value before any database work, also when a live record turns out
	/// to exist. The result for a live record does not include its value.
	pub async fn set_bytes_if_absent(
		&self,
		database: impl WriteDatabase,
		key: &RecordKey,
		plaintext: &[u8],
		ttl: Ttl,
	) -> Result<ConditionalRawWriteResult, PgEncryptedRawError> {
		let parsed = prepare_published_item_key(key)
			.map_err(|error| self.failure(key, EncryptedRawCause::Input(error)))?;
		let envelope = PublishedItemEncryption::new(PublishedItemEncryptionFields {
			keyset: self.keyset,
			scope: self.raw.scope(),
			key: &parsed,
		})
		.encrypt(plaintext)
		.map_err(|error| self.failure(key, EncryptedRawCause::Encryption(error)))?;
		self.raw
			.set_bytes_if_absent(database, key, envelope, ttl)
			.await
			.map_err(|error| self.failure(key, EncryptedRawCause::Raw(error)))
	}

	fn failure(&self, key: &RecordKey, source: EncryptedRawCause) -> PgEncryptedRawError {
		PgEncryptedRawError::new(PgEncryptedRawErrorFields {
			raw: self.raw,
			key,
			source,
		})
	}
}
impl std::fmt::Debug for PgEncryptedRaw<'_, '_, '_> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("PgEncryptedRaw").finish_non_exhaustive()
	}
}
