//! Owned secret byte buffers with explicit access and zeroizing destruction.
//!
//! Enable the `secret-bytes` feature to use these buffers without encryption
//! dependencies.
//!
//! ```
//! use primitives_rs::secret_bytes::SecretBytes;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let token = SecretBytes::generate_random(48)?;
//! assert_eq!(token.len(), 48);
//! let mut copied = SecretBytes::from_slice(b"secret input")?;
//! copied.expose_secret_mut()[0] = b'S';
//! # Ok(())
//! # }
//! ```

use std::error::Error as StdError;
use std::fmt;

use subtle::{Choice, ConstantTimeEq};
use zeroize::{Zeroize, Zeroizing};

use crate::random::secure_bytes::{RandomError, fill_random_bytes};

/// An owned byte buffer that zeroizes its allocation on drop.
///
/// `Debug` reports only the length. Access to the contents requires
/// [`expose_secret`](SecretBytes::expose_secret) or
/// [`expose_secret_mut`](SecretBytes::expose_secret_mut). Copies that the
/// application makes require their own protection and clearing.
pub struct SecretBytes {
	secret: Zeroizing<Vec<u8>>,
}

impl SecretBytes {
	/// Copies a slice into a new zeroizing buffer.
	///
	/// The caller retains responsibility for the original slice.
	pub fn from_slice(bytes: &[u8]) -> Result<Self, AllocationFailedError> {
		let mut buffer = allocate_byte_buffer(bytes.len())?;
		buffer.extend_from_slice(bytes);
		Ok(Self::from_vec(buffer))
	}

	/// Takes ownership of a byte buffer and zeroizes its contents on drop.
	///
	/// This transfers the existing buffer without copying its bytes. Copies that the
	/// application made before this transfer require their own protection.
	pub fn from_vec(bytes: Vec<u8>) -> Self {
		Self {
			secret: Zeroizing::new(bytes),
		}
	}

	/// Allocates a zeroizing buffer of exactly `size` zero bytes.
	pub fn zeroed(size: usize) -> Result<Self, AllocationFailedError> {
		let mut buffer = allocate_byte_buffer(size)?;
		buffer.resize(size, 0);
		Ok(Self::from_vec(buffer))
	}

	/// Generates exactly `size` cryptographically secure random bytes.
	///
	/// An empty buffer succeeds. Allocation and entropy failures are distinct
	/// errors, and a partially filled buffer is zeroized before it is released.
	pub fn generate_random(size: usize) -> Result<Self, SecretBytesGenerationError> {
		let mut bytes = Self::zeroed(size)?;
		fill_random_bytes(bytes.expose_secret_mut())?;
		Ok(bytes)
	}

	/// Returns the buffer length.
	pub fn len(&self) -> usize {
		self.secret.len()
	}

	/// Returns whether the buffer is empty.
	pub fn is_empty(&self) -> bool {
		self.secret.is_empty()
	}

	/// Explicitly exposes the secret bytes.
	pub fn expose_secret(&self) -> &[u8] {
		self.secret.as_slice()
	}

	/// Explicitly exposes the mutable secret bytes.
	pub fn expose_secret_mut(&mut self) -> &mut [u8] {
		self.secret.as_mut_slice()
	}
}

impl fmt::Debug for SecretBytes {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("SecretBytes")
			.field("len", &self.len())
			.finish()
	}
}

/// A byte-buffer allocation failed or its requested capacity was too large.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AllocationFailedError;

impl fmt::Display for AllocationFailedError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("primitives-rs: byte buffer allocation failed")
	}
}

impl StdError for AllocationFailedError {}

/// A failure to allocate or generate a secret byte buffer.
#[derive(Debug)]
pub enum SecretBytesGenerationError {
	/// The buffer could not be allocated.
	AllocationFailed(AllocationFailedError),
	/// The platform's secure random source failed.
	Random(RandomError),
}

impl fmt::Display for SecretBytesGenerationError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::AllocationFailed(error) => error.fmt(f),
			Self::Random(error) => {
				write!(f, "primitives-rs: secret byte generation failed: {error}")
			}
		}
	}
}

