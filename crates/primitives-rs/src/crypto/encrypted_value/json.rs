use super::{DecryptedValue, EncryptedValue};
use crate::{
	crypto::{
		encryption as envelope,
		encryption::{
			DecryptError, EncryptError, EncryptInput, MAX_ASSOCIATED_DATA_LEN, MAX_PLAINTEXT_LEN,
		},
		keysets::Keyset,
	},
	serialization::json::{self, JsonError, JsonFormat, JsonLimits, SerializeOptions},
};
use serde::{Serialize, de::DeserializeOwned};
use std::fmt;

/// A JSON value and the application context that its envelope authenticates.
pub struct JsonEncryptInput<'a, T> {
	/// The value to serialize once as compact Serde JSON.
	pub value: &'a T,
	/// Bytes that decryption must supply unchanged.
	pub associated_data: &'a [u8],
}

impl<T: Serialize> EncryptedValue<T> {
	/// Serializes once as JSON and encrypts the result with the shared envelope.
	///
	/// Serialization stops when the output would exceed [`MAX_PLAINTEXT_LEN`].
	/// This operation zeroizes its owned plaintext buffers, including buffers
	/// replaced during growth. The application retains ownership of the input value. JSON follows
	/// Serde's representation; schema evolution belongs to the application.
	pub fn encrypt_json(
		keyset: &Keyset,
		input: JsonEncryptInput<'_, T>,
	) -> Result<Self, EncryptJsonError> {
		if input.associated_data.len() > MAX_ASSOCIATED_DATA_LEN {
			return Err(EncryptJsonError::Encryption(
				EncryptError::AssociatedDataTooLarge {
					actual: input.associated_data.len(),
					max: MAX_ASSOCIATED_DATA_LEN,
				},
			));
		}
		let plaintext = json::serialize_to_secret_bytes(
			input.value,
			SerializeOptions {
				limits: JsonLimits {
					max_size: MAX_PLAINTEXT_LEN,
					max_depth: usize::MAX,
				},
				format: JsonFormat::Compact,
			},
		)
		.map_err(|error| match error {
			JsonError::TooLarge { max, .. } => EncryptJsonError::PlaintextTooLarge { max },
			JsonError::AllocationFailed => EncryptJsonError::AllocationFailed,
			error => EncryptJsonError::Serialization(error.into_serde_json_error()),
		})?;
		let bytes = envelope::encrypt(
			keyset,
			EncryptInput {
				plaintext: plaintext.expose_secret(),
				associated_data: input.associated_data,
			},
		)
		.map_err(EncryptJsonError::Encryption)?;
		Ok(Self::from_encrypted_bytes(bytes))
	}
}
impl<T: DeserializeOwned> EncryptedValue<T> {
	/// Authenticates, decrypts, and decodes one complete JSON value.
	///
	/// Trailing non-whitespace bytes and Serde validation failures are rejected.
	/// The serialized plaintext buffer is zeroized; the caller owns the decoded
	/// value and any allocations that its deserializer creates. Serde JSON's
	/// ordinary recursion limit applies.
	pub fn decrypt_json(
		&self,
		keyset: &Keyset,
		associated_data: &[u8],
	) -> Result<DecryptedValue<T>, DecryptJsonError> {
		let decrypted = self
			.decrypt_bytes(keyset, associated_data)
			.map_err(DecryptJsonError::Decryption)?;
		let value = serde_json::from_slice(decrypted.value.expose_secret())
			.map_err(DecryptJsonError::Deserialization)?;
		Ok(DecryptedValue {
			value,
			key_match: decrypted.key_match,
		})
	}
}

/// JSON serialization or envelope encryption failed.
///
/// Display and Debug omit Serde error messages, which may contain plaintext.
/// Explicitly inspecting `Serialization` may expose that plaintext.
pub enum EncryptJsonError {
	/// Serialization exceeded the envelope's plaintext limit.
	PlaintextTooLarge {
		/// The maximum serialized byte count.
		max: usize,
	},
	/// A temporary plaintext buffer could not be allocated.
	AllocationFailed,
	/// The value could not be serialized. Its message may include plaintext.
	Serialization(serde_json::Error),
	/// Envelope encryption failed.
	Encryption(EncryptError),
}
impl fmt::Display for EncryptJsonError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::PlaintextTooLarge { max } => write!(f, "JSON plaintext exceeds {max} bytes"),
			Self::AllocationFailed => f.write_str("JSON plaintext allocation failed"),
			Self::Serialization(_) => f.write_str("JSON plaintext serialization failed"),
			Self::Encryption(error) => error.fmt(f),
		}
	}
}
impl fmt::Debug for EncryptJsonError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		fmt::Display::fmt(self, f)
	}
}
impl std::error::Error for EncryptJsonError {}

/// Envelope decryption or JSON decoding failed.
///
/// Display and Debug omit Serde error messages, which may contain plaintext.
/// Explicitly inspecting `Deserialization` may expose that plaintext.
pub enum DecryptJsonError {
	/// The envelope did not decrypt under the supplied keys and associated data.
	Decryption(DecryptError),
	/// JSON decoding or application validation failed. Its message may include plaintext.
	Deserialization(serde_json::Error),
}
impl fmt::Display for DecryptJsonError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Decryption(error) => error.fmt(f),
			Self::Deserialization(_) => f.write_str("JSON plaintext deserialization failed"),
		}
	}
}
impl fmt::Debug for DecryptJsonError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		fmt::Display::fmt(self, f)
	}
}
impl std::error::Error for DecryptJsonError {}
