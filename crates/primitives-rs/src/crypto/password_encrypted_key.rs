//! Password-protected 32-byte keys through the shared ENVL encryption protocol.
//!
//! Encryption generates a fresh public salt, stretches the password once, and
//! encrypts an existing key. Decryption checks the record's structure and caller's
//! resource limits before stretching the password. A wrong password, wrong
//! additional secret, wrong application context, or altered authenticatable
//! content produces an authentication failure. Parsing alone does not
//! authenticate a record.
//!
//! Applications own persistent replacement: decrypt a record, then encrypt the same key
//! with a new password or profile and save the replacement atomically. This keeps
//! the protected key stable without re-encrypting application data. An optional
//! additional secret is never stored here; losing that secret prevents decryption.
//!
//! # Version 2 protocol
//!
//! The record contains `PSKY || 0x02 || additional_secret_present:u8 ||
//! password_parameters:49 || envelope`. The flag is exactly zero or one. The
//! envelope contains exactly 32 plaintext bytes. All lengths and versions are checked before derivation.
//!
//! The root comes from [`super::password_derivation::Parameters::derive_key`]. Build a
//! one-key keyset with purpose `primitives-rs.password-sealed-key.v2`. The
//! envelope's associated data is `hash::hash_fields(Fields { purpose:
//! "primitives-rs/password-sealed-key/v2", fields: [complete_header, context] })`.
//! This binds every header byte and the external application context. The
//! envelope supplies both AEAD layers, a hedged key commitment, random padding,
//! and nonce generation; this module adds no encryption algorithm.
//!
//! ```
//! use primitives_rs::crypto::{Key32, algorithms::argon2id, password_derivation};
//! use primitives_rs::crypto::password_encrypted_key::{PasswordEncryptedKey, PasswordEncryptInput, PasswordDecryptInput};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let profile = argon2id::Profile::new(argon2id::Costs {
//!     memory_kib: 65536, iterations: 3, lanes: 4,
//! })?;
//! let limits = password_derivation::ResourceLimits {
//!     max_memory_kib: 65536, max_iterations: 3, max_lanes: 4, max_password_len: 1024,
//! };
//! let password = password_derivation::Input {
//!     password: b"example password", additional_secret: None,
//! };
//! let master = Key32::generate()?;
//! let record = PasswordEncryptedKey::encrypt(&master, PasswordEncryptInput {
//!     password, profile, limits, context: b"example/account:42/master:v1",
//! })?;
//! let restored = PasswordEncryptedKey::parse(record.as_bytes())?.decrypt(PasswordDecryptInput {
//!     password, limits, context: b"example/account:42/master:v1",
//! })?;
//! assert_eq!(master.expose_secret(), restored.expose_secret());
//! # Ok(())
//! # }
//! ```

use crate::crypto::{
	Key32,
	password_derivation::Parameters,
	password_record::{self, RecordFormat},
};

pub use super::password_encryption::{Error, PasswordDecryptInput, PasswordEncryptInput};

const FORMAT: RecordFormat = RecordFormat {
	prefix: b"PSKY\x02",
	key_purpose: "primitives-rs.password-sealed-key.v2",
	context_purpose: b"primitives-rs/password-sealed-key/v2",
};
/// The exact byte length of a version 2 record.
pub const RECORD_LEN: usize = password_record::HEADER_LEN
	+ match super::encryption::envelope_len_for_plaintext_len(32) {
		Some(len) => len,
		None => panic!("a 32-byte key fits the envelope"),
	};

/// A structurally parsed record. No password-derived key is retained.
#[derive(Clone, Eq, PartialEq)]
pub struct PasswordEncryptedKey([u8; RECORD_LEN]);

impl PasswordEncryptedKey {
	/// Encrypts a key with fresh salt and the shared envelope.
	pub fn encrypt(key: &Key32, input: PasswordEncryptInput<'_>) -> Result<Self, Error> {
		let record = password_record::encrypt(key.expose_secret(), input, FORMAT)?;
		Ok(Self(
			record
				.try_into()
				.expect("a 32-byte key has the fixed record length"),
		))
	}

	/// Checks an exact record encoding without allocating or running Argon2id.
	pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
		if bytes.len() != RECORD_LEN {
			return Err(Error::InvalidRecord);
		}
		password_record::validate(bytes, FORMAT)?;
		Ok(Self(bytes.try_into().expect("record length checked")))
	}

	/// Returns the unauthenticated parameters without performing derivation.
	pub fn parameters(&self) -> Parameters {
		password_record::parameters(&self.0)
	}

	/// Reports the unauthenticated record's additional-secret requirement.
	pub fn requires_additional_secret(&self) -> bool {
		self.0[5] == 1
	}

	/// Returns the public record bytes for storage.
	pub fn as_bytes(&self) -> &[u8; RECORD_LEN] {
		&self.0
	}

	/// Authenticates and decrypts a key after enforcing the resource policy.
	pub fn decrypt(&self, input: PasswordDecryptInput<'_>) -> Result<Key32, Error> {
		let plaintext = password_record::decrypt(&self.0, input, FORMAT)?;
		if plaintext.len() != crate::crypto::KEY32_LEN {
			return Err(Error::AuthenticationFailed);
		}
		let mut key = Key32::zeroed();
		key.as_mut_bytes()
			.copy_from_slice(plaintext.expose_secret());
		Ok(key)
	}
}

impl std::fmt::Debug for PasswordEncryptedKey {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("PasswordEncryptedKey")
			.field("record_len", &RECORD_LEN)
			.finish()
	}
}
impl TryFrom<&[u8]> for PasswordEncryptedKey {
	type Error = Error;
	fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
		Self::parse(bytes)
	}
}
impl AsRef<[u8]> for PasswordEncryptedKey {
	fn as_ref(&self) -> &[u8] {
		self.as_bytes()
	}
}
