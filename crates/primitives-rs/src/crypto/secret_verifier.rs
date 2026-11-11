//! Versioned, purpose-bound verification of secret bytes.
//!
//! A verifier is `0x01 || body`, where `body` is the 32-byte result of the shared
//! hedged KDF under the newest keyset key, with an empty salt, purpose
//! `primitives-rs/secret-verifier/v1`, and fields `[context, secret]`. Both fields
//! are length-framed by the KDF. Verification checks every rotation key.
//!
//! The single combined result preserves the KDF's stated hedging properties;
//! separate component results are never stored. Deterministic verifiers reveal
//! equality under the same key and context. Bind a record identity in context
//! when independent records should differ. Keep the keyset secret. Low-entropy
//! codes also need application attempt limits; this fast protocol does not
//! replace password derivation.
//! ```
//! use primitives_rs::crypto::{Key32, keysets::derive_keyset_from_latest_first_keys};
//! use primitives_rs::crypto::secret_verifier::{Input, SecretVerifier};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let keys = derive_keyset_from_latest_first_keys([Key32::generate()?], "codes.v1")?;
//! let input = Input { secret: b"12345678", context: b"ceremony:42" };
//! let stored = SecretVerifier::new(&keys, input);
//! let parsed = SecretVerifier::parse(stored.as_bytes())?;
//! assert!(parsed.verify(&keys, input)?.is_latest());
//! # Ok(())
//! # }
//! ```

use crate::crypto::{
	Key32, VerificationError, key_derivation,
	keysets::{KeyMatch, Keyset},
};

use std::fmt;
use subtle::{Choice, ConditionallySelectable, ConstantTimeEq, CtOption};

/// Size of a version byte followed by a 32-byte combined verifier.
pub const ENCODED_LEN: usize = 33;
const VERSION: u8 = 1;

/// Secret bytes and the application context that their verifier binds.
#[derive(Clone, Copy)]
pub struct Input<'a> {
	/// The secret value to authenticate.
	pub secret: &'a [u8],
	/// Application context, such as a record identity and schema version.
	pub context: &'a [u8],
}

/// A structurally parsed public verifier of a secret value.
///
/// Parsing checks the format, not authenticity. Use [`verify`](Self::verify)
/// before accepting a candidate secret.
#[derive(Clone, Copy)]
pub struct SecretVerifier([u8; ENCODED_LEN]);

impl SecretVerifier {
	/// Creates a verifier under the newest purpose-bound key.
	pub fn new(keyset: &Keyset, input: Input<'_>) -> Self {
		Self::with_key(keyset.latest_key(), input)
	}

