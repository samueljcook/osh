use std::error::Error as StdError;
use std::fmt;

use crate::crypto::{KEY32_LEN, KEY32_TEXT_LEN};

pub use crate::{random::secure_bytes::RandomError, secret_bytes::AllocationFailedError};

/// A derivation requested more output than its algorithm permits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputLengthError {
	/// The requested output size in bytes.
	pub actual: usize,
	/// The maximum output size in bytes.
	pub max: usize,
}
impl fmt::Display for OutputLengthError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(
			f,
			"crypto output size {}, maximum {}",
			self.actual, self.max
		)
	}
}
impl StdError for OutputLengthError {}

/// Message authentication failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerificationError;
impl fmt::Display for VerificationError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("message authentication failed")
	}
}
impl StdError for VerificationError {}

/// A raw authenticated-encryption operation failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AeadError {
	/// The message exceeded the algorithm's length limit.
	MsgTooLarge,
	/// The output buffer could not be allocated.
	AllocationFailed,
	/// The ciphertext, key, nonce, or associated data did not authenticate.
	AuthenticationFailed,
}
impl fmt::Display for AeadError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::MsgTooLarge => "authenticated-encryption message is too large",
			Self::AllocationFailed => "authenticated-encryption output allocation failed",
			Self::AuthenticationFailed => "ciphertext authentication failed",
		})
	}
}
impl StdError for AeadError {}

/// Failures of [`encrypt`](crate::crypto::encryption::encrypt).
#[derive(Debug)]
pub enum EncryptError {
	/// The plaintext exceeded [`MAX_PLAINTEXT_LEN`](crate::crypto::encryption::MAX_PLAINTEXT_LEN).
	PlaintextTooLarge {
		/// Actual plaintext byte length.
		actual: usize,
		/// Maximum accepted plaintext byte length.
		max: usize,
	},
	/// The associated data exceeded
	/// [`MAX_ASSOCIATED_DATA_LEN`](crate::crypto::encryption::MAX_ASSOCIATED_DATA_LEN).
	AssociatedDataTooLarge {
		/// Actual associated-data byte length.
		actual: usize,
		/// Maximum accepted associated-data byte length.
		max: usize,
	},
	/// Secure random byte generation failed.
	Random(RandomError),
	/// Output allocation failed.
	AllocationFailed,
}

impl fmt::Display for EncryptError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::PlaintextTooLarge { actual, max } => {
				write!(
					f,
					"primitives-rs crypto: plaintext size {actual}, max {max}"
				)
			}
			Self::AssociatedDataTooLarge { actual, max } => {
				write!(
					f,
					"primitives-rs crypto: associated data size {actual}, max {max}"
				)
			}
			Self::Random(err) => write!(f, "primitives-rs crypto: random bytes: {err}"),
			Self::AllocationFailed => {
				write!(f, "primitives-rs crypto: output allocation failed")
			}
		}
	}
}

impl StdError for EncryptError {
	fn source(&self) -> Option<&(dyn StdError + 'static)> {
		match self {
			Self::Random(err) => Some(err),
			Self::PlaintextTooLarge { .. }
			| Self::AssociatedDataTooLarge { .. }
			| Self::AllocationFailed => None,
		}
	}
}

impl From<RandomError> for EncryptError {
	fn from(value: RandomError) -> Self {
		Self::Random(value)
	}
}

impl From<AllocationFailedError> for EncryptError {
	fn from(_: AllocationFailedError) -> Self {
		Self::AllocationFailed
	}
}

/// Failures of [`decrypt`](crate::crypto::encryption::decrypt).
#[derive(Debug)]
pub enum DecryptError {
	/// The envelope was empty.
	EmptyEnvelope,
	/// The envelope exceeded [`MAX_ENVELOPE_LEN`](crate::crypto::encryption::MAX_ENVELOPE_LEN).
	EnvelopeTooLarge {
		/// Actual envelope byte length.
		actual: usize,
		/// Maximum accepted envelope byte length.
		max: usize,
	},
	/// The envelope was too short to contain a valid envelope-v2 payload.
	EnvelopeTooShort {
		/// Actual envelope byte length.
		actual: usize,
		/// Minimum accepted envelope byte length.
		min: usize,
	},
	/// The envelope length was not a valid envelope-v2 length bucket.
	InvalidEnvelopeLength {
		/// Actual envelope byte length.
		actual: usize,
	},
	/// The envelope did not start with the envelope-v2 magic marker.
	InvalidMagic,
	/// The envelope used an unsupported version.
	UnsupportedVersion {
		/// Version byte found in the envelope.
		version: u8,
	},
	/// The envelope used an unsupported suite.
	UnsupportedSuite {
		/// Suite byte found in the envelope.
		suite: u8,
	},
	/// The associated data exceeded
	/// [`MAX_ASSOCIATED_DATA_LEN`](crate::crypto::encryption::MAX_ASSOCIATED_DATA_LEN).
	AssociatedDataTooLarge {
		/// Actual associated-data byte length.
		actual: usize,
		/// Maximum accepted associated-data byte length.
		max: usize,
	},
	/// Output allocation failed.
	AllocationFailed,
	/// No key in the keyset opened the envelope under this associated data.
	DecryptionFailed,
}

