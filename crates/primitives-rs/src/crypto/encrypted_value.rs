//! Typed stored values over the shared ENVL encryption protocol.
//!
//! `EncryptedValue<T>` owns a structurally parsed envelope. Parsing does not
//! authenticate it. `T` is a compile-time marker, not an authenticated schema
//! identifier; bind the schema and record identity through a keyset purpose and
//! associated data. Rotation information identifies values that an application
//! can rewrite under its newest key after successful decryption.
//!
//! The `crypto-json` feature adds explicit JSON serialization. JSON payloads are
//! ordinary Serde JSON bytes inside the same envelope. Byte payloads can use any
//! application-selected encoding; no serialization format is inferred.
//! The [`envelope_len_for_plaintext_len`](super::encryption::envelope_len_for_plaintext_len)
//! and [`max_plaintext_len_for_envelope_limit`](super::encryption::max_plaintext_len_for_envelope_limit)
//! functions apply to the byte envelope. For JSON values, the plaintext length is
//! the serialized JSON length, not the Rust value's in-memory size.
//!
//! ```
//! use primitives_rs::crypto::{Key32, encryption::EncryptInput, keysets::derive_keyset_from_latest_first_keys, encrypted_value::EncryptedValue};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let keys = derive_keyset_from_latest_first_keys([Key32::generate()?], "records.v1")?;
//! let encrypted = EncryptedValue::encrypt_bytes(&keys, EncryptInput {
//!     plaintext: b"value", associated_data: b"record:42",
//! })?;
//! let stored = encrypted.into_bytes();
//! let parsed: EncryptedValue = EncryptedValue::try_from(stored)?;
//! let decrypted = parsed.decrypt_bytes(&keys, b"record:42")?;
//! assert!(decrypted.key_match.is_latest());
//! assert_eq!(decrypted.value.expose_secret(), b"value");
//! # Ok(())
//! # }
//! ```
//! With `crypto-json`, a typed JSON value uses the same envelope:
//!
//! ```
//! # #[cfg(feature = "crypto-json")]
//! # fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use primitives_rs::crypto::{Key32, keysets::derive_keyset_from_latest_first_keys};
//! use primitives_rs::crypto::encrypted_value::{EncryptedValue, JsonEncryptInput};
//! use serde::{Deserialize, Serialize};
//! #[derive(Serialize, Deserialize)]
//! struct Settings { theme: String }
//! let keys = derive_keyset_from_latest_first_keys([Key32::generate()?], "settings.v1")?;
//! let value = Settings { theme: "dark".to_owned() };
//! let encrypted = EncryptedValue::encrypt_json(&keys, JsonEncryptInput {
//!     value: &value, associated_data: b"account:42/schema:1",
//! })?;
//! let decoded = encrypted.decrypt_json(&keys, b"account:42/schema:1")?;
//! assert_eq!(decoded.value.theme, "dark");
//! # Ok(())
//! # }
//! ```

use crate::{
	crypto::{
		encryption as envelope,
		encryption::{DecryptError, DecryptInput, EncryptError, EncryptInput},
		keysets::{KeyMatch, Keyset},
	},
	secret_bytes::{SecretBytes, allocate_byte_buffer},
};

use std::{fmt, marker::PhantomData};

#[cfg(feature = "crypto-json")]
mod json;
#[cfg(feature = "crypto-json")]
pub use json::{DecryptJsonError, EncryptJsonError, JsonEncryptInput};

/// A marker for byte payloads without a Rust value type.
pub enum OpaqueBytes {}

/// An owned, structurally parsed envelope with a compile-time payload marker.
///
/// With the `serde` feature, this type serializes as bytes. Deserialization
/// bounds the envelope size and checks its structure without authenticating it.
pub struct EncryptedValue<T = OpaqueBytes> {
	bytes: Vec<u8>,
	marker: PhantomData<fn() -> T>,
}

/// A decrypted value and the rotation key that authenticated it.
pub struct DecryptedValue<T> {
	/// The plaintext. The caller owns any values that a decoder allocates.
	pub value: T,
	/// The matching key's position in the supplied latest-first keyset.
	pub key_match: KeyMatch,
}
impl<T> fmt::Debug for DecryptedValue<T> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("DecryptedValue")
			.field("value", &"[redacted]")
			.field("key_match", &self.key_match)
			.finish()
	}
}

impl EncryptedValue {
	/// Encrypts explicit plaintext bytes without applying a serialization format.
	pub fn encrypt_bytes(keyset: &Keyset, input: EncryptInput<'_>) -> Result<Self, EncryptError> {
		envelope::encrypt(keyset, input).map(Self::from_encrypted_bytes)
	}
}