impl StdError for SecretBytesGenerationError {
	fn source(&self) -> Option<&(dyn StdError + 'static)> {
		match self {
			Self::AllocationFailed(error) => Some(error),
			Self::Random(error) => Some(error),
		}
	}
}

impl From<AllocationFailedError> for SecretBytesGenerationError {
	fn from(error: AllocationFailedError) -> Self {
		Self::AllocationFailed(error)
	}
}

impl From<RandomError> for SecretBytesGenerationError {
	fn from(error: RandomError) -> Self {
		Self::Random(error)
	}
}

/// A fixed-size owned byte array that zeroizes its contents when dropped and supports
/// constant-time comparison through [`subtle::ConstantTimeEq`].
///
/// Its `Debug` implementation writes a redacted placeholder without exposing the secret bytes.
/// `PartialEq` is intentionally omitted so that callers compare secret values explicitly through
/// [`subtle::ConstantTimeEq`].
#[derive(Clone)]
pub struct SecretByteArray<const LEN: usize> {
	bytes: [u8; LEN],
}

/// A 32-byte secret capability token with zeroizing storage and constant-time comparison.
pub type SecretToken32 = SecretByteArray<32>;

/// A byte slice did not have the exact length `LEN` that [`SecretByteArray`] requires.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SecretByteLengthError {
	/// Expected byte count (`LEN`).
	pub expected: usize,
	/// Actual slice length that the caller supplied.
	pub actual: usize,
}

impl fmt::Display for SecretByteLengthError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(
			f,
			"secret byte slice has length {}, expected {}",
			self.actual, self.expected
		)
	}
}

impl StdError for SecretByteLengthError {}

impl<const LEN: usize> SecretByteArray<LEN> {
	/// Wraps a fixed-length byte array in zeroizing storage.
	pub const fn from_array(bytes: [u8; LEN]) -> Self {
		Self { bytes }
	}

	/// Generates `LEN` cryptographically secure random bytes.
	///
	/// Any partially written bytes are zeroized if the platform random source fails.
	pub fn generate() -> Result<Self, RandomError> {
		let mut secret = Self { bytes: [0_u8; LEN] };
		fill_random_bytes(&mut secret.bytes)?;
		Ok(secret)
	}

	/// Copies an exact-length byte slice into zeroizing fixed-size storage.
	pub fn from_slice(bytes: &[u8]) -> Result<Self, SecretByteLengthError> {
		if bytes.len() != LEN {
			return Err(SecretByteLengthError {
				expected: LEN,
				actual: bytes.len(),
			});
		}
		let mut secret = Self { bytes: [0_u8; LEN] };
		secret.bytes.copy_from_slice(bytes);
		Ok(secret)
	}

	/// Explicitly exposes the fixed-length secret byte array.
	pub const fn expose_secret(&self) -> &[u8; LEN] {
		&self.bytes
	}

	/// Explicitly exposes the mutable fixed-length secret byte array.
	pub const fn expose_secret_mut(&mut self) -> &mut [u8; LEN] {
		&mut self.bytes
	}
}

impl<const LEN: usize> ConstantTimeEq for SecretByteArray<LEN> {
	fn ct_eq(&self, other: &Self) -> Choice {
		self.bytes.ct_eq(&other.bytes)
	}
}

impl<const LEN: usize> Zeroize for SecretByteArray<LEN> {
	fn zeroize(&mut self) {
		self.bytes.zeroize();
	}
}

impl<const LEN: usize> Drop for SecretByteArray<LEN> {
	fn drop(&mut self) {
		self.bytes.zeroize();
	}
}

impl<const LEN: usize> fmt::Debug for SecretByteArray<LEN> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("SecretByteArray([REDACTED])")
	}
}

pub(crate) fn allocate_byte_buffer(capacity: usize) -> Result<Vec<u8>, AllocationFailedError> {
	let mut bytes = Vec::new();
	bytes
		.try_reserve_exact(capacity)
		.map_err(|_| AllocationFailedError)?;
	Ok(bytes)
}
