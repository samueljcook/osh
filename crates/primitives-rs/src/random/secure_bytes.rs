//! Cryptographically secure bytes from the platform's random source.
//!
//! Enable the `random` feature to use these operations without encryption
//! dependencies. Browser WASM applications must enable `getrandom` 0.3's
//! `wasm_js` feature to select its Web Crypto backend.
//!
//! ```
//! use primitives_rs::random::secure_bytes::{fill_random_bytes, generate_random_byte_array};
//!
//! # fn main() -> Result<(), primitives_rs::random::secure_bytes::RandomError> {
//! let nonce: [u8; 24] = generate_random_byte_array()?;
//! let mut buffer = [0_u8; 64];
//! fill_random_bytes(&mut buffer)?;
//! # Ok(())
//! # }
//! ```

use std::error::Error as StdError;
use std::fmt;

use zeroize::{Zeroize, Zeroizing};

/// An error from the platform's secure random source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RandomError {
	source: getrandom::Error,
}

impl RandomError {
	/// Wraps a `getrandom` error.
	pub fn new(source: getrandom::Error) -> Self {
		Self { source }
	}

	/// Returns the underlying `getrandom` error.
	pub fn getrandom_error(&self) -> &getrandom::Error {
		&self.source
	}
}

impl fmt::Display for RandomError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{}", self.source)
	}
}

impl StdError for RandomError {}

/// Fills the entire buffer with cryptographically secure random bytes.
///
/// On failure, the entire destination is zeroized before the error is returned,
/// including any bytes that the random source wrote before failing. An empty
/// destination succeeds without calling the platform random source.
pub fn fill_random_bytes(bytes: &mut [u8]) -> Result<(), RandomError> {
	fill_random_bytes_with_source(bytes, &mut |destination| {
		getrandom::fill(destination).map_err(RandomError::new)
	})
}

/// Generates a fixed-size array of cryptographically secure random bytes.
///
/// The caller owns the returned bytes and must protect and clear them if they
/// contain a secret. The temporary generation buffer is zeroized on return.
pub fn generate_random_byte_array<const SIZE: usize>() -> Result<[u8; SIZE], RandomError> {
	let mut bytes = Zeroizing::new([0_u8; SIZE]);
	fill_random_bytes(bytes.as_mut())?;
	Ok(*bytes)
}

pub(crate) fn fill_random_bytes_with_source(
	bytes: &mut [u8],
	random_fill_fn: &mut impl FnMut(&mut [u8]) -> Result<(), RandomError>,
) -> Result<(), RandomError> {
	if let Err(error) = random_fill_fn(bytes) {
		bytes.zeroize();
		return Err(error);
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn partial_entropy_failure_clears_the_entire_destination() {
		let mut destination = [0x55_u8; 32];
		let source = RandomError::new(getrandom::Error::UNSUPPORTED);
		let result = fill_random_bytes_with_source(&mut destination, &mut |bytes| {
			bytes[..7].fill(0xa5);
			Err(source)
		});

		assert_eq!(result, Err(source));
		assert_eq!(destination, [0_u8; 32]);
	}

	#[test]
	fn successful_entropy_is_returned_without_alteration() {
		let mut destination = [0_u8; 9];
		fill_random_bytes_with_source(&mut destination, &mut |bytes| {
			for (index, byte) in bytes.iter_mut().enumerate() {
				*byte = index as u8;
			}
			Ok(())
		})
		.expect("entropy");

		assert_eq!(destination, [0, 1, 2, 3, 4, 5, 6, 7, 8]);
	}
}
