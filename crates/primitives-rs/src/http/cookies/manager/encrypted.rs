use super::*;
use crate::{
	crypto::{self, encrypted_value::EncryptedValue, encryption::EncryptInput, keysets::Keyset},
	encoding::binary_to_text::{ByteEncoding, DecodeOptions},
	secret_bytes::SecretBytes,
};
use serde::{Serialize, de::DeserializeOwned};
use std::{fmt, marker::PhantomData, sync::Arc};

const ENCRYPTED_PURPOSE: &str = "primitives-rs.cookies.encrypted.v1";

/// An encrypted-cookie failure. Messages never contain payloads or key material.
#[derive(Debug)]
pub enum EncryptedCookieError {
	/// Cookie syntax, scope, or size validation failed.
	Cookie(CookieError),
	/// The application's keyset source could not supply keys.
	KeysetUnavailable,
	/// A keyset could not derive the cookie protocol's child purpose.
	Keyset(crypto::keysets::KeysetError),
	/// The serialized payload exceeded the bounded temporary buffer.
	PayloadTooLarge,
	/// JSON serialization failed. The underlying message is deliberately omitted.
	Serialization,
	/// JSON decoding or application type validation failed.
	Deserialization,
	/// A temporary buffer could not be allocated.
	AllocationFailed,
	/// The encrypted value has an invalid encoding, version, or authentication tag.
	InvalidEncryptedValue,
	/// The shared encryption protocol could not create an envelope.
	Encryption(crypto::encryption::EncryptError),
}
impl fmt::Display for EncryptedCookieError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Cookie(error) => error.fmt(f),
			Self::Keyset(error) => error.fmt(f),
			Self::Encryption(error) => error.fmt(f),
			Self::KeysetUnavailable => f.write_str("cookie keyset is unavailable"),
			Self::PayloadTooLarge => f.write_str("cookie JSON exceeds the size limit"),
			Self::Serialization => f.write_str("cookie JSON serialization failed"),
			Self::Deserialization => f.write_str("cookie JSON decoding failed"),
			Self::AllocationFailed => f.write_str("cookie buffer allocation failed"),
			Self::InvalidEncryptedValue => f.write_str("invalid encrypted cookie value"),
		}
	}
}
impl std::error::Error for EncryptedCookieError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Cookie(error) => Some(error),
			Self::Keyset(error) => Some(error),
			Self::Encryption(error) => Some(error),
			_ => None,
		}
	}
}
impl From<CookieError> for EncryptedCookieError {
	fn from(error: CookieError) -> Self {
		Self::Cookie(error)
	}
}

/// Live keyset access shared by encrypted cookies. Each operation reads it once.
/// Supply an application-purpose keyset; each protocol derives its own child.
#[derive(Clone)]
pub struct CookieKeysetSource {
	get_keyset_fn: Arc<dyn Fn() -> Result<Arc<Keyset>, EncryptedCookieError> + Send + Sync>,
}
impl CookieKeysetSource {
	/// Uses a live source so rotations do not require rebuilding cookie definitions.
	pub fn new(
		get_keyset_fn: impl Fn() -> Result<Arc<Keyset>, EncryptedCookieError> + Send + Sync + 'static,
	) -> Self {
		Self {
			get_keyset_fn: Arc::new(get_keyset_fn),
		}
	}
	/// Uses one fixed keyset for this source's lifetime.
	pub fn from_keyset(keyset: Keyset) -> Self {
		let keyset = Arc::new(keyset);
		Self::new(move || Ok(keyset.clone()))
	}
	fn derive_child_keyset(&self, purpose: &str) -> Result<Keyset, EncryptedCookieError> {
		(self.get_keyset_fn)()?
			.derive_child_keyset(purpose)
			.map_err(EncryptedCookieError::Keyset)
	}
}
impl fmt::Debug for CookieKeysetSource {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("CookieKeysetSource([callback])")
	}
}