impl<T> EncryptedValue<T> {
	/// Parses the structure before copying an envelope into owned storage.
	pub fn parse(bytes: &[u8]) -> Result<Self, DecryptError> {
		envelope::validate_envelope_structure(bytes)?;
		let mut owned = allocate_byte_buffer(bytes.len())?;
		owned.extend_from_slice(bytes);
		Ok(Self::from_encrypted_bytes(owned))
	}

	/// Returns the exact envelope bytes.
	pub fn as_bytes(&self) -> &[u8] {
		&self.bytes
	}

	/// Transfers the envelope storage without copying it.
	pub fn into_bytes(self) -> Vec<u8> {
		self.bytes
	}

	/// Authenticates and decrypts the exact payload bytes without decoding them.
	pub fn decrypt_bytes(
		&self,
		keyset: &Keyset,
		associated_data: &[u8],
	) -> Result<DecryptedValue<SecretBytes>, DecryptError> {
		let (value, key_match) = envelope::decrypt_with_key_match(
			keyset,
			DecryptInput {
				envelope: &self.bytes,
				associated_data,
			},
		)?;
		Ok(DecryptedValue { value, key_match })
	}

	fn from_encrypted_bytes(bytes: Vec<u8>) -> Self {
		Self {
			bytes,
			marker: PhantomData,
		}
	}
}
impl<T> TryFrom<Vec<u8>> for EncryptedValue<T> {
	type Error = DecryptError;
	fn try_from(bytes: Vec<u8>) -> Result<Self, Self::Error> {
		envelope::validate_envelope_structure(&bytes)?;
		Ok(Self::from_encrypted_bytes(bytes))
	}
}
impl<T> TryFrom<&[u8]> for EncryptedValue<T> {
	type Error = DecryptError;
	fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
		Self::parse(bytes)
	}
}
impl<T> AsRef<[u8]> for EncryptedValue<T> {
	fn as_ref(&self) -> &[u8] {
		self.as_bytes()
	}
}
impl<T> Clone for EncryptedValue<T> {
	fn clone(&self) -> Self {
		Self::from_encrypted_bytes(self.bytes.clone())
	}
}
impl<T> PartialEq for EncryptedValue<T> {
	fn eq(&self, other: &Self) -> bool {
		self.bytes == other.bytes
	}
}
impl<T> Eq for EncryptedValue<T> {}
impl<T> fmt::Debug for EncryptedValue<T> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("EncryptedValue")
			.field("envelope_len", &self.bytes.len())
			.finish()
	}
}

#[cfg(feature = "serde")]
mod serde_impl {
	use super::*;
	use serde::{
		Deserialize, Deserializer, Serialize, Serializer,
		de::{self, SeqAccess, Visitor},
	};
	impl<T> Serialize for EncryptedValue<T> {
		fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
			serializer.serialize_bytes(self.as_bytes())
		}
	}
	impl<'de, T> Deserialize<'de> for EncryptedValue<T> {
		fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
			struct EnvelopeVisitor<T>(PhantomData<fn() -> T>);
			impl<'de, T> Visitor<'de> for EnvelopeVisitor<T> {
				type Value = EncryptedValue<T>;
				fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
					f.write_str("a supported encrypted envelope")
				}
				fn visit_bytes<E: de::Error>(self, bytes: &[u8]) -> Result<Self::Value, E> {
					EncryptedValue::parse(bytes).map_err(E::custom)
				}
				fn visit_byte_buf<E: de::Error>(self, bytes: Vec<u8>) -> Result<Self::Value, E> {
					EncryptedValue::try_from(bytes).map_err(E::custom)
				}
				fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
					if seq
						.size_hint()
						.is_some_and(|len| len > envelope::MAX_ENVELOPE_LEN)
					{
						return Err(de::Error::custom(
							"encrypted envelope exceeds the size limit",
						));
					}
					let mut bytes = Vec::new();
					while let Some(byte) = seq.next_element::<u8>()? {
						if bytes.len() == envelope::MAX_ENVELOPE_LEN {
							return Err(de::Error::custom(
								"encrypted envelope exceeds the size limit",
							));
						}
						bytes.try_reserve(1).map_err(|_| {
							de::Error::custom("encrypted envelope allocation failed")
						})?;
						bytes.push(byte);
					}
					self.visit_byte_buf(bytes)
				}
			}
			deserializer.deserialize_bytes(EnvelopeVisitor(PhantomData))
		}
	}
}
