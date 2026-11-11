//! Password-encrypted byte payloads through the shared ENVL protocol.
//!
//! This optional composition stores the public password parameters beside an
//! authenticated encrypted payload. Ordinary content encryption accepts a key
//! independently of this record. Applications can recover stable account secrets
//! here and use those secrets without repeating password derivation.
//!
//! The version 2 record is `PSVL || 0x02 || additional_secret_present:u8 ||
//! password_parameters:49 || envelope`. The shared password derivation stretches
//! the password once. The keyset purpose is
//! `primitives-rs.password-encrypted-value.v2`. Associated data is the shared
//! hedged hash of the complete header and application context under
//! `primitives-rs/password-encrypted-value/v2`. Every header byte is authenticated.
//! Structural parsing and explicit resource checks precede password derivation.
//!
//! Password replacement decrypts and reencrypts this payload with fresh parameters;
//! the application atomically replaces the stored record. This facility does not
//! define account schemas, unlock lifetimes, storage, or recovery policy.

//! ```no_run
//! use primitives_rs::crypto::{algorithms::argon2id, password_derivation};
//! use primitives_rs::crypto::password_encryption::{
//!     PasswordEncryptedValue, PasswordEncryptInput, PasswordDecryptInput,
//! };
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let limits = password_derivation::ResourceLimits {
//!     max_memory_kib: 262_144, max_iterations: 8, max_lanes: 4, max_password_len: 1024,
//! };
//! // Applications choose costs for their supported devices.
//! let profile = argon2id::Profile::new(argon2id::Costs {
//!     memory_kib: 65_536, iterations: 3, lanes: 1,
//! })?;
//! let password = password_derivation::Input {
//!     password: b"example password", additional_secret: None,
//! };
//! let record = PasswordEncryptedValue::encrypt(b"recovery payload", PasswordEncryptInput {
//!     password, profile, limits, context: b"example/recovery/v1",
//! })?;
//! let recovered = record.decrypt(PasswordDecryptInput {
//!     password, limits, context: b"example/recovery/v1",
//! })?;
//! assert_eq!(recovered.expose_secret(), b"recovery payload");
//! # Ok(())
//! # }
//! ```

use crate::{
	crypto::{algorithms::argon2id, password_derivation},
	secret_bytes::{SecretBytes, allocate_byte_buffer},
};

use super::password_record::{self, RecordFormat};

const FORMAT: RecordFormat = RecordFormat {
	prefix: b"PSVL\x02",
	key_purpose: "primitives-rs.password-encrypted-value.v2",
	context_purpose: b"primitives-rs/password-encrypted-value/v2",
};

/// The largest supported plaintext payload, in bytes.
pub const MAX_PLAINTEXT_LEN: usize = super::encryption::MAX_PLAINTEXT_LEN;
/// The largest serialized password-encrypted record, in bytes.
pub const MAX_RECORD_LEN: usize = password_record::HEADER_LEN + super::encryption::MAX_ENVELOPE_LEN;

/// Password, resource policy, and authenticated context for encryption.
pub struct PasswordEncryptInput<'a> {
	/// The password and an optional separately held secret.
	pub password: password_derivation::Input<'a>,
	/// Explicit password-stretching costs for the target devices.
	pub profile: argon2id::Profile,
	/// Maximum accepted derivation costs and password length.
	pub limits: password_derivation::ResourceLimits,
	/// Application purpose and identity, authenticated with the complete header.
	pub context: &'a [u8],
}

/// Password, independently chosen resource policy, and context for decryption.
pub struct PasswordDecryptInput<'a> {
	/// The password and optional secret that encrypted the record.
	pub password: password_derivation::Input<'a>,
	/// Resource ceilings independent of the untrusted stored parameters.
	pub limits: password_derivation::ResourceLimits,
	/// The exact context that encrypted the record.
	pub context: &'a [u8],
}

/// A structurally parsed password-encrypted value. Parsing does not authenticate it.
#[derive(Clone, Eq, PartialEq)]
pub struct PasswordEncryptedValue(Vec<u8>);