	/// Returns a lazy iterator that derives one lookup candidate per retained key, newest first.
	///
	/// The first candidate equals [`new`](Self::new). The remaining candidates
	/// permit indexed lookup of verifiers stored before key rotation. Applications
	/// can collect the candidates for one database query instead of querying each
	/// candidate separately. Removing a key also removes its lookup candidate.
	/// Use `new` when storing a new verifier; use these candidates to find existing
	/// records. Each candidate uses the existing verifier format and input context.
	///
	/// ```
	/// use primitives_rs::crypto::{Key32, keysets::derive_keyset_from_latest_first_keys};
	/// use primitives_rs::crypto::secret_verifier::{Input, SecretVerifier};
	/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
	/// let old_key = Key32::generate()?;
	/// let old_keys = derive_keyset_from_latest_first_keys(
	///     [Key32::from_array(*old_key.expose_secret())], "app.lookup.v1")?;
	/// let input = Input { secret: b"credential bytes", context: b"account lookup" };
	/// let stored = SecretVerifier::new(&old_keys, input);
	/// let rotated = derive_keyset_from_latest_first_keys(
	///     [Key32::generate()?, old_key], "app.lookup.v1")?;
	/// let candidates: Vec<_> = SecretVerifier::lookup_candidates(&rotated, input).collect();
	/// assert_eq!(candidates[0], SecretVerifier::new(&rotated, input));
	/// assert_eq!(candidates[1], stored);
	/// # Ok(())
	/// # }
	/// ```
	pub fn lookup_candidates<'a>(
		keyset: &'a Keyset,
		input: Input<'a>,
	) -> impl ExactSizeIterator<Item = Self> + 'a {
		keyset
			.latest_first_keys()
			.iter()
			.map(move |key| Self::with_key(key, input))
	}

	/// Parses the exact versioned byte representation without allocating.
	pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
		let bytes: [u8; ENCODED_LEN] = bytes.try_into().map_err(|_| ParseError::InvalidLength {
			actual: bytes.len(),
		})?;
		if bytes[0] != VERSION {
			return Err(ParseError::UnsupportedVersion { version: bytes[0] });
		}
		Ok(Self(bytes))
	}

	/// Returns the version byte and combined verifier body.
	pub fn as_bytes(&self) -> &[u8; ENCODED_LEN] {
		&self.0
	}

	/// Checks every rotation key and reports the first matching key's position.
	pub fn verify(&self, keyset: &Keyset, input: Input<'_>) -> Result<KeyMatch, VerificationError> {
		let mut selected = CtOption::new(0_u8, Choice::from(0));
		for (index, candidate) in Self::lookup_candidates(keyset, input).enumerate() {
			let equal = candidate.0.ct_eq(&self.0);
			let replace = equal & !selected.is_some();
			selected.conditional_assign(&CtOption::new(index as u8, Choice::from(1)), replace);
		}
		Option::<u8>::from(selected)
			.map(|index| KeyMatch {
				index: usize::from(index),
			})
			.ok_or(VerificationError)
	}

	fn with_key(key: &Key32, input: Input<'_>) -> Self {
		let body = key_derivation::derive_key(
			key,
			key_derivation::Context {
				salt: b"",
				purpose: b"primitives-rs/secret-verifier/v1",
				fields: &[input.context, input.secret],
			},
		);
		let mut bytes = [0_u8; ENCODED_LEN];
		bytes[0] = VERSION;
		bytes[1..].copy_from_slice(body.expose_secret());
		Self(bytes)
	}
}
impl AsRef<[u8]> for SecretVerifier {
	fn as_ref(&self) -> &[u8] {
		self.as_bytes()
	}
}
impl TryFrom<&[u8]> for SecretVerifier {
	type Error = ParseError;
	fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
		Self::parse(bytes)
	}
}
impl fmt::Debug for SecretVerifier {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("SecretVerifier([redacted])")
	}
}
impl PartialEq for SecretVerifier {
	fn eq(&self, other: &Self) -> bool {
		self.0.ct_eq(&other.0).into()
	}
}
impl Eq for SecretVerifier {}

/// A stored verifier did not have a supported byte representation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParseError {
	/// The byte count was not [`ENCODED_LEN`].
	InvalidLength {
		/// The received byte count.
		actual: usize,
	},
	/// The format version was unsupported.
	UnsupportedVersion {
		/// The received version byte.
		version: u8,
	},
}
impl fmt::Display for ParseError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::InvalidLength { actual } => {
				write!(f, "secret verifier size {actual}, expected {ENCODED_LEN}")
			}
			Self::UnsupportedVersion { version } => {
				write!(f, "unsupported secret verifier version {version}")
			}
		}
	}
}
impl std::error::Error for ParseError {}

#[cfg(feature = "serde")]
mod serde_impl {
	use super::*;
	use serde::{
		Deserialize, Deserializer, Serialize, Serializer,
		de::{self, SeqAccess, Visitor},
	};
	impl Serialize for SecretVerifier {
		fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
			serializer.serialize_bytes(self.as_bytes())
		}
	}
	impl<'de> Deserialize<'de> for SecretVerifier {
		fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
			struct VerifierVisitor;
			impl<'de> Visitor<'de> for VerifierVisitor {
				type Value = SecretVerifier;
				fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
					f.write_str("a 33-byte versioned secret verifier")
				}
				fn visit_bytes<E: de::Error>(self, bytes: &[u8]) -> Result<Self::Value, E> {
					SecretVerifier::parse(bytes).map_err(E::custom)
				}
				fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
					let mut bytes = [0_u8; ENCODED_LEN];
					for (index, byte) in bytes.iter_mut().enumerate() {
						*byte = seq
							.next_element()?
							.ok_or_else(|| de::Error::invalid_length(index, &self))?;
					}
					if seq.next_element::<de::IgnoredAny>()?.is_some() {
						return Err(de::Error::invalid_length(ENCODED_LEN + 1, &self));
					}
					self.visit_bytes(&bytes)
				}
			}
			deserializer.deserialize_bytes(VerifierVisitor)
		}
	}
}
