//! KMAC256 from NIST SP 800-185 over RustCrypto's zeroizing cSHAKE256.
//!
//! The output length is part of KMAC's input. A 32-byte output is not the prefix
//! of a 64-byte output. This module implements KMAC, not KMACXOF.

use crate::crypto::{Key32, OutputLengthError};
use cshake::digest::{ExtendableOutput, Update, XofReader};
use zeroize::Zeroizing;

const RATE: usize = 136;
/// The maximum output size with a bit length that fits KMAC's 64-bit encoding.
pub const MAX_OUTPUT_LEN: usize = if usize::BITS < 64 {
	usize::MAX
} else {
	(u64::MAX / 8) as usize
};

/// An incremental KMAC256 with a 256-bit key and zeroizing keyed state.
pub struct Authenticator(cshake::CShake256);

impl Authenticator {
	/// Starts KMAC256 with a key and an application customization string.
	pub fn new(key: &Key32, customization: &[u8]) -> Self {
		let mut state = cshake::CShake256::new_with_function_name(b"KMAC", customization);
		// bytepad(encode_string(K), 136) for a 32-byte key occupies one rate block.
		let mut encoded_key = Zeroizing::new([0_u8; RATE]);
		encoded_key[..5].copy_from_slice(&[1, 136, 2, 1, 0]);
		encoded_key[5..37].copy_from_slice(key.expose_secret());
		state.update(encoded_key.as_ref());
		Self(state)
	}

	/// Appends message bytes without inserting separators or lengths.
	pub fn update(&mut self, bytes: &[u8]) {
		self.0.update(bytes);
	}

	/// Consumes the message and fills the output with a length-bound KMAC tag.
	///
	/// An invalid output length leaves the output unchanged.
	pub fn finalize_into(self, output: &mut [u8]) -> Result<(), OutputLengthError> {
		let mut reader = self.finalize_reader(output.len())?;
		reader.read(output);
		Ok(())
	}

	pub(crate) fn finalize_reader(
		mut self,
		output_len: usize,
	) -> Result<cshake::CShake256Reader, OutputLengthError> {
		// The comparison is made in `u64` so that it has the same form on every pointer width.
		if output_len as u64 > u64::MAX / 8 {
			return Err(OutputLengthError {
				actual: output_len,
				max: MAX_OUTPUT_LEN,
			});
		}
		let bits = (output_len as u64) * 8;
		let bytes = bits.to_be_bytes();
		let first = bytes.iter().position(|&byte| byte != 0).unwrap_or(7);
		self.0.update(&bytes[first..]);
		self.0.update(&[(8 - first) as u8]);
		Ok(self.0.finalize_xof())
	}
}

impl std::fmt::Debug for Authenticator {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("Authenticator([redacted])")
	}
}