impl PasswordEncryptedValue {
	/// Encrypts bounded bytes with a fresh salt and the shared encrypted envelope.
	pub fn encrypt(plaintext: &[u8], input: PasswordEncryptInput<'_>) -> Result<Self, Error> {
		password_record::encrypt(plaintext, input, FORMAT).map(Self)
	}

	/// Checks the structure and size before copying the encrypted record.
	pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
		password_record::validate(bytes, FORMAT)?;
		let mut owned = allocate_byte_buffer(bytes.len()).map_err(|_| Error::AllocationFailed)?;
		owned.extend_from_slice(bytes);
		Ok(Self(owned))
	}

	/// Returns the unauthenticated parameters without performing derivation.
	pub fn parameters(&self) -> password_derivation::Parameters {
		password_record::parameters(&self.0)
	}

	/// Reports the unauthenticated record's separately held secret requirement.
	pub fn requires_additional_secret(&self) -> bool {
		self.0[5] == 1
	}

	/// Authenticates and decrypts the payload into a zeroizing byte buffer.
	pub fn decrypt(&self, input: PasswordDecryptInput<'_>) -> Result<SecretBytes, Error> {
		password_record::decrypt(&self.0, input, FORMAT)
	}

	/// Returns the complete encrypted record, including its public parameters.
	pub fn as_bytes(&self) -> &[u8] {
		&self.0
	}

	/// Transfers the encrypted record without copying it.
	pub fn into_bytes(self) -> Vec<u8> {
		self.0
	}
}

impl TryFrom<Vec<u8>> for PasswordEncryptedValue {
	type Error = Error;
	fn try_from(bytes: Vec<u8>) -> Result<Self, Error> {
		password_record::validate(&bytes, FORMAT)?;
		Ok(Self(bytes))
	}
}

impl TryFrom<&[u8]> for PasswordEncryptedValue {
	type Error = Error;
	fn try_from(bytes: &[u8]) -> Result<Self, Error> {
		Self::parse(bytes)
	}
}

impl AsRef<[u8]> for PasswordEncryptedValue {
	fn as_ref(&self) -> &[u8] {
		self.as_bytes()
	}
}

impl std::fmt::Debug for PasswordEncryptedValue {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("PasswordEncryptedValue")
			.field("record_len", &self.0.len())
			.finish()
	}
}

/// A password record, resource policy, or encryption failure.
#[derive(Debug)]
pub enum Error {
	/// The record's length, version, flags, parameters, or envelope is invalid.
	InvalidRecord,
	/// The plaintext exceeds the encrypted envelope's bound.
	PlaintextTooLong,
	/// The application context exceeds the envelope's associated-data bound.
	ContextTooLong,
	/// The supplied optional secret disagrees with the record's requirement.
	AdditionalSecretMismatch,
	/// The password parameters or resource policy rejected derivation.
	Derivation(password_derivation::Error),
	/// The platform could not generate a salt.
	Random(super::RandomError),
	/// The shared encrypted envelope could not be created.
	Encryption(super::encryption::EncryptError),
	/// A record or secret buffer could not be allocated.
	AllocationFailed,
	/// The password, optional secret, context, ciphertext, or protected key length is wrong.
	AuthenticationFailed,
}

impl std::fmt::Display for Error {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::InvalidRecord => f.write_str("invalid password-encrypted record"),
			Self::PlaintextTooLong => {
				f.write_str("password-encrypted plaintext exceeds the size limit")
			}
			Self::ContextTooLong => {
				f.write_str("password-encrypted context exceeds the size limit")
			}
			Self::AdditionalSecretMismatch => {
				f.write_str("password-encrypted record additional-secret requirement mismatch")
			}
			Self::Derivation(error) => write!(f, "{error}"),
			Self::Random(error) => write!(f, "{error}"),
			Self::Encryption(error) => write!(f, "{error}"),
			Self::AllocationFailed => f.write_str("password-encrypted record allocation failed"),
			Self::AuthenticationFailed => {
				f.write_str("password-encrypted record authentication failed")
			}
		}
	}
}

impl std::error::Error for Error {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Derivation(error) => Some(error),
			Self::Random(error) => Some(error),
			Self::Encryption(error) => Some(error),
			_ => None,
		}
	}
}
