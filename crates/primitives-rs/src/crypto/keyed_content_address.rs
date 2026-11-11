//! Stable content addresses that require a secret key to compute.
//!
//! The version 1 format is `0x01 || body`, where `body` is the 64-byte output
//! of the shared hedged KDF with empty salt, purpose
//! `primitives-rs/keyed-content-address/v1`, and fields `[context, content]`.
//! Both fields are length-framed. The KDF combines HKDF-SHA512 and KMAC256 with
//! XOR; neither component output appears separately in the address.
//!
//! Privacy requires one complete KDF branch to remain pseudorandom given the
//! other branch. Under that assumption, an observer without the addressing key
//! cannot compute addresses for guessed plaintext. The 512-bit body also avoids
//! reducing the generic classical collision bound below 256 bits. These are
//! conditional security arguments, not protection against disclosure of the
//! shared key or arbitrary correlated failures of both branches.
//!
//! Keep one high-entropy addressing key stable for the required deduplication
//! scope. Password changes and encryption-key rotation must not implicitly
//! replace it. Changing this key, the context, or the protocol version changes
//! addresses and requires an application migration. Save the key in recoverable
//! secret storage independently of ordinary content encryption.
//!
//! Equal content under the same key and context has equal addresses. This
//! reveals equality; it does not conceal sizes or access patterns. An observer
//! with access to an address-computation service can test guesses through that
//! service. Do not use an address as authorization or as proof of storage.
//! Authenticate encrypted content before accepting it, then verify its address.
//!
//! This operation borrows the complete input without copying it or allocating.
//! Bound content before calling it; chunking can provide those bounded inputs.
//! Arbitrary streaming input and content normalization are separate facilities.
//!
//! ```
//! use primitives_rs::crypto::{Key32, keyed_content_address::{Address, Input}};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let addressing_key = Key32::generate()?;
//! let input = Input { context: b"backup/chunks/v1", content: b"document bytes" };
//! let address = Address::new(&addressing_key, input);
//! let restored = Address::parse(address.as_bytes())?;
//! restored.verify(&addressing_key, input)?;
//! # Ok(())
//! # }
//! ```

use crate::crypto::{Key32, VerificationError, key_derivation};
use subtle::ConstantTimeEq;

/// Size of the version byte and complete 64-byte address body.
pub const ENCODED_LEN: usize = 65;
const VERSION: u8 = 1;

/// Exact content bytes and the application domain in which they are addressed.
#[derive(Clone, Copy)]
pub struct Input<'a> {
	/// A stable application context, such as a namespace and content-format version.
	pub context: &'a [u8],
	/// The complete content bytes; no normalization or text conversion occurs.
	pub content: &'a [u8],
}

/// A versioned, secret-keyed content address suitable for storage keys.
///
/// Parsing checks structure only. Use [`verify`](Self::verify) to check content.
/// Addresses are public identifiers, so comparison, ordering, hashing, and
/// Debug formatting are not secret operations. Verification compares in
/// constant time after computing the candidate address.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Address([u8; ENCODED_LEN]);

impl Address {
	/// Computes the version 1 address under an explicit, stable addressing key.
	pub fn new(addressing_key: &Key32, input: Input<'_>) -> Self {
		let mut bytes = [0_u8; ENCODED_LEN];
		bytes[0] = VERSION;
		key_derivation::derive_into(
			addressing_key,
			key_derivation::Context {
				salt: b"",
				purpose: b"primitives-rs/keyed-content-address/v1",
				fields: &[input.context, input.content],
			},
			&mut bytes[1..],
		)
		.expect("64 bytes fit the derivation limit");
		Self(bytes)
	}

	/// Parses the exact versioned representation without allocating.
	pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
		let bytes: [u8; ENCODED_LEN] = bytes.try_into().map_err(|_| ParseError::InvalidLength {
			actual: bytes.len(),
		})?;
		if bytes[0] != VERSION {
			return Err(ParseError::UnsupportedVersion { version: bytes[0] });
		}
		Ok(Self(bytes))
	}

	/// Returns the version byte followed by the complete combined address body.
	pub fn as_bytes(&self) -> &[u8; ENCODED_LEN] {
		&self.0
	}

	/// Checks the complete address against the supplied key, context, and content.
	pub fn verify(
		&self,
		addressing_key: &Key32,
		input: Input<'_>,
	) -> Result<(), VerificationError> {
		if self.0.ct_eq(&Self::new(addressing_key, input).0).into() {
			Ok(())
		} else {
			Err(VerificationError)
		}
	}
}

impl AsRef<[u8]> for Address {
	fn as_ref(&self) -> &[u8] {
		self.as_bytes()
	}
}
impl TryFrom<&[u8]> for Address {
	type Error = ParseError;
	fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
		Self::parse(bytes)
	}
}

/// A content address did not have a supported byte representation.
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
impl std::fmt::Display for ParseError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::InvalidLength { actual } => {
				write!(
					f,
					"keyed content address size {actual}, expected {ENCODED_LEN}"
				)
			}
			Self::UnsupportedVersion { version } => {
				write!(f, "unsupported keyed content address version {version}")
			}
		}
	}
}
impl std::error::Error for ParseError {}