/// Typed JSON cookies protected by the shared hedged encryption envelope.
///
/// Values are canonical unpadded Base64URL envelopes. Associated data contains
/// length-prefixed protocol context, final name, path, normalized domain, and
/// host-only/partitioned scope flags. It does not contain the request host, so
/// a host-only value decrypts on every host that uses the same keyset and
/// defines the same cookie; see the [module documentation](super). The source
/// derives the fixed encrypted-cookie child purpose. Changing those identity
/// attributes makes old values invalid; lifetime, HttpOnly, and SameSite
/// changes do not invalidate values. Browser Max-Age does not enforce
/// application-level payload expiration on the server.
pub struct EncryptedCookieDefinition<T> {
	definition: CookieDefinition,
	keys: CookieKeysetSource,
	associated_data: Vec<u8>,
	value: PhantomData<fn() -> T>,
}
impl<T> EncryptedCookieDefinition<T> {
	/// Combines a validated definition with shared live key access.
	pub(super) fn new(definition: CookieDefinition, keys: CookieKeysetSource) -> Self {
		let associated_data = definition.associated_data();
		Self {
			definition,
			keys,
			associated_data,
			value: PhantomData,
		}
	}
	/// Returns the shared creation/deletion definition.
	pub fn definition(&self) -> &CookieDefinition {
		&self.definition
	}
}
impl<T: Serialize> EncryptedCookieDefinition<T> {
	/// Serializes bounded JSON and emits an encrypted cookie under the newest key.
	///
	/// The returned cookie's `Debug` output includes its encrypted value, which
	/// can be a bearer credential. Do not log it.
	pub fn create_cookie(&self, value: &T) -> Result<Cookie<'static>, EncryptedCookieError> {
		let bytes = serialize_json(value)?;
		let keyset = self.keys.derive_child_keyset(ENCRYPTED_PURPOSE)?;
		let envelope = EncryptedValue::encrypt_bytes(
			&keyset,
			EncryptInput {
				plaintext: bytes.expose_secret(),
				associated_data: &self.associated_data,
			},
		)
		.map_err(EncryptedCookieError::Encryption)?;
		Ok(self
			.definition
			.create_cookie(ByteEncoding::Base64UrlUnpadded.encode(envelope.as_bytes()))?)
	}
	/// Checks, without reading keys or encrypting, that `create_cookie` can emit
	/// `value` within this definition's size bound.
	#[cfg(feature = "csrf")]
	pub(crate) fn check_capacity(&self, value: &T) -> Result<(), EncryptedCookieError> {
		let plaintext_len = serialize_json(value)?.expose_secret().len();
		let envelope_len = crypto::encryption::envelope_len_for_plaintext_len(plaintext_len)
			.ok_or(EncryptedCookieError::PayloadTooLarge)?;
		self.definition
			.create_cookie(ByteEncoding::Base64UrlUnpadded.encode(&vec![0; envelope_len]))?;
		Ok(())
	}
}
impl<T: DeserializeOwned> EncryptedCookieDefinition<T> {
	/// Authenticates and decodes a raw cookie value with any retained rotation key.
	pub fn decode_value(&self, value: &str) -> Result<T, EncryptedCookieError> {
		let bytes = decode_envelope_bytes(value)?;
		let envelope: EncryptedValue = EncryptedValue::parse(bytes.expose_secret())
			.map_err(|_| EncryptedCookieError::InvalidEncryptedValue)?;
		let keyset = self.keys.derive_child_keyset(ENCRYPTED_PURPOSE)?;
		let plaintext = envelope
			.decrypt_bytes(&keyset, &self.associated_data)
			.map_err(|_| EncryptedCookieError::InvalidEncryptedValue)?;
		serde_json::from_slice(plaintext.value.expose_secret())
			.map_err(|_| EncryptedCookieError::Deserialization)
	}
	/// Reads and decodes a unique cookie. None means absence, not invalid input.
	pub fn read_value(&self, headers: &HeaderMap) -> Result<Option<T>, EncryptedCookieError> {
		self.definition
			.read_value(headers)?
			.map(|value| self.decode_value(value))
			.transpose()
	}
}
impl<T> Clone for EncryptedCookieDefinition<T> {
	fn clone(&self) -> Self {
		Self::new(self.definition.clone(), self.keys.clone())
	}
}
impl<T> fmt::Debug for EncryptedCookieDefinition<T> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("EncryptedCookieDefinition")
			.field("definition", &self.definition)
			.finish()
	}
}

fn serialize_json<T: Serialize>(value: &T) -> Result<SecretBytes, EncryptedCookieError> {
	use crate::serialization::json::{self, JsonError, JsonFormat, JsonLimits, SerializeOptions};
	json::serialize_to_secret_bytes(
		value,
		SerializeOptions {
			limits: JsonLimits {
				max_size: MAX_SET_COOKIE_SIZE,
				max_depth: usize::MAX,
			},
			format: JsonFormat::Compact,
		},
	)
	.map_err(|error| match error {
		JsonError::TooLarge { .. } => EncryptedCookieError::PayloadTooLarge,
		JsonError::AllocationFailed => EncryptedCookieError::AllocationFailed,
		_ => EncryptedCookieError::Serialization,
	})
}
fn decode_envelope_bytes(value: &str) -> Result<SecretBytes, EncryptedCookieError> {
	if value.len() > MAX_SET_COOKIE_SIZE {
		return Err(CookieError::ValueTooLarge.into());
	}
	let bytes = ByteEncoding::Base64UrlUnpadded
		.decode(
			value,
			DecodeOptions {
				max_decoded_size: MAX_SET_COOKIE_SIZE,
			},
		)
		.map_err(|_| EncryptedCookieError::InvalidEncryptedValue)?;
	Ok(SecretBytes::from_vec(bytes))
}