impl fmt::Display for DecryptError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::EmptyEnvelope => write!(f, "primitives-rs crypto: encrypted envelope is empty"),
			Self::EnvelopeTooLarge { actual, max } => {
				write!(
					f,
					"primitives-rs crypto: encrypted envelope size {actual}, max {max}"
				)
			}
			Self::EnvelopeTooShort { actual, min } => {
				write!(
					f,
					"primitives-rs crypto: encrypted envelope size {actual}, min {min}"
				)
			}
			Self::InvalidEnvelopeLength { actual } => {
				write!(
					f,
					"primitives-rs crypto: invalid encrypted envelope size {actual}"
				)
			}
			Self::InvalidMagic => write!(f, "primitives-rs crypto: invalid envelope magic"),
			Self::UnsupportedVersion { version } => {
				write!(f, "primitives-rs crypto: unsupported version {version}")
			}
			Self::UnsupportedSuite { suite } => {
				write!(f, "primitives-rs crypto: unsupported suite {suite}")
			}
			Self::AssociatedDataTooLarge { actual, max } => {
				write!(
					f,
					"primitives-rs crypto: associated data size {actual}, max {max}"
				)
			}
			Self::AllocationFailed => {
				write!(f, "primitives-rs crypto: output allocation failed")
			}
			Self::DecryptionFailed => write!(f, "primitives-rs crypto: decryption failed"),
		}
	}
}

impl StdError for DecryptError {}

impl From<AllocationFailedError> for DecryptError {
	fn from(_: AllocationFailedError) -> Self {
		Self::AllocationFailed
	}
}

/// Failures of deriving a keyset or a child keyset.
#[derive(Debug)]
pub enum KeysetError {
	/// A keyset must contain at least one key.
	EmptyKeyset,
	/// A keyset contained the same key more than once.
	DuplicateKey {
		/// Latest-first key index where the duplicate was found.
		index: usize,
	},
	/// A keyset contained more keys than [`MAX_KEYSET_KEYS`](crate::crypto::keysets::MAX_KEYSET_KEYS).
	TooManyKeys {
		/// Maximum accepted key count.
		max: usize,
	},
	/// A purpose must not be empty.
	EmptyPurpose,
	/// A purpose exceeded [`MAX_PURPOSE_LEN`](crate::crypto::keysets::MAX_PURPOSE_LEN).
	PurposeTooLong {
		/// Actual purpose byte length.
		actual: usize,
		/// Maximum accepted purpose byte length.
		max: usize,
	},
	/// A purpose must use visible, non-slash ASCII bytes.
	InvalidPurposeByte {
		/// Zero-based byte index where validation failed.
		index: usize,
		/// Invalid byte value.
		byte: u8,
	},
	/// Output allocation failed.
	AllocationFailed,
}

impl fmt::Display for KeysetError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::EmptyKeyset => write!(f, "primitives-rs crypto: at least one key is required"),
			Self::DuplicateKey { index } => {
				write!(f, "primitives-rs crypto: duplicate key at index {index}")
			}
			Self::TooManyKeys { max } => {
				write!(f, "primitives-rs crypto: keyset accepts at most {max} keys")
			}
			Self::EmptyPurpose => write!(f, "primitives-rs crypto: purpose is empty"),
			Self::PurposeTooLong { actual, max } => {
				write!(
					f,
					"primitives-rs crypto: purpose length {actual}, max {max}"
				)
			}
			Self::InvalidPurposeByte { index, byte } => {
				write!(
					f,
					"primitives-rs crypto: purpose byte at index {index} is not visible non-slash ASCII: {byte}"
				)
			}
			Self::AllocationFailed => {
				write!(f, "primitives-rs crypto: output allocation failed")
			}
		}
	}
}

impl StdError for KeysetError {}

impl From<AllocationFailedError> for KeysetError {
	fn from(_: AllocationFailedError) -> Self {
		Self::AllocationFailed
	}
}

/// Failures of parsing a [`Key32`](crate::crypto::Key32) from its text form.
#[derive(Debug, Eq, PartialEq)]
pub enum Key32Error {
	/// The text was not exactly [`KEY32_TEXT_LEN`] bytes long.
	InvalidLength {
		/// Actual text byte length.
		actual: usize,
	},
	/// The text was not the canonical unpadded URL-safe Base64 encoding of
	/// 32 bytes: it contained a byte outside the alphabet, or its final
	/// character carried non-zero unused bits.
	InvalidEncoding,
}

impl fmt::Display for Key32Error {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::InvalidLength { actual } => {
				write!(
					f,
					"primitives-rs crypto: key text length {actual}, want {KEY32_TEXT_LEN} (the unpadded URL-safe Base64 text of {KEY32_LEN} bytes)"
				)
			}
			Self::InvalidEncoding => write!(
				f,
				"primitives-rs crypto: key text is not canonical unpadded URL-safe Base64"
			),
		}
	}
}

impl StdError for Key32Error {}
